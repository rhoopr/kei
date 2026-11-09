//! One database-level sync epoch with lazy per-library enrollment.
//!
//! The epoch is created once by the schema migration. It never changes during
//! ordinary sync. Each selected library keeps exactly one record that owns its
//! phase, attempt nonce and retry deadline and, while a first-run baseline is
//! ready, the minimal binding to the exact staged token, observation descriptor
//! and fresh boundary. The observation descriptor remains the owner of
//! observation policy, plans and admission; the record binds to it rather than
//! copying it.
use super::SqliteStateDb;
use super::provider_generations::observation;
use crate::state::error::StateError;
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};

pub(crate) const SYNC_EPOCH_KEY: &str = "sync_epoch:v1";
#[cfg(test)]
pub(crate) const TEST_PREEXISTING_EPOCH_KEY: &str = "sync_epoch_test_preexisting:v1";
const LIBRARY_PREFIX: &str = "sync_epoch_library:v1:";
// Candidate-era per-attempt records. This version never writes them; they are
// read only to refuse an exact library that still carries that evidence.
const LEGACY_MARKER_PREFIX: &str = "source_revalidation_reset_pending:v1:";
const LEGACY_PROOF_PREFIX: &str = "source_revalidation_reset_proof:v1:";
const BASELINE_PREFIX: &str = "baseline_cursor:v27:";
/// Payload format owned by the per-library record. The durable key keeps its
/// enrollment prefix so an earlier payload is detected instead of orphaned.
const LIBRARY_VERSION: u8 = 2;

const INITIAL_RETRY_SECONDS: i64 = 60 * 60;
pub(crate) const MAX_RETRY_SECONDS: i64 = 24 * 60 * 60;
const MAX_BACKOFF_SHIFT: u32 = 5;

const QUALIFICATION_OUTCOME_PREFIX: &str = "qualification_in_progress:";
const QUALIFICATION_OUTCOME_VERSION: u8 = 1;
const QUALIFICATION_QUALIFIED_OUTCOME: &str = "indexing_inventory_qualified";
const QUALIFICATION_INCOMPLETE_PREFIX: &str = "qualification_incomplete:v1:";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum EpochOrigin {
    FreshInstall,
    ExistingUpgrade,
}

impl EpochOrigin {
    const fn as_str(self) -> &'static str {
        match self {
            Self::FreshInstall => "fresh_install",
            Self::ExistingUpgrade => "existing_upgrade",
        }
    }

    fn parse(value: &str) -> Result<Self, StateError> {
        match value {
            "fresh_install" => Ok(Self::FreshInstall),
            "existing_upgrade" => Ok(Self::ExistingUpgrade),
            _ => Err(StateError::SyncEpochMismatch),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SyncEpoch {
    pub(crate) version: u8,
    pub(crate) epoch: String,
    pub(crate) account_key: String,
    pub(crate) provider_key: String,
    pub(crate) origin: String,
    pub(crate) established_at: i64,
    pub(crate) quarantined_rounds: u64,
    pub(crate) retained_debts: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SyncEpochLibrary {
    pub(crate) version: u8,
    pub(crate) epoch: String,
    pub(crate) library: String,
    pub(crate) state: String,
    pub(crate) reason: String,
    pub(crate) attempt: String,
    pub(crate) attempts: u32,
    pub(crate) next_attempt_at: i64,
    pub(crate) last_outcome: Option<String>,
    pub(crate) baseline_scope: String,
    pub(crate) enrolled_at: i64,
    pub(crate) completed_at: Option<i64>,
    /// Present only while `state == "ready"`. The parent record owns the epoch,
    /// attempt and library identity; this binds them to one exact staged token,
    /// observation descriptor and fresh boundary.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) binding: Option<BaselineBinding>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct BaselineBinding {
    pub(crate) token_key: String,
    pub(crate) token_sha256: String,
    pub(crate) descriptor_key: String,
    pub(crate) descriptor_proof_hash: String,
    pub(crate) baseline_key: String,
    pub(crate) baseline_sha256: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum SyncEpochGate {
    Open,
    Baseline { attempt: String },
    Promote { attempt: String },
    Defer { reason: &'static str },
    Blocked { reason: &'static str },
}

pub(crate) struct EpochCompletion {
    pub(crate) library: String,
    pub(crate) attempt: String,
    pub(crate) baseline_key: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum QualificationReserve {
    Reserved(Box<QualificationReservation>),
    NotDue { next_attempt_at: i64 },
    NotPending,
    StaleFence,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum QualificationFinish {
    Finished,
    NotOwner,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(
    dead_code,
    reason = "constructed by the L1 shared-seam qualification classifier"
)]
pub(crate) enum QualificationFailure {
    EnumerationIncomplete,
    ProofIncomplete,
    ReadinessRegressed,
    StalePlan,
    NotFullEnumeration,
    FailedTransfers,
    MetadataFailures,
}

impl QualificationFailure {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::EnumerationIncomplete => "enumeration_incomplete",
            Self::ProofIncomplete => "proof_incomplete",
            Self::ReadinessRegressed => "readiness_regressed",
            Self::StalePlan => "stale_plan",
            Self::NotFullEnumeration => "not_full_enumeration",
            Self::FailedTransfers => "failed_transfers",
            Self::MetadataFailures => "metadata_failures",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct QualificationReservation {
    pub(crate) epoch: String,
    pub(crate) library: String,
    pub(crate) attempt: String,
    pub(crate) nonce: String,
    #[allow(
        dead_code,
        reason = "exact prior record retained for the L1 success transaction"
    )]
    pub(crate) prior_record: String,
    pub(crate) reserved_record: String,
    #[allow(
        dead_code,
        reason = "consumed by commit_indexing_qualification once the L1 helper lands"
    )]
    pub(crate) restored_record: String,
    pub(crate) reserved_outcome: String,
    pub(crate) expected_metadata: Vec<(String, Option<String>)>,
    #[allow(
        dead_code,
        reason = "consumed by commit_indexing_qualification once the L1 helper lands"
    )]
    pub(crate) indexing_key: String,
    #[allow(
        dead_code,
        reason = "consumed by commit_indexing_qualification once the L1 helper lands"
    )]
    pub(crate) replay_key: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct QualificationOutcomeV1 {
    version: u8,
    nonce: String,
    prior_next_attempt_at: i64,
}

pub(crate) fn library_key(library: &str) -> String {
    format!("{LIBRARY_PREFIX}{library}")
}

fn legacy_marker_key(library: &str) -> String {
    format!("{LEGACY_MARKER_PREFIX}{library}")
}

pub(crate) fn epoch_baseline_prefix(epoch: &str) -> String {
    format!("{BASELINE_PREFIX}{epoch}:")
}

pub(crate) fn is_epoch_baseline_key(key: &str, epoch: &str) -> bool {
    key.strip_prefix(&epoch_baseline_prefix(epoch))
        .is_some_and(|rest| !rest.is_empty())
}

fn metadata_value(conn: &Connection, key: &str) -> Result<Option<String>, StateError> {
    Ok(conn
        .query_row("SELECT value FROM metadata WHERE key=?1", [key], |row| {
            row.get(0)
        })
        .optional()?)
}

fn account_keys(conn: &Connection) -> Result<(String, String), StateError> {
    let exists: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name='account_owner')",
        [],
        |row| row.get(0),
    )?;
    if !exists {
        #[cfg(test)]
        {
            // Generic state tests migrate in-memory databases without an owner.
            // Owned production and upgrade tests bind a real account first.
            return Ok(("test-account".to_owned(), "test-provider".to_owned()));
        }
        #[cfg(not(test))]
        {
            return Err(StateError::AccountOwnerMissing);
        }
    }
    let count: i64 = conn.query_row("SELECT count(*) FROM account_owner", [], |row| row.get(0))?;
    if count != 1 {
        return Err(StateError::AccountOwnerMismatch);
    }
    conn.query_row(
        "SELECT account_key, provider_key FROM account_owner WHERE singleton=1",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )
    .map_err(StateError::Migration)
}

fn owner_matches(conn: &Connection, epoch: &SyncEpoch) -> Result<bool, StateError> {
    let exists: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name='account_owner')",
        [],
        |row| row.get(0),
    )?;
    if !exists {
        #[cfg(test)]
        {
            return Ok(true);
        }
        #[cfg(not(test))]
        {
            return Err(StateError::AccountOwnerMissing);
        }
    }
    let (account, provider) = account_keys(conn)?;
    Ok(epoch.account_key == account && epoch.provider_key == provider)
}

fn parse_epoch(value: &str) -> Result<SyncEpoch, StateError> {
    serde_json::from_str(value).map_err(|_error| StateError::SyncEpochMismatch)
}

fn validate_epoch_record(conn: &Connection, epoch: &SyncEpoch) -> Result<(), StateError> {
    if epoch.version != 1
        || epoch.epoch.is_empty()
        || uuid::Uuid::parse_str(&epoch.epoch).is_err()
        || epoch.account_key.is_empty()
        || epoch.provider_key.is_empty()
        || !owner_matches(conn, epoch)?
    {
        return Err(StateError::SyncEpochMismatch);
    }
    EpochOrigin::parse(&epoch.origin)?;
    Ok(())
}

fn read_epoch(conn: &Connection) -> Result<Option<SyncEpoch>, StateError> {
    metadata_value(conn, SYNC_EPOCH_KEY)?
        .map(|value| parse_epoch(&value))
        .transpose()
}

pub(crate) fn validate(conn: &Connection) -> Result<SyncEpoch, StateError> {
    let epoch = read_epoch(conn)?.ok_or(StateError::SyncEpochMissing {
        schema: crate::state::schema::SCHEMA_VERSION,
    })?;
    validate_epoch_record(conn, &epoch)?;
    Ok(epoch)
}

pub(crate) fn validate_if_present(conn: &Connection) -> Result<Option<SyncEpoch>, StateError> {
    let Some(epoch) = read_epoch(conn)? else {
        return Ok(None);
    };
    validate_epoch_record(conn, &epoch)?;
    Ok(Some(epoch))
}

pub(crate) fn establish_epoch(conn: &Connection, origin: EpochOrigin) -> Result<(), StateError> {
    if let Some(existing) = read_epoch(conn)? {
        return validate_epoch_record(conn, &existing);
    }
    let (account_key, provider_key) = account_keys(conn)?;
    let (quarantined_rounds, retained_debts) =
        super::source_revalidation::quarantine_all_rounds(conn)?;
    let epoch = SyncEpoch {
        version: 1,
        epoch: uuid::Uuid::new_v4().to_string(),
        account_key,
        provider_key,
        origin: origin.as_str().to_owned(),
        established_at: chrono::Utc::now().timestamp(),
        quarantined_rounds,
        retained_debts: u64::try_from(retained_debts)
            .map_err(|_error| StateError::ProviderSelectionInvalid)?,
    };
    let encoded = serde_json::to_string(&epoch).map_err(|_error| StateError::SyncEpochMismatch)?;
    conn.execute(
        "INSERT INTO metadata(key,value) VALUES(?1,?2)",
        params![SYNC_EPOCH_KEY, encoded],
    )?;
    Ok(())
}

#[allow(
    dead_code,
    reason = "consumed by the L1 shared-seam qualification integration"
)]
fn encode_qualification_outcome(
    nonce: &str,
    prior_next_attempt_at: i64,
) -> Result<String, StateError> {
    let payload = QualificationOutcomeV1 {
        version: QUALIFICATION_OUTCOME_VERSION,
        nonce: nonce.to_owned(),
        prior_next_attempt_at,
    };
    let body = serde_json::to_string(&payload).map_err(|_error| StateError::SyncEpochMismatch)?;
    Ok(format!(
        "{QUALIFICATION_OUTCOME_PREFIX}v{QUALIFICATION_OUTCOME_VERSION}:{body}"
    ))
}

