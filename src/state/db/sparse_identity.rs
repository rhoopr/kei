//! Durable sparse-source retry evidence. Stored keys are opaque to SQLite.

use async_trait::async_trait;
use chrono::{DateTime, TimeDelta, Utc};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use std::sync::Arc;

use super::SqliteStateDb;
use crate::state::error::StateError;

const INITIAL_RETRY_SECONDS: i64 = 60 * 60;
const MAX_RETRY_SECONDS: i64 = 24 * INITIAL_RETRY_SECONDS;
// Five doublings exceed the 24-hour cap; larger shifts are unnecessary.
const MAX_BACKOFF_SHIFT: u32 = 5;
const GENERATION_KEY: &str = "sparse_identity_generation";

#[derive(Clone, PartialEq, Eq, Hash)]
pub(crate) struct SparseSourceId(Box<str>);

impl SparseSourceId {
    pub(crate) fn new(value: &str) -> Self {
        Self(value.into())
    }
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for SparseSourceId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SparseSourceId(<redacted>)")
    }
}

/// Canonical, versioned evidence encoded only by the provider adapter.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct SparseEvidence(Box<str>);

impl SparseEvidence {
    pub(crate) fn new(value: String) -> Self {
        Self(value.into())
    }
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for SparseEvidence {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SparseEvidence(<redacted>)")
    }
}

/// Authoritative source deletion scoped to a complete provider delta snapshot.
/// Reuse at a different snapshot requires a complete source-change validation.
#[derive(Clone, PartialEq, Eq, Hash)]
pub(crate) struct SparseDeletionCheckpoint(Box<str>);

impl SparseDeletionCheckpoint {
    pub(crate) fn new(token: &str) -> Option<Self> {
        (!token.trim().is_empty()).then(|| Self(token.into()))
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }

    fn to_outcome(&self) -> String {
        serde_json::json!(["source_deleted_v1", self.0]).to_string()
    }

    fn from_outcome(outcome: Option<&str>) -> Result<Option<Self>, StateError> {
        let Some(encoded) = outcome.filter(|value| value.starts_with('[')) else {
            return Ok(None);
        };
        let (kind, token): (String, String) = serde_json::from_str(encoded)
            .map_err(|_invalid_evidence| invariant("invalid sparse deletion evidence"))?;
        if kind != "source_deleted_v1" {
            return Err(invariant("unsupported sparse deletion evidence"));
        }
        Self::new(&token)
            .map(Some)
            .ok_or_else(|| invariant("empty sparse deletion checkpoint"))
    }
}

impl std::fmt::Debug for SparseDeletionCheckpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SparseDeletionCheckpoint(<redacted>)")
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct SparseGeneration(i64);

#[derive(Clone)]
pub(crate) struct SparseIdentity {
    pub(crate) library: String,
    pub(crate) source: SparseSourceId,
    pub(crate) original: SparseEvidence,
    pub(crate) observed: SparseEvidence,
    pub(crate) lookup: Option<SparseEvidence>,
    pub(crate) generation: SparseGeneration,
    pub(crate) origin: Option<String>,
    pub(crate) first_seen: DateTime<Utc>,
    pub(crate) last_attempt: Option<DateTime<Utc>>,
    pub(crate) next_retry: Option<DateTime<Utc>>,
    pub(crate) deletion_checkpoint: Option<SparseDeletionCheckpoint>,
}

impl SparseIdentity {
    pub(crate) fn matches_scope(&self, scope: &str) -> bool {
        match self.origin.as_deref() {
            None | Some("historical-v1") => true,
            Some(encoded) => serde_json::from_str::<(String, String)>(encoded)
                .is_ok_and(|(kind, stored)| kind == "current-v1" && stored == scope),
        }
    }

    pub(crate) fn proof(&self) -> SparseIdentityProof {
        SparseIdentityProof {
            library: self.library.clone(),
            source: self.source.clone(),
            generation: self.generation,
            target: None,
        }
    }
}

/// A source was resolved in this execution. Not a persisted completion claim.
#[must_use]
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct SparseIdentityProof {
    library: String,
    source: SparseSourceId,
    generation: SparseGeneration,
    target: Option<SparseTargetState>,
}

#[derive(Clone)]
pub(crate) struct SparseTargetState {
    library: String,
    child: String,
    master: String,
    evidence: String,
    validate_files: Option<Arc<SparseFileValidation>>,
    current_receipts: Vec<super::download_receipts::DownloadReceipt>,
    config_fences: Vec<(String, Option<String>)>,
}
type SparseFileValidation = dyn Fn() -> Result<(), StateError> + Send + Sync;
impl PartialEq for SparseTargetState {
    fn eq(&self, other: &Self) -> bool {
        self.library == other.library
            && self.child == other.child
            && self.master == other.master
            && self.evidence == other.evidence
            && self.current_receipts == other.current_receipts
            && self.config_fences == other.config_fences
    }
}
impl Eq for SparseTargetState {}
impl SparseTargetState {
    pub(crate) fn current_coverage(
        receipts: Vec<super::download_receipts::DownloadReceipt>,
        config_fences: Vec<(String, Option<String>)>,
    ) -> Result<Self, StateError> {
        let first = receipts
            .first()
            .ok_or_else(|| invariant("empty current target coverage"))?;
        if receipts.iter().any(|receipt| {
            receipt.scope != first.scope
                || receipt.library != first.library
                || receipt.child != first.child
                || receipt.master != first.master
                || receipt.account_key != first.account_key
                || receipt.provider_key != first.provider_key
        }) {
            return Err(invariant("mixed current target coverage"));
        }
        Ok(Self {
            library: first.library.clone(),
            child: first.child.clone(),
            master: first.master.clone(),
            evidence: String::new(),
            validate_files: None,
            current_receipts: receipts,
            config_fences,
        })
    }

