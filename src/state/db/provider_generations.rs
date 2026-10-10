//! Bounded private selection roots and independently verified destinations.
//!
//! Rank coverage is observational. Neither these roots nor their page IDs own a
//! provider cursor, determine a current version, prove absence or retire debt.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;

use chrono::Utc;
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::asset_writes::upsert_asset_row;
use super::membership::refresh_asset_album_groupings_tx;
use super::provider_selection::{SelectionDecision, SelectionDestination, SelectionOutcome};
use super::rows::encode_asset_date;
use super::{SqliteStateDb, account};
use crate::state::{AssetRecord, error::StateError};

pub(crate) const MAX_GENERATION_BYTES: u64 = 512 * 1024 * 1024;
const MAX_CHUNK_BYTES: usize = 16 * 1024 * 1024;
// Original serialized inputs per validation operation, not an allocator or disk bound.
const MAX_VALIDATION_BYTES: usize = 512 * 1024 * 1024;
const MAX_OBSERVED_BYTES: usize = 64 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct GenerationSpec {
    pub(crate) format: u8,
    pub(crate) scope: String,
    pub(crate) zone: Value,
    pub(crate) config_hash: String,
    pub(crate) basis: String,
    pub(crate) profile: Value,
    pub(crate) metadata_enabled: bool,
    pub(crate) metadata_flags: u8,
}

#[derive(Clone, Debug)]
pub(crate) struct ActiveGeneration {
    pub(crate) id: String,
    pub(crate) spec: GenerationSpec,
    pub(crate) sealed: bool,
    pub(crate) checkpoint_ready: bool,
    pub(crate) checkpoint_veto: Option<String>,
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

#[derive(Debug)]
pub(crate) struct GenerationProjection {
    pub(crate) admitted: bool,
    pub(crate) reason: String,
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
    let mut hash = Sha256::new();
    let mut statement=conn.prepare("SELECT p.request_cursor,p.body_hash FROM provider_shadow_pages p JOIN provider_catalog_pages c ON c.page_id=p.id AND c.body_hash=p.body_hash AND c.projector_version=1 WHERE p.scope=?1 AND EXISTS(SELECT 1 FROM provider_catalog_records r WHERE r.page_id=p.id) ORDER BY p.request_cursor,p.body_hash")?;
    let mut rows = statement.query([scope])?;
    while let Some(row) = rows.next()? {
        for value in [row.get::<_, String>(0)?, row.get::<_, String>(1)?] {
            hash.update((value.len() as u64).to_le_bytes());
            hash.update(value.as_bytes());
        }
    }
    Ok(format!("{:x}", hash.finalize()))
}

fn validate_spec(spec: &GenerationSpec) -> Result<(), StateError> {
    let scope: Value = serde_json::from_str(&spec.scope)
        .map_err(|_invalid| StateError::ProviderSelectionInvalid)?;
    if spec.format != 1
        || spec.metadata_enabled != (spec.metadata_flags != 0)
        || spec.metadata_flags & !0b0011_1111 != 0
        || scope.get("database").and_then(Value::as_str) != Some("private")
        || spec.zone.get("ownerRecordName").and_then(Value::as_str) != Some("_defaultOwner")
        || spec.zone.get("zoneName") != scope.pointer("/zone/zoneName")
        || spec.zone.get("ownerRecordName") != scope.pointer("/zone/ownerRecordName")
        || [&spec.config_hash, &spec.basis]
            .iter()
            .any(|v| v.len() != 64 || !v.bytes().all(|b| b.is_ascii_hexdigit()))
        || spec.profile.get("coverage").and_then(Value::as_str) != Some("observed_selection_window")
    {
        return invalid();
    }
    let passes = spec
        .profile
        .get("passes")
        .and_then(Value::as_array)
        .ok_or(StateError::ProviderSelectionInvalid)?;
    if passes.is_empty()
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
    Ok(())
}

type DestinationProofRow = (
    Option<String>,
    Option<String>,
    bool,
    bool,
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
    let (intent,local,source):(String,String,String)=conn.query_row(
        "SELECT intent_hash,local_checksum,source_checksum FROM provider_active_destinations WHERE generation=?1 AND pass_key=?2 AND child=?3 AND version_size=?4 AND path=?5 AND admitted=1 AND verified_media=1",
        params![root.id,manifest.decision.pass_key,manifest.decision.child,destination.version_size,destination.path.key()?],
        |r|Ok((r.get(0)?,r.get(1)?,r.get(2)?)),
    )?;
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
    let hash = if media {
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

fn charge(conn: &Connection, additional: u64, capacity: u64) -> Result<(), StateError> {
    let used:i64=conn.query_row("SELECT COALESCE((SELECT SUM(charged_bytes) FROM provider_active_generations),0)+COALESCE((SELECT SUM(charged_bytes) FROM provider_selection_rank_pages),0)+COALESCE((SELECT SUM(charged_bytes) FROM provider_active_decisions),0)",[],|r|r.get(0))?;
    if additional
        > capacity.saturating_sub(
            u64::try_from(used).map_err(|_invalid| StateError::ProviderSelectionInvalid)?,
        )
    {
        return Err(StateError::ProviderSelectionFull);
    }
    Ok(())
}

fn pass<'a>(root: &'a ActiveGeneration, key: &str) -> Result<&'a Value, StateError> {
    root.spec
        .profile
        .get("passes")
        .and_then(Value::as_array)
        .and_then(|p| {
            p.iter()
                .find(|p| p.get("key").and_then(Value::as_str) == Some(key))
        })
        .ok_or(StateError::ProviderSelectionInvalid)
}

#[cfg(test)]
thread_local! {
    static RANK_DECODE_METRICS: std::cell::Cell<(usize, usize)> = const { std::cell::Cell::new((0, 0)) };
}

fn load_rank(
    conn: &Connection,
    root: &ActiveGeneration,
    page: &str,
) -> Result<(String, Value), StateError> {
    let (request_length, body_length): (i64, i64) = conn.query_row(
        "SELECT length(request),length(body) FROM provider_selection_rank_pages WHERE id=?1",
        [page],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    if [request_length, body_length]
        .into_iter()
        .any(|n| usize::try_from(n).map_or(true, |n| n > MAX_CHUNK_BYTES))
    {
        return invalid();
    }
    let (generation,key,request,request_hash,body,body_hash):(String,String,Vec<u8>,String,Vec<u8>,String)=conn.query_row("SELECT generation,pass_key,request,request_hash,body,body_hash FROM provider_selection_rank_pages WHERE id=?1",[page],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?)))?;
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
    let response = crate::icloud::photos::validated_rank_page(&request, &body)
        .map_err(|_invalid| StateError::ProviderSelectionInvalid)?;
    let records = response
        .get("records")
        .and_then(Value::as_array)
        .ok_or(StateError::ProviderSelectionInvalid)?;
    let mut statement = conn.prepare("SELECT ordinal,record_name,record_type,deleted FROM provider_selection_rank_records WHERE page_id=?1 ORDER BY ordinal")?;
    let mut rows = statement.query([page])?;
    for (ordinal, record) in records.iter().enumerate() {
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
            || row.get::<_, Option<String>>(2)?.as_deref()
                != record.get("recordType").and_then(Value::as_str)
            || row.get::<_, bool>(3)?
                != record
                    .get("deleted")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
        {
            return invalid();
        }
    }
    if rows.next()?.is_some() {
        return invalid();
    }
    Ok((key, response))
}