fn parse_qualification_outcome(
    value: Option<&str>,
) -> Result<Option<QualificationOutcomeV1>, StateError> {
    let Some(value) = value else {
        return Ok(None);
    };
    let Some(rest) = value.strip_prefix(QUALIFICATION_OUTCOME_PREFIX) else {
        return Ok(None);
    };
    let Some((version, body)) = rest.split_once(':') else {
        return Err(StateError::SyncEpochMismatch);
    };
    if version != "v1" {
        return Err(StateError::SyncEpochMismatch);
    }
    let payload: QualificationOutcomeV1 =
        serde_json::from_str(body).map_err(|_error| StateError::SyncEpochMismatch)?;
    if payload.version != QUALIFICATION_OUTCOME_VERSION
        || uuid::Uuid::parse_str(&payload.nonce).is_err()
    {
        return Err(StateError::SyncEpochMismatch);
    }
    Ok(Some(payload))
}

fn encode_library(record: &SyncEpochLibrary) -> Result<String, StateError> {
    serde_json::to_string(record).map_err(|_error| StateError::SyncEpochMismatch)
}

/// The single shape guard for a per-library record. Phase and binding are one
/// invariant: a ready record owns exactly one binding and no other phase does.
fn validate_library_record(record: &SyncEpochLibrary, library: &str) -> Result<(), StateError> {
    let binding_shape = match record.state.as_str() {
        "pending" | "blocked" | "complete" => record.binding.is_none(),
        "ready" => record.binding.is_some(),
        _ => false,
    };
    if record.version != LIBRARY_VERSION
        || record.library != library
        || record.epoch.is_empty()
        || record.attempt.is_empty()
        || !binding_shape
    {
        return Err(StateError::SyncEpochMismatch);
    }
    let qualification = parse_qualification_outcome(record.last_outcome.as_deref())?;
    if qualification.is_some() && record.state != "pending" {
        return Err(StateError::SyncEpochMismatch);
    }
    Ok(())
}

/// Candidate-era marker or proof evidence for this exact library. `attempt` is a
/// colon-free nonce, so the remainder after the first colon is the whole
/// library name, including names that contain colons.
fn legacy_evidence_matches(conn: &Connection, library: &str) -> Result<bool, StateError> {
    if metadata_value(conn, &legacy_marker_key(library))?.is_some() {
        return Ok(true);
    }
    let keys: Vec<String> = conn
        .prepare("SELECT key FROM metadata WHERE substr(key, 1, length(?1)) = ?1")?
        .query_map([LEGACY_PROOF_PREFIX], |row| row.get(0))?
        .collect::<Result<_, _>>()?;
    for key in keys {
        let Some(rest) = key.strip_prefix(LEGACY_PROOF_PREFIX) else {
            continue;
        };
        let Some((_, record_library)) = rest.split_once(':') else {
            continue;
        };
        if record_library == library {
            return Ok(true);
        }
    }
    Ok(false)
}

pub(crate) fn read_library(
    conn: &Connection,
    library: &str,
) -> Result<Option<SyncEpochLibrary>, StateError> {
    let Some(value) = metadata_value(conn, &library_key(library))? else {
        // No record yet. Candidate-era evidence for this exact library must
        // refuse here, before enrollment can clear checkpoints or mint a fresh
        // attempt, instead of silently starting a new baseline.
        if legacy_evidence_matches(conn, library)? {
            return Err(StateError::SyncEpochMismatch);
        }
        return Ok(None);
    };
    let record: SyncEpochLibrary =
        serde_json::from_str(&value).map_err(|_error| StateError::SyncEpochMismatch)?;
    validate_library_record(&record, library)?;
    Ok(Some(record))
}

fn write_library(conn: &Connection, record: &SyncEpochLibrary) -> Result<(), StateError> {
    let encoded = encode_library(record)?;
    write_library_encoded(conn, record, &encoded)
}

fn write_library_encoded(
    conn: &Connection,
    record: &SyncEpochLibrary,
    encoded: &str,
) -> Result<(), StateError> {
    validate_library_record(record, &record.library)?;
    conn.execute(
        "INSERT INTO metadata(key,value) VALUES(?1,?2) \
         ON CONFLICT(key) DO UPDATE SET value=excluded.value",
        params![library_key(&record.library), encoded],
    )?;
    Ok(())
}

fn staged_zone(key: &str) -> Option<&str> {
    let rest = key
        .strip_prefix("pending_sync_token:v27:")
        .or_else(|| key.strip_prefix("observed_selection_staged:v27:"))?;
    let (_, zone) = rest.split_once(':')?;
    Some(zone)
}

fn clear_checkpoint_keys(conn: &Connection, library: &str) -> Result<(), StateError> {
    for key in [
        format!("sync_token:v27:{library}"),
        format!("sync_token:{library}"),
        format!("observed_selection:v27:{library}"),
    ] {
        conn.execute("DELETE FROM metadata WHERE key=?1", [key])?;
    }
    let staged: Vec<String> = conn
        .prepare(
            "SELECT key FROM metadata WHERE key LIKE 'pending_sync_token:v27:%' \
             OR key LIKE 'observed_selection_staged:v27:%'",
        )?
        .query_map([], |row| row.get(0))?
        .collect::<Result<_, _>>()?;
    for key in staged {
        if staged_zone(&key) == Some(library) {
            conn.execute("DELETE FROM metadata WHERE key=?1", [key])?;
        }
    }
    Ok(())
}

fn has_library_evidence(conn: &Connection, library: &str) -> Result<bool, StateError> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM assets WHERE library=?1) \
         OR EXISTS(SELECT 1 FROM source_revalidation_rounds WHERE library=?1) \
         OR EXISTS(SELECT 1 FROM metadata WHERE key=?2 OR key=?3)",
        params![
            library,
            format!("sync_token:v27:{library}"),
            format!("sync_token:{library}")
        ],
        |row| row.get(0),
    )?)
}

pub(crate) fn enroll_library(
    conn: &Connection,
    library: &str,
    baseline_scope: &str,
) -> Result<(), StateError> {
    let epoch = validate(conn)?;
    if let Some(existing) = read_library(conn, library)? {
        if existing.epoch != epoch.epoch {
            return Err(StateError::SyncEpochMismatch);
        }
        return Ok(());
    }
    #[cfg(test)]
    if metadata_value(conn, TEST_PREEXISTING_EPOCH_KEY)?.is_some() {
        let now = chrono::Utc::now().timestamp();
        write_library(
            conn,
            &SyncEpochLibrary {
                version: LIBRARY_VERSION,
                epoch: epoch.epoch,
                library: library.to_owned(),
                state: "complete".to_owned(),
                reason: "test_preexisting_epoch".to_owned(),
                attempt: uuid::Uuid::new_v4().to_string(),
                attempts: 0,
                next_attempt_at: 0,
                last_outcome: Some("test_preexisting_epoch".to_owned()),
                baseline_scope: baseline_scope.to_owned(),
                enrolled_at: now,
                completed_at: Some(now),
                binding: None,
            },
        )?;
        return Ok(());
    }
    let now = chrono::Utc::now().timestamp();
    if EpochOrigin::parse(&epoch.origin)? == EpochOrigin::FreshInstall {
        write_library(
            conn,
            &SyncEpochLibrary {
                version: LIBRARY_VERSION,
                epoch: epoch.epoch,
                library: library.to_owned(),
                state: "complete".to_owned(),
                reason: EpochOrigin::FreshInstall.as_str().to_owned(),
                attempt: uuid::Uuid::new_v4().to_string(),
                attempts: 0,
                next_attempt_at: 0,
                last_outcome: Some("fresh_install".to_owned()),
                baseline_scope: baseline_scope.to_owned(),
                enrolled_at: now,
                completed_at: Some(now),
                binding: None,
            },
        )?;
        return Ok(());
    }
    clear_checkpoint_keys(conn, library)?;
    let reason = if has_library_evidence(conn, library)? {
        EpochOrigin::ExistingUpgrade.as_str()
    } else {
        "new_library"
    };
    write_library(
        conn,
        &SyncEpochLibrary {
            version: LIBRARY_VERSION,
            epoch: epoch.epoch,
            library: library.to_owned(),
            state: "pending".to_owned(),
            reason: reason.to_owned(),
            attempt: uuid::Uuid::new_v4().to_string(),
            attempts: 0,
            next_attempt_at: now,
            last_outcome: None,
            baseline_scope: baseline_scope.to_owned(),
            enrolled_at: now,
            completed_at: None,
            binding: None,
        },
    )
}

pub(crate) fn retry_floor(watch_interval: Option<u64>) -> i64 {
    watch_interval
        .and_then(|watch| i64::try_from(watch).ok())
        .map_or(INITIAL_RETRY_SECONDS, |watch| {
            INITIAL_RETRY_SECONDS.max(watch)
        })
}

pub(crate) fn retry_delay(watch_interval: Option<u64>, attempts: u32) -> i64 {
    let floor = retry_floor(watch_interval);
    let cap = watch_interval
        .and_then(|watch| i64::try_from(watch).ok())
        .map_or(MAX_RETRY_SECONDS, |watch| MAX_RETRY_SECONDS.max(watch));
    let shift = attempts.saturating_sub(1).min(MAX_BACKOFF_SHIFT);
    floor.saturating_mul(1_i64 << shift).min(cap)
}

fn rotate_attempt(
    conn: &Connection,
    record: &mut SyncEpochLibrary,
    now: i64,
    watch_interval: Option<u64>,
    outcome: &'static str,
) -> Result<(), StateError> {
    record.attempts = record.attempts.saturating_add(1);
    record.attempt = uuid::Uuid::new_v4().to_string();
    record.state = "pending".to_owned();
    record.next_attempt_at = now.saturating_add(retry_delay(watch_interval, record.attempts));
    record.last_outcome = Some(outcome.to_owned());
    record.binding = None;
    write_library(conn, record)
}

pub(crate) fn gate(
    conn: &Connection,
    library: &str,
    baseline_scope: &str,
    now: i64,
    watch_interval: Option<u64>,
) -> Result<SyncEpochGate, StateError> {
    let epoch = validate(conn)?;
    if read_library(conn, library)?.is_none() {
        enroll_library(conn, library, baseline_scope)?;
    }
    let mut record = read_library(conn, library)?.ok_or(StateError::SyncEpochMismatch)?;
    if record.epoch != epoch.epoch {
        return Err(StateError::SyncEpochMismatch);
    }
    let qualification = parse_qualification_outcome(record.last_outcome.as_deref())?;
    match record.state.as_str() {
        "complete" => return Ok(SyncEpochGate::Open),
        "blocked" => {
            return Ok(SyncEpochGate::Blocked {
                reason: "epoch_blocked",
            });
        }
        "pending" | "ready" => {}
        _ => return Err(StateError::SyncEpochMismatch),
    }
    if record.state == "ready" {
        if observation::epoch_proof_ready(conn, &epoch.epoch, &record)? {
            return Ok(SyncEpochGate::Promote {
                attempt: record.attempt,
            });
        }
        rotate_attempt(conn, &mut record, now, watch_interval, "stale_staged_proof")?;
        return Ok(SyncEpochGate::Baseline {
            attempt: record.attempt,
        });
    }
    if qualification.is_some() {
        if now < record.next_attempt_at {
            return Ok(SyncEpochGate::Defer {
                reason: "epoch_backoff",
            });
        }
        return Ok(SyncEpochGate::Blocked {
            reason: "qualification_reservation_pending",
        });
    }
    if now < record.next_attempt_at {
        return Ok(SyncEpochGate::Defer {
            reason: "epoch_backoff",
        });
    }
    // The attempt nonce changes for each baseline, but the provider-failure
    // count and retry delay are only advanced after a real failure recovery.
    // The future timestamp here is a crash guard, not a consumed attempt.
    record.attempt = uuid::Uuid::new_v4().to_string();
    record.state = "pending".to_owned();
    record.next_attempt_at = now.saturating_add(retry_floor(watch_interval));
    record.last_outcome = Some("baseline_claimed".to_owned());
    record.binding = None;
    write_library(conn, &record)?;
    Ok(SyncEpochGate::Baseline {
        attempt: record.attempt,
    })
}