    fn validate_current(&self, conn: &Connection) -> Result<(), StateError> {
        for receipt in &self.current_receipts {
            let path = receipt.native_path.to_path();
            let path_key = crate::fs_util::confined_path_key(&path)
                .map_err(|_invalid| invariant("invalid current target path"))?;
            super::primary_layout::guard_writer(conn, &path)?;
            super::legacy_preservation::guard_writer(conn, &path)?;
            if super::provider_generations::pending_destination_reservations_for_path(conn, &path)?
                .iter()
                .any(|reservation| reservation.destination_path_key.0 == path_key)
            {
                return Err(invariant(
                    "current target still has pending destination authority",
                ));
            }
            let competing:bool=conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM reconciliation_paths WHERE (requested_path_key=?1 OR destination_path_key=?1) AND (destination_path_key<>?1 OR library<>?2 OR id<>?3 OR version_size<>?4 OR provider_checksum IS NULL OR provider_checksum<>?5 OR provider_size IS NULL OR provider_size<>?6)) OR EXISTS(SELECT 1 FROM owned_temp_files WHERE publication IS NOT NULL AND json_extract(publication,'$.library')=?2 AND json_extract(publication,'$.id')=?3)",
                params![path_key,receipt.library,receipt.state_id,receipt.version_size,receipt.provider_checksum,i64::try_from(receipt.provider_size).map_err(|_invalid|invariant("invalid current target size"))?],|row|row.get(0))?;
            if competing {
                return Err(invariant("current target destination authority changed"));
            }
            if super::download_receipts::load(conn, &receipt.id()?)?.as_ref() != Some(receipt) {
                return Err(invariant("current target receipt changed"));
            }
        }
        for (key, expected) in &self.config_fences {
            let actual: Option<String> = conn
                .query_row("SELECT value FROM metadata WHERE key=?1", [key], |row| {
                    row.get(0)
                })
                .optional()?;
            if &actual != expected {
                return Err(invariant("current target configuration changed"));
            }
        }
        Ok(())
    }
    pub(crate) fn with_file_validation(mut self, validation: Arc<SparseFileValidation>) -> Self {
        self.validate_files = Some(validation);
        self
    }
}
impl SparseIdentityProof {
    pub(crate) fn with_target(mut self, target: SparseTargetState) -> Self {
        self.target = Some(target);
        self
    }
}

impl std::fmt::Debug for SparseIdentityProof {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SparseIdentityProof(<redacted>)")
    }
}

pub(crate) enum SparseAttemptOutcome {
    Unresolved(SparseEvidence),
    Inconclusive,
    Recovered,
    SourceDeleted(SparseDeletionCheckpoint),
}

#[async_trait]
pub(crate) trait SparseIdentityStore: Send + Sync {
    async fn sparse_identities(&self, library: &str) -> Result<Vec<SparseIdentity>, StateError>;
    async fn sparse_target_state(
        &self,
        _library: &str,
        _child: &str,
        _master: &str,
    ) -> Result<Option<SparseTargetState>, StateError> {
        Ok(None)
    }

    async fn observe_sparse_identity(
        &self,
        library: &str,
        source: &SparseSourceId,
        evidence: &SparseEvidence,
        now: DateTime<Utc>,
    ) -> Result<SparseIdentity, StateError>;
    async fn observe_current_sparse_identity(
        &self,
        library: &str,
        source: &SparseSourceId,
        evidence: &SparseEvidence,
        scope: &str,
        now: DateTime<Utc>,
    ) -> Result<SparseIdentity, StateError> {
        // An adapter without atomic scope admission must not return a retained
        // historical row as though it had accepted a current observation.
        let _ = (library, source, evidence, scope, now);
        Err(invariant("current sparse admission unavailable"))
    }

    async fn record_sparse_attempt(
        &self,
        identity: &SparseIdentity,
        outcome: SparseAttemptOutcome,
        now: DateTime<Utc>,
    ) -> Result<SparseIdentity, StateError>;
}

fn invariant(detail: &'static str) -> StateError {
    StateError::Invariant {
        operation: "sparse_identity",
        detail: detail.into(),
    }
}

// Keep generations monotonic even if a cleared source later reappears.
fn next_generation(tx: &Transaction<'_>) -> Result<i64, StateError> {
    let current: Option<String> = tx
        .query_row(
            "SELECT value FROM metadata WHERE key=?1",
            [GENERATION_KEY],
            |row| row.get(0),
        )
        .optional()?;
    let current = current
        .map(|value| {
            value
                .parse::<i64>()
                .map_err(|_invalid_counter| invariant("invalid sparse generation counter"))
        })
        .transpose()?
        .unwrap_or(0);
    let next = current
        .checked_add(1)
        .filter(|value| *value > 0)
        .ok_or_else(|| invariant("invalid sparse generation counter"))?;
    tx.execute("INSERT INTO metadata(key,value) VALUES (?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value", params![GENERATION_KEY, next.to_string()])?;
    Ok(next)
}

fn read_rows(
    conn: &Connection,
    library: &str,
    source: Option<&str>,
) -> Result<Vec<SparseIdentity>, StateError> {
    let mut stmt = conn.prepare("SELECT source_record_name, original_evidence, observed_evidence, lookup_evidence, generation, first_seen_at, next_retry_at, last_attempt_at, last_outcome, observation_origin FROM unresolved_sparse_identities WHERE epoch=1 AND library=?1 AND (?2 IS NULL OR source_record_name=?2) ORDER BY COALESCE(next_retry_at, first_seen_at), source_record_name")?;
    let rows = stmt.query_map(params![library, source], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, Option<String>>(3)?,
            row.get::<_, i64>(4)?,
            row.get::<_, i64>(5)?,
            row.get::<_, Option<i64>>(6)?,
            row.get::<_, Option<i64>>(7)?,
            row.get::<_, Option<String>>(8)?,
            row.get::<_, Option<String>>(9)?,
        ))
    })?;
    let mut identities = Vec::new();
    for row in rows {
        let (source, original, observed, lookup, generation, first, next, last, outcome, origin) =
            row?;
        if let Some(encoded) = origin
            .as_deref()
            .filter(|origin| *origin != "historical-v1")
        {
            let (kind, scope): (String, String) = serde_json::from_str(encoded)
                .map_err(|_invalid_origin| invariant("invalid sparse observation origin"))?;
            if kind != "current-v1" {
                return Err(invariant("unsupported sparse observation origin"));
            }
            validate_current_scope(conn, library, &scope)?;
        }
        if source.trim().is_empty() || generation < 1 {
            return Err(invariant("invalid sparse identity row"));
        }
        let first_seen = DateTime::from_timestamp(first, 0)
            .ok_or_else(|| invariant("invalid sparse retry timestamp"))?;
        let next_retry = next
            .map(|stamp| {
                DateTime::from_timestamp(stamp, 0)
                    .ok_or_else(|| invariant("invalid sparse retry timestamp"))
            })
            .transpose()?;
        identities.push(SparseIdentity {
            library: library.to_owned(),
            source: SparseSourceId::new(&source),
            original: SparseEvidence::new(original),
            observed: SparseEvidence::new(observed),
            lookup: lookup.map(SparseEvidence::new),
            generation: SparseGeneration(generation),
            origin,
            first_seen,
            last_attempt: last
                .map(|stamp| {
                    DateTime::from_timestamp(stamp, 0)
                        .ok_or_else(|| invariant("invalid sparse attempt timestamp"))
                })
                .transpose()?,
            next_retry,
            deletion_checkpoint: SparseDeletionCheckpoint::from_outcome(outcome.as_deref())?,
        });
    }
    Ok(identities)
}