/// Compact raw/index proofs live only within one owning operation. Parsed pages
/// are released immediately. The existing evidence quota bounds all unique raw
/// inputs; this does not introduce a smaller historical eligibility threshold.
struct RankProofs<'a> {
    conn: &'a Connection,
    root: &'a ActiveGeneration,
    pages: HashMap<String, String>,
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
        if !self.pages.contains_key(&source.page_id) {
            let additional = rank_input_bytes(self.conn, &source.page_id)?;
            if additional > self.capacity.saturating_sub(self.bytes) {
                return Err(StateError::ProviderSelectionFull);
            }
            let (pass_key, _) = load_rank(self.conn, self.root, &source.page_id)?;
            self.pages.insert(source.page_id.clone(), pass_key);
            self.bytes += additional;
        }
        let valid: bool = self.conn.query_row("SELECT EXISTS(SELECT 1 FROM provider_selection_rank_records WHERE page_id=?1 AND ordinal=?2 AND record_name=?3)", params![source.page_id,source.ordinal,child], |r| r.get(0))?;
        if self.pages.get(&source.page_id).map(String::as_str) != Some(key) || !valid {
            return invalid();
        }
        Ok(())
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
        let (key, body) = load_rank(self.conn, self.root, page)?;
        let body = Arc::new(body);
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
    let query = if key == "global-frontier" {
        serde_json::json!({"list_type":"CPLAssetAndMasterByAssetDateWithoutHiddenOrDeleted","query_filter":null})
    } else {
        pass(root, key)?
            .get("scope")
            .cloned()
            .ok_or(StateError::ProviderSelectionInvalid)?
    };
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
    }
}