pub(crate) fn should_wake(conn: &Connection, library: &str, now: i64) -> Result<bool, StateError> {
    validate(conn)?;
    let Some(record) = read_library(conn, library)? else {
        // No record means the runner has not enrolled this library yet. Do not
        // wake a quiet cycle for it; enrollment runs before the next precheck.
        return Ok(false);
    };
    match record.state.as_str() {
        "complete" => Ok(false),
        "blocked" => Ok(false),
        "ready" => Ok(true),
        "pending" => Ok(now >= record.next_attempt_at),
        _ => Err(StateError::SyncEpochMismatch),
    }
}

pub(crate) fn has_pending_work(conn: &Connection, library: &str) -> Result<bool, StateError> {
    let Some(record) = read_library(conn, library)? else {
        return Ok(false);
    };
    Ok(matches!(
        record.state.as_str(),
        "pending" | "ready" | "blocked"
    ))
}

pub(crate) fn backoff(conn: &Connection, library: &str, now: i64) -> Result<bool, StateError> {
    match read_library(conn, library)? {
        Some(record) => Ok(record.state == "pending" && record.next_attempt_at > now),
        None => Ok(false),
    }
}

pub(crate) fn baseline_evidence(
    conn: &Connection,
    library: &str,
) -> Result<Option<(String, Option<String>)>, StateError> {
    let Some(record) = read_library(conn, library)? else {
        return Ok(None);
    };
    let Some(binding) = record.binding else {
        return Ok(None);
    };
    let value = metadata_value(conn, &binding.baseline_key)?;
    Ok(Some((binding.baseline_key, value)))
}

pub(crate) fn recover_attempt(
    conn: &Connection,
    library: &str,
    provider_failure: bool,
    watch_interval: Option<u64>,
) -> Result<(), StateError> {
    let Some(mut record) = read_library(conn, library)? else {
        return Ok(());
    };
    if record.state != "pending" {
        return Ok(());
    }
    if parse_qualification_outcome(record.last_outcome.as_deref())?.is_some() {
        // A committed qualification guard owns its deadline and outcome.
        // Ordinary baseline recovery must not overwrite or rearm it.
        return Ok(());
    }
    let now = chrono::Utc::now().timestamp();
    if provider_failure {
        record.attempts = record.attempts.saturating_add(1);
        record.next_attempt_at = now.saturating_add(retry_delay(watch_interval, record.attempts));
        record.last_outcome = Some("provider_failure".to_owned());
    } else {
        // Preserve the claim's crash-guard deadline, or raise it to the bounded
        // floor. Auth, cancellation and shutdown must never rearm immediately
        // and must not consume a provider-failure attempt.
        let floor = now.saturating_add(retry_floor(watch_interval));
        record.next_attempt_at = record.next_attempt_at.max(floor);
        record.last_outcome = Some("auth_cancel_or_shutdown".to_owned());
    }
    write_library(conn, &record)
}

pub(crate) fn defer_without_attempt(
    conn: &Connection,
    library: &str,
    now: i64,
    watch_interval: Option<u64>,
    outcome: &'static str,
) -> Result<(), StateError> {
    let Some(mut record) = read_library(conn, library)? else {
        return Ok(());
    };
    if record.state != "pending" && record.state != "ready" {
        return Ok(());
    }
    if parse_qualification_outcome(record.last_outcome.as_deref())?.is_some() {
        // Preserve the qualification guard exactly.
        return Ok(());
    }
    if record.next_attempt_at > now {
        return Ok(());
    }
    record.next_attempt_at = now.saturating_add(retry_floor(watch_interval));
    record.last_outcome = Some(outcome.to_owned());
    write_library(conn, &record)
}

#[allow(
    dead_code,
    reason = "consumed by the L1 shared-seam qualification integration"
)]
fn fence_matches(
    conn: &Connection,
    expected: &[(String, Option<String>)],
) -> Result<bool, StateError> {
    for (key, expected_value) in expected {
        if metadata_value(conn, key)? != *expected_value {
            return Ok(false);
        }
    }
    Ok(true)
}

#[allow(
    dead_code,
    reason = "consumed by the L1 shared-seam qualification integration"
)]
pub(crate) fn reserve_indexing_qualification(
    conn: &Connection,
    library: &str,
    indexing_key: &str,
    replay_key: &str,
    expected_metadata: &[(String, Option<String>)],
    now: i64,
    watch_interval: Option<u64>,
) -> Result<QualificationReserve, StateError> {
    let epoch = match validate(conn) {
        Ok(epoch) => epoch,
        Err(StateError::SyncEpochMismatch) => return Ok(QualificationReserve::StaleFence),
        Err(error) => return Err(error),
    };
    let Some(raw_epoch) = metadata_value(conn, SYNC_EPOCH_KEY)? else {
        return Ok(QualificationReserve::StaleFence);
    };
    let Some((_, expected_epoch)) = expected_metadata
        .iter()
        .find(|(key, _)| key == SYNC_EPOCH_KEY)
    else {
        return Ok(QualificationReserve::StaleFence);
    };
    if expected_epoch.as_deref() != Some(raw_epoch.as_str()) {
        return Ok(QualificationReserve::StaleFence);
    }
    if !fence_matches(conn, expected_metadata)? {
        return Ok(QualificationReserve::StaleFence);
    }
    let record_key = library_key(library);
    for required in [indexing_key, replay_key] {
        if !expected_metadata.iter().any(|(key, _)| key == required) {
            return Ok(QualificationReserve::StaleFence);
        }
    }
    if expected_metadata.iter().any(|(key, _)| key == &record_key) {
        // The owner adds the exact reserved record itself.
        return Ok(QualificationReserve::StaleFence);
    }
    let record = match read_library(conn, library) {
        Ok(Some(record)) => record,
        Ok(None) => return Ok(QualificationReserve::StaleFence),
        Err(StateError::SyncEpochMismatch) => return Ok(QualificationReserve::StaleFence),
        Err(error) => return Err(error),
    };
    if record.epoch != epoch.epoch {
        return Ok(QualificationReserve::StaleFence);
    }
    match record.state.as_str() {
        "pending" => {}
        "ready" | "blocked" | "complete" => return Ok(QualificationReserve::NotPending),
        _ => return Ok(QualificationReserve::StaleFence),
    }
    if record.binding.is_some() {
        return Ok(QualificationReserve::StaleFence);
    }
    parse_qualification_outcome(record.last_outcome.as_deref())?;
    if now < record.next_attempt_at {
        return Ok(QualificationReserve::NotDue {
            next_attempt_at: record.next_attempt_at,
        });
    }

    let prior_record = encode_library(&record)?;
    let nonce = uuid::Uuid::new_v4().to_string();
    let prior_next_attempt_at = record.next_attempt_at;
    let reserved_next_attempt_at = now.saturating_add(retry_floor(watch_interval));
    let reserved_outcome = encode_qualification_outcome(&nonce, prior_next_attempt_at)?;

    let mut reserved = record.clone();
    reserved.next_attempt_at = reserved_next_attempt_at;
    reserved.last_outcome = Some(reserved_outcome.clone());
    let reserved_record = encode_library(&reserved)?;

    let mut restored = record.clone();
    restored.next_attempt_at = prior_next_attempt_at;
    restored.last_outcome = Some(QUALIFICATION_QUALIFIED_OUTCOME.to_owned());
    let restored_record = encode_library(&restored)?;

    let mut expected = expected_metadata.to_vec();
    expected.push((record_key, Some(reserved_record.clone())));

    write_library_encoded(conn, &reserved, &reserved_record)?;
    Ok(QualificationReserve::Reserved(Box::new(
        QualificationReservation {
            epoch: epoch.epoch,
            library: library.to_owned(),
            attempt: record.attempt,
            nonce,
            prior_record,
            reserved_record,
            restored_record,
            reserved_outcome,
            expected_metadata: expected,
            indexing_key: indexing_key.to_owned(),
            replay_key: replay_key.to_owned(),
        },
    )))
}

#[allow(
    dead_code,
    reason = "consumed by the L1 shared-seam qualification integration"
)]
fn qualification_record_for_finish(
    conn: &Connection,
    reservation: &QualificationReservation,
) -> Result<Option<SyncEpochLibrary>, StateError> {
    let epoch = match validate(conn) {
        Ok(epoch) => epoch,
        Err(StateError::SyncEpochMismatch) => return Ok(None),
        Err(error) => return Err(error),
    };
    if epoch.epoch != reservation.epoch {
        return Ok(None);
    }
    if !fence_matches(conn, &reservation.expected_metadata)? {
        return Ok(None);
    }
    let Some(actual) = metadata_value(conn, &library_key(&reservation.library))? else {
        return Ok(None);
    };
    if actual != reservation.reserved_record {
        return Ok(None);
    }
    let record = match read_library(conn, &reservation.library) {
        Ok(Some(record)) => record,
        Ok(None) => return Ok(None),
        Err(StateError::SyncEpochMismatch) => return Ok(None),
        Err(error) => return Err(error),
    };
    if record.epoch != reservation.epoch
        || record.attempt != reservation.attempt
        || record.state != "pending"
        || record.binding.is_some()
        || record.last_outcome.as_deref() != Some(reservation.reserved_outcome.as_str())
    {
        return Ok(None);
    }
    Ok(Some(record))
}

#[allow(
    dead_code,
    reason = "consumed by the L1 shared-seam qualification integration"
)]
pub(crate) fn finish_qualification_nonqualified(
    conn: &Connection,
    reservation: &QualificationReservation,
    reason: QualificationFailure,
) -> Result<QualificationFinish, StateError> {
    let Some(mut record) = qualification_record_for_finish(conn, reservation)? else {
        return Ok(QualificationFinish::NotOwner);
    };
    record.last_outcome = Some(format!(
        "{QUALIFICATION_INCOMPLETE_PREFIX}{}",
        reason.as_str()
    ));
    write_library(conn, &record)?;
    Ok(QualificationFinish::Finished)
}

#[allow(
    dead_code,
    reason = "consumed by the L1 shared-seam qualification integration"
)]
pub(crate) fn finish_qualification_provider_failure(
    conn: &Connection,
    reservation: &QualificationReservation,
    now: i64,
    watch_interval: Option<u64>,
) -> Result<QualificationFinish, StateError> {
    let Some(mut record) = qualification_record_for_finish(conn, reservation)? else {
        return Ok(QualificationFinish::NotOwner);
    };
    record.attempts = record.attempts.saturating_add(1);
    record.next_attempt_at = now.saturating_add(retry_delay(watch_interval, record.attempts));
    record.last_outcome = Some("provider_failure".to_owned());
    write_library(conn, &record)?;
    Ok(QualificationFinish::Finished)
}

#[allow(
    dead_code,
    reason = "consumed by the L1 shared-seam qualification integration"
)]
pub(crate) fn finish_qualification_auth_or_cancel(
    conn: &Connection,
    reservation: &QualificationReservation,
    now: i64,
    watch_interval: Option<u64>,
) -> Result<QualificationFinish, StateError> {
    let Some(mut record) = qualification_record_for_finish(conn, reservation)? else {
        return Ok(QualificationFinish::NotOwner);
    };
    let floor = now.saturating_add(retry_floor(watch_interval));
    record.next_attempt_at = record.next_attempt_at.max(floor);
    record.last_outcome = Some("auth_cancel_or_shutdown".to_owned());
    write_library(conn, &record)?;
    Ok(QualificationFinish::Finished)
}