/// Report provenance categories without turning historical associations into
/// missing selected files. Unknown origins remain explicitly unclassified.
pub(super) fn recorded_observation_counts(
    conn: &Connection,
) -> Result<(u64, u64, u64), StateError> {
    let rows: Vec<(i64, String, Option<String>)> = conn
        .prepare("SELECT epoch,library,observation_origin FROM unresolved_sparse_identities")?
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
        .collect::<Result<_, _>>()?;
    let (mut current, mut historical, mut unclassified) = (0, 0, 0);
    for (epoch, library, origin) in rows {
        if epoch == 0 {
            historical += 1;
        } else if epoch == 1
            && origin
                .as_deref()
                .and_then(|value| serde_json::from_str::<(String, String)>(value).ok())
                .is_some_and(|(kind, scope)| {
                    kind == "current-v1" && validate_current_scope(conn, &library, &scope).is_ok()
                })
        {
            current += 1;
        } else {
            unclassified += 1;
        }
    }
    Ok((current, historical, unclassified))
}

fn validate_current_scope(conn: &Connection, library: &str, scope: &str) -> Result<(), StateError> {
    let parsed: serde_json::Value = serde_json::from_str(scope)
        .map_err(|_invalid_scope| invariant("invalid sparse observation scope"))?;
    if parsed
        .pointer("/zone/zoneName")
        .and_then(serde_json::Value::as_str)
        != Some(library)
        || parsed
            .pointer("/zone/ownerRecordName")
            .and_then(serde_json::Value::as_str)
            .is_none_or(|s| s.trim().is_empty())
        || !matches!(
            parsed.get("database").and_then(serde_json::Value::as_str),
            Some("private" | "shared")
        )
    {
        return Err(invariant("invalid sparse observation scope"));
    }
    let owned: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM account_owner WHERE singleton=1)",
        [],
        |r| r.get(0),
    )?;
    if !owned {
        return Err(invariant("sparse observation needs an owned account"));
    }
    super::provider_generations::guard_library_scope(conn, scope, library)
}

impl SqliteStateDb {
    async fn observe_sparse_with_origin(
        &self,
        library: &str,
        source: &SparseSourceId,
        evidence: &SparseEvidence,
        scope: Option<String>,
        now: DateTime<Utc>,
    ) -> Result<SparseIdentity, StateError> {
        let (library, source, evidence) = (library.to_owned(), source.clone(), evidence.clone());
        self.with_conn_mut("observe_sparse_identity", move |conn| {
            // Acquire the writer slot before reading generation evidence so
            // another WAL writer cannot invalidate a deferred read snapshot.
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            if let Some(scope) = scope.as_deref() {
                validate_current_scope(&tx, &library, scope)?;
                tx.execute("INSERT OR IGNORE INTO metadata(key,value) VALUES (?1,?2)",
                    params![format!("download_ledger_scope:{library}"),scope])?;
            }
            if source.as_str().trim().is_empty() {
                if scope.is_some() {
                    tx.execute("INSERT INTO metadata(key,value) VALUES (?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                        params![crate::state::unresolved_identity_key(&library),crate::state::UNACCOUNTED_CURRENT_IDENTITY])?;
                    tx.commit()?;
                }
                return Err(invariant("empty sparse source identity"));
            }
            let origin = scope.as_ref().map(|scope| serde_json::json!(["current-v1",scope]).to_string());
            let generation = next_generation(&tx)?;
            tx.execute("INSERT INTO unresolved_sparse_identities (library, source_record_name, original_evidence, observed_evidence, generation, first_seen_at) VALUES (?1,?2,?3,?3,?5,?4) ON CONFLICT(epoch,library,source_record_name) DO UPDATE SET observed_evidence=excluded.observed_evidence, generation=excluded.generation, attempts=0, next_retry_at=NULL, last_outcome=NULL WHERE observed_evidence <> excluded.observed_evidence", params![library, source.as_str(), evidence.as_str(), now.timestamp(), generation])?;
            if let Some(origin) = &origin {
                tx.execute("UPDATE unresolved_sparse_identities SET observation_origin=?3,generation=?4 WHERE epoch=1 AND library=?1 AND source_record_name=?2 AND observation_origin IS NOT ?3",
                    params![library,source.as_str(),origin,generation])?;
            }
            tx.execute("INSERT INTO metadata (key,value) VALUES (?1,'1') ON CONFLICT(key) DO UPDATE SET value=CASE WHEN metadata.value=?2 THEN metadata.value ELSE '1' END", params![crate::state::unresolved_identity_key(&library),crate::state::UNACCOUNTED_CURRENT_IDENTITY])?;
            let row = read_rows(&tx, &library, Some(source.as_str()))?
                .into_iter()
                .next()
                .ok_or_else(|| invariant("sparse observation was not retained"))?;
            tx.commit()?;
            Ok(row)
        })
        .await
    }
}

#[async_trait]
impl SparseIdentityStore for SqliteStateDb {
    async fn sparse_target_state(
        &self,
        library: &str,
        child: &str,
        master: &str,
    ) -> Result<Option<SparseTargetState>, StateError> {
        let (library, child, master) = (library.to_owned(), child.to_owned(), master.to_owned());
        self.with_conn("sparse_target_state", move |conn| {
            target_state(conn, &library, &child, &master)
        })
        .await
    }

    async fn sparse_identities(&self, library: &str) -> Result<Vec<SparseIdentity>, StateError> {
        let library = library.to_owned();
        self.with_conn("sparse_identities", move |conn| {
            read_rows(conn, &library, None)
        })
        .await
    }

    async fn observe_sparse_identity(
        &self,
        library: &str,
        source: &SparseSourceId,
        evidence: &SparseEvidence,
        now: DateTime<Utc>,
    ) -> Result<SparseIdentity, StateError> {
        self.observe_sparse_with_origin(library, source, evidence, None, now)
            .await
    }

    async fn observe_current_sparse_identity(
        &self,
        library: &str,
        source: &SparseSourceId,
        evidence: &SparseEvidence,
        scope: &str,
        now: DateTime<Utc>,
    ) -> Result<SparseIdentity, StateError> {
        self.observe_sparse_with_origin(library, source, evidence, Some(scope.to_owned()), now)
            .await
    }