fn validate_decision_with_pages(
    conn: &Connection,
    root: &ActiveGeneration,
    manifest: &ActiveDecision,
    verify_body: bool,
    pages: &mut RankProofs<'_>,
) -> Result<(), StateError> {
    let decision = &manifest.decision;
    pass(root, &decision.pass_key)?;
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
        if !verify_body {
            let valid:bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM provider_selection_rank_records r JOIN provider_selection_rank_pages p ON p.id=r.page_id WHERE p.generation=?1 AND p.pass_key=?2 AND r.page_id=?3 AND r.ordinal=?4 AND r.record_name=?5)",params![root.id,decision.pass_key,source.page_id,source.ordinal,decision.child],|r|r.get(0))?;
            if !valid {
                return invalid();
            }
            continue;
        }
        pages.verify_source(source, &decision.pass_key, &decision.child)?;
    }
    let photo = match (&decision.confirmation, &decision.master) {
        (Some(bytes), Some(master)) if bytes.len() <= MAX_CHUNK_BYTES => Some(
            crate::icloud::photos::current_asset(bytes, &decision.child, master, &root.spec.zone)
                .map_err(|_invalid| StateError::ProviderSelectionInvalid)?,
        ),
        (None, None) if decision.outcome == SelectionOutcome::Deferred => None,
        _ => return invalid(),
    };
    if manifest.state_id != decision.child {
        let Some(master) = decision.master.as_deref() else {
            return invalid();
        };
        let valid:bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM legacy_master_state_owners WHERE library=?1 AND master_record_name=?2 AND asset_record_name=?3)",params![root.spec.zone.get("zoneName").and_then(Value::as_str),manifest.state_id,decision.child],|r|r.get(0))?;
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
    Ok(())
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
            Ok(())
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
            let id:Option<String>=conn.query_row("SELECT id FROM provider_active_generations WHERE scope=?1 AND config_hash=?2 AND basis=?3 ORDER BY created_at DESC,id DESC LIMIT 1",params![scope,config_hash,current],|r|r.get(0)).optional()?;
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
            let tx=conn.transaction_with_behavior(TransactionBehavior::Immediate)?;account::validate_authenticated(&tx,&owner)?;validate_spec(&spec)?;
            if basis(&tx,&spec.scope)?!=spec.basis {return Err(StateError::ProviderWorkConflict)}
            freeze_interrupted_groupings(&tx, &spec.scope)?;
            let bytes=encode(&spec)?;let (account,provider)=owner_keys(&tx)?;
            let id=format!("{:032x}",rand::random::<u128>());
            let charged=u64::try_from(bytes.len()+id.len()+account.len()+provider.len()+spec.scope.len()+spec.config_hash.len()+spec.basis.len()+128).map_err(|_invalid|StateError::ProviderSelectionFull)?;
            charge(&tx,charged,capacity)?;
            tx.execute("INSERT INTO provider_active_generations(id,account_key,provider_key,scope,config_hash,basis,specification,specification_hash,created_at,charged_bytes,metadata_enabled) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",params![id,account,provider,spec.scope,spec.config_hash,spec.basis,bytes,digest(&bytes),Utc::now().timestamp_millis(),i64::try_from(charged).map_err(|_invalid|StateError::ProviderSelectionFull)?,spec.metadata_enabled])?;
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
        self.with_conn_mut("capturing scoped selection rank page",move|conn|{
            let tx=conn.transaction_with_behavior(TransactionBehavior::Immediate)?;account::validate_authenticated(&tx,&owner)?;
            let root=load_generation(&tx,&generation)?;validate_rank_request(&root,&key,&request)?;
            let response=crate::icloud::photos::validated_rank_page(&request,&body).map_err(|_invalid|StateError::ProviderSelectionInvalid)?;
            let request=encode(&request)?;let request_hash=digest(&request);let body_hash=digest(&body);
            let id=digest(&encode(&(&generation,&key,&request_hash,&body_hash))?);
            let exists:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM provider_selection_rank_pages WHERE id=?1)",[&id],|r|r.get(0))?;
            if exists {load_rank(&tx,&root,&id)?;tx.commit()?;return Ok(())}
            if root.sealed {return invalid()}
            let records=response.get("records").and_then(Value::as_array).ok_or(StateError::ProviderSelectionInvalid)?;
            let charged=u64::try_from(request.len()+body.len()+generation.len()+key.len()+id.len()+records.iter().map(|r|r.get("recordName").and_then(Value::as_str).map_or(0,str::len)+r.get("recordType").and_then(Value::as_str).map_or(0,str::len)+id.len()+32).sum::<usize>()+256).map_err(|_invalid|StateError::ProviderSelectionFull)?;
            charge(&tx,charged,capacity)?;
            tx.execute("INSERT INTO provider_selection_rank_pages(id,generation,pass_key,request,request_hash,body,body_hash,charged_bytes) VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",params![id,generation,key,request,request_hash,body,body_hash,i64::try_from(charged).map_err(|_invalid|StateError::ProviderSelectionFull)?])?;
            for (ordinal,record) in records.iter().enumerate() {
                tx.execute("INSERT INTO provider_selection_rank_records(page_id,ordinal,record_name,record_type,deleted) VALUES(?1,?2,?3,?4,?5)",params![id,i64::try_from(ordinal).map_err(|_invalid|StateError::ProviderSelectionFull)?,record.get("recordName").and_then(Value::as_str),record.get("recordType").and_then(Value::as_str),record.get("deleted").and_then(Value::as_bool).unwrap_or(false)])?;
            }
            tx.commit()?;Ok(())
        }).await
    }

    pub(crate) async fn selection_rank_sources(
        &self,
        owner: account::AccountOwner,
        generation: String,
        key: String,
        child: String,
    ) -> Result<Vec<RankSource>, StateError> {
        self.with_conn("reading selection source links",move|conn|{
            account::validate_authenticated(conn,&owner)?;let root=load_generation(conn,&generation)?;pass(&root,&key)?;
            let rows=conn.prepare("SELECT r.page_id,r.ordinal FROM provider_selection_rank_records r JOIN provider_selection_rank_pages p ON p.id=r.page_id WHERE p.generation=?1 AND p.pass_key=?2 AND r.record_name=?3 ORDER BY r.page_id,r.ordinal")?.query_map(params![generation,key,child],|r|Ok(RankSource{page_id:r.get(0)?,ordinal:r.get(1)?}))?.collect::<Result<Vec<_>,_>>()?;Ok(rows)
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
    let length:i64=conn.query_row("SELECT length(manifest) FROM provider_active_decisions WHERE generation=?1 AND pass_key=?2 AND child=?3",params![root.id,key,child],|r|r.get(0))?;
    if usize::try_from(length).map_or(true, |n| n > MAX_CHUNK_BYTES) {
        return invalid();
    }
    let (bytes,hash,outcome):(Vec<u8>,String,String)=conn.query_row("SELECT manifest,manifest_hash,outcome FROM provider_active_decisions WHERE generation=?1 AND pass_key=?2 AND child=?3",params![root.id,key,child],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?;
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
    validate_decision_with_pages(conn, root, &manifest, verify_body, pages)?;
    validate_decision_rows(conn, root, &manifest)?;
    Ok(manifest)
}

fn admission_hash(
    conn: &Connection,
    root: &ActiveGeneration,
    key: &str,
    child: &str,
) -> Result<String, StateError> {
    let (manifest,admission,reason,attempts,deadline):(String,String,String,i64,i64)=conn.query_row("SELECT manifest_hash,admission,reason,attempts,next_retry_at FROM provider_active_decisions WHERE generation=?1 AND pass_key=?2 AND child=?3",params![root.id,key,child],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?)))?;
    Ok(digest(&encode(&(
        &root.id, key, child, manifest, admission, reason, attempts, deadline,
    ))?))
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
) -> Result<(), StateError> {
    let d = &manifest.decision;
    let stored:Option<String>=conn.query_row("SELECT admission_hash FROM provider_active_decisions WHERE generation=?1 AND pass_key=?2 AND child=?3",params![root.id,d.pass_key,d.child],|r|r.get(0))?;
    if stored.as_deref() != Some(admission_hash(conn, root, &d.pass_key, &d.child)?.as_str()) {
        return invalid();
    }

    let (admission,reason,attempts,deadline):(String,String,i64,i64)=conn.query_row(
        "SELECT admission,reason,attempts,next_retry_at FROM provider_active_decisions WHERE generation=?1 AND pass_key=?2 AND child=?3",
        params![root.id,d.pass_key,d.child],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?)),
    )?;
    let legal = match admission.as_str() {
        "admitted" => {
            d.outcome == SelectionOutcome::Selected
                && ((reason.is_empty() && attempts == 0 && deadline == 0)
                    || (reason == "current_confirmation_unavailable"
                        && (1..=32).contains(&attempts)
                        && deadline > 0))
        }
        "excluded" => {
            d.outcome == SelectionOutcome::Excluded
                && reason == d.reason
                && attempts == 0
                && deadline == 0
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
    let dates = match (&d.confirmation, &d.master) {
        (Some(body), Some(master)) => Some(
            crate::icloud::photos::current_asset(body, &d.child, master, &root.spec.zone)
                .map_err(|_invalid| StateError::ProviderSelectionInvalid)?,
        ),
        _ => None,
    };
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
        let valid:bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM provider_active_destinations WHERE generation=?1 AND pass_key=?2 AND child=?3 AND asset_id=?4 AND version_size=?5 AND path=?6 AND compat_path=?12 AND checksum=?7 AND size_bytes=?8 AND metadata_hash=?9 AND master=?10 AND library=?11 AND (verified_media=0 OR (admitted=1 AND length(local_checksum)=64 AND length(source_checksum)=64)))",params![root.id,d.pass_key,d.child,manifest.state_id,destination.version_size,destination.path.key()?,destination.checksum,i64::try_from(destination.size).map_err(|_invalid|StateError::ProviderSelectionInvalid)?,destination.metadata_hash,d.master,root.spec.zone.get("zoneName").and_then(Value::as_str),destination.path.to_path().to_string_lossy()],|r|r.get(0))?;
        if !valid {
            return invalid();
        }
        let photo = dates.as_ref().ok_or(StateError::ProviderSelectionInvalid)?;
        let normalized:bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM provider_active_destinations WHERE generation=?1 AND pass_key=?2 AND child=?3 AND version_size=?4 AND path=?5 AND admitted=?6 AND created_at IS ?7 AND added_at IS ?8)",params![root.id,d.pass_key,d.child,destination.version_size,destination.path.key()?,admission=="admitted",encode_asset_date(photo.created()),Some(encode_asset_date(photo.added_date()))],|r|r.get(0))?;
        if !normalized {
            return invalid();
        }
        let (group,intent,media,metadata,local,source,progress):DestinationProofRow=conn.query_row("SELECT grouping_hash,intent_hash,verified_media,verified_metadata,local_checksum,source_checksum,progress_hash FROM provider_active_destinations WHERE generation=?1 AND pass_key=?2 AND child=?3 AND version_size=?4 AND path=?5",params![root.id,d.pass_key,d.child,destination.version_size,destination.path.key()?],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?,r.get(6)?)))?;
        match (&group, &intent) {
            (None, None) if !root.sealed => {}
            (Some(group), Some(stored))
                if intent_hash(root, manifest, destination, group)? == *stored => {}
            _ => return invalid(),
        }
        if prepared_metadata(conn, root, manifest, destination)?.is_some()
            && (!media || metadata || !root.spec.metadata_enabled || intent.is_none())
        {
            return invalid();
        }
        if metadata && (!media || (root.spec.metadata_enabled && intent.is_none())) {
            return invalid();
        }
        if media {
            let (Some(local), Some(source), Some(stored)) = (local, source, progress) else {
                return invalid();
            };
            if source.len() != 64
                || local.len() != 64
                || !source
                    .bytes()
                    .chain(local.bytes())
                    .all(|b| b.is_ascii_hexdigit())
                || data_encoding::HEXLOWER
                    .decode(source.as_bytes())
                    .ok()
                    .map(|sha| data_encoding::BASE64.encode(&sha))
                    .as_deref()
                    != Some(destination.checksum.as_str())
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
        let page_ids:Vec<String>=conn.prepare("SELECT DISTINCT p.id FROM provider_selection_rank_records r JOIN provider_selection_rank_pages p ON p.id=r.page_id WHERE p.generation=?1 AND r.record_name=?2 ORDER BY p.id LIMIT 65")?.query_map(params![root.id,master],|r|r.get(0))?.collect::<Result<_,_>>()?;
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
                    && !crate::icloud::photos::same_selected_facts(previous, &asset)
                {
                    return Ok(None);
                }
                observed = Some(asset);
            }
        }
    }
    Ok(observed)
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
            crate::icloud::photos::current_asset(body, &d.child, master, &root.spec.zone)
                .map_err(|_invalid| StateError::ProviderSelectionInvalid)?,
        ),
        _ => None,
    };
    let mut versions = HashSet::new();
    for record in records {
        let photo = photo.as_ref().ok_or(StateError::ProviderSelectionInvalid)?;
        if record.library.as_ref()
            != root
                .spec
                .zone
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
    pub(crate) async fn selection_attempt_due(
        &self,
        owner: account::AccountOwner,
        generation: String,
        key: String,
        child: String,
    ) -> Result<bool, StateError> {
        self.with_conn("checking selection retry deadline",move|conn|{
            account::validate_authenticated(conn,&owner)?;let root=load_generation(conn,&generation)?;
            let state:Option<(String,i64)>=conn.query_row("SELECT admission,next_retry_at FROM provider_active_decisions WHERE generation=?1 AND pass_key=?2 AND child=?3",params![generation,key,child],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
            if let Some((admission,deadline))=state {load_decision(conn,&root,&key,&child)?;Ok(admission=="excluded" || deadline<=Utc::now().timestamp())} else {Ok(true)}
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
            account::validate_authenticated(conn,&owner)?;
            let id:Option<String>=conn.query_row("SELECT g.id FROM provider_active_generations g WHERE g.scope=?1 AND g.config_hash=?2 AND g.id<>?3 AND (EXISTS(SELECT 1 FROM provider_active_decisions d WHERE d.generation=g.id AND d.admission='deferred' AND d.next_retry_at<=?4) OR EXISTS(SELECT 1 FROM provider_active_destinations w WHERE w.generation=g.id AND w.admitted=1 AND EXISTS(SELECT 1 FROM provider_active_decisions d WHERE d.generation=w.generation AND d.pass_key=w.pass_key AND d.child=w.child AND d.next_retry_at<=?4) AND (w.verified_media=0 OR (g.metadata_enabled=1 AND w.verified_metadata=0)))) ORDER BY g.last_replayed_at,g.created_at,g.id LIMIT 1",params![scope,config_hash,current,Utc::now().timestamp()],|r|r.get(0)).optional()?;
            id.map(|id|load_generation(conn,&id)).transpose()
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
            let query=|after:Option<&(String,String)>|->Result<Vec<(String,String,i64)>,StateError> {
                Ok(tx.prepare("SELECT d.pass_key,d.child,length(d.manifest) FROM provider_active_decisions d WHERE d.generation=?1 AND (?2 IS NULL OR (d.pass_key,d.child)>(?2,?3)) AND d.next_retry_at<=?4 AND (d.admission='deferred' OR EXISTS(SELECT 1 FROM provider_active_destinations w WHERE w.generation=d.generation AND w.pass_key=d.pass_key AND w.child=d.child AND w.admitted=1 AND (w.verified_media=0 OR (?5=1 AND w.verified_metadata=0)))) ORDER BY d.pass_key,d.child LIMIT 64")?.query_map(params![generation,after.map(|a|&a.0),after.map(|a|&a.1),Utc::now().timestamp(),root.spec.metadata_enabled],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?.collect::<Result<_,_>>()?)
            };
            let mut keys=query(after.as_ref())?;if keys.is_empty() && after.is_some() {keys=query(None)?;}
            let mut bytes=0_usize;let mut decisions=Vec::new();let mut last=None;
            let mut pages=RankProofs::new(&tx,&root);
            for (key,child,length) in keys {
                let length=usize::try_from(length).map_err(|_invalid|StateError::ProviderSelectionInvalid)?;
                if length>MAX_CHUNK_BYTES {return invalid()}
                if bytes.saturating_add(length)>MAX_CHUNK_BYTES {break}
                let decision=match load_decision_with_pages(&tx,&root,&key,&child,true,&mut pages) {
                    Ok(decision)=>decision,
                    Err(StateError::ProviderSelectionFull)=>break,
                    Err(error)=>return Err(error),
                };
                bytes+=length;decisions.push(decision);last=Some((key,child));
            }
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
            Ok(conn.query_row("SELECT EXISTS(SELECT 1 FROM provider_active_decisions WHERE generation=?1 AND admission<>'excluded' AND next_retry_at>?2) AND NOT EXISTS(SELECT 1 FROM provider_active_decisions WHERE generation=?1 AND admission='deferred' AND next_retry_at<=?2) AND NOT EXISTS(SELECT 1 FROM provider_active_destinations WHERE generation=?1 AND admitted=1 AND EXISTS(SELECT 1 FROM provider_active_decisions d WHERE d.generation=provider_active_destinations.generation AND d.pass_key=provider_active_destinations.pass_key AND d.child=provider_active_destinations.child AND d.next_retry_at<=?2) AND (verified_media=0 OR (?3=1 AND verified_metadata=0)))",params![generation,Utc::now().timestamp(),root.spec.metadata_enabled],|r|r.get(0))?)
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
        self.with_conn_mut("projecting selection destinations",move|conn|{
            let tx=conn.transaction_with_behavior(TransactionBehavior::Immediate)?;account::validate_authenticated(&tx,&owner)?;
            let root=load_generation(&tx,&generation)?;
            let changed_basis=basis(&tx,&root.spec.scope)?!=root.spec.basis;
            validate_decision(&tx,&root,&manifest,true)?;validate_records(&root,&manifest,&records)?;
            let d=&manifest.decision;let bytes=encode(&manifest)?;let hash=digest(&bytes);
            let existing:Option<(Vec<u8>,String)>=tx.query_row("SELECT manifest,admission FROM provider_active_decisions WHERE generation=?1 AND pass_key=?2 AND child=?3",params![generation,d.pass_key,d.child],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
            let mut resolving=false;
            if let Some((previous,admission))=&existing {
                let old=load_decision(&tx,&root,&d.pass_key,&d.child)?;
                resolving=admission=="deferred" && old.decision.outcome==SelectionOutcome::Deferred && old.decision.confirmation.is_none() && manifest.decision.confirmation.is_some() && !root.sealed && old.sources.iter().all(|source|manifest.sources.contains(source));
                if previous!=&bytes && !resolving {return Err(StateError::ProviderWorkConflict)}
                if resolving {
                    let observed=observed_asset(&tx,&root,&old)?;
                    let current=crate::icloud::photos::current_asset(manifest.decision.confirmation.as_deref().ok_or(StateError::ProviderSelectionInvalid)?,&d.child,d.master.as_deref().ok_or(StateError::ProviderSelectionInvalid)?,&root.spec.zone).map_err(|_invalid|StateError::ProviderSelectionInvalid)?;
                    if observed.as_ref().is_none_or(|observed|!crate::icloud::photos::same_selected_facts(observed,&current)) {
                        defer_attempt(&tx,&root,&d.pass_key,&d.child)?;tx.commit()?;
                        return Ok(GenerationProjection {admitted:false,reason:"original_selected_facts_unresolved".to_owned()});
                    }
                }
                if changed_basis && !resolving && admission=="deferred" && old.decision.confirmation.is_some() {
                    let Some(fresh)=fresh_confirmation.as_deref().filter(|body|body.len()<=MAX_CHUNK_BYTES) else {return Err(StateError::ProviderWorkConflict)};
                    let master=old.decision.master.as_deref().ok_or(StateError::ProviderSelectionInvalid)?;
                    let previous=crate::icloud::photos::current_asset(old.decision.confirmation.as_deref().ok_or(StateError::ProviderSelectionInvalid)?,&d.child,master,&root.spec.zone).map_err(|_invalid|StateError::ProviderSelectionInvalid)?;
                    let current=crate::icloud::photos::current_asset(fresh,&d.child,master,&root.spec.zone).map_err(|_invalid|StateError::ProviderSelectionInvalid)?;
                    if !crate::icloud::photos::same_selected_facts(&previous,&current) {return Err(StateError::ProviderWorkConflict)}
                }
                if admission!="deferred" {tx.commit()?;return Ok(GenerationProjection {admitted:admission=="admitted",reason:String::new()})}
            } else if root.sealed {return invalid()} else if changed_basis {return Err(StateError::ProviderWorkConflict)}
            let mut reason=d.reason.clone();
            if d.outcome==SelectionOutcome::Selected {
                let blocked:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM unattributed_legacy WHERE library=?1 AND asset_id IN (?2,?3))",params![root.spec.zone.get("zoneName").and_then(Value::as_str),manifest.state_id,d.master],|r|r.get(0))?;
                if blocked {reason="identity_conflict".to_owned()}
                for record in &records {
                    match super::provider_work::guard_projected_generation(&tx,record) {
                        Ok(())=>{},Err(StateError::ProviderWorkConflict)=>{reason="unfinished_generation_conflict".to_owned();break},Err(error)=>return Err(error),
                    }
                }
                if let Some(master)=&d.master {
                    match super::provider_work::guard_projected_mapping(&tx,root.spec.zone.get("zoneName").and_then(Value::as_str).ok_or(StateError::ProviderSelectionInvalid)?,&d.child,master) {
                        Ok(())=>{},Err(StateError::ProviderWorkConflict)=>reason="unfinished_mapping_conflict".to_owned(),Err(error)=>return Err(error),
                    }
                }
            }
            if d.outcome==SelectionOutcome::Selected
                && let Some(album)=pass(&root,&d.pass_key)?.pointer("/scope/name").and_then(Value::as_str).filter(|name|!name.is_empty()) {
                let conflict:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM provider_active_destinations w JOIN provider_active_generations g ON g.id=w.generation WHERE w.library=?1 AND w.asset_id=?2 AND w.admitted=1 AND g.metadata_enabled=1 AND w.verified_metadata=0 AND w.grouping_hash IS NOT NULL) AND NOT EXISTS(SELECT 1 FROM asset_albums WHERE library=?1 AND asset_id=?2 AND album_name=?3)",params![root.spec.zone.get("zoneName").and_then(Value::as_str),manifest.state_id,album],|r|r.get(0))?;
                if conflict {reason="unfinished_grouping_conflict".to_owned()}
            }
            let admitted=d.outcome==SelectionOutcome::Selected && reason.is_empty();
            let admission=if admitted {"admitted"} else if d.outcome==SelectionOutcome::Excluded {"excluded"} else {"deferred"};
            if existing.is_none() || resolving {
                let charged=u64::try_from(bytes.len()+manifest.sources.iter().map(|s|s.page_id.len()+generation.len()+d.pass_key.len()+d.child.len()+16).sum::<usize>()
                    + d.destinations.iter().map(|destination|destination.path.key().map(|path|path.len()+destination.path.to_path().to_string_lossy().len()+destination.checksum.len()+destination.metadata_hash.len()+generation.len()+d.pass_key.len()+d.child.len()+manifest.state_id.len()+destination.version_size.len()+d.master.as_ref().map_or(0,String::len)+root.spec.zone.get("zoneName").and_then(Value::as_str).map_or(0,str::len)+1024)).collect::<Result<Vec<_>,_>>()?.iter().sum::<usize>()+encode(&(&d.pass_key,&d.child))?.len()+256).map_err(|_invalid|StateError::ProviderSelectionFull)?;
                charge(&tx,charged,capacity)?;
                tx.execute("INSERT INTO provider_active_decisions(generation,pass_key,child,manifest,manifest_hash,outcome,admission,reason,charged_bytes) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9) ON CONFLICT(generation,pass_key,child) DO UPDATE SET manifest=excluded.manifest,manifest_hash=excluded.manifest_hash,outcome=excluded.outcome,admission=excluded.admission,reason=excluded.reason,attempts=CASE WHEN excluded.admission='deferred' THEN provider_active_decisions.attempts ELSE 0 END,next_retry_at=CASE WHEN excluded.admission='deferred' THEN provider_active_decisions.next_retry_at ELSE 0 END,charged_bytes=provider_active_decisions.charged_bytes+excluded.charged_bytes",params![generation,d.pass_key,d.child,bytes,hash,outcome_name(d.outcome),admission,reason,i64::try_from(charged).map_err(|_invalid|StateError::ProviderSelectionFull)?])?;
                for source in &manifest.sources {
                    tx.execute("INSERT OR IGNORE INTO provider_active_sources(generation,pass_key,child,page_id,ordinal) VALUES(?1,?2,?3,?4,?5)",params![generation,d.pass_key,d.child,source.page_id,source.ordinal])?;
                }
                let photo=match (&d.confirmation,&d.master) {(Some(body),Some(master))=>Some(crate::icloud::photos::current_asset(body,&d.child,master,&root.spec.zone).map_err(|_invalid|StateError::ProviderSelectionInvalid)?),_=>None};
                for destination in &d.destinations {
                    let photo=photo.as_ref().ok_or(StateError::ProviderSelectionInvalid)?;
                    tx.execute("INSERT INTO provider_active_destinations(generation,pass_key,child,library,asset_id,master,version_size,path,checksum,size_bytes,created_at,added_at,metadata_hash,admitted,compat_path) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15)",params![generation,d.pass_key,d.child,root.spec.zone.get("zoneName").and_then(Value::as_str),manifest.state_id,d.master,destination.version_size,destination.path.key()?,destination.checksum,i64::try_from(destination.size).map_err(|_invalid|StateError::ProviderSelectionInvalid)?,encode_asset_date(photo.created()),Some(encode_asset_date(photo.added_date())),destination.metadata_hash,admitted,destination.path.to_path().to_string_lossy()])?;
                }
            }
            refresh_admission_hash(&tx,&root,&d.pass_key,&d.child)?;
            if admitted {
                let library=root.spec.zone.get("zoneName").and_then(Value::as_str).ok_or(StateError::ProviderSelectionInvalid)?;
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
                if let Some(album)=pass(&root,&d.pass_key)?.pointer("/scope/name").and_then(Value::as_str).filter(|name|!name.is_empty()) {
                    tx.execute("INSERT OR IGNORE INTO asset_albums(library,asset_id,album_name,source) VALUES(?1,?2,?3,'icloud')",params![library,manifest.state_id,album])?;
                }
                refresh_asset_album_groupings_tx(&tx,library,&manifest.state_id,None)?;
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
            if basis(&tx,&root.spec.scope)?!=root.spec.basis {return Err(StateError::ProviderWorkConflict)}
            if !root.sealed {
                let assets:Vec<(String,String)>=tx.prepare("SELECT DISTINCT library,asset_id FROM provider_active_destinations WHERE generation=?1")?.query_map([&generation],|r|Ok((r.get(0)?,r.get(1)?)))?.collect::<Result<_,_>>()?;
                for (library,asset) in assets {
                    let hash=grouping_hash(&tx,&library,&asset)?;
                    let drift:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM provider_active_destinations WHERE generation=?1 AND library=?2 AND asset_id=?3 AND grouping_hash IS NOT NULL AND grouping_hash<>?4)",params![generation,library,asset,hash],|r|r.get(0))?;
                    if drift {return Err(StateError::ProviderWorkConflict)}
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
            let children:Vec<String>=conn.prepare("SELECT d.child FROM provider_active_decisions d WHERE d.generation=?1 AND d.pass_key=?2 AND (?3 IS NULL OR d.child>?3) AND d.next_retry_at<=?4 AND (d.admission='deferred' OR EXISTS(SELECT 1 FROM provider_active_destinations w WHERE w.generation=d.generation AND w.pass_key=d.pass_key AND w.child=d.child AND w.admitted=1 AND (w.verified_media=0 OR (?5=1 AND w.verified_metadata=0)))) ORDER BY d.child LIMIT ?6")?.query_map(params![generation,key,after,Utc::now().timestamp(),root.spec.metadata_enabled,limit],|r|r.get(0))?.collect::<Result<_,_>>()?;
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
            Ok(conn.query_row("SELECT EXISTS(SELECT 1 FROM provider_active_decisions WHERE generation=?1 AND admission='deferred' AND next_retry_at<=?2) OR EXISTS(SELECT 1 FROM provider_active_destinations WHERE generation=?1 AND admitted=1 AND EXISTS(SELECT 1 FROM provider_active_decisions d WHERE d.generation=provider_active_destinations.generation AND d.pass_key=provider_active_destinations.pass_key AND d.child=provider_active_destinations.child AND d.next_retry_at<=?2) AND (verified_media=0 OR (?3=1 AND verified_metadata=0)))",params![generation,Utc::now().timestamp(),root.spec.metadata_enabled],|r|r.get(0))?)
        }).await
    }
}

fn seal_hash(
    conn: &Connection,
    root: &ActiveGeneration,
    ready: bool,
    veto: Option<&str>,
) -> Result<String, StateError> {
    let mut hash = Sha256::new();
    hash.update(encode(&(&root.spec, ready, veto))?);
    let pages: Vec<String> = conn
        .prepare("SELECT id FROM provider_selection_rank_pages WHERE generation=?1 ORDER BY id")?
        .query_map([&root.id], |r| r.get(0))?
        .collect::<Result<_, _>>()?;
    if pages.is_empty() {
        return invalid();
    }
    for page in pages {
        load_rank(conn, root, &page)?;
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
    Ok(digest(&encode(&groups)?))
}

fn freeze_asset_intents(
    conn: &Connection,
    root: &ActiveGeneration,
    library: &str,
    asset: &str,
    group: &str,
) -> Result<(), StateError> {
    let keys:Vec<(String,String)>=conn.prepare("SELECT DISTINCT pass_key,child FROM provider_active_destinations WHERE generation=?1 AND library=?2 AND asset_id=?3 ORDER BY pass_key,child")?.query_map(params![root.id,library,asset],|r|Ok((r.get(0)?,r.get(1)?)))?.collect::<Result<_,_>>()?;
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
    let conflict:bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM provider_active_destinations w JOIN provider_active_generations g ON g.id=w.generation WHERE w.library=?1 AND w.asset_id=?2 AND w.admitted=1 AND g.metadata_enabled=1 AND w.verified_metadata=0 AND w.grouping_hash IS NOT NULL AND w.grouping_hash IS NOT ?3)",params![library,asset,current],|r|r.get(0))?;
    if conflict {
        Err(StateError::ProviderWorkConflict)
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
    let mut statement=conn.prepare("SELECT DISTINCT generation,pass_key,child FROM provider_active_destinations WHERE library=?1 AND (asset_id=?2 OR child=?2) ORDER BY generation,pass_key,child")?;
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

const UNFINISHED: &str =
    "w.admitted=1 AND (w.verified_media=0 OR (g.metadata_enabled=1 AND w.verified_metadata=0))";

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
        Err(StateError::ProviderWorkConflict)
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
        Err(StateError::ProviderWorkConflict)
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
        Err(StateError::ProviderWorkConflict)
    } else {
        Ok(())
    }
}

/// Called inside the existing verified-download transaction using the actual
/// native path and SHA-verified source checksum, never a lossy legacy path row.
/// A preservation-backed layout commit proves the selected destination using
/// pinned provider identity, independent local bytes, and prepared metadata.
pub(super) fn record_layout_destination(
    conn: &Connection,
    record: &AssetRecord,
    file: &super::primary_layout::LayoutFile,
    flags: u8,
) -> Result<(), StateError> {
    validate_identity_decisions(conn, &record.library, &record.id)?;
    let path = file.path.key()?;
    let rows:Vec<(String,String,String)>=conn.prepare("SELECT generation,pass_key,child FROM provider_active_destinations WHERE library=?1 AND asset_id=?2 AND version_size=?3 AND path=?4 AND checksum=?5 AND size_bytes=?6 AND metadata_hash IS ?7 AND admitted=1")?.query_map(params![record.library,record.id,record.version_size.as_str(),path,record.checksum.as_ref(),i64::try_from(record.size_bytes).map_err(|_error|StateError::ProviderSelectionInvalid)?,record.metadata.metadata_hash],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?.collect::<Result<_,_>>()?;
    for (generation, key, child) in rows {
        let root = load_generation(conn, &generation)?;
        if root.spec.metadata_flags & flags != root.spec.metadata_flags {
            return Err(StateError::ProviderWorkConflict);
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
        conn.execute("UPDATE provider_active_destinations SET prepared_checksum=NULL,prepared_size=NULL,prepared_hash=NULL,verified_media=1,verified_metadata=1,local_checksum=?6,source_checksum=?7 WHERE generation=?1 AND pass_key=?2 AND child=?3 AND version_size=?4 AND path=?5",params![generation,key,child,record.version_size.as_str(),path,local,file.source_checksum])?;
        refresh_progress_hash(conn, &root, &manifest, destination)?;
    }
    Ok(())
}

pub(super) fn record_verified_destination(
    conn: &Connection,
    library: &str,
    asset: &str,
    version: &str,
    path: &Path,
    local: &str,
    source: &str,
) -> Result<(), StateError> {
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
    let expected = data_encoding::BASE64.encode(
        &data_encoding::HEXLOWER
            .decode(source.as_bytes())
            .map_err(|_invalid| StateError::ProviderSelectionInvalid)?,
    );
    let rows:Vec<(String,String,String)>=conn.prepare("SELECT generation,pass_key,child FROM provider_active_destinations WHERE library=?1 AND asset_id=?2 AND version_size=?3 AND path=?4 AND admitted=1 AND checksum=?7 AND EXISTS(SELECT 1 FROM assets a WHERE a.library=?1 AND a.id=?2 AND a.version_size=?3 AND a.checksum=provider_active_destinations.checksum AND a.size_bytes=provider_active_destinations.size_bytes AND a.metadata_hash=provider_active_destinations.metadata_hash)")?.query_map(params![library,asset,version,path,local,source,expected],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?.collect::<Result<_,_>>()?;
    for (generation, key, child) in rows {
        let root = load_generation(conn, &generation)?;
        let manifest = load_decision(conn, &root, &key, &child)?;
        let destination = manifest
            .decision
            .destinations
            .iter()
            .find(|d| {
                d.version_size == version && d.path.key().ok().as_deref() == Some(path.as_str())
            })
            .ok_or(StateError::ProviderSelectionInvalid)?;
        conn.execute("UPDATE provider_active_destinations SET prepared_checksum=NULL,prepared_size=NULL,prepared_hash=NULL,verified_media=1,local_checksum=?6,source_checksum=?7 WHERE generation=?1 AND pass_key=?2 AND child=?3 AND version_size=?4 AND path=?5",params![generation,key,child,version,path,local,source])?;
        refresh_progress_hash(conn, &root, &manifest, destination)?;
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
    ) -> Result<bool, StateError> {
        let (local, size) = fingerprint;
        self.with_conn_mut("reusing exact destination publication",move|conn|{
            let tx=conn.transaction_with_behavior(TransactionBehavior::Immediate)?;account::validate_authenticated(&tx,&owner)?;
            let root=load_generation(&tx,&generation)?;let manifest=load_decision(&tx,&root,&key,&child)?;
            if !manifest.decision.destinations.iter().any(|d|d.path==path) {return invalid()}
            let native=path.to_path();
            let selected=manifest.decision.destinations.iter().find(|d|d.path==path).ok_or(StateError::ProviderSelectionInvalid)?;
            let previous:Option<(String,String,String)>=tx.query_row("SELECT w.generation,w.source_checksum,w.pass_key FROM provider_active_destinations w JOIN provider_active_generations g ON g.id=w.generation WHERE g.scope=?1 AND w.child=?3 AND w.version_size=?4 AND w.path=?5 AND w.checksum=?6 AND w.size_bytes=?7 AND w.master=?8 AND w.admitted=1 AND w.verified_media=1 AND (w.local_checksum=?9 OR (w.prepared_checksum=?9 AND w.prepared_size=?10)) ORDER BY g.created_at DESC LIMIT 1",params![root.spec.scope,key,child,selected.version_size,path.key()?,selected.checksum,i64::try_from(selected.size).map_err(|_invalid|StateError::ProviderSelectionInvalid)?,manifest.decision.master,local,i64::try_from(size).map_err(|_invalid|StateError::ProviderSelectionInvalid)?],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
            if let Some((previous,source,previous_key))=previous {
                let previous_root=load_generation(&tx,&previous)?;load_decision(&tx,&previous_root,&previous_key,&child)?;
                let prepared:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM provider_active_destinations WHERE generation=?1 AND pass_key=?2 AND child=?3 AND version_size=?4 AND path=?5 AND prepared_checksum=?6 AND prepared_size=?7)",params![previous,previous_key,child,selected.version_size,path.key()?,local,i64::try_from(size).map_err(|_invalid|StateError::ProviderSelectionInvalid)?],|r|r.get(0))?;
                if prepared {
                    let pending=metadata_receipt(&tx,&previous,&previous_key,&child,&selected.version_size,&path.key()?)?;
                    super::metadata::settle_selection_prepared_input(&tx,&pending,&(local.clone(),size))?;
                }
                record_verified_destination(&tx,root.spec.zone.get("zoneName").and_then(Value::as_str).ok_or(StateError::ProviderSelectionInvalid)?,&manifest.state_id,&selected.version_size,&native,&local,&source)?;
                tx.commit()?;return Ok(true)
            }
            let Some(text)=native.to_str() else {tx.commit()?;return Ok(false)};
            let receipt:Option<String>=tx.query_row("SELECT p.source_checksum FROM asset_metadata_paths p JOIN assets a USING(library,id,version_size) WHERE p.library=?1 AND p.id=?2 AND p.local_path=?3 AND p.local_checksum=?4 AND p.source_checksum IS NOT NULL AND a.metadata_hash IS ?5 AND p.version_size=?6",params![root.spec.zone.get("zoneName").and_then(Value::as_str),manifest.state_id,text,local,manifest.decision.destinations.iter().find(|d|d.path==path).map(|d|&d.metadata_hash),manifest.decision.destinations.iter().find(|d|d.path==path).map(|d|&d.version_size)],|r|r.get(0)).optional()?;
            if let Some(source)=receipt {
                record_verified_destination(&tx,root.spec.zone.get("zoneName").and_then(Value::as_str).ok_or(StateError::ProviderSelectionInvalid)?,&manifest.state_id,
                    &manifest.decision.destinations.iter().find(|d|d.path==path).ok_or(StateError::ProviderSelectionInvalid)?.version_size,&native,&local,&source)?;
                tx.commit()?;Ok(true)
            } else {tx.commit()?;Ok(false)}
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
                let keys:Vec<(String,String)>=tx.prepare("SELECT DISTINCT pass_key,child FROM provider_active_destinations WHERE generation=?1 AND admitted=1 AND verified_media=1")?.query_map([&generation],|r|Ok((r.get(0)?,r.get(1)?)))?.collect::<Result<_,_>>()?;
                for (key,child) in keys {
                    let manifest=load_decision(&tx,&root,&key,&child)?;
                    tx.execute("UPDATE provider_active_destinations SET verified_metadata=1 WHERE generation=?1 AND pass_key=?2 AND child=?3 AND admitted=1 AND verified_media=1",params![generation,key,child])?;
                    for destination in &manifest.decision.destinations {refresh_progress_hash(&tx,&root,&manifest,destination)?;}
                }
            }
            tx.commit()?;Ok(())
        }).await
    }
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
    let root = load_generation(conn, generation)?;
    let manifest = load_decision(conn, &root, key, child)?;
    let destination = manifest
        .decision
        .destinations
        .iter()
        .find(|d| d.version_size == version && d.path.key().ok().as_deref() == Some(path_key))
        .ok_or(StateError::ProviderSelectionInvalid)?;
    let (local,source,group):(String,String,String)=conn.query_row("SELECT local_checksum,source_checksum,grouping_hash FROM provider_active_destinations WHERE generation=?1 AND pass_key=?2 AND child=?3 AND version_size=?4 AND path=?5 AND admitted=1 AND verified_media=1 AND verified_metadata=0",params![generation,key,child,version,path_key],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?;
    let library = root
        .spec
        .zone
        .get("zoneName")
        .and_then(Value::as_str)
        .ok_or(StateError::ProviderSelectionInvalid)?;
    if grouping_hash(conn, library, &manifest.state_id)? != group {
        return Err(StateError::ProviderWorkConflict);
    }
    let mut asset=conn.query_row(&format!("SELECT {} FROM assets WHERE library=?1 AND id=?2 AND version_size=?3 AND checksum=?4 AND metadata_hash=?5 AND is_deleted=0",super::rows::ASSET_COLUMNS),params![library,manifest.state_id,version,destination.checksum,destination.metadata_hash],super::rows::row_to_asset_record)?;
    asset.local_path = Some(destination.path.to_path());
    asset.local_checksum = Some(local.clone());
    asset.download_checksum = Some(source.clone());
    asset.status = crate::state::AssetStatus::Downloaded;
    Ok(super::PendingMetadataRewrite {
        asset,
        capture_repair_receipt: None,
        source_checksum: Some(source),
        selection_receipt: Some(super::contracts::SelectionMetadataReceipt {
            generation: generation.to_owned(),
            pass_key: key.to_owned(),
            child: child.to_owned(),
            grouping_hash: group,
            metadata_flags: root.spec.metadata_flags,
            prepared: prepared_metadata(conn, &root, &manifest, destination)?,
        }),
    })
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
        return Err(StateError::ProviderWorkConflict);
    }
    let path = super::provider_selection::SelectionPath::from_path(
        pending
            .asset
            .local_path
            .as_deref()
            .ok_or(StateError::ProviderSelectionInvalid)?,
    )
    .key()?;
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
        return Err(StateError::ProviderWorkConflict);
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
    let changed=conn.execute("UPDATE provider_active_destinations SET prepared_checksum=NULL,prepared_size=NULL,prepared_hash=NULL,local_checksum=COALESCE(?8,local_checksum),verified_metadata=CASE WHEN ?9=1 THEN 1 ELSE verified_metadata END WHERE generation=?1 AND pass_key=?2 AND child=?3 AND version_size=?4 AND path=?5 AND admitted=1 AND verified_media=1 AND verified_metadata=0 AND local_checksum IS ?6 AND grouping_hash=?7 AND source_checksum IS ?10 AND metadata_hash IS ?11 AND EXISTS(SELECT 1 FROM assets a WHERE a.library=provider_active_destinations.library AND a.id=provider_active_destinations.asset_id AND a.version_size=provider_active_destinations.version_size AND a.checksum=provider_active_destinations.checksum AND a.metadata_hash=provider_active_destinations.metadata_hash AND a.created_at IS provider_active_destinations.created_at AND a.added_at IS provider_active_destinations.added_at AND a.is_deleted=0)",params![receipt.generation,receipt.pass_key,receipt.child,pending.asset.version_size.as_str(),path,pending.asset.local_checksum,receipt.grouping_hash,local,complete,pending.source_checksum,pending.asset.metadata.metadata_hash])?;
    if changed > 0 {
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
    if changed > 0 && (complete || local != pending.asset.local_checksum.as_deref()) {
        // A single physical write can satisfy multiple passes only when every
        // frozen input and writer dependency is exactly compatible. Different
        // native paths always retain independent publication obligations.
        let mut statement=conn.prepare("SELECT generation,pass_key,child FROM provider_active_destinations WHERE library=?1 AND asset_id=?2 AND version_size=?3 AND path=?4 AND checksum=?5 AND metadata_hash IS ?6 AND created_at IS ?7 AND added_at IS ?8 AND admitted=1 AND verified_media=1 AND verified_metadata=0 AND local_checksum IS ?9 AND source_checksum IS ?10 AND grouping_hash=?11 AND size_bytes=?12 ORDER BY generation,pass_key,child")?;
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
            conn.execute("UPDATE provider_active_destinations SET prepared_checksum=NULL,prepared_size=NULL,prepared_hash=NULL,local_checksum=COALESCE(?6,local_checksum),verified_metadata=CASE WHEN ?10=1 THEN 1 ELSE verified_metadata END WHERE generation=?1 AND pass_key=?2 AND child=?3 AND version_size=?4 AND path=?5 AND local_checksum IS ?7 AND source_checksum IS ?8 AND grouping_hash=?9 AND verified_media=1 AND verified_metadata=0",params![generation,key,child,pending.asset.version_size.as_str(),path,local,pending.asset.local_checksum,pending.source_checksum,receipt.grouping_hash,complete])?;
            let destination = alias
                .decision
                .destinations
                .iter()
                .find(|d| {
                    d.version_size == pending.asset.version_size.as_str()
                        && d.path.key().ok().as_deref() == Some(path.as_str())
                })
                .ok_or(StateError::ProviderSelectionInvalid)?;
            refresh_progress_hash(conn, &alias_root, &alias, destination)?;
        }
    }
    Ok(Some(changed > 0 && complete))
}

impl SqliteStateDb {
    /// Lossless paths from independently verified generations, limited before
    /// materialization. Current provider facts supplied by the caller decide
    /// version compatibility; generation order only schedules candidate reuse.
    pub(crate) async fn verified_selection_paths(
        &self,
        owner: account::AccountOwner,
        scope: String,
        key: String,
        child: String,
        master: String,
        version: String,
        resource: (String, u64),
    ) -> Result<Vec<super::provider_selection::SelectionPath>, StateError> {
        let (checksum, size) = resource;
        self.with_conn("reading independent native publication paths",move|conn|{
            account::validate_authenticated(conn,&owner)?;
            let candidates:Vec<(String,String,String)>=conn.prepare("SELECT w.generation,w.path,w.pass_key FROM provider_active_destinations w JOIN provider_active_generations g ON g.id=w.generation WHERE g.scope=?1 AND w.child=?3 AND w.master=?4 AND w.version_size=?5 AND w.checksum=?6 AND w.size_bytes=?7 AND w.admitted=1 AND w.verified_media=1 ORDER BY (w.pass_key=?2) DESC,g.created_at DESC,w.path LIMIT 32")?.query_map(params![scope,key,child,master,version,checksum,i64::try_from(size).map_err(|_invalid|StateError::ProviderSelectionInvalid)?],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?.collect::<Result<_,_>>()?;
            let mut paths=Vec::with_capacity(candidates.len());
            for (generation,path,previous_key) in candidates {
                let root=load_generation(conn,&generation)?;load_decision(conn,&root,&previous_key,&child)?;
                paths.push(serde_json::from_str(&path).map_err(|_invalid|StateError::ProviderSelectionInvalid)?);
            }
            Ok(paths)
        }).await
    }
}

impl SqliteStateDb {
    pub(crate) async fn selection_path_matches_local(
        &self,
        owner: account::AccountOwner,
        scope: String,
        key: String,
        child: String,
        path: super::provider_selection::SelectionPath,
        fingerprint: (String, u64),
    ) -> Result<bool, StateError> {
        let (local, size) = fingerprint;
        self.with_conn("checking independent publication fingerprint",move|conn|{
            account::validate_authenticated(conn,&owner)?;
            let previous:Option<(String,String)>=conn.query_row("SELECT w.generation,w.pass_key FROM provider_active_destinations w JOIN provider_active_generations g ON g.id=w.generation WHERE g.scope=?1 AND w.child=?3 AND w.path=?4 AND (w.local_checksum=?5 OR (w.prepared_checksum=?5 AND w.prepared_size=?6)) AND w.admitted=1 AND w.verified_media=1 ORDER BY (w.pass_key=?2) DESC,g.created_at DESC LIMIT 1",params![scope,key,child,path.key()?,local,i64::try_from(size).map_err(|_invalid|StateError::ProviderSelectionInvalid)?],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
            if let Some((generation,previous_key))=previous {let root=load_generation(conn,&generation)?;load_decision(conn,&root,&previous_key,&child)?;Ok(true)} else {Ok(false)}
        }).await
    }
}

impl SqliteStateDb {
    /// All retained unfinished obligations veto the existing source cursor,
    /// including old roots and deferred/backoff work. New healthy capture or
    /// publication never silently acknowledges that older debt.
    pub(crate) async fn unfinished_selection_debt(
        &self,
        owner: account::AccountOwner,
        scope: String,
    ) -> Result<bool, StateError> {
        self.with_conn("inspecting retained selection debt",move|conn|{
            let tx=conn.unchecked_transaction()?;
            account::validate_authenticated(&tx,&owner)?;
            let conn=&tx;
            validate_generation_headers(conn)?;
            let mut statement=conn.prepare("SELECT d.generation,d.pass_key,d.child FROM provider_active_decisions d JOIN provider_active_generations g ON g.id=d.generation WHERE g.scope=?1 ORDER BY d.generation,d.pass_key,d.child")?;
            let mut rows=statement.query([&scope])?;
            while let Some(row)=rows.next()? {
                let generation:String=row.get(0)?;let key:String=row.get(1)?;let child:String=row.get(2)?;
                let root=load_generation(conn,&generation)?;load_decision_with_sources(conn,&root,&key,&child,false)?;
            }
            Ok(conn.query_row("SELECT EXISTS(SELECT 1 FROM provider_active_decisions d JOIN provider_active_generations g ON g.id=d.generation WHERE g.scope=?1 AND d.admission='deferred') OR EXISTS(SELECT 1 FROM provider_active_destinations w JOIN provider_active_generations g ON g.id=w.generation WHERE g.scope=?1 AND w.admitted=1 AND (w.verified_media=0 OR (g.metadata_enabled=1 AND w.verified_metadata=0)))",[scope],|r|r.get(0))?)
        }).await
    }
}

impl SqliteStateDb {
    pub(crate) async fn selection_destination_verified(
        &self,
        owner: account::AccountOwner,
        generation: String,
        key: String,
        child: String,
        version: String,
    ) -> Result<bool, StateError> {
        self.with_conn("checking independent destination completion",move|conn|{
            account::validate_authenticated(conn,&owner)?;let root=load_generation(conn,&generation)?;load_decision(conn,&root,&key,&child)?;
            Ok(conn.query_row("SELECT EXISTS(SELECT 1 FROM provider_active_destinations WHERE generation=?1 AND pass_key=?2 AND child=?3 AND version_size=?4 AND admitted=1 AND verified_media=1)",params![generation,key,child,version],|r|r.get(0))?)
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
            if current.asset.local_checksum!=pending.asset.local_checksum || current.source_checksum!=pending.source_checksum || current.selection_receipt.as_ref().is_none_or(|r|r.grouping_hash!=receipt.grouping_hash || r.metadata_flags!=receipt.metadata_flags || r.prepared!=receipt.prepared) {return Ok(None)}
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
    let result = validate_decision_with_pages(conn, root, manifest, true, &mut pages);
    (result, RANK_DECODE_METRICS.with(std::cell::Cell::get))
}