/// Stage the single ready transition. The binding is validated and written with
/// the record in one update so phase and evidence cannot disagree.
pub(crate) fn record_ready(
    conn: &Connection,
    epoch: &SyncEpoch,
    library: &str,
    attempt: &str,
    binding: BaselineBinding,
) -> Result<(), StateError> {
    let mut record = read_library(conn, library)?.ok_or(StateError::SyncEpochMismatch)?;
    if record.epoch != epoch.epoch
        || record.attempt != attempt
        || binding.token_key.is_empty()
        || binding.token_sha256.is_empty()
        || binding.descriptor_key.is_empty()
        || binding.descriptor_proof_hash.is_empty()
        || binding.baseline_sha256.is_empty()
        || !is_epoch_baseline_key(&binding.baseline_key, &epoch.epoch)
    {
        return Err(StateError::SyncEpochMismatch);
    }
    if parse_qualification_outcome(record.last_outcome.as_deref())?.is_some() {
        return Err(StateError::SyncEpochMismatch);
    }
    if record.state == "ready" {
        return if record.binding.as_ref() == Some(&binding) {
            Ok(())
        } else {
            Err(StateError::SyncEpochMismatch)
        };
    }
    if record.state != "pending" {
        return Err(StateError::SyncEpochMismatch);
    }
    record.state = "ready".to_owned();
    record.next_attempt_at = 0;
    record.last_outcome = Some("baseline_staged".to_owned());
    record.binding = Some(binding);
    write_library(conn, &record)
}

pub(crate) fn ready_proof_baseline_keys(
    conn: &Connection,
) -> Result<rustc_hash::FxHashSet<String>, StateError> {
    let mut baselines = rustc_hash::FxHashSet::default();
    let Some(epoch) = validate_if_present(conn)? else {
        return Ok(baselines);
    };
    let library_keys: Vec<String> = conn
        .prepare("SELECT key FROM metadata WHERE substr(key, 1, length(?1)) = ?1")?
        .query_map([LIBRARY_PREFIX], |row| row.get(0))?
        .collect::<Result<_, _>>()?;
    for key in library_keys {
        let library = key
            .strip_prefix(LIBRARY_PREFIX)
            .ok_or(StateError::SyncEpochMismatch)?;
        let Ok(Some(record)) = read_library(conn, library) else {
            // A malformed unrelated library must not abort a healthy zone's
            // checkpoint transition. It simply cannot preserve a baseline.
            continue;
        };
        if record.epoch != epoch.epoch || record.state != "ready" {
            continue;
        }
        let Some(binding) = record.binding else {
            continue;
        };
        if !is_epoch_baseline_key(&binding.baseline_key, &epoch.epoch) {
            continue;
        }
        baselines.insert(binding.baseline_key);
    }
    Ok(baselines)
}

const QUARANTINE_PROOF_PREFIX: &str = "source_revalidation_reset_proof_quarantine:v1:";
const QUARANTINE_DESCRIPTOR_PREFIX: &str = "observed_selection_staged_quarantine:v1:";

/// The explicit reset command is the only caller. Rotate every enrolled library
/// to a fresh, due baseline attempt and quarantine any ready binding and its
/// descriptor before removing them. Generations, destinations, decisions and
/// receipts are not touched. The whole operation must run inside the caller's
/// IMMEDIATE transaction; any error rolls back the token reset too.
pub(crate) fn reset_libraries_for_token_reset(
    conn: &Connection,
    now: i64,
) -> Result<Vec<String>, StateError> {
    let Some(epoch) = validate_if_present(conn)? else {
        return Ok(Vec::new());
    };
    let library_keys: Vec<String> = conn
        .prepare("SELECT key FROM metadata WHERE substr(key, 1, length(?1)) = ?1")?
        .query_map([LIBRARY_PREFIX], |row| row.get(0))?
        .collect::<Result<_, _>>()?;
    let mut changed = Vec::new();
    for key in library_keys {
        let library = key
            .strip_prefix(LIBRARY_PREFIX)
            .ok_or(StateError::SyncEpochMismatch)?
            .to_owned();
        let mut record = read_library(conn, &library)?.ok_or(StateError::SyncEpochMismatch)?;
        if record.epoch != epoch.epoch {
            return Err(StateError::SyncEpochMismatch);
        }
        let old_attempt = record.attempt.clone();
        if let Some(binding) = record.binding.take() {
            let encoded =
                serde_json::to_string(&binding).map_err(|_error| StateError::SyncEpochMismatch)?;
            let quarantine_key = format!("{QUARANTINE_PROOF_PREFIX}{old_attempt}:{library}");
            conn.execute(
                "INSERT OR REPLACE INTO metadata(key,value) VALUES(?1,?2)",
                params![quarantine_key, encoded],
            )?;
            changed.push(quarantine_key);
            if let Some(descriptor) = metadata_value(conn, &binding.descriptor_key)? {
                let descriptor_key =
                    format!("{QUARANTINE_DESCRIPTOR_PREFIX}{old_attempt}:{library}");
                conn.execute(
                    "INSERT OR REPLACE INTO metadata(key,value) VALUES(?1,?2)",
                    params![descriptor_key, descriptor],
                )?;
                changed.push(descriptor_key);
            }
            for key in [
                binding.descriptor_key,
                binding.baseline_key,
                binding.token_key,
            ] {
                conn.execute("DELETE FROM metadata WHERE key=?1", [&key])?;
                changed.push(key);
            }
        }
        for key in [
            format!("sync_token:v27:{library}"),
            format!("sync_token:{library}"),
            format!("observed_selection:v27:{library}"),
        ] {
            conn.execute("DELETE FROM metadata WHERE key=?1", [&key])?;
            changed.push(key);
        }
        let staged: Vec<String> = conn
            .prepare(
                "SELECT key FROM metadata WHERE key LIKE 'pending_sync_token:v27:%'                  OR key LIKE 'observed_selection_staged:v27:%'",
            )?
            .query_map([], |row| row.get(0))?
            .collect::<Result<_, _>>()?;
        for key in staged {
            if staged_zone(&key) == Some(library.as_str()) {
                conn.execute("DELETE FROM metadata WHERE key=?1", [&key])?;
                changed.push(key);
            }
        }
        record.state = "pending".to_owned();
        record.reason = "sync_token_reset".to_owned();
        record.attempt = uuid::Uuid::new_v4().to_string();
        record.attempts = 0;
        record.next_attempt_at = now;
        record.last_outcome = Some("reset_sync_token".to_owned());
        record.completed_at = None;
        write_library(conn, &record)?;
        changed.push(library_key(&library));
    }
    Ok(changed)
}

pub(crate) fn complete(conn: &Connection, library: &str, attempt: &str) -> Result<(), StateError> {
    let mut record = read_library(conn, library)?.ok_or(StateError::SyncEpochMismatch)?;
    if record.attempt != attempt {
        return Err(StateError::SyncEpochMismatch);
    }
    if parse_qualification_outcome(record.last_outcome.as_deref())?.is_some() {
        return Err(StateError::SyncEpochMismatch);
    }
    let now = chrono::Utc::now().timestamp();
    record.state = "complete".to_owned();
    record.next_attempt_at = 0;
    record.last_outcome = Some("complete".to_owned());
    record.completed_at = Some(now);
    record.binding = None;
    write_library(conn, &record)
}

impl SqliteStateDb {
    /// Enroll every currently selected library in the database epoch. This is
    /// idempotent and must run after plans resolve but before the watch precheck.
    pub(crate) async fn enroll_sync_epoch(
        &self,
        libraries: Vec<(String, String)>,
    ) -> Result<(), StateError> {
        self.with_conn_mut("enrolling sync epoch libraries", move |conn| {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            for (library, baseline_scope) in libraries {
                enroll_library(&tx, &library, &baseline_scope)?;
            }
            tx.commit()?;
            Ok(())
        })
        .await
    }

    pub(crate) async fn sync_epoch_gate(
        &self,
        library: String,
        baseline_scope: String,
        watch_interval: Option<u64>,
    ) -> Result<SyncEpochGate, StateError> {
        self.with_conn_mut("gating sync epoch library", move |conn| {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let gate = gate(
                &tx,
                &library,
                &baseline_scope,
                chrono::Utc::now().timestamp(),
                watch_interval,
            )?;
            tx.commit()?;
            Ok(gate)
        })
        .await
    }

    pub(crate) async fn sync_epoch_should_wake(&self, library: String) -> Result<bool, StateError> {
        self.with_conn("checking sync epoch wake", move |conn| {
            should_wake(conn, &library, chrono::Utc::now().timestamp())
        })
        .await
    }

    pub(crate) async fn sync_epoch_has_pending(&self, library: String) -> Result<bool, StateError> {
        self.with_conn("checking sync epoch pending work", move |conn| {
            has_pending_work(conn, &library)
        })
        .await
    }

    pub(crate) async fn sync_epoch_backoff(&self, library: String) -> Result<bool, StateError> {
        self.with_conn("checking sync epoch backoff", move |conn| {
            backoff(conn, &library, chrono::Utc::now().timestamp())
        })
        .await
    }

    pub(crate) async fn sync_epoch_baseline_evidence(
        &self,
        library: String,
    ) -> Result<Option<(String, Option<String>)>, StateError> {
        self.with_conn("reading sync epoch baseline evidence", move |conn| {
            baseline_evidence(conn, &library)
        })
        .await
    }

    pub(crate) async fn recover_sync_epoch_attempt(
        &self,
        library: String,
        provider_failure: bool,
        watch_interval: Option<u64>,
    ) -> Result<(), StateError> {
        self.with_conn_mut("recovering sync epoch attempt", move |conn| {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            recover_attempt(&tx, &library, provider_failure, watch_interval)?;
            tx.commit()?;
            Ok(())
        })
        .await
    }

    pub(crate) async fn defer_sync_epoch(
        &self,
        library: String,
        watch_interval: Option<u64>,
        outcome: &'static str,
    ) -> Result<(), StateError> {
        self.with_conn_mut("deferring sync epoch", move |conn| {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            defer_without_attempt(
                &tx,
                &library,
                chrono::Utc::now().timestamp(),
                watch_interval,
                outcome,
            )?;
            tx.commit()?;
            Ok(())
        })
        .await
    }