    async fn record_sparse_attempt(
        &self,
        identity: &SparseIdentity,
        outcome: SparseAttemptOutcome,
        now: DateTime<Utc>,
    ) -> Result<SparseIdentity, StateError> {
        let identity = identity.clone();
        self.with_conn_mut("record_sparse_attempt", move |conn| {
            // Acquire the writer slot before reading generation evidence so
            // another WAL writer cannot invalidate a deferred read snapshot.
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let attempts: Option<u32> = tx.query_row("SELECT attempts FROM unresolved_sparse_identities WHERE epoch=1 AND library=?1 AND source_record_name=?2 AND generation=?3", params![identity.library, identity.source.as_str(), identity.generation.0], |r| r.get(0)).optional()?;
            let attempts = attempts.ok_or_else(|| invariant("stale sparse retry result"))?;
            let (lookup, label, next) = match outcome {
                SparseAttemptOutcome::Unresolved(evidence) => {
                    let same = evidence == identity.observed;
                    let delay = INITIAL_RETRY_SECONDS.saturating_mul(1_i64 << attempts.min(MAX_BACKOFF_SHIFT)).min(MAX_RETRY_SECONDS);
                    let next = if same {
                        Some(now.checked_add_signed(TimeDelta::seconds(delay))
                            .ok_or_else(|| invariant("invalid sparse retry deadline"))?)
                    } else {
                        None
                    };
                    (Some(evidence), String::from(if same { "unresolved" } else { "changed" }), next)
                }
                SparseAttemptOutcome::Inconclusive => (None, "inconclusive".into(), None),
                SparseAttemptOutcome::Recovered => (None, "recovered".into(), None),
                SparseAttemptOutcome::SourceDeleted(checkpoint) => (None, checkpoint.to_outcome(), None),
            };
            let attempts = if label == "unresolved" {
                attempts.saturating_add(1)
            } else {
                0
            };
            let generation = next_generation(&tx)?;
            tx.execute("UPDATE unresolved_sparse_identities SET generation=?9,lookup_evidence=COALESCE(?4,lookup_evidence),last_outcome=?5,last_attempt_at=?6,attempts=?7,next_retry_at=?8 WHERE epoch=1 AND library=?1 AND source_record_name=?2 AND generation=?3", params![identity.library, identity.source.as_str(), identity.generation.0, lookup.as_ref().map(SparseEvidence::as_str), label, now.timestamp(), attempts, next.map(|t| t.timestamp()), generation])?;
            let row = read_rows(&tx, &identity.library, Some(identity.source.as_str()))?
                .into_iter()
                .next()
                .ok_or_else(|| invariant("sparse retry row disappeared"))?;
            tx.commit()?;
            Ok(row)
        })
        .await
    }
}

fn target_state(
    conn: &Connection,
    library: &str,
    child: &str,
    master: &str,
) -> Result<Option<SparseTargetState>, StateError> {
    let mapped: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM asset_master_mappings WHERE library=?1 AND asset_record_name=?2 AND master_record_name=?3)",
        params![library, child, master], |row| row.get(0))?;
    if !mapped || !super::legacy_preservation::child_receipts_ready(conn, library, child)? {
        return Ok(None);
    }
    validate_target_paths(conn, library, child)?;
    let dependencies = super::legacy_preservation::dependencies(conn, library, master)?;
    let mut statement = conn.prepare("SELECT key,value FROM metadata WHERE key IN ('enum_config_hash','config_hash','pending_enum_config_hash','pending_download_config_hash') ORDER BY key")?;
    let config = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let evidence = serde_json::json!([dependencies, config]).to_string();
    Ok(Some(SparseTargetState {
        library: library.into(),
        child: child.into(),
        master: master.into(),
        evidence,
        validate_files: None,
        current_receipts: Vec::new(),
        config_fences: Vec::new(),
    }))
}

