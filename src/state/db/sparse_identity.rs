//! Durable sparse-source retry evidence. Stored keys are opaque to SQLite.

use async_trait::async_trait;
use chrono::{DateTime, TimeDelta, Utc};
use rusqlite::{Connection, OptionalExtension, Transaction, params};

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
/// A different snapshot must revalidate the source, even if its link is unchanged.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct SparseDeletionCheckpoint(Box<str>);

impl SparseDeletionCheckpoint {
    pub(crate) fn new(token: &str) -> Option<Self> {
        (!token.trim().is_empty()).then(|| Self(token.into()))
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
    pub(crate) first_seen: DateTime<Utc>,
    pub(crate) last_attempt: Option<DateTime<Utc>>,
    pub(crate) next_retry: Option<DateTime<Utc>>,
    pub(crate) deletion_checkpoint: Option<SparseDeletionCheckpoint>,
}

impl SparseIdentity {
    pub(crate) fn proof(&self) -> SparseIdentityProof {
        SparseIdentityProof {
            library: self.library.clone(),
            source: self.source.clone(),
            generation: self.generation,
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
    async fn observe_sparse_identity(
        &self,
        library: &str,
        source: &SparseSourceId,
        evidence: &SparseEvidence,
        now: DateTime<Utc>,
    ) -> Result<SparseIdentity, StateError>;
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
    let mut stmt = conn.prepare("SELECT source_record_name, original_evidence, observed_evidence, lookup_evidence, generation, first_seen_at, next_retry_at, last_attempt_at, last_outcome FROM unresolved_sparse_identities WHERE library=?1 AND (?2 IS NULL OR source_record_name=?2) ORDER BY COALESCE(next_retry_at, first_seen_at), source_record_name")?;
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
        ))
    })?;
    let mut identities = Vec::new();
    for row in rows {
        let (source, original, observed, lookup, generation, first, next, last, outcome) = row?;
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

#[async_trait]
impl SparseIdentityStore for SqliteStateDb {
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
        let (library, source, evidence) = (library.to_owned(), source.clone(), evidence.clone());
        self.with_conn_mut("observe_sparse_identity", move |conn| {
            if source.as_str().trim().is_empty() {
                return Err(invariant("empty sparse source identity"));
            }
            let tx = conn.transaction()?;
            let generation = next_generation(&tx)?;
            tx.execute("INSERT INTO unresolved_sparse_identities (library, source_record_name, original_evidence, observed_evidence, generation, first_seen_at) VALUES (?1,?2,?3,?3,?5,?4) ON CONFLICT(library,source_record_name) DO UPDATE SET observed_evidence=excluded.observed_evidence, generation=excluded.generation, attempts=0, next_retry_at=NULL, last_outcome=NULL WHERE observed_evidence <> excluded.observed_evidence", params![library, source.as_str(), evidence.as_str(), now.timestamp(), generation])?;
            tx.execute("INSERT INTO metadata (key,value) VALUES (?1,'1') ON CONFLICT(key) DO UPDATE SET value='1'", [crate::state::unresolved_identity_key(&library)])?;
            let row = read_rows(&tx, &library, Some(source.as_str()))?
                .into_iter()
                .next()
                .ok_or_else(|| invariant("sparse observation was not retained"))?;
            tx.commit()?;
            Ok(row)
        })
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
            let tx = conn.transaction()?;
            let attempts: Option<u32> = tx.query_row("SELECT attempts FROM unresolved_sparse_identities WHERE library=?1 AND source_record_name=?2 AND generation=?3", params![identity.library, identity.source.as_str(), identity.generation.0], |r| r.get(0)).optional()?;
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
            tx.execute("UPDATE unresolved_sparse_identities SET generation=?9,lookup_evidence=COALESCE(?4,lookup_evidence),last_outcome=?5,last_attempt_at=?6,attempts=?7,next_retry_at=?8 WHERE library=?1 AND source_record_name=?2 AND generation=?3", params![identity.library, identity.source.as_str(), identity.generation.0, lookup.as_ref().map(SparseEvidence::as_str), label, now.timestamp(), attempts, next.map(|t| t.timestamp()), generation])?;
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

/// Remove only receipts covered by the same successful checkpoint transaction.
/// Missing or stale receipts fail closed, including a record absent from replay.
pub(super) fn clear_proven(
    tx: &Transaction<'_>,
    library: &str,
    proofs: &[SparseIdentityProof],
) -> Result<(), StateError> {
    for identity in read_rows(tx, library, None)? {
        if !proofs.contains(&identity.proof()) {
            return Err(invariant("checkpoint lacks current sparse identity proof"));
        }
    }
    tx.execute(
        "DELETE FROM unresolved_sparse_identities WHERE library=?1",
        [library],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{SparseAttemptOutcome, SparseEvidence, SparseIdentityStore, SparseSourceId};
    use crate::state::{CheckpointTransition, SqliteStateDb, unresolved_identity_key};
    use chrono::{TimeDelta, Utc};

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