    #[allow(
        dead_code,
        reason = "consumed by the L1 shared-seam qualification integration"
    )]
    pub(crate) async fn reserve_indexing_qualification(
        &self,
        library: String,
        indexing_key: String,
        replay_key: String,
        expected_metadata: Vec<(String, Option<String>)>,
        watch_interval: Option<u64>,
    ) -> Result<QualificationReserve, StateError> {
        self.with_conn_mut("reserving indexing qualification", move |conn| {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let outcome = reserve_indexing_qualification(
                &tx,
                &library,
                &indexing_key,
                &replay_key,
                &expected_metadata,
                chrono::Utc::now().timestamp(),
                watch_interval,
            )?;
            tx.commit()?;
            Ok(outcome)
        })
        .await
    }

    #[allow(
        dead_code,
        reason = "consumed by the L1 shared-seam qualification integration"
    )]
    pub(crate) async fn finish_indexing_qualification_nonqualified(
        &self,
        reservation: QualificationReservation,
        reason: QualificationFailure,
    ) -> Result<QualificationFinish, StateError> {
        self.with_conn_mut("finishing indexing qualification", move |conn| {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let outcome = finish_qualification_nonqualified(&tx, &reservation, reason)?;
            tx.commit()?;
            Ok(outcome)
        })
        .await
    }

    #[allow(
        dead_code,
        reason = "consumed by the L1 shared-seam qualification integration"
    )]
    pub(crate) async fn finish_indexing_qualification_provider_failure(
        &self,
        reservation: QualificationReservation,
        watch_interval: Option<u64>,
    ) -> Result<QualificationFinish, StateError> {
        self.with_conn_mut("finishing indexing qualification", move |conn| {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let outcome = finish_qualification_provider_failure(
                &tx,
                &reservation,
                chrono::Utc::now().timestamp(),
                watch_interval,
            )?;
            tx.commit()?;
            Ok(outcome)
        })
        .await
    }

    #[allow(
        dead_code,
        reason = "consumed by the L1 shared-seam qualification integration"
    )]
    pub(crate) async fn finish_indexing_qualification_auth_or_cancel(
        &self,
        reservation: QualificationReservation,
        watch_interval: Option<u64>,
    ) -> Result<QualificationFinish, StateError> {
        self.with_conn_mut("finishing indexing qualification", move |conn| {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let outcome = finish_qualification_auth_or_cancel(
                &tx,
                &reservation,
                chrono::Utc::now().timestamp(),
                watch_interval,
            )?;
            tx.commit()?;
            Ok(outcome)
        })
        .await
    }

    pub(crate) async fn sync_epoch_id(&self) -> Result<Option<String>, StateError> {
        self.with_conn("reading sync epoch", |conn| {
            validate_if_present(conn).map(|epoch| epoch.map(|epoch| epoch.epoch))
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::schema;

    fn owner() -> crate::state::db::account::AccountOwner {
        crate::state::db::account::AccountOwner::authenticated(
            "epoch@example.invalid",
            "com",
            &serde_json::from_value(serde_json::json!({"dsInfo":{"dsid":"synthetic-epoch"}}))
                .unwrap(),
        )
        .unwrap()
    }

    fn metadata_rows(conn: &Connection) -> Vec<(String, String)> {
        conn.prepare("SELECT key,value FROM metadata ORDER BY key")
            .unwrap()
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    }

    #[test]
    fn retry_delay_uses_watch_floor_and_cap() {
        assert_eq!(retry_floor(None), 60 * 60);
        assert_eq!(retry_floor(Some(60)), 60 * 60);
        assert_eq!(retry_floor(Some(1800)), 60 * 60);
        assert_eq!(retry_floor(Some(7200)), 7200);
        assert_eq!(retry_delay(Some(90000), 1), 90000);
        assert_eq!(retry_delay(Some(90000), 20), 90000);
        assert_eq!(retry_delay(None, 1), 60 * 60);
        assert_eq!(retry_delay(None, 2), 2 * 60 * 60);
        assert_eq!(retry_delay(None, 20), 24 * 60 * 60);
    }

    #[test]
    fn fresh_epoch_enrolls_clean_without_binding() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE metadata(key TEXT PRIMARY KEY, value TEXT NOT NULL);")
            .unwrap();
        conn.execute_batch(super::super::source_revalidation::DDL)
            .unwrap();
        establish_epoch(&conn, EpochOrigin::FreshInstall).unwrap();
        enroll_library(&conn, "PrimarySync", "scope").unwrap();
        let record = read_library(&conn, "PrimarySync").unwrap().unwrap();
        assert_eq!(record.state, "complete");
        assert_eq!(record.reason, "fresh_install");
        assert!(record.binding.is_none());
    }

    #[test]
    fn existing_epoch_enrolls_pending_and_clears_only_exact_keys() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE metadata(key TEXT PRIMARY KEY, value TEXT NOT NULL);
             CREATE TABLE assets(library TEXT NOT NULL, id TEXT NOT NULL, PRIMARY KEY(library,id));",
        )
        .unwrap();
        conn.execute_batch(super::super::source_revalidation::DDL)
            .unwrap();
        conn.execute_batch(
            "INSERT INTO assets(library,id) VALUES('PrimarySync','asset');
             INSERT INTO metadata(key,value) VALUES
              ('sync_token:v27:PrimarySync','active'),
              ('sync_token:PrimarySync','legacy'),
              ('pending_sync_token:v27:enum:PrimarySync','staged'),
              ('observed_selection:v27:PrimarySync','observed'),
              ('observed_selection_staged:v27:enum:PrimarySync','observed-staged'),
              ('baseline_cursor:v27:old:scope:enum:preserve','boundary'),
              ('retained_checkpoint_hold:v27:PrimarySync','hold'),
              ('indexing_replay_required:PrimarySync','1'),
              ('download_ledger_scope:PrimarySync','scope'),
              ('unknown:v1','keep');",
        )
        .unwrap();
        establish_epoch(&conn, EpochOrigin::ExistingUpgrade).unwrap();
        enroll_library(&conn, "PrimarySync", "scope").unwrap();
        let record = read_library(&conn, "PrimarySync").unwrap().unwrap();
        assert_eq!(record.state, "pending");
        assert!(record.binding.is_none());
        for key in [
            "sync_token:v27:PrimarySync",
            "sync_token:PrimarySync",
            "pending_sync_token:v27:enum:PrimarySync",
            "observed_selection:v27:PrimarySync",
            "observed_selection_staged:v27:enum:PrimarySync",
        ] {
            let present: bool = conn
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM metadata WHERE key=?1)",
                    [key],
                    |row| row.get(0),
                )
                .unwrap();
            assert!(!present, "checkpoint key must be cleared: {key}");
        }
        for key in [
            "baseline_cursor:v27:old:scope:enum:preserve",
            "retained_checkpoint_hold:v27:PrimarySync",
            "indexing_replay_required:PrimarySync",
            "download_ledger_scope:PrimarySync",
            "unknown:v1",
        ] {
            let present: bool = conn
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM metadata WHERE key=?1)",
                    [key],
                    |row| row.get(0),
                )
                .unwrap();
            assert!(present, "unrelated key must be preserved: {key}");
        }
    }

    #[test]
    fn candidate_version_1_record_refuses_before_any_write() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE metadata(key TEXT PRIMARY KEY, value TEXT NOT NULL);
             CREATE TABLE assets(library TEXT NOT NULL, id TEXT NOT NULL, PRIMARY KEY(library,id));",
        )
        .unwrap();
        conn.execute_batch(super::super::source_revalidation::DDL)
            .unwrap();
        establish_epoch(&conn, EpochOrigin::ExistingUpgrade).unwrap();
        let epoch = validate(&conn).unwrap();
        conn.execute(
            "INSERT INTO assets(library,id) VALUES('PrimarySync','asset')",
            [],
        )
        .unwrap();
        let candidate = serde_json::json!({
            "version": 1,
            "epoch": epoch.epoch.clone(),
            "library": "PrimarySync",
            "state": "ready",
            "reason": "existing_upgrade",
            "attempt": "old-attempt",
            "attempts": 1,
            "next_attempt_at": 0,
            "last_outcome": "baseline_staged",
            "baseline_scope": "scope",
            "enrolled_at": 0,
            "completed_at": null
        });
        let peer = serde_json::json!({
            "version": 2,
            "epoch": epoch.epoch.clone(),
            "library": "PeerSync",
            "state": "complete",
            "reason": "existing_upgrade",
            "attempt": "peer-attempt",
            "attempts": 0,
            "next_attempt_at": 0,
            "last_outcome": "complete",
            "baseline_scope": "scope",
            "enrolled_at": 0,
            "completed_at": 0
        });
        let before = {
            for (key, value) in [
                (library_key("PrimarySync"), candidate.to_string()),
                (
                    "source_revalidation_reset_pending:v1:PrimarySync".to_owned(),
                    "marker".to_owned(),
                ),
                (
                    "source_revalidation_reset_proof:v1:old-attempt:PrimarySync".to_owned(),
                    "proof".to_owned(),
                ),
                ("sync_token:v27:PrimarySync".to_owned(), "active".to_owned()),
                (
                    format!("baseline_cursor:v27:{}:scope:enum:", epoch.epoch),
                    "boundary".to_owned(),
                ),
                (library_key("PeerSync"), peer.to_string()),
                (library_key("BrokenPeer"), "not-json".to_owned()),
            ] {
                conn.execute(
                    "INSERT INTO metadata(key,value) VALUES(?1,?2)",
                    params![key, value],
                )
                .unwrap();
            }
            metadata_rows(&conn)
        };
        assert!(matches!(
            enroll_library(&conn, "PrimarySync", "scope"),
            Err(StateError::SyncEpochMismatch)
        ));
        assert!(matches!(
            gate(&conn, "PrimarySync", "scope", 0, None),
            Err(StateError::SyncEpochMismatch)
        ));
        assert_eq!(
            metadata_rows(&conn),
            before,
            "refusing a candidate record must not write or delete any row"
        );
        assert!(
            read_library(&conn, "PeerSync").unwrap().is_some(),
            "an unrelated current peer must still work"
        );
        assert!(
            ready_proof_baseline_keys(&conn).unwrap().is_empty(),
            "unreadable or unrelated peers must not abort a healthy transition"
        );
        conn.execute(
            "INSERT INTO metadata(key,value) VALUES(?1,?2),(?3,?4)",
            params![
                "source_revalidation_reset_proof:v1:old-attempt:Parent:Child",
                "proof",
                "sync_token:v27:Parent:Child",
                "child-token"
            ],
        )
        .unwrap();
        assert!(
            read_library(&conn, "Child").unwrap().is_none(),
            "legacy evidence must match the whole library name, not a suffix"
        );
        assert!(matches!(
            read_library(&conn, "Parent:Child"),
            Err(StateError::SyncEpochMismatch)
        ));
        let before_child = metadata_rows(&conn);
        assert!(matches!(
            enroll_library(&conn, "Parent:Child", "scope"),
            Err(StateError::SyncEpochMismatch)
        ));
        assert_eq!(
            metadata_rows(&conn),
            before_child,
            "legacy proof refusal must not clear the zone checkpoint"
        );
        conn.execute(
            "INSERT INTO metadata(key,value) VALUES(?1,?2),(?3,?4)",
            params![
                "source_revalidation_reset_pending:v1:MarkerOnly",
                "marker",
                "sync_token:v27:MarkerOnly",
                "marker-token"
            ],
        )
        .unwrap();
        let before_marker = metadata_rows(&conn);
        assert!(matches!(
            enroll_library(&conn, "MarkerOnly", "scope"),
            Err(StateError::SyncEpochMismatch)
        ));
        assert_eq!(
            metadata_rows(&conn),
            before_marker,
            "legacy marker refusal must not clear the zone checkpoint"
        );
    }

    #[tokio::test]
    async fn missing_epoch_fails_validation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        let db = SqliteStateDb::open_owned(&path, &owner()).await.unwrap();
        let conn = db.acquire_lock("delete epoch").unwrap();
        conn.execute("DELETE FROM metadata WHERE key=?1", [SYNC_EPOCH_KEY])
            .unwrap();
        assert!(matches!(
            validate(&conn),
            Err(StateError::SyncEpochMissing { .. })
        ));
    }

    #[test]
    fn epoch_validation_rejects_wrong_account() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE metadata(key TEXT PRIMARY KEY, value TEXT NOT NULL);
             CREATE TABLE account_owner(singleton INTEGER PRIMARY KEY, version INTEGER NOT NULL, account_key TEXT NOT NULL, provider_key TEXT NOT NULL);
             INSERT INTO account_owner VALUES(1,1,'account','provider');",
        )
        .unwrap();
        conn.execute_batch(super::super::source_revalidation::DDL)
            .unwrap();
        establish_epoch(&conn, EpochOrigin::ExistingUpgrade).unwrap();
        conn.execute(
            "UPDATE account_owner SET account_key='other' WHERE singleton=1",
            [],
        )
        .unwrap();
        assert!(matches!(
            validate(&conn),
            Err(StateError::SyncEpochMismatch)
        ));
    }

    #[test]
    fn migrate_to_current_writes_epoch() {
        let conn = Connection::open_in_memory().unwrap();
        schema::migrate(&conn).unwrap();
        let epoch = validate(&conn).unwrap();
        assert_eq!(epoch.version, 1);
    }
    #[tokio::test]
    async fn same_number_43_without_epoch_fails_before_any_write() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        let db = SqliteStateDb::open_owned(&path, &owner()).await.unwrap();
        {
            let conn = db.acquire_lock("prepare bare 43").unwrap();
            conn.execute("DELETE FROM metadata WHERE key=?1", [SYNC_EPOCH_KEY])
                .unwrap();
            conn.execute(
                "INSERT INTO metadata(key,value) VALUES('sentinel:v1','keep')",
                [],
            )
            .unwrap();
        }
        drop(db);
        let error = SqliteStateDb::open_owned(&path, &owner())
            .await
            .expect_err("schema 43 without an epoch must fail closed");
        assert!(matches!(error, StateError::SyncEpochMissing { .. }));
        let conn = Connection::open(&path).unwrap();
        let version: i32 = conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, 43);
        let sentinel: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM metadata WHERE key='sentinel:v1')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(sentinel, "refusal must not delete existing state");
        assert!(read_epoch(&conn).unwrap().is_none());
    }

    #[tokio::test]
    async fn future_schema_44_is_refused_before_writes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        let db = SqliteStateDb::open_owned(&path, &owner()).await.unwrap();
        {
            let conn = db.acquire_lock("prepare future schema").unwrap();
            conn.execute(
                "INSERT INTO metadata(key,value) VALUES('sentinel:v1','keep')",
                [],
            )
            .unwrap();
            conn.execute_batch("PRAGMA user_version=44").unwrap();
        }
        drop(db);
        let error = SqliteStateDb::open_owned(&path, &owner())
            .await
            .expect_err("schema 44 must be refused");
        assert!(matches!(
            error,
            StateError::UnsupportedSchemaVersion {
                found: 44,
                expected: 43
            }
        ));
        let conn = Connection::open(&path).unwrap();
        let sentinel: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM metadata WHERE key='sentinel:v1')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(sentinel);
    }

    #[tokio::test]
    async fn owned_schema_zero_is_existing_and_unowned_fails() {
        let dir = tempfile::tempdir().unwrap();
        let owned_path = dir.path().join("owned-zero.db");
        {
            let conn = super::super::account::open(&owned_path, &owner()).unwrap();
            conn.execute_batch("PRAGMA user_version=0").unwrap();
        }
        let db = SqliteStateDb::open_owned(&owned_path, &owner())
            .await
            .unwrap();
        {
            let conn = db.acquire_lock("inspect owned zero").unwrap();
            let epoch = read_epoch(&conn).unwrap().unwrap();
            assert_eq!(epoch.origin, "existing_upgrade");
        }
        drop(db);

        let unowned_path = dir.path().join("unowned-zero.db");
        std::fs::write(&unowned_path, b"").unwrap();
        let error = SqliteStateDb::open_owned(&unowned_path, &owner())
            .await
            .expect_err("unowned schema zero must fail");
        assert!(matches!(error, StateError::AccountOwnerMissing));
    }

    #[tokio::test]
    async fn crash_after_bind_is_conservatively_existing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        {
            let conn = super::super::account::open(&path, &owner()).unwrap();
            assert_eq!(schema::get_schema_version(&conn).unwrap(), 0);
        }
        let db = SqliteStateDb::open_owned(&path, &owner()).await.unwrap();
        let conn = db.acquire_lock("inspect crash after bind").unwrap();
        let epoch = read_epoch(&conn).unwrap().unwrap();
        assert_eq!(epoch.origin, "existing_upgrade");
    }

    #[tokio::test]
    async fn crash_after_migration_before_enrollment_preserves_epoch() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        {
            let conn = super::super::account::open(&path, &owner()).unwrap();
            conn.execute_batch("PRAGMA user_version=0").unwrap();
        }
        let db = SqliteStateDb::open_owned(&path, &owner()).await.unwrap();
        let epoch = {
            let conn = db.acquire_lock("read epoch before crash").unwrap();
            read_epoch(&conn).unwrap().unwrap().epoch
        };
        drop(db);
        let db = SqliteStateDb::open_owned(&path, &owner()).await.unwrap();
        {
            let conn = db.acquire_lock("inspect pre-enrollment crash").unwrap();
            assert_eq!(read_epoch(&conn).unwrap().unwrap().epoch, epoch);
            assert!(read_library(&conn, "PrimarySync").unwrap().is_none());
        }
        db.enroll_sync_epoch(vec![("PrimarySync".to_owned(), "scope".to_owned())])
            .await
            .unwrap();
        let conn = db.acquire_lock("inspect enrollment").unwrap();
        let record = read_library(&conn, "PrimarySync").unwrap().unwrap();
        assert_eq!(record.state, "pending");
    }

    #[tokio::test]
    async fn later_selected_library_enrolls_pending() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        {
            let conn = super::super::account::open(&path, &owner()).unwrap();
            conn.execute_batch("PRAGMA user_version=0").unwrap();
        }
        let db = SqliteStateDb::open_owned(&path, &owner()).await.unwrap();
        db.enroll_sync_epoch(vec![("PrimarySync".to_owned(), "scope-a".to_owned())])
            .await
            .unwrap();
        db.enroll_sync_epoch(vec![("SharedSync-later".to_owned(), "scope-b".to_owned())])
            .await
            .unwrap();
        let conn = db.acquire_lock("inspect later library").unwrap();
        let later = read_library(&conn, "SharedSync-later").unwrap().unwrap();
        assert_eq!(later.state, "pending");
        assert_eq!(later.reason, "new_library");
    }

    #[test]
    fn epoch_creation_quarantines_rounds_and_retains_debt() {
        let conn = Connection::open_in_memory().unwrap();
        for version in 1..=42 {
            schema::migrate_to_version(&conn, 0, version).unwrap();
        }
        conn.execute(
            "INSERT INTO source_revalidation_rounds VALUES('r1','PrimarySync','scope','policy','boundary','high','after','active')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO source_revalidation_debt(round,id) VALUES('r1','asset')",
            [],
        )
        .unwrap();
        establish_epoch(&conn, EpochOrigin::ExistingUpgrade).unwrap();
        let phase: String = conn
            .query_row(
                "SELECT phase FROM source_revalidation_rounds WHERE id='r1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(phase, "abandoned");
        let debts: i64 = conn
            .query_row("SELECT COUNT(*) FROM source_revalidation_debt", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(debts, 1);
        assert_eq!(read_epoch(&conn).unwrap().unwrap().quarantined_rounds, 1);
    }
    #[test]
    fn epoch_enrollment_preserves_receipts_generations_and_local_obligations() {
        let conn = Connection::open_in_memory().unwrap();
        for version in 1..=42 {
            schema::migrate_to_version(&conn, 0, version).unwrap();
        }
        conn.execute(
            "INSERT INTO source_revalidation_rounds VALUES('r1','PrimarySync','scope','policy','boundary','high','after','active')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO source_revalidation_debt(round,id) VALUES('r1','asset')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO metadata(key,value) VALUES
              ('sync_token:v27:PrimarySync','active'),
              ('pending_sync_token:v27:enum:PrimarySync','staged'),
              ('observed_selection:v27:PrimarySync','observed'),
              ('observed_selection_staged:v27:enum:PrimarySync','observed-staged'),
              ('baseline_cursor:v27:old:scope:enum:preserve','old-boundary'),
              ('retained_checkpoint_hold:v27:PrimarySync','hold'),
              ('download_ledger_scope:PrimarySync','ledger-scope'),
              ('unresolved_current_asset_identity:PrimarySync','identity'),
              ('indexing_replay_required:PrimarySync','1'),
              ('unknown:v1','keep')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO owned_temp_files(path,claimed_at) VALUES(x'01',0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO metadata_capture_retries(library,asset_id,target_revision,evidence,generation,attempts,last_attempt_at,next_retry_at) VALUES('PrimarySync','asset',1,'e',1,1,0,0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO reconciliation_paths(library,id,version_size,requested_path_key,destination_path_key,destination_path,provider_checksum,provider_size) VALUES('PrimarySync','asset','original','requested','destination','/old/path','checksum',1)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO primary_layout_operations(operation,family,library,source_library,prior_generation,decision,policy,phase,header,conflict) VALUES('op','family','PrimarySync','source',1,'decision','policy','committed',x'00',NULL)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO provider_active_generations(id,account_key,provider_key,scope,config_hash,basis,metadata_enabled,specification,specification_hash,created_at,charged_bytes) VALUES('generation','account','provider','scope','config','basis',0,x'00','hash',0,1)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO download_receipts(id,account_key,provider_key,scope,library,child,master,state_id,origin_pass,version_size,provider_checksum,provider_size,native_path,source_checksum,local_checksum,proof_hash) VALUES('receipt','account','provider','scope','PrimarySync','child','master','state','pass','original','provider-checksum',1,'/old/path','source-checksum','local-checksum','proof')",
            [],
        )
        .unwrap();
        establish_epoch(&conn, EpochOrigin::ExistingUpgrade).unwrap();
        enroll_library(&conn, "PrimarySync", "scope").unwrap();
        for table in [
            "owned_temp_files",
            "metadata_capture_retries",
            "reconciliation_paths",
            "primary_layout_operations",
            "provider_active_generations",
            "download_receipts",
        ] {
            let count: i64 = conn
                .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                    row.get(0)
                })
                .unwrap();
            assert_eq!(count, 1, "epoch must preserve {table}");
        }
        for key in [
            "baseline_cursor:v27:old:scope:enum:preserve",
            "retained_checkpoint_hold:v27:PrimarySync",
            "download_ledger_scope:PrimarySync",
            "unresolved_current_asset_identity:PrimarySync",
            "indexing_replay_required:PrimarySync",
            "unknown:v1",
        ] {
            let present: bool = conn
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM metadata WHERE key=?1)",
                    [key],
                    |row| row.get(0),
                )
                .unwrap();
            assert!(present, "epoch must preserve {key}");
        }
        let phase: String = conn
            .query_row(
                "SELECT phase FROM source_revalidation_rounds WHERE id='r1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(phase, "abandoned");
        let debts: i64 = conn
            .query_row("SELECT COUNT(*) FROM source_revalidation_debt", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(debts, 1);
    }
    #[tokio::test]
    async fn genuine_versions_35_through_42_upgrade_as_existing() {
        let test_owner = owner();
        let (account_key, provider_key) = test_owner.authenticated_keys().unwrap();
        for version in [35, 36, 40, 41, 42] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join(format!("v{version}.db"));
            {
                let conn = Connection::open(&path).unwrap();
                for step in 1..=version {
                    schema::migrate_to_version(&conn, 0, step).unwrap();
                }
                conn.execute_batch(
                    "CREATE TABLE account_owner(singleton INTEGER PRIMARY KEY CHECK(singleton=1),version INTEGER NOT NULL,account_key TEXT NOT NULL,provider_key TEXT NOT NULL);",
                )
                .unwrap();
                conn.execute(
                    "INSERT INTO account_owner VALUES(1,1,?1,?2)",
                    rusqlite::params![account_key, provider_key],
                )
                .unwrap();
                if version >= 40 {
                    conn.execute(
                        "INSERT INTO source_revalidation_rounds VALUES('r1','PrimarySync','scope','policy','boundary','high','after','active')",
                        [],
                    )
                    .unwrap();
                    conn.execute(
                        "INSERT INTO source_revalidation_debt(round,id) VALUES('r1','asset')",
                        [],
                    )
                    .unwrap();
                }
            }
            let db = SqliteStateDb::open_owned(&path, &owner())
                .await
                .unwrap_or_else(|error| panic!("v{version}: {error}"));
            {
                let conn = db.acquire_lock("inspect genuine upgrade").unwrap();
                let epoch = validate(&conn).unwrap();
                assert_eq!(epoch.origin, "existing_upgrade", "v{version}");
                if version >= 40 {
                    let phase: String = conn
                        .query_row(
                            "SELECT phase FROM source_revalidation_rounds WHERE id='r1'",
                            [],
                            |row| row.get(0),
                        )
                        .unwrap();
                    assert_eq!(phase, "abandoned", "v{version}");
                    let debts: i64 = conn
                        .query_row("SELECT COUNT(*) FROM source_revalidation_debt", [], |row| {
                            row.get(0)
                        })
                        .unwrap();
                    assert_eq!(debts, 1, "v{version}");
                }
            }
            db.enroll_sync_epoch(vec![("PrimarySync".to_owned(), "scope".to_owned())])
                .await
                .unwrap();
            let conn = db.acquire_lock("inspect genuine enrollment").unwrap();
            let record = read_library(&conn, "PrimarySync").unwrap().unwrap();
            assert_eq!(record.state, "pending", "v{version}");
        }
    }
    #[tokio::test]
    async fn enrollment_abort_rolls_back_keys_and_record() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        {
            let conn = super::super::account::open(&path, &owner()).unwrap();
            conn.execute_batch("PRAGMA user_version=0").unwrap();
        }
        let db = SqliteStateDb::open_owned(&path, &owner()).await.unwrap();
        {
            let conn = db.acquire_lock("seed enrollment abort").unwrap();
            conn.execute(
                "INSERT INTO metadata(key,value) VALUES(?1,'active')",
                ["sync_token:v27:PrimarySync"],
            )
            .unwrap();
            conn.execute_batch(
                "CREATE TRIGGER fail_epoch_record BEFORE INSERT ON metadata \
                 WHEN NEW.key = 'sync_epoch_library:v1:PrimarySync' \
                 BEGIN SELECT RAISE(ABORT,'synthetic enrollment abort'); END;",
            )
            .unwrap();
        }
        assert!(
            db.enroll_sync_epoch(vec![("PrimarySync".to_owned(), "scope".to_owned())])
                .await
                .is_err()
        );
        {
            let conn = db.acquire_lock("inspect aborted enrollment").unwrap();
            let token: Option<String> = conn
                .query_row(
                    "SELECT value FROM metadata WHERE key=?1",
                    ["sync_token:v27:PrimarySync"],
                    |row| row.get(0),
                )
                .ok();
            assert_eq!(token.as_deref(), Some("active"));
            assert!(read_library(&conn, "PrimarySync").unwrap().is_none());
            conn.execute("DROP TRIGGER fail_epoch_record", []).unwrap();
        }
        db.enroll_sync_epoch(vec![("PrimarySync".to_owned(), "scope".to_owned())])
            .await
            .unwrap();
        let conn = db.acquire_lock("inspect retried enrollment").unwrap();
        let record = read_library(&conn, "PrimarySync").unwrap().unwrap();
        assert_eq!(record.state, "pending");
        let token: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM metadata WHERE key=?1)",
                ["sync_token:v27:PrimarySync"],
                |row| row.get(0),
            )
            .unwrap();
        assert!(!token, "retried enrollment must clear the checkpoint");
    }
    #[tokio::test]
    async fn auth_cancel_recovery_does_not_consume_attempt_or_delay() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        {
            let conn = super::super::account::open(&path, &owner()).unwrap();
            conn.execute_batch("PRAGMA user_version=0").unwrap();
        }
        let db = SqliteStateDb::open_owned(&path, &owner()).await.unwrap();
        db.enroll_sync_epoch(vec![("PrimarySync".to_owned(), "scope".to_owned())])
            .await
            .unwrap();
        let gate = db
            .sync_epoch_gate("PrimarySync".to_owned(), "scope".to_owned(), None)
            .await
            .unwrap();
        assert!(matches!(gate, SyncEpochGate::Baseline { .. }));
        let claimed_deadline = {
            let conn = db.acquire_lock("inspect claim").unwrap();
            let record = read_library(&conn, "PrimarySync").unwrap().unwrap();
            assert_eq!(
                record.attempts, 0,
                "claim must not count a provider failure"
            );
            assert!(record.next_attempt_at > chrono::Utc::now().timestamp());
            record.next_attempt_at
        };
        db.recover_sync_epoch_attempt("PrimarySync".to_owned(), false, None)
            .await
            .unwrap();
        {
            let conn = db.acquire_lock("inspect auth recovery").unwrap();
            let record = read_library(&conn, "PrimarySync").unwrap().unwrap();
            assert_eq!(record.attempts, 0);
            assert!(
                record.next_attempt_at >= claimed_deadline,
                "auth recovery must preserve the claim's crash-guard deadline"
            );
            assert!(
                record.next_attempt_at > chrono::Utc::now().timestamp(),
                "auth recovery must not rearm immediately"
            );
        }
        let gate = db
            .sync_epoch_gate("PrimarySync".to_owned(), "scope".to_owned(), None)
            .await
            .unwrap();
        assert!(
            matches!(
                gate,
                SyncEpochGate::Defer {
                    reason: "epoch_backoff"
                }
            ),
            "auth recovery must defer a bounded retry, got {gate:?}"
        );
        db.recover_sync_epoch_attempt("PrimarySync".to_owned(), true, None)
            .await
            .unwrap();
        let conn = db
            .acquire_lock("inspect provider failure recovery")
            .unwrap();
        let record = read_library(&conn, "PrimarySync").unwrap().unwrap();
        assert_eq!(record.attempts, 1);
        assert!(record.next_attempt_at > chrono::Utc::now().timestamp());
    }

    const TEST_LIBRARY: &str = "PrimarySync";
    const TEST_INDEXING_KEY: &str = "indexing_reconciliation_required:PrimarySync";
    const TEST_REPLAY_KEY: &str = "indexing_replay_required:v27:PrimarySync";
    const TEST_MATERIALIZATION_KEY: &str = "indexing_materialization_required:PrimarySync";
    const TEST_SYNC_TOKEN_KEY: &str = "sync_token:v27:PrimarySync";
    const TEST_HOLD_KEY: &str = "retained_checkpoint_hold:v27:PrimarySync";

    fn qualification_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE metadata(key TEXT PRIMARY KEY, value TEXT NOT NULL);
             CREATE TABLE assets(library TEXT NOT NULL, id TEXT NOT NULL, PRIMARY KEY(library,id));
             CREATE TABLE account_owner (
                 singleton INTEGER PRIMARY KEY CHECK(singleton=1),
                 version INTEGER NOT NULL,
                 account_key TEXT NOT NULL,
                 provider_key TEXT NOT NULL
             );",
        )
        .unwrap();
        conn.execute_batch(super::super::source_revalidation::DDL)
            .unwrap();
        conn.execute(
            "INSERT INTO account_owner VALUES(1,1,'account-a',?1)",
            ["a".repeat(64)],
        )
        .unwrap();
        establish_epoch(&conn, EpochOrigin::ExistingUpgrade).unwrap();
        conn.execute(
            "INSERT INTO assets(library,id) VALUES(?1,'historical')",
            [TEST_LIBRARY],
        )
        .unwrap();
        enroll_library(&conn, TEST_LIBRARY, "scope").unwrap();
        let mut record = read_library(&conn, TEST_LIBRARY).unwrap().unwrap();
        record.next_attempt_at = 1_000;
        write_library(&conn, &record).unwrap();
        for (key, value) in [
            (crate::sync_cycle::ENUM_CONFIG_HASH_KEY, "enum-hash"),
            (
                crate::sync_cycle::PENDING_ENUM_CONFIG_HASH_KEY,
                "pending-enum-hash",
            ),
            (crate::download::DOWNLOAD_CONFIG_HASH_KEY, "config-hash"),
            (
                crate::sync_cycle::PENDING_DOWNLOAD_CONFIG_HASH_KEY,
                "pending-config-hash",
            ),
            (TEST_MATERIALIZATION_KEY, "materialization"),
            (TEST_INDEXING_KEY, "1"),
            (TEST_REPLAY_KEY, "retained-replay"),
            (TEST_SYNC_TOKEN_KEY, "retained-token"),
            (TEST_HOLD_KEY, "retained-hold"),
        ] {
            conn.execute(
                "INSERT OR REPLACE INTO metadata(key,value) VALUES(?1,?2)",
                params![key, value],
            )
            .unwrap();
        }
        conn
    }

    fn qualification_fence(conn: &Connection) -> Vec<(String, Option<String>)> {
        let mut fence: Vec<(String, Option<String>)> = [
            crate::sync_cycle::ENUM_CONFIG_HASH_KEY,
            crate::sync_cycle::PENDING_ENUM_CONFIG_HASH_KEY,
            crate::download::DOWNLOAD_CONFIG_HASH_KEY,
            crate::sync_cycle::PENDING_DOWNLOAD_CONFIG_HASH_KEY,
            TEST_MATERIALIZATION_KEY,
            TEST_INDEXING_KEY,
            TEST_REPLAY_KEY,
            TEST_SYNC_TOKEN_KEY,
            TEST_HOLD_KEY,
        ]
        .into_iter()
        .map(|key| (key.to_owned(), metadata_value(conn, key).unwrap()))
        .collect();
        fence.push((
            SYNC_EPOCH_KEY.to_owned(),
            metadata_value(conn, SYNC_EPOCH_KEY).unwrap(),
        ));
        fence
    }

    fn reserve_qualification(
        conn: &Connection,
        fence: &[(String, Option<String>)],
        now: i64,
    ) -> QualificationReserve {
        reserve_indexing_qualification(
            conn,
            TEST_LIBRARY,
            TEST_INDEXING_KEY,
            TEST_REPLAY_KEY,
            fence,
            now,
            None,
        )
        .unwrap()
    }

    fn reserved_or_panic(outcome: QualificationReserve) -> QualificationReservation {
        match outcome {
            QualificationReserve::Reserved(reservation) => *reservation,
            other => panic!("expected reservation, got {other:?}"),
        }
    }

    fn current_record(conn: &Connection) -> SyncEpochLibrary {
        read_library(conn, TEST_LIBRARY).unwrap().unwrap()
    }

    fn stored_record(conn: &Connection) -> Option<String> {
        metadata_value(conn, &library_key(TEST_LIBRARY)).unwrap()
    }

    fn write_raw_record(conn: &Connection, record: &SyncEpochLibrary) {
        conn.execute(
            "UPDATE metadata SET value=?1 WHERE key=?2",
            params![
                serde_json::to_string(record).unwrap(),
                library_key(TEST_LIBRARY)
            ],
        )
        .unwrap();
    }

    fn mutate_owner_and_epoch_consistently(conn: &Connection) {
        let provider = "b".repeat(64);
        conn.execute(
            "UPDATE account_owner SET account_key='account-b', provider_key=?1 WHERE singleton=1",
            [&provider],
        )
        .unwrap();
        let raw = metadata_value(conn, SYNC_EPOCH_KEY).unwrap().unwrap();
        let mut epoch: SyncEpoch = serde_json::from_str(&raw).unwrap();
        epoch.account_key = "account-b".to_owned();
        epoch.provider_key = provider;
        conn.execute(
            "UPDATE metadata SET value=?1 WHERE key=?2",
            params![serde_json::to_string(&epoch).unwrap(), SYNC_EPOCH_KEY],
        )
        .unwrap();
    }

    #[test]
    fn qualification_reservation_preserves_phase_and_captures_fences() {
        let conn = qualification_db();
        let before = current_record(&conn);
        let fence = qualification_fence(&conn);
        let raw_epoch = metadata_value(&conn, SYNC_EPOCH_KEY).unwrap().unwrap();
        let reservation = reserved_or_panic(reserve_qualification(&conn, &fence, 10_000));
        let record = current_record(&conn);

        assert_eq!(record.state, "pending");
        assert!(record.binding.is_none());
        assert_eq!(record.attempt, before.attempt);
        assert_eq!(record.attempts, before.attempts);
        assert_eq!(
            record.next_attempt_at,
            10_000_i64.saturating_add(retry_floor(None))
        );
        let outcome = parse_qualification_outcome(record.last_outcome.as_deref())
            .unwrap()
            .unwrap();
        assert_eq!(outcome.version, 1);
        assert_eq!(outcome.nonce, reservation.nonce);
        assert_eq!(outcome.prior_next_attempt_at, before.next_attempt_at);
        assert_eq!(
            reservation.reserved_record,
            encode_library(&record).unwrap()
        );
        assert!(reservation.expected_metadata.iter().any(|(key, value)| {
            key == SYNC_EPOCH_KEY && value.as_deref() == Some(raw_epoch.as_str())
        }));
        assert!(reservation.expected_metadata.iter().any(|(key, value)| {
            key == &library_key(TEST_LIBRARY)
                && value.as_deref() == Some(reservation.reserved_record.as_str())
        }));
        let restored: SyncEpochLibrary =
            serde_json::from_str(&reservation.restored_record).unwrap();
        assert_eq!(restored.state, "pending");
        assert!(restored.binding.is_none());
        assert_eq!(restored.attempt, before.attempt);
        assert_eq!(restored.attempts, before.attempts);
        assert_eq!(restored.next_attempt_at, before.next_attempt_at);
        assert_eq!(
            restored.last_outcome.as_deref(),
            Some(QUALIFICATION_QUALIFIED_OUTCOME)
        );
    }

    #[test]
    fn qualification_not_due_and_not_pending_do_not_enter_lane() {
        let conn = qualification_db();
        let mut record = current_record(&conn);
        record.next_attempt_at = 100_000;
        write_library(&conn, &record).unwrap();
        let before = stored_record(&conn);
        let fence = qualification_fence(&conn);
        assert_eq!(
            reserve_qualification(&conn, &fence, 99_999),
            QualificationReserve::NotDue {
                next_attempt_at: 100_000
            }
        );
        assert_eq!(stored_record(&conn), before);

        record.state = "complete".to_owned();
        write_library(&conn, &record).unwrap();
        assert_eq!(
            reserve_qualification(&conn, &qualification_fence(&conn), 200_000),
            QualificationReserve::NotPending
        );
    }

    #[test]
    fn qualification_stale_fence_refuses_finish_and_reentry() {
        let conn = qualification_db();
        let fence = qualification_fence(&conn);
        let mut missing_epoch = fence.clone();
        missing_epoch.retain(|(key, _)| key != SYNC_EPOCH_KEY);
        assert_eq!(
            reserve_qualification(&conn, &missing_epoch, 10_000),
            QualificationReserve::StaleFence
        );
        let mut missing_hold = fence.clone();
        missing_hold.retain(|(key, _)| key != TEST_INDEXING_KEY);
        assert_eq!(
            reserve_qualification(&conn, &missing_hold, 10_000),
            QualificationReserve::StaleFence
        );
        let reservation = reserved_or_panic(reserve_qualification(&conn, &fence, 10_000));
        let reserved = stored_record(&conn);
        conn.execute(
            "UPDATE metadata SET value='changed' WHERE key=?1",
            [crate::download::DOWNLOAD_CONFIG_HASH_KEY],
        )
        .unwrap();

        assert_eq!(
            finish_qualification_nonqualified(
                &conn,
                &reservation,
                QualificationFailure::ProofIncomplete
            )
            .unwrap(),
            QualificationFinish::NotOwner
        );
        assert_eq!(stored_record(&conn), reserved);
        assert_eq!(
            reserve_qualification(&conn, &fence, 20_000),
            QualificationReserve::StaleFence
        );
        assert_eq!(stored_record(&conn), reserved);
    }

    #[test]
    fn qualification_consistent_owner_and_epoch_mutation_refuses() {
        let conn = qualification_db();
        let fence = qualification_fence(&conn);
        let reservation = reserved_or_panic(reserve_qualification(&conn, &fence, 10_000));
        let reserved = stored_record(&conn);
        mutate_owner_and_epoch_consistently(&conn);
        validate(&conn).unwrap();

        assert_eq!(
            finish_qualification_nonqualified(
                &conn,
                &reservation,
                QualificationFailure::ProofIncomplete
            )
            .unwrap(),
            QualificationFinish::NotOwner
        );
        assert_eq!(stored_record(&conn), reserved);
        assert_eq!(
            reserve_qualification(&conn, &fence, 20_000),
            QualificationReserve::StaleFence
        );
        assert_eq!(stored_record(&conn), reserved);
    }

    #[test]
    fn qualification_new_nonce_supersedes_stale_finisher_and_reset_refuses() {
        let conn = qualification_db();
        let fence = qualification_fence(&conn);
        let first = reserved_or_panic(reserve_qualification(&conn, &fence, 10_000));
        let first_deadline = current_record(&conn).next_attempt_at;
        let second = reserved_or_panic(reserve_qualification(&conn, &fence, first_deadline));
        assert_ne!(first.nonce, second.nonce);

        assert_eq!(
            finish_qualification_nonqualified(&conn, &first, QualificationFailure::ProofIncomplete)
                .unwrap(),
            QualificationFinish::NotOwner
        );
        assert_eq!(stored_record(&conn), Some(second.reserved_record.clone()));
        let second_deadline = current_record(&conn).next_attempt_at;
        assert_eq!(
            finish_qualification_nonqualified(
                &conn,
                &second,
                QualificationFailure::ProofIncomplete
            )
            .unwrap(),
            QualificationFinish::Finished
        );
        assert_eq!(
            finish_qualification_nonqualified(
                &conn,
                &second,
                QualificationFailure::ProofIncomplete
            )
            .unwrap(),
            QualificationFinish::NotOwner,
            "the same nonce cannot finish twice"
        );

        let third = reserved_or_panic(reserve_qualification(&conn, &fence, second_deadline));
        reset_libraries_for_token_reset(&conn, 2_000_000).unwrap();
        assert_eq!(
            finish_qualification_nonqualified(&conn, &third, QualificationFailure::ProofIncomplete)
                .unwrap(),
            QualificationFinish::NotOwner
        );
    }

    #[test]
    fn qualification_finish_paths_keep_floor_or_bounded_backoff() {
        let conn = qualification_db();
        let fence = qualification_fence(&conn);
        let nonqualified = reserved_or_panic(reserve_qualification(&conn, &fence, 10_000));
        let floor = current_record(&conn).next_attempt_at;
        assert_eq!(
            finish_qualification_nonqualified(
                &conn,
                &nonqualified,
                QualificationFailure::EnumerationIncomplete
            )
            .unwrap(),
            QualificationFinish::Finished
        );
        let record = current_record(&conn);
        assert_eq!(
            record.last_outcome.as_deref(),
            Some("qualification_incomplete:v1:enumeration_incomplete")
        );
        assert_eq!(record.next_attempt_at, floor);
        assert_eq!(record.attempts, 0);

        let conn = qualification_db();
        let fence = qualification_fence(&conn);
        let provider = reserved_or_panic(reserve_qualification(&conn, &fence, 20_000));
        assert_eq!(
            finish_qualification_provider_failure(&conn, &provider, 30_000, None).unwrap(),
            QualificationFinish::Finished
        );
        let record = current_record(&conn);
        assert_eq!(record.attempts, 1);
        assert_eq!(
            record.next_attempt_at,
            30_000_i64.saturating_add(retry_delay(None, 1))
        );
        assert_eq!(record.last_outcome.as_deref(), Some("provider_failure"));

        let conn = qualification_db();
        let fence = qualification_fence(&conn);
        let auth = reserved_or_panic(reserve_qualification(&conn, &fence, 30_000));
        let reserved_deadline = current_record(&conn).next_attempt_at;
        assert_eq!(
            finish_qualification_auth_or_cancel(&conn, &auth, 40_000, None).unwrap(),
            QualificationFinish::Finished
        );
        let record = current_record(&conn);
        assert_eq!(record.attempts, 0);
        assert_eq!(
            record.next_attempt_at,
            reserved_deadline.max(40_000_i64.saturating_add(retry_floor(None)))
        );
        assert_eq!(
            record.last_outcome.as_deref(),
            Some("auth_cancel_or_shutdown")
        );
    }

    #[test]
    fn qualification_valid_in_progress_gate_defers_then_blocks() {
        let conn = qualification_db();
        let fence = qualification_fence(&conn);
        let reservation = reserved_or_panic(reserve_qualification(&conn, &fence, 10_000));
        let deadline = current_record(&conn).next_attempt_at;
        assert_eq!(
            gate(&conn, TEST_LIBRARY, "scope", deadline - 1, None).unwrap(),
            SyncEpochGate::Defer {
                reason: "epoch_backoff"
            }
        );
        assert_eq!(
            gate(&conn, TEST_LIBRARY, "scope", deadline, None).unwrap(),
            SyncEpochGate::Blocked {
                reason: "qualification_reservation_pending"
            }
        );
        assert_eq!(stored_record(&conn), Some(reservation.reserved_record));
    }

    #[test]
    fn qualification_unknown_namespace_blocks_gate_and_reserve() {
        let conn = qualification_db();
        let fence = qualification_fence(&conn);
        let _reservation = reserved_or_panic(reserve_qualification(&conn, &fence, 10_000));
        let mut record = current_record(&conn);
        record.last_outcome = Some("qualification_in_progress:v1:{not-json}".to_owned());
        write_raw_record(&conn, &record);
        let stored = stored_record(&conn);

        assert!(read_library(&conn, TEST_LIBRARY).is_err());
        assert!(gate(&conn, TEST_LIBRARY, "scope", 9_999_999, None).is_err());
        assert_eq!(
            reserve_qualification(&conn, &fence, 9_999_999),
            QualificationReserve::StaleFence
        );
        assert_eq!(stored_record(&conn), stored);

        record.last_outcome = Some(
            "qualification_in_progress:v2:{\"version\":2,\"nonce\":\"5154f8f1-7e3b-4c5a-9f2d-6a1b0c9d8e7f\",\"prior_next_attempt_at\":0}"
                .to_owned(),
        );
        write_raw_record(&conn, &record);
        assert!(gate(&conn, TEST_LIBRARY, "scope", 9_999_999, None).is_err());
        assert_eq!(
            reserve_qualification(&conn, &fence, 9_999_999),
            QualificationReserve::StaleFence
        );
    }

    #[test]
    fn qualification_in_progress_blocks_token_stage_complete_and_recovery() {
        let conn = qualification_db();
        let fence = qualification_fence(&conn);
        let reservation = reserved_or_panic(reserve_qualification(&conn, &fence, 10_000));
        let epoch = validate(&conn).unwrap();
        let binding = BaselineBinding {
            token_key: "pending_sync_token:v27:enum:PrimarySync".to_owned(),
            token_sha256: "token-sha".to_owned(),
            descriptor_key: "observed_selection_staged:v27:enum:PrimarySync".to_owned(),
            descriptor_proof_hash: "descriptor-sha".to_owned(),
            baseline_key: format!("baseline_cursor:v27:{}:scope:enum:", epoch.epoch),
            baseline_sha256: "baseline-sha".to_owned(),
        };
        assert!(record_ready(&conn, &epoch, TEST_LIBRARY, &reservation.attempt, binding).is_err());
        assert!(complete(&conn, TEST_LIBRARY, &reservation.attempt).is_err());
        assert_eq!(
            stored_record(&conn),
            Some(reservation.reserved_record.clone())
        );

        recover_attempt(&conn, TEST_LIBRARY, true, None).unwrap();
        defer_without_attempt(&conn, TEST_LIBRARY, 9_999_999, None, "deferred").unwrap();
        assert_eq!(
            stored_record(&conn),
            Some(reservation.reserved_record),
            "ordinary recovery must not overwrite the qualification guard"
        );
    }

    #[test]
    fn qualification_outcome_codec_is_strict_and_bounded() {
        let nonce = "5154f8f1-7e3b-4c5a-9f2d-6a1b0c9d8e7f";
        let encoded = encode_qualification_outcome(nonce, 0).unwrap();
        let parsed = parse_qualification_outcome(Some(&encoded))
            .unwrap()
            .unwrap();
        assert_eq!(parsed.version, QUALIFICATION_OUTCOME_VERSION);
        assert_eq!(parsed.nonce, nonce);
        assert_eq!(parsed.prior_next_attempt_at, 0);
        assert_eq!(
            parse_qualification_outcome(Some("baseline_claimed")).unwrap(),
            None
        );

        for invalid in [
            "qualification_in_progress:v2:{}",
            "qualification_in_progress:v1:{not-json}",
            "qualification_in_progress:v1:{\"version\":2,\"nonce\":\"5154f8f1-7e3b-4c5a-9f2d-6a1b0c9d8e7f\",\"prior_next_attempt_at\":0}",
            "qualification_in_progress:v1:{\"version\":1,\"nonce\":\"not-a-uuid\",\"prior_next_attempt_at\":0}",
        ] {
            assert!(
                parse_qualification_outcome(Some(invalid)).is_err(),
                "must reject {invalid}"
            );
        }

        let mut lengths = Vec::new();
        for prior in [i64::MIN, -1, 0, 1, i64::MAX] {
            let value = encode_qualification_outcome(nonce, prior).unwrap();
            lengths.push(value.len());
            let parsed = parse_qualification_outcome(Some(&value)).unwrap().unwrap();
            assert_eq!(parsed.prior_next_attempt_at, prior);
        }
        let max = *lengths.iter().max().unwrap();
        let min = *lengths.iter().min().unwrap();
        assert!(max >= min);
        assert!(
            max <= QUALIFICATION_OUTCOME_PREFIX.len() + 128,
            "serialized qualification outcome must stay bounded: {max}"
        );
    }
}