// Repeat global authority checks inside the checkpoint transaction. Family
// snapshots alone cannot see a competing owner created in another library.
fn validate_target_paths(conn: &Connection, library: &str, child: &str) -> Result<(), StateError> {
    let mut statement = conn.prepare("SELECT library,id,local_path FROM assets WHERE local_path IS NOT NULL UNION SELECT library,id,local_path FROM asset_metadata_paths")?;
    let catalog = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let mut paths = conn.prepare(
        "SELECT local_path FROM assets WHERE library=?1 AND id=?2 AND local_path IS NOT NULL",
    )?;
    let paths = paths
        .query_map(params![library, child], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    for media in paths {
        for (path, sidecar) in [(media.clone(), false), (format!("{media}.xmp"), true)] {
            let path = std::path::Path::new(&path);
            super::primary_layout::guard_writer(conn, path)?;
            super::legacy_preservation::guard_writer(conn, path)?;
            let key = crate::fs_util::confined_path_key(path)
                .map_err(|_private_error| invariant("invalid sparse target path"))?;
            for (owner_library, owner_child, owner_path) in &catalog {
                let owner_key = crate::fs_util::confined_path_key(std::path::Path::new(owner_path))
                    .map_err(|_private_error| invariant("invalid sparse catalog path"))?;
                if owner_key == key && (sidecar || owner_library != library || owner_child != child)
                {
                    return Err(invariant("sparse target has competing path authority"));
                }
            }
            let reserved: bool = conn.query_row("SELECT EXISTS(SELECT 1 FROM reconciliation_paths WHERE (requested_path_key=?1 OR destination_path_key=?1) AND (?4 OR library<>?2 OR id<>?3))",
                params![key,library,child,sidecar], |row| row.get(0))?;
            if reserved {
                return Err(invariant("sparse target has competing path reservation"));
            }
        }
    }
    Ok(())
}

/// Remove only receipts covered by the same successful checkpoint transaction.
/// Missing or stale receipts fail closed, including a record absent from replay.
pub(super) fn clear_proven(
    tx: &Transaction<'_>,
    library: &str,
    proofs: &[SparseIdentityProof],
) -> Result<(), StateError> {
    clear_selected(tx, library, proofs, false)
}

pub(super) fn clear_current_proven(
    tx: &Transaction<'_>,
    library: &str,
    proofs: &[SparseIdentityProof],
) -> Result<(), StateError> {
    clear_selected(tx, library, proofs, true)
}

fn clear_selected(
    tx: &Transaction<'_>,
    library: &str,
    proofs: &[SparseIdentityProof],
    current_checkpoint: bool,
) -> Result<(), StateError> {
    let rows = read_rows(tx, library, None)?;
    let marker: Option<String> = tx
        .query_row(
            "SELECT value FROM metadata WHERE key=?1",
            [crate::state::unresolved_identity_key(library)],
            |row| row.get(0),
        )
        .optional()?;
    if marker.as_deref() == Some(crate::state::UNACCOUNTED_CURRENT_IDENTITY) {
        return Err(invariant(
            "current sparse observation has no usable source identity",
        ));
    }
    if current_checkpoint && marker.is_some() && rows.is_empty() {
        return Err(invariant("unattributed sparse marker has no current proof"));
    }
    let mut resolved = Vec::new();
    for identity in rows {
        let current = identity.proof();
        let proof = proofs
            .iter()
            .find(|proof| {
                proof.library == current.library
                    && proof.source == current.source
                    && proof.generation == current.generation
            })
            .ok_or_else(|| invariant("checkpoint lacks current sparse identity proof"))?;
        if let Some(target) = &proof.target {
            if target.current_receipts.is_empty() {
                if target_state(tx, &target.library, &target.child, &target.master)?.as_ref()
                    != Some(target)
                {
                    return Err(invariant("sparse target receipt state changed"));
                }
            } else {
                target.validate_current(tx)?;
            }
        }
        if let Some(target) = &proof.target {
            target
                .validate_files
                .as_ref()
                .ok_or_else(|| invariant("sparse target file proof missing"))?()?;
        }
        resolved.push(identity.source);
    }
    for source in resolved {
        tx.execute(
            "DELETE FROM unresolved_sparse_identities WHERE epoch=1 AND library=?1 AND source_record_name=?2",
            params![library, source.as_str()],
        )?;
    }
    if current_checkpoint {
        let retained: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM unresolved_sparse_identities WHERE epoch=1 AND library=?1)",
            [library],
            |r| r.get(0),
        )?;
        if !retained {
            tx.execute(
                "DELETE FROM metadata WHERE key=?1",
                [crate::state::unresolved_identity_key(library)],
            )?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{SparseAttemptOutcome, SparseEvidence, SparseIdentityStore, SparseSourceId};
    use crate::state::{CheckpointTransition, SqliteStateDb, unresolved_identity_key};
    use chrono::{TimeDelta, Utc};

    #[tokio::test]
    async fn sparse_target_path_authority_rechecks_competing_catalog_owner() {
        let db = SqliteStateDb::open_in_memory().unwrap();
        db.with_conn_mut("sparse_target_test", |conn| {
            conn.execute("INSERT INTO assets(library,id,version_size,checksum,filename,created_at,size_bytes,media_type,status,last_seen_at,local_path,local_checksum) VALUES ('SharedSync','target','original','provider','image.jpg',1,10,'photo','downloaded',1,'/photos/image.jpg','local')", [])?;
            super::validate_target_paths(conn, "SharedSync", "target")?;
            // New authority is outside the target family and was previously invisible.
            conn.execute("INSERT INTO assets(library,id,version_size,checksum,filename,created_at,size_bytes,media_type,status,last_seen_at,local_path,local_checksum) VALUES ('OtherSync','other','original','provider','image.jpg',1,10,'photo','downloaded',1,'/photos/image.jpg','local')", [])?;
            assert!(super::validate_target_paths(conn, "SharedSync", "target").is_err());
            conn.execute("UPDATE assets SET local_path='/photos/image.jpg.xmp' WHERE library='OtherSync'", [])?;
            assert!(super::validate_target_paths(conn, "SharedSync", "target").is_err());
            conn.execute("UPDATE assets SET local_path='/photos/unrelated.jpg' WHERE library='OtherSync'", [])?;
            super::validate_target_paths(conn, "SharedSync", "target")?;
            Ok(())
        }).await.unwrap();
    }

    #[tokio::test]
    async fn sparse_target_checkpoint_checks_callback_authority_and_pending_promotion_atomically() {
        use std::sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        };
        let db = SqliteStateDb::open_in_memory().unwrap();
        db.with_conn_mut("sparse_target_seed", |conn| {
            conn.execute("INSERT INTO assets(library,id,version_size,checksum,filename,created_at,size_bytes,media_type,status,last_seen_at,local_path,local_checksum,metadata_hash) VALUES ('SharedSync','target','original','provider','image.jpg',1,10,'photo','downloaded',1,'/photos/image.jpg','local','metadata')", [])?;
            conn.execute("INSERT INTO asset_master_mappings(library,asset_record_name,master_record_name,updated_at) VALUES ('SharedSync','target','master',1)", [])?;
            conn.execute("INSERT INTO asset_metadata_capture_revisions(library,asset_id,revision,updated_at) VALUES ('SharedSync','target',?1,1)", [crate::state::METADATA_CAPTURE_REVISION])?;
            Ok(())
        }).await.unwrap();
        db.set_metadata("config_hash", "old").await.unwrap();
        db.set_metadata("pending_download_config_hash", "new")
            .await
            .unwrap();
        db.set_metadata("sync_token:PrimarySync", "before")
            .await
            .unwrap();
        let identity = db
            .observe_sparse_identity(
                "PrimarySync",
                &SparseSourceId::new("source"),
                &evidence("target"),
                Utc::now(),
            )
            .await
            .unwrap();
        let stable = Arc::new(AtomicBool::new(true));
        let check = Arc::clone(&stable);
        let target = db
            .sparse_target_state("SharedSync", "target", "master")
            .await
            .unwrap()
            .unwrap()
            .with_file_validation(Arc::new(move || {
                if check.load(Ordering::SeqCst) {
                    Ok(())
                } else {
                    Err(super::invariant("synthetic file proof drift"))
                }
            }));
        let proof = identity.proof().with_target(target);
        let transition = || CheckpointTransition {
            expected_metadata: Vec::new(),
            legacy_preservation_proofs: Vec::new(),
            legacy_config_hash: None,
            sparse_identity_proofs: vec![proof.clone()],
            metadata_updates: vec![
                ("sync_token:PrimarySync".into(), "after".into()),
                ("config_hash".into(), "new".into()),
            ],
            metadata_deletes: vec![
                "pending_download_config_hash".into(),
                unresolved_identity_key("PrimarySync"),
            ],
        };
        stable.store(false, Ordering::SeqCst);
        assert!(db.commit_checkpoint_transition(transition()).await.is_err());
        stable.store(true, Ordering::SeqCst);
        db.with_conn_mut("sparse_target_real_debt", |conn| {
            conn.execute("UPDATE assets SET metadata_write_failed_at=7 WHERE library='SharedSync' AND id='target'", [])?;
            Ok(())
        }).await.unwrap();
        assert!(db.commit_checkpoint_transition(transition()).await.is_err());
        assert_eq!(db.sparse_identities("PrimarySync").await.unwrap().len(), 1);
        assert_eq!(
            db.get_metadata("sync_token:PrimarySync")
                .await
                .unwrap()
                .as_deref(),
            Some("before")
        );
        db.with_conn_mut("sparse_target_debt_preserved", |conn| {
            let marker: i64 = conn.query_row("SELECT metadata_write_failed_at FROM assets WHERE library='SharedSync' AND id='target'", [], |row| row.get(0))?;
            assert_eq!(marker, 7);
            // Restore this synthetic prerequisite before the other independent faults.
            conn.execute("UPDATE assets SET metadata_write_failed_at=NULL WHERE library='SharedSync' AND id='target'", [])?;
            Ok(())
        }).await.unwrap();
        db.with_conn_mut("sparse_target_competitor", |conn| {
            conn.execute("INSERT INTO assets(library,id,version_size,checksum,filename,created_at,size_bytes,media_type,status,last_seen_at,local_path) VALUES ('OtherSync','other','original','provider','image.jpg',1,10,'photo','downloaded',1,'/photos/image.jpg')", [])?;
            Ok(())
        }).await.unwrap();
        assert!(db.commit_checkpoint_transition(transition()).await.is_err());
        db.with_conn_mut("sparse_target_failure", |conn| {
            conn.execute("DELETE FROM assets WHERE library='OtherSync'", [])?;
            conn.execute_batch("CREATE TRIGGER sparse_checkpoint_failure BEFORE UPDATE ON metadata WHEN NEW.key='config_hash' BEGIN SELECT RAISE(FAIL,'injected checkpoint failure'); END;")?;
            Ok(())
        }).await.unwrap();
        assert!(db.commit_checkpoint_transition(transition()).await.is_err());
        assert_eq!(db.sparse_identities("PrimarySync").await.unwrap().len(), 1);
        assert_eq!(
            db.get_metadata("sync_token:PrimarySync")
                .await
                .unwrap()
                .as_deref(),
            Some("before")
        );
        assert_eq!(
            db.get_metadata("pending_download_config_hash")
                .await
                .unwrap()
                .as_deref(),
            Some("new")
        );
        db.with_conn_mut("sparse_target_failure_remove", |conn| {
            conn.execute_batch("DROP TRIGGER sparse_checkpoint_failure;")?;
            Ok(())
        })
        .await
        .unwrap();
        db.commit_checkpoint_transition(transition()).await.unwrap();
        assert!(
            db.sparse_identities("PrimarySync")
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            db.get_metadata("sync_token:PrimarySync")
                .await
                .unwrap()
                .as_deref(),
            Some("after")
        );
        assert_eq!(
            db.get_metadata("config_hash").await.unwrap().as_deref(),
            Some("new")
        );
        assert!(
            db.get_metadata("pending_download_config_hash")
                .await
                .unwrap()
                .is_none()
        );
    }

    fn evidence(target: &str) -> SparseEvidence {
        SparseEvidence::new(format!(
            r#"[1,"{target}","SharedSync-private","private-owner"]"#
        ))
    }

    #[test]
    fn sparse_deletion_checkpoint_roundtrip_is_versioned_and_redacted() {
        use super::SparseDeletionCheckpoint;
        let checkpoint = SparseDeletionCheckpoint::new("private-token").unwrap();
        assert_eq!(
            SparseDeletionCheckpoint::from_outcome(Some(&checkpoint.to_outcome())).unwrap(),
            Some(checkpoint.clone())
        );
        assert!(!format!("{checkpoint:?}").contains("private-token"));
        for old in [
            None,
            Some("recovered"),
            Some("unresolved"),
            Some("inconclusive"),
        ] {
            assert_eq!(SparseDeletionCheckpoint::from_outcome(old).unwrap(), None);
        }
        for invalid in [
            "[",
            r#"["source_deleted_v2","token"]"#,
            r#"["source_deleted_v1"," "]"#,
        ] {
            assert!(SparseDeletionCheckpoint::from_outcome(Some(invalid)).is_err());
        }
    }

    #[tokio::test]
    async fn sparse_concurrent_connections_preserve_generations_and_retry_evidence() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.db");
        let seed = SqliteStateDb::open(&path).await.unwrap();
        seed.set_metadata("unrelated-checkpoint", "retained-token")
            .await
            .unwrap();
        drop(seed);
        let workers = 16;
        let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(workers));
        let mut tasks = Vec::new();
        for index in 0..workers {
            // Each writer has its own WAL connection, as scan-time arrivals do.
            let db = SqliteStateDb::open(&path).await.unwrap();
            let barrier = barrier.clone();
            tasks.push(tokio::spawn(async move {
                let source = SparseSourceId::new(&format!("concurrent-source-{index}"));
                let original = evidence(&format!("original-{index}"));
                let now = chrono::DateTime::from_timestamp(1_700_000_000, 0).unwrap();
                barrier.wait().await;
                let observed = db
                    .observe_sparse_identity("PrimarySync", &source, &original, now)
                    .await
                    .unwrap();
                let retried = db
                    .record_sparse_attempt(
                        &observed,
                        SparseAttemptOutcome::Unresolved(original.clone()),
                        now,
                    )
                    .await
                    .unwrap();
                assert_eq!(retried.original, original);
                assert_eq!(retried.lookup.as_ref(), Some(&original));
                assert_eq!(retried.next_retry, Some(now + TimeDelta::hours(1)));
                assert_ne!(observed.proof(), retried.proof());
                (observed.generation, retried.generation)
            }));
        }
        let mut generations = std::collections::HashSet::new();
        for task in tasks {
            let (observed, retried) = task.await.unwrap();
            assert!(generations.insert(observed.0));
            assert!(generations.insert(retried.0));
        }
        assert_eq!(generations.len(), workers * 2);
        let reopened = SqliteStateDb::open(&path).await.unwrap();
        let rows = reopened.sparse_identities("PrimarySync").await.unwrap();
        assert_eq!(rows.len(), workers);
        assert!(
            rows.iter()
                .all(|row| row.lookup.as_ref() == Some(&row.original))
        );
        assert!(
            reopened
                .get_metadata(&unresolved_identity_key("PrimarySync"))
                .await
                .unwrap()
                .is_some()
        );
        assert_eq!(
            reopened
                .get_metadata("unrelated-checkpoint")
                .await
                .unwrap()
                .as_deref(),
            Some("retained-token")
        );
    }

    #[tokio::test]
    async fn sparse_retry_survives_restart_and_preserves_original_evidence() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.db");
        let source = SparseSourceId::new("private-source");
        let original = evidence("original-target");
        let changed = evidence("changed-target");
        let now = chrono::DateTime::from_timestamp(1_700_000_000, 0).unwrap();
        let db = SqliteStateDb::open(&path).await.unwrap();
        let mut row = db
            .observe_sparse_identity("PrimarySync", &source, &original, now)
            .await
            .unwrap();
        let initial_proof = row.proof();
        for hours in [1, 2, 4, 8, 16, 24, 24] {
            row = db
                .record_sparse_attempt(
                    &row,
                    SparseAttemptOutcome::Unresolved(original.clone()),
                    now,
                )
                .await
                .unwrap();
            assert_eq!(row.next_retry, Some(now + TimeDelta::hours(hours)));
            assert_eq!(row.original, original);
            assert_eq!(row.lookup.as_ref(), Some(&original));
        }
        assert_ne!(row.proof(), initial_proof);
        drop(db);
        let db = SqliteStateDb::open(&path).await.unwrap();
        let replay = db
            .observe_sparse_identity("PrimarySync", &source, &original, now)
            .await
            .unwrap();
        assert_eq!(replay.proof(), row.proof());
        assert_eq!(replay.next_retry, row.next_retry);
        assert_eq!(db.get_summary().await.unwrap().unresolved_sparse_records, 1);
        let changed_row = db
            .observe_sparse_identity("PrimarySync", &source, &changed, now)
            .await
            .unwrap();
        assert_eq!(changed_row.original, original);
        assert_eq!(changed_row.observed, changed);
        assert_eq!(changed_row.lookup, Some(original));
        assert_eq!(changed_row.next_retry, None);
        assert!(
            db.record_sparse_attempt(&row, SparseAttemptOutcome::Recovered, now)
                .await
                .is_err()
        );
        let changed_lookup = db
            .record_sparse_attempt(
                &changed_row,
                SparseAttemptOutcome::Unresolved(evidence("third-target")),
                now,
            )
            .await
            .unwrap();
        assert_eq!(changed_lookup.next_retry, None);
        assert_eq!(changed_lookup.observed, changed);
        let transient = db
            .record_sparse_attempt(&changed_lookup, SparseAttemptOutcome::Inconclusive, now)
            .await
            .unwrap();
        assert_eq!(transient.next_retry, None);
        assert_eq!(transient.lookup, changed_lookup.lookup);
        assert_ne!(transient.proof(), changed_lookup.proof());
        let other = db
            .observe_sparse_identity("SharedSync-other", &source, &changed, now)
            .await
            .unwrap();
        assert_ne!(other.proof(), transient.proof());
        assert_eq!(db.get_summary().await.unwrap().unresolved_identity_zones, 2);
        for debug in [
            format!("{source:?}"),
            format!("{changed:?}"),
            format!("{:?}", row.proof()),
        ] {
            assert!(debug.contains("redacted"));
            assert!(!debug.contains("private"));
        }
    }

    #[tokio::test]
    async fn sparse_observation_and_marker_are_atomic() {
        let db = SqliteStateDb::open_in_memory().unwrap();
        db.with_conn_mut("test_trigger", |conn| {
            conn.execute_batch("CREATE TRIGGER reject_marker BEFORE INSERT ON metadata BEGIN SELECT RAISE(FAIL, 'injected marker failure'); END;")?;
            Ok(())
        })
        .await.unwrap();
        assert!(
            db.observe_sparse_identity(
                "PrimarySync",
                &SparseSourceId::new("source"),
                &evidence("target"),
                Utc::now()
            )
            .await
            .is_err()
        );
        assert!(
            db.sparse_identities("PrimarySync")
                .await
                .unwrap()
                .is_empty()
        );
        assert!(
            db.get_metadata(&unresolved_identity_key("PrimarySync"))
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn sparse_checkpoint_requires_all_current_scoped_receipts_and_rolls_back() {
        let db = SqliteStateDb::open_in_memory().unwrap();
        let now = Utc::now();
        let source = SparseSourceId::new("source");
        let row = db
            .observe_sparse_identity("PrimarySync", &source, &evidence("target"), now)
            .await
            .unwrap();
        let other = db
            .observe_sparse_identity("SharedSync-other", &source, &evidence("target"), now)
            .await
            .unwrap();
        db.set_metadata("sync_token:PrimarySync", "before")
            .await
            .unwrap();
        db.set_metadata("pending_enum_config_hash", "pending")
            .await
            .unwrap();
        let recovered = db
            .record_sparse_attempt(&row, SparseAttemptOutcome::Recovered, now)
            .await
            .unwrap();
        let additional = db
            .observe_sparse_identity(
                "PrimarySync",
                &SparseSourceId::new("absent-from-replay"),
                &evidence("target"),
                now,
            )
            .await
            .unwrap();
        let transition = |proofs| CheckpointTransition {
            expected_metadata: Vec::new(),
            legacy_preservation_proofs: Vec::new(),
            legacy_config_hash: None,
            sparse_identity_proofs: proofs,
            metadata_updates: vec![
                ("sync_token:PrimarySync".into(), "after".into()),
                ("enum_config_hash".into(), "new".into()),
            ],
            metadata_deletes: vec![
                "pending_enum_config_hash".into(),
                unresolved_identity_key("PrimarySync"),
            ],
        };
        for proofs in [
            vec![],
            vec![other.proof()],
            vec![row.proof(), additional.proof()],
            vec![recovered.proof()],
        ] {
            assert!(
                db.commit_checkpoint_transition(transition(proofs))
                    .await
                    .is_err()
            );
            assert_eq!(
                db.get_metadata("sync_token:PrimarySync")
                    .await
                    .unwrap()
                    .as_deref(),
                Some("before")
            );
            assert_eq!(
                db.get_metadata("pending_enum_config_hash")
                    .await
                    .unwrap()
                    .as_deref(),
                Some("pending")
            );
            assert!(db.get_metadata("enum_config_hash").await.unwrap().is_none());
            assert_eq!(db.sparse_identities("PrimarySync").await.unwrap().len(), 2);
            assert_eq!(db.get_summary().await.unwrap().unresolved_identity_zones, 2);
        }
        db.with_conn_mut("test_trigger", |conn| {
            conn.execute_batch("CREATE TRIGGER reject_sparse_clear BEFORE DELETE ON unresolved_sparse_identities BEGIN SELECT RAISE(FAIL, 'injected clear failure'); END;")?;
            Ok(())
        })
        .await.unwrap();
        assert!(
            db.commit_checkpoint_transition(transition(vec![
                recovered.proof(),
                additional.proof()
            ]))
            .await
            .is_err()
        );
        assert_eq!(
            db.get_metadata("sync_token:PrimarySync")
                .await
                .unwrap()
                .as_deref(),
            Some("before")
        );
        db.with_conn_mut("test_trigger", |conn| {
            conn.execute_batch("DROP TRIGGER reject_sparse_clear;")?;
            Ok(())
        })
        .await
        .unwrap();
        db.commit_checkpoint_transition(transition(vec![recovered.proof(), additional.proof()]))
            .await
            .unwrap();
        assert!(
            db.sparse_identities("PrimarySync")
                .await
                .unwrap()
                .is_empty()
        );
        let reappeared = db
            .observe_sparse_identity("PrimarySync", &source, &evidence("target"), now)
            .await
            .unwrap();
        assert_ne!(reappeared.proof(), recovered.proof());
        assert!(
            db.commit_checkpoint_transition(transition(vec![recovered.proof()]))
                .await
                .is_err()
        );
        db.commit_checkpoint_transition(transition(vec![reappeared.proof()]))
            .await
            .unwrap();
        assert_eq!(
            db.sparse_identities("SharedSync-other")
                .await
                .unwrap()
                .len(),
            1
        );
        assert!(
            db.get_metadata(&unresolved_identity_key("PrimarySync"))
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(
            db.get_metadata("sync_token:PrimarySync")
                .await
                .unwrap()
                .as_deref(),
            Some("after")
        );
    }
}

#[cfg(test)]
mod origin_tests;

#[cfg(test)]
impl SqliteStateDb {
    pub(crate) async fn test_current_coverage_checkpoint_fences(&self, child: &str) {
        use crate::state::CheckpointTransition;
        let receipt = self
            .with_conn("real completed coverage receipt", {
                let child = child.to_owned();
                move |conn| {
                    let id: String = conn.query_row(
                        "SELECT id FROM download_receipts WHERE child=?1 LIMIT 1",
                        [child],
                        |r| r.get(0),
                    )?;
                    Ok(super::download_receipts::load(conn, &id)?.unwrap())
                }
            })
            .await
            .unwrap();
        let path = receipt.native_path.to_path();
        let original = std::fs::read(&path).unwrap();
        let key = crate::fs_util::confined_path_key(&path).unwrap();
        let identity = self
            .observe_sparse_identity(
                "ProofSource",
                &SparseSourceId::new("proof-source"),
                &SparseEvidence("synthetic-proof-evidence".into()),
                Utc::now(),
            )
            .await
            .unwrap();
        self.set_metadata("sync_token:v27:ProofSource", "before")
            .await
            .unwrap();
        for fault in [
            "config",
            "receipt",
            "redirect",
            "competitor",
            "file",
            "checkpoint",
            "none",
        ] {
            let expected = self.get_metadata("config_hash").await.unwrap();
            let validation =
                crate::download::sparse_target::test_current_file_validation(&path).await;
            let target = SparseTargetState::current_coverage(
                vec![receipt.clone()],
                vec![("config_hash".into(), expected.clone())],
            )
            .unwrap()
            .with_file_validation(validation);
            let proof = identity.proof().with_target(target);
            {
                let conn = self
                    .acquire_lock("mutate after proof construction")
                    .unwrap();
                match fault {
                    "config"=>{conn.execute("INSERT INTO metadata(key,value) VALUES('config_hash','changed') ON CONFLICT(key) DO UPDATE SET value='changed'",[]).unwrap();}
                    "receipt"=>{conn.execute("DELETE FROM download_receipts WHERE id=?1",[receipt.id().unwrap()]).unwrap();}
                    "redirect"|"competitor"=>{
                        let destination=if fault=="redirect" {path.with_file_name("redirected.JPG")} else {path.clone()};
                        let destination_key=crate::fs_util::confined_path_key(&destination).unwrap();
                        conn.execute("INSERT INTO reconciliation_paths(library,id,version_size,requested_path_key,destination_path_key,destination_path,provider_checksum,provider_size) VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",params![if fault=="competitor" {"OtherLibrary"} else {&receipt.library},receipt.state_id,receipt.version_size,key,destination_key,destination.to_str().unwrap(),receipt.provider_checksum,receipt.provider_size as i64]).unwrap();
                    }
                    "file"=>std::fs::write(&path,b"external replacement").unwrap(),
                    "checkpoint"=>conn.execute_batch("CREATE TRIGGER coverage_token_failure BEFORE UPDATE ON metadata WHEN NEW.key='sync_token:v27:ProofSource' BEGIN SELECT RAISE(ABORT,'coverage token failure'); END;").unwrap(),
                    _=>{}
                }
            }
            let result = self
                .commit_checkpoint_transition(CheckpointTransition {
                    expected_metadata: Vec::new(),
                    legacy_preservation_proofs: Vec::new(),
                    legacy_config_hash: None,
                    sparse_identity_proofs: vec![proof],
                    metadata_updates: vec![("sync_token:v27:ProofSource".into(), "after".into())],
                    metadata_deletes: vec![crate::state::unresolved_identity_key("ProofSource")],
                })
                .await;
            if fault == "none" {
                result.unwrap();
                assert!(
                    self.sparse_identities("ProofSource")
                        .await
                        .unwrap()
                        .is_empty()
                );
            } else {
                assert!(result.is_err(), "{fault} must invalidate checkpoint");
                assert_eq!(
                    self.get_metadata("sync_token:v27:ProofSource")
                        .await
                        .unwrap()
                        .as_deref(),
                    Some("before"),
                    "{fault}"
                );
                assert_eq!(
                    self.sparse_identities("ProofSource").await.unwrap().len(),
                    1,
                    "{fault}"
                );
                assert!(
                    self.get_metadata(&crate::state::unresolved_identity_key("ProofSource"))
                        .await
                        .unwrap()
                        .is_some(),
                    "{fault}"
                );
            }
            let conn = self.acquire_lock("restore one synthetic mutation").unwrap();
            match fault {
                "config" => match expected {
                    Some(value) => {
                        conn.execute(
                            "UPDATE metadata SET value=?1 WHERE key='config_hash'",
                            [value],
                        )
                        .unwrap();
                    }
                    None => {
                        conn.execute("DELETE FROM metadata WHERE key='config_hash'", [])
                            .unwrap();
                    }
                },
                "receipt" => {
                    receipt.save(&conn).unwrap();
                }
                "redirect" | "competitor" => {
                    conn.execute(
                        "DELETE FROM reconciliation_paths WHERE requested_path_key=?1",
                        [&key],
                    )
                    .unwrap();
                }
                "file" => std::fs::write(&path, &original).unwrap(),
                "checkpoint" => conn
                    .execute_batch("DROP TRIGGER coverage_token_failure")
                    .unwrap(),
                _ => {}
            }
        }
        assert_eq!(std::fs::read(path).unwrap(), original);
    }
}
