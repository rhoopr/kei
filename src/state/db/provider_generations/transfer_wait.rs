//! Authenticated, per-rendition failed-transfer and shared-wait proofs.
//! These rows never complete an observation or change metadata.
use super::super::provider_selection::SelectionPath;
use super::{
    ActiveDecision, ActiveGeneration, MAX_CHUNK_BYTES, SqliteStateDb, account, charge,
    defer_attempt, digest, encode, invalid, load_decision, load_generation,
};
use crate::state::error::StateError;
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};

pub(crate) const DDL: &str = r"
CREATE TABLE provider_transfer_dispatch (
 generation TEXT NOT NULL, pass_key TEXT NOT NULL, child TEXT NOT NULL,
 version_size TEXT NOT NULL, path TEXT NOT NULL,
 state TEXT NOT NULL CHECK(state IN ('never','dispatched')),
 proof_hash TEXT NOT NULL, charged_bytes INTEGER NOT NULL CHECK(charged_bytes>0),
 PRIMARY KEY(generation,pass_key,child,version_size,path),
 FOREIGN KEY(generation,pass_key,child,version_size,path) REFERENCES provider_active_destinations(generation,pass_key,child,version_size,path) ON DELETE CASCADE
) WITHOUT ROWID;
CREATE TABLE provider_transfer_failures (
 generation TEXT NOT NULL, pass_key TEXT NOT NULL, child TEXT NOT NULL,
 version_size TEXT NOT NULL, path TEXT NOT NULL, url TEXT NOT NULL,
 attempt INTEGER NOT NULL CHECK(attempt>0), deadline INTEGER NOT NULL CHECK(deadline>0),
 proof_hash TEXT NOT NULL, charged_bytes INTEGER NOT NULL CHECK(charged_bytes>0),
 PRIMARY KEY(generation,pass_key,child,version_size,path),
 FOREIGN KEY(generation,pass_key,child,version_size,path) REFERENCES provider_active_destinations(generation,pass_key,child,version_size,path) ON DELETE CASCADE
) WITHOUT ROWID;
CREATE INDEX idx_transfer_failure_roots ON provider_active_generations(scope,config_hash,id);
CREATE TABLE provider_transfer_waits (
 generation TEXT NOT NULL, pass_key TEXT NOT NULL, child TEXT NOT NULL,
 version_size TEXT NOT NULL, path TEXT NOT NULL,
 source_generation TEXT NOT NULL, source_pass TEXT NOT NULL, source_child TEXT NOT NULL,
 source_version TEXT NOT NULL, source_path TEXT NOT NULL, source_proof TEXT NOT NULL,
 proof_hash TEXT NOT NULL, charged_bytes INTEGER NOT NULL CHECK(charged_bytes>0),
 PRIMARY KEY(generation,pass_key,child,version_size,path),
 FOREIGN KEY(generation,pass_key,child,version_size,path) REFERENCES provider_active_destinations(generation,pass_key,child,version_size,path) ON DELETE CASCADE,
 FOREIGN KEY(source_generation,source_pass,source_child,source_version,source_path) REFERENCES provider_transfer_failures(generation,pass_key,child,version_size,path) ON DELETE CASCADE
) WITHOUT ROWID;
CREATE INDEX idx_transfer_wait_source ON provider_transfer_waits(source_generation,source_pass,source_child,source_version,source_path);
-- SQLite foreign-key enforcement may be disabled on an existing connection.
-- Explicit retirement also prevents stale links or uncharged orphan evidence.
CREATE TRIGGER provider_transfer_destination_retire AFTER DELETE ON provider_active_destinations BEGIN
 DELETE FROM provider_transfer_dispatch WHERE generation=OLD.generation AND pass_key=OLD.pass_key AND child=OLD.child AND version_size=OLD.version_size AND path=OLD.path;
 DELETE FROM provider_transfer_waits WHERE generation=OLD.generation AND pass_key=OLD.pass_key AND child=OLD.child AND version_size=OLD.version_size AND path=OLD.path;
 DELETE FROM provider_transfer_failures WHERE generation=OLD.generation AND pass_key=OLD.pass_key AND child=OLD.child AND version_size=OLD.version_size AND path=OLD.path;
END;
CREATE TRIGGER provider_transfer_failure_retire AFTER DELETE ON provider_transfer_failures BEGIN
 DELETE FROM provider_transfer_waits WHERE source_generation=OLD.generation AND source_pass=OLD.pass_key AND source_child=OLD.child AND source_version=OLD.version_size AND source_path=OLD.path;
END;
";

/// Exact selected task identity. URL is sensitive and must never be logged.
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct SelectionTransferKey {
    pub(crate) generation: String,
    pub(crate) pass_key: String,
    pub(crate) child: String,
    pub(crate) state_id: String,
    pub(crate) master: String,
    pub(crate) version_size: String,
    pub(crate) path: SelectionPath,
    pub(crate) checksum: String,
    pub(crate) size: u64,
    pub(crate) url: String,
}

// Reconstructed records cannot certify source ownership. Until compact
// receipt admission is wired, no shared-wait proof is available.
fn observation(
    _conn: &Connection,
    _root: &ActiveGeneration,
    _decision: &ActiveDecision,
) -> Result<Option<Vec<u8>>, StateError> {
    Ok(None)
}

pub(super) fn matches_task(
    conn: &Connection,
    root: &ActiveGeneration,
    decision: &ActiveDecision,
    key: &SelectionTransferKey,
) -> Result<bool, StateError> {
    if root.spec.format != 2
        || decision.state_id != key.state_id
        || decision.decision.master.as_deref() != Some(key.master.as_str())
        || decision.decision.pass_key != key.pass_key
        || decision.decision.child != key.child
    {
        return Ok(false);
    }
    let Some(dest) = decision.decision.destinations.iter().find(|d| {
        d.version_size == key.version_size
            && d.path == key.path
            && d.checksum == key.checksum
            && d.size == key.size
    }) else {
        return Ok(false);
    };
    let path = dest.path.key()?;
    let active: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM provider_active_destinations WHERE generation=?1 AND pass_key=?2 AND child=?3 AND version_size=?4 AND path=?5 AND admitted=1 AND verified_media=0)",
        params![root.id,key.pass_key,key.child,key.version_size,path], |r| r.get(0))?;
    if !active {
        return Ok(false);
    }
    let Some(body) = decision.decision.confirmation.as_deref() else {
        return Ok(false);
    };
    let photo =
        crate::icloud::photos::current_asset(body, &key.child, &key.master, &root.spec.zone)
            .map_err(|_error| StateError::ProviderSelectionInvalid)?;
    let logical = crate::state::VersionSizeKey::from_str(&key.version_size)
        .ok_or(StateError::ProviderSelectionInvalid)?;
    Ok(photo.versions().iter().any(|(version, value)| {
        let provider = crate::state::VersionSizeKey::from(*version);
        (provider == logical
            || matches!(
                (provider, logical),
                (
                    crate::state::VersionSizeKey::Original,
                    crate::state::VersionSizeKey::Alternative
                ) | (
                    crate::state::VersionSizeKey::Alternative,
                    crate::state::VersionSizeKey::Original
                )
            ))
            && value.url.as_ref() == key.url.as_str()
            && value.checksum.as_ref() == key.checksum.as_str()
            && value.size == key.size
    }))
}

fn dispatch_hash(
    root: &ActiveGeneration,
    decision: &ActiveDecision,
    version: &str,
    path: &str,
    state: &str,
) -> Result<String, StateError> {
    Ok(digest(&encode(&(
        "selection-dispatch-v1",
        &root.id,
        digest(&encode(decision)?),
        version,
        path,
        state,
    ))?))
}

/// Initialize only a newly admitted destination, never an existing legacy row.
pub(super) fn initialize_dispatch(
    conn: &Connection,
    root: &ActiveGeneration,
    decision: &ActiveDecision,
    capacity: u64,
) -> Result<(), StateError> {
    for dest in &decision.decision.destinations {
        let path = dest.path.key()?;
        let hash = dispatch_hash(root, decision, &dest.version_size, &path, "never")?;
        let charged = u64::try_from(
            root.id.len()
                + decision.decision.pass_key.len()
                + decision.decision.child.len()
                + dest.version_size.len()
                + path.len()
                + hash.len()
                + 128,
        )
        .map_err(|_error| StateError::ProviderSelectionFull)?;
        charge(conn, charged, capacity, 2)?;
        conn.execute("INSERT INTO provider_transfer_dispatch(generation,pass_key,child,version_size,path,state,proof_hash,charged_bytes) VALUES(?1,?2,?3,?4,?5,'never',?6,?7)", params![root.id,decision.decision.pass_key,decision.decision.child,dest.version_size,path,hash,i64::try_from(charged).map_err(|_error|StateError::ProviderSelectionFull)?])?;
    }
    Ok(())
}

pub(super) fn dispatch_state(
    conn: &Connection,
    root: &ActiveGeneration,
    decision: &ActiveDecision,
    version: &str,
    path: &str,
) -> Result<Option<String>, StateError> {
    let row: Option<(String,String)> = conn.query_row("SELECT state,proof_hash FROM provider_transfer_dispatch WHERE generation=?1 AND pass_key=?2 AND child=?3 AND version_size=?4 AND path=?5",params![root.id,decision.decision.pass_key,decision.decision.child,version,path],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
    let Some((state, hash)) = row else {
        return Ok(None);
    };
    if !matches!(state.as_str(), "never" | "dispatched")
        || dispatch_hash(root, decision, version, path, &state)? != hash
    {
        return invalid();
    }
    Ok(Some(state))
}

fn self_waiting(
    conn: &Connection,
    root: &ActiveGeneration,
    decision: &ActiveDecision,
    key: &SelectionTransferKey,
    now: i64,
) -> Result<bool, StateError> {
    let path = key.path.key()?;
    let marker:Option<(String,i64,i64,String)>=conn.query_row("SELECT url,attempt,deadline,proof_hash FROM provider_transfer_failures WHERE generation=?1 AND pass_key=?2 AND child=?3 AND version_size=?4 AND path=?5",params![root.id,key.pass_key,key.child,key.version_size,path],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).optional()?;
    let Some((url, attempt, deadline, hash)) = marker else {
        return Ok(false);
    };
    if deadline <= now || url != key.url || !matches_task(conn, root, decision, key)? {
        return Ok(false);
    }
    let actual:(i64,i64)=conn.query_row("SELECT attempts,next_retry_at FROM provider_active_decisions WHERE generation=?1 AND pass_key=?2 AND child=?3",params![root.id,key.pass_key,key.child],|r|Ok((r.get(0)?,r.get(1)?)))?;
    if actual != (attempt, deadline) {
        return Ok(false);
    }
    let Some(observed) = observation(conn, root, decision)? else {
        return invalid();
    };
    if failure_hash(key, attempt, deadline, &observed)? != hash {
        return invalid();
    }
    Ok(true)
}

impl SqliteStateDb {
    /// Atomically revalidate waits and mark physical dispatch before any GET.
    pub(crate) async fn begin_selection_transfer(
        &self,
        owner: account::AccountOwner,
        key: SelectionTransferKey,
        capacity: u64,
    ) -> Result<bool, StateError> {
        self.begin_selection_transfer_checked(owner, key, capacity, None)
            .await
    }

    /// The final IMMEDIATE transaction re-reads the entire bounded future set;
    /// a new or changed candidate never slips between discovery and dispatch.
    pub(crate) async fn begin_selection_transfer_checked(
        &self,
        owner: account::AccountOwner,
        key: SelectionTransferKey,
        _capacity: u64,
        checked: Option<(
            crate::download::CurrentPolicyEligibility,
            Vec<super::immutable_epochs::EpochHint>,
        )>,
    ) -> Result<bool, StateError> {
        self.with_conn_mut("beginning selected transfer",move|conn|{
            let tx=conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            account::validate_authenticated(&tx,&owner)?;
            let root=load_generation(&tx,&key.generation)?;
            let decision=load_decision(&tx,&root,&key.pass_key,&key.child)?;
            if !matches_task(&tx,&root,&decision,&key)? {return Err(StateError::ProviderWorkConflict)}
            let now=crate::state::db::retry_clock::now_for_connection(&tx);
            if let Some((policy,expected))=checked.as_ref(){
                let locator=super::immutable_epochs::exact_locator(&tx,&root,&decision,&key,policy)?.ok_or(StateError::ProviderWorkConflict)?;
                let current=super::immutable_epochs::future_hints(&tx,&locator,now)?;
                if &current!=expected{return Err(StateError::ProviderWorkConflict)}
            }
            if self_waiting(&tx,&root,&decision,&key,now)? || waiting(&tx,&root,&decision,&key,now)? {
                tx.commit()?;
                return Ok(false);
            }
            let path=key.path.key()?;
            let state=dispatch_state(&tx,&root,&decision,&key.version_size,&path)?;
            let (admission,deadline):(String,i64)=tx.query_row("SELECT admission,next_retry_at FROM provider_active_decisions WHERE generation=?1 AND pass_key=?2 AND child=?3",params![root.id,key.pass_key,key.child],|r|Ok((r.get(0)?,r.get(1)?)))?;
            #[cfg(test)]
            if std::env::var_os("KEI_ISSUE845_TRACE_RETRY").is_some() {
                eprintln!("ISSUE845_TRACE dispatch generation={} pass={} child={} version={} state={:?} deadline={} now={}",root.id,key.pass_key,key.child,key.version_size,state,deadline,now);
            }
            if deadline>now && !(admission=="admitted" && state.as_deref()==Some("never") && never_dispatched_due(&tx,&root,&decision,now)?) {
                tx.commit()?;
                #[cfg(test)]
                if std::env::var_os("KEI_ISSUE845_TRACE_RETRY").is_some() { eprintln!("ISSUE845_TRACE dispatch_committed generation={} pass={} child={} version={} allowed=false",root.id,key.pass_key,key.child,key.version_size); }
                return Ok(false);
            }
            if state.is_some() {
                let hash=dispatch_hash(&root,&decision,&key.version_size,&path,"dispatched")?;
                tx.execute("UPDATE provider_transfer_dispatch SET state='dispatched',proof_hash=?6 WHERE generation=?1 AND pass_key=?2 AND child=?3 AND version_size=?4 AND path=?5",params![root.id,key.pass_key,key.child,key.version_size,path,hash])?;
            }
            tx.commit()?;
            #[cfg(test)]
            if std::env::var_os("KEI_ISSUE845_TRACE_RETRY").is_some() { eprintln!("ISSUE845_TRACE dispatch_committed generation={} pass={} child={} version={} allowed=true",root.id,key.pass_key,key.child,key.version_size); }
            #[cfg(all(test, target_os = "linux"))]
            crate::test_helpers::process_death_point("selection-transfer-dispatched");
            Ok(true)
        }).await
    }
}

fn failure_hash(
    key: &SelectionTransferKey,
    attempt: i64,
    deadline: i64,
    observed: &[u8],
) -> Result<String, StateError> {
    Ok(digest(&encode(&(
        "expired-transfer-v1",
        key,
        attempt,
        deadline,
        digest(observed),
    ))?))
}

impl SqliteStateDb {
    /// Record a real HTTP 410 for this exact task and its retry epoch atomically.
    pub(crate) async fn record_expired_selection_transfer(
        &self,
        owner: account::AccountOwner,
        key: SelectionTransferKey,
        capacity: u64,
    ) -> Result<(), StateError> {
        self.with_conn_mut("recording scoped expired transfer", move |conn| {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            account::validate_authenticated(&tx, &owner)?;
            let root = load_generation(&tx, &key.generation)?;
            let decision = load_decision(&tx, &root, &key.pass_key, &key.child)?;
            if !matches_task(&tx, &root, &decision, &key)? { return Err(StateError::ProviderWorkConflict); }
            let (attempt, deadline): (i64, i64) = tx.query_row(
                "SELECT attempts,next_retry_at FROM provider_active_decisions WHERE generation=?1 AND pass_key=?2 AND child=?3",
                params![key.generation,key.pass_key,key.child], |r| Ok((r.get(0)?, r.get(1)?)))?;
            let newly_deferred = deadline <= crate::state::db::retry_clock::now_for_connection(&tx);
            #[cfg(test)]
            if std::env::var_os("KEI_ISSUE845_TRACE_RETRY").is_some() {
                eprintln!("ISSUE845_TRACE fallback_410 generation={} pass={} child={} version={} prior_attempts={} deadline={} newly_deferred={}", root.id, key.pass_key, key.child, key.version_size, attempt, deadline, newly_deferred);
            }
            if newly_deferred { defer_attempt(&tx, &root, &key.pass_key, &key.child)?; }
            let (attempt, deadline): (i64, i64) = if newly_deferred {
                tx.query_row("SELECT attempts,next_retry_at FROM provider_active_decisions WHERE generation=?1 AND pass_key=?2 AND child=?3",
                    params![key.generation,key.pass_key,key.child], |r| Ok((r.get(0)?, r.get(1)?)))?
            } else { (attempt, deadline) };
            // Do not attribute another rendition's child-level retry epoch to this task.
            let path = key.path.key()?;
            let prior: Option<(String, i64, i64)> = tx.query_row(
                "SELECT proof_hash,attempt,deadline FROM provider_transfer_failures WHERE generation=?1 AND pass_key=?2 AND child=?3 AND version_size=?4 AND path=?5",
                params![key.generation,key.pass_key,key.child,key.version_size,path], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
            let mut recorded = false;
            if let Some(observed) = observation(&tx, &root, &decision)? {
                if !newly_deferred && prior.as_ref() != Some(&(failure_hash(&key, attempt, deadline, &observed)?, attempt, deadline)) {
                    tx.commit()?;
                    return Ok(());
                }
                let hash = failure_hash(&key, attempt, deadline, &observed)?;
                let path = key.path.key()?;
                let charged = u64::try_from(encode(&(&key, attempt, deadline, &hash))?.len() + 128)
                    .map_err(|_error| StateError::ProviderSelectionFull)?;
                let old_charge: Option<i64> = tx.query_row("SELECT charged_bytes FROM provider_transfer_failures WHERE generation=?1 AND pass_key=?2 AND child=?3 AND version_size=?4 AND path=?5",
                    params![key.generation,key.pass_key,key.child,key.version_size,path], |r| r.get(0)).optional()?;
                let old_charge = old_charge.map(u64::try_from).transpose().map_err(|_error| StateError::ProviderSelectionInvalid)?.unwrap_or(0);
                charge(&tx, charged.saturating_sub(old_charge), capacity, 2)?;
                tx.execute("INSERT INTO provider_transfer_failures(generation,pass_key,child,version_size,path,url,attempt,deadline,proof_hash,charged_bytes) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10) ON CONFLICT(generation,pass_key,child,version_size,path) DO UPDATE SET url=excluded.url,attempt=excluded.attempt,deadline=excluded.deadline,proof_hash=excluded.proof_hash,charged_bytes=excluded.charged_bytes",
                    params![key.generation,key.pass_key,key.child,key.version_size,path,key.url,attempt,deadline,hash,i64::try_from(charged).map_err(|_error|StateError::ProviderSelectionFull)?])?;
                recorded = true;
            }
            tx.commit()?;
            #[cfg(test)]
            if std::env::var_os("KEI_ISSUE845_TRACE_RETRY").is_some() { eprintln!("ISSUE845_TRACE fallback_410_committed generation={} pass={} child={} version={} attempt={} deadline={} recorded={}",root.id,key.pass_key,key.child,key.version_size,attempt,deadline,recorded); }
            #[cfg(all(test, target_os = "linux"))]
            if recorded { crate::test_helpers::process_death_point("selection-expired-transfer-recorded"); }
            #[cfg(not(all(test, target_os = "linux")))]
            let _ = recorded;
            Ok(())
        }).await
    }
}

#[cfg(test)]
impl SqliteStateDb {
    /// Advance an already-proven 410 epoch to due without fabricating a failure.
    pub(crate) async fn make_expired_transfer_due(
        &self,
        owner: account::AccountOwner,
        key: SelectionTransferKey,
    ) -> Result<(), StateError> {
        self.with_conn_mut("advancing synthetic expired transfer clock", move |conn| {
            let tx=conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            account::validate_authenticated(&tx,&owner)?;
            let root=load_generation(&tx,&key.generation)?;
            let decision=load_decision(&tx,&root,&key.pass_key,&key.child)?;
            let now=crate::state::db::retry_clock::now_for_connection(&tx);
            if !self_waiting(&tx,&root,&decision,&key,now)? {return Err(StateError::ProviderWorkConflict)}
            let observed=observation(&tx,&root,&decision)?.ok_or(StateError::ProviderSelectionInvalid)?;
            let attempt:i64=tx.query_row("SELECT attempts FROM provider_active_decisions WHERE generation=?1 AND pass_key=?2 AND child=?3",params![root.id,key.pass_key,key.child],|r|r.get(0))?;
            let deadline=now.saturating_sub(1).max(1);
            let hash=failure_hash(&key,attempt,deadline,&observed)?;
            tx.execute("UPDATE provider_active_decisions SET next_retry_at=?4 WHERE generation=?1 AND pass_key=?2 AND child=?3",params![root.id,key.pass_key,key.child,deadline])?;
            super::refresh_admission_hash(&tx,&root,&key.pass_key,&key.child)?;
            let changed=tx.execute("UPDATE provider_transfer_failures SET deadline=?6,proof_hash=?7 WHERE generation=?1 AND pass_key=?2 AND child=?3 AND version_size=?4 AND path=?5",params![root.id,key.pass_key,key.child,key.version_size,key.path.key()?,deadline,hash])?;
            if changed != 1 {return Err(StateError::ProviderWorkConflict)}
            tx.commit()?;
            Ok(())
        }).await
    }
}

// A missing change tag cannot prove that two parsed observations represent the
// same provider revision. current_asset separately validates membership/scope.
fn has_pair_tags(body: &[u8], child: &str, master: &str) -> bool {
    let Ok(value) = crate::icloud::photos::changes_json::parse(body) else {
        return false;
    };
    let Some(records) = value.get("records").and_then(serde_json::Value::as_array) else {
        return false;
    };
    [child, master].into_iter().all(|name| {
        let mut matching = records
            .iter()
            .filter(|r| r.get("recordName").and_then(serde_json::Value::as_str) == Some(name));
        matching.next().is_some_and(|record| {
            record
                .get("recordChangeTag")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|tag| !tag.trim().is_empty())
        }) && matching.next().is_none()
    })
}

// Compare complete parsed records, not the differently wrapped/serialized
// envelopes. No unknown fields, tags, URLs, or duplicate identities are dropped.
fn paired_records_equal(observed: &[u8], confirmation: &[u8], child: &str, master: &str) -> bool {
    fn pair(
        body: &[u8],
        child: &str,
        master: &str,
    ) -> Option<(serde_json::Value, serde_json::Value)> {
        let value = crate::icloud::photos::changes_json::parse(body).ok()?;
        let records = value.get("records")?.as_array()?;
        if records.len() != 2 {
            return None;
        }
        let find = |name: &str| {
            let mut matching = records
                .iter()
                .filter(|r| r.get("recordName").and_then(serde_json::Value::as_str) == Some(name));
            let record = matching.next()?.clone();
            if matching.next().is_some() {
                return None;
            }
            Some(record)
        };
        Some((find(child)?, find(master)?))
    }
    if child == master
        || !has_pair_tags(observed, child, master)
        || !has_pair_tags(confirmation, child, master)
    {
        return false;
    }
    pair(observed, child, master)
        .zip(pair(confirmation, child, master))
        .is_some_and(|(a, b)| a == b)
}

const PREDECESSOR_ROOT_QUERY: &str = "SELECT id FROM provider_active_generations INDEXED BY idx_transfer_failure_roots WHERE scope=?1 AND config_hash=?2 AND id<>?3 ORDER BY id LIMIT 65";
const PREDECESSOR_FAILURE_QUERY: &str = "SELECT url,attempt,deadline,proof_hash FROM provider_transfer_failures WHERE generation=?1 AND pass_key=?2 AND child=?3 AND version_size=?4 AND path=?5 AND deadline>?6";

fn bounded_predecessor_roots(
    conn: &Connection,
    scope: &str,
    config: &str,
    current: &str,
) -> Result<Vec<String>, StateError> {
    let roots: Vec<String> = conn
        .prepare(PREDECESSOR_ROOT_QUERY)?
        .query_map(params![scope, config, current], |r| r.get(0))?
        .collect::<Result<_, _>>()?;
    if roots.len() > 64 {
        return Err(StateError::ProviderSelectionAmbiguous);
    }
    Ok(roots)
}

/// Persist the new source observation only when the original and fresh paired
/// bytes agree exactly. Missing or divergent evidence means independent work.
pub(super) fn bind_observation_and_waits(
    conn: &Connection,
    root: &ActiveGeneration,
    decision: &ActiveDecision,
    observed: Option<&[u8]>,
    fresh: Option<&[u8]>,
    capacity: u64,
) -> Result<(), StateError> {
    let (Some(observed), Some(fresh), Some(confirmed)) =
        (observed, fresh, decision.decision.confirmation.as_deref())
    else {
        return Ok(());
    };
    if root.spec.format != 2
        || !decision
            .decision
            .master
            .as_deref()
            .is_some_and(|master| has_pair_tags(observed, &decision.decision.child, master))
        || observed.len() > MAX_CHUNK_BYTES
        || fresh != confirmed
        || !decision.decision.master.as_deref().is_some_and(|master| {
            paired_records_equal(observed, fresh, &decision.decision.child, master)
        })
    {
        return Ok(());
    }
    let hash = digest(observed);
    let existing: Option<(Vec<u8>,String)> = conn.query_row(
        "SELECT body,body_hash FROM provider_transfer_observations WHERE generation=?1 AND pass_key=?2 AND child=?3",
        params![root.id,decision.decision.pass_key,decision.decision.child],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
    if let Some((saved, saved_hash)) = existing {
        if saved != observed || saved_hash != hash {
            return invalid();
        }
    } else {
        let charged = u64::try_from(
            observed.len()
                + hash.len()
                + root.id.len()
                + decision.decision.pass_key.len()
                + decision.decision.child.len()
                + 128,
        )
        .map_err(|_error| StateError::ProviderSelectionFull)?;
        charge(conn, charged, capacity, 2)?;
        conn.execute("INSERT INTO provider_transfer_observations(generation,pass_key,child,body,body_hash,charged_bytes) VALUES(?1,?2,?3,?4,?5,?6)",
            params![root.id,decision.decision.pass_key,decision.decision.child,observed,hash,i64::try_from(charged).map_err(|_error|StateError::ProviderSelectionFull)?])?;
    }
    let master = decision
        .decision
        .master
        .as_deref()
        .ok_or(StateError::ProviderSelectionInvalid)?;
    for dest in &decision.decision.destinations {
        let target_path = dest.path.key()?;
        // Bound root probes before concluding that a predecessor is unique.
        // Failure lookups use the exact destination primary key; no copied
        // scope/config fields can hide a matching authenticated owner.
        let roots =
            bounded_predecessor_roots(conn, &root.spec.scope, &root.spec.config_hash, &root.id)?;
        let now = crate::state::db::retry_clock::now_for_connection(conn);
        let mut candidates = Vec::new();
        for source_id in roots {
            let marker: Option<(String, i64, i64, String)> = conn
                .query_row(
                    PREDECESSOR_FAILURE_QUERY,
                    params![
                        source_id,
                        decision.decision.pass_key,
                        decision.decision.child,
                        dest.version_size,
                        target_path,
                        now
                    ],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
                )
                .optional()?;
            if let Some((url, attempt, deadline, hash)) = marker {
                candidates.push((source_id, url, attempt, deadline, hash));
            }
        }
        let mut matches = Vec::new();
        for (source_id, url, attempt, deadline, stored_hash) in candidates {
            let source = load_generation(conn, &source_id)?;
            if source.spec.scope != root.spec.scope
                || source.spec.zone != root.spec.zone
                || source.spec.config_hash != root.spec.config_hash
                || source.spec.profile != root.spec.profile
                || source.spec.metadata_enabled != root.spec.metadata_enabled
                || source.spec.metadata_flags != root.spec.metadata_flags
            {
                continue;
            }
            let previous = load_decision(
                conn,
                &source,
                &decision.decision.pass_key,
                &decision.decision.child,
            )?;
            if previous.state_id != decision.state_id
                || previous.decision.master.as_deref() != Some(master)
                || previous.decision.confirmation.as_deref() != Some(confirmed)
            {
                continue;
            }
            let old = SelectionTransferKey {
                generation: source_id.clone(),
                pass_key: decision.decision.pass_key.clone(),
                child: decision.decision.child.clone(),
                state_id: decision.state_id.clone(),
                master: master.to_owned(),
                version_size: dest.version_size.clone(),
                path: dest.path.clone(),
                checksum: dest.checksum.clone(),
                size: dest.size,
                url,
            };
            if !matches_task(conn, &source, &previous, &old)? {
                continue;
            }
            let Some(old_observed) = observation(conn, &source, &previous)? else {
                continue;
            };
            if old_observed != observed {
                continue;
            }
            let actual: (i64,i64)=conn.query_row("SELECT attempts,next_retry_at FROM provider_active_decisions WHERE generation=?1 AND pass_key=?2 AND child=?3",
                params![source_id,old.pass_key,old.child],|r|Ok((r.get(0)?,r.get(1)?)))?;
            if actual != (attempt, deadline)
                || deadline <= crate::state::db::retry_clock::now_for_connection(conn)
            {
                continue;
            }
            if failure_hash(&old, attempt, deadline, &old_observed)? != stored_hash {
                return invalid();
            }
            matches.push((old, stored_hash));
        }
        // Ambiguous predecessor provenance cannot authorize a shared wait.
        if matches.len() != 1 {
            continue;
        }
        let (source, source_proof) = matches.pop().ok_or(StateError::ProviderSelectionInvalid)?;
        let source_path = source.path.key()?;
        let link_hash = digest(&encode(&(
            "shared-transfer-wait-v1",
            &root.id,
            &decision.decision.pass_key,
            &decision.decision.child,
            &dest.version_size,
            &target_path,
            &source.generation,
            &source.pass_key,
            &source.child,
            &source.version_size,
            &source_path,
            &source_proof,
            &hash,
        ))?);
        let charged = u64::try_from(
            link_hash.len()
                + source_proof.len()
                + source.generation.len()
                + source.pass_key.len()
                + source.child.len()
                + source.version_size.len()
                + source_path.len()
                + root.id.len()
                + decision.decision.pass_key.len()
                + decision.decision.child.len()
                + dest.version_size.len()
                + target_path.len()
                + 128,
        )
        .map_err(|_error| StateError::ProviderSelectionFull)?;
        let already_exists: bool = conn.query_row("SELECT EXISTS(SELECT 1 FROM provider_transfer_waits WHERE generation=?1 AND pass_key=?2 AND child=?3 AND version_size=?4 AND path=?5)", params![root.id,decision.decision.pass_key,decision.decision.child,dest.version_size,target_path], |r| r.get(0))?;
        if already_exists {
            continue;
        }
        charge(conn, charged, capacity, 2)?;
        conn.execute("INSERT INTO provider_transfer_waits(generation,pass_key,child,version_size,path,source_generation,source_pass,source_child,source_version,source_path,source_proof,proof_hash,charged_bytes) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13) ON CONFLICT(generation,pass_key,child,version_size,path) DO NOTHING",
            params![root.id,decision.decision.pass_key,decision.decision.child,dest.version_size,target_path,source.generation,source.pass_key,source.child,source.version_size,source_path,source_proof,link_hash,i64::try_from(charged).map_err(|_error|StateError::ProviderSelectionFull)?])?;
    }
    Ok(())
}

/// Validate a saved link without mutating either owner. Missing, expired or
/// superseded predecessor evidence makes the target independently runnable.
pub(super) fn waiting(
    conn: &Connection,
    root: &ActiveGeneration,
    decision: &ActiveDecision,
    key: &SelectionTransferKey,
    now: i64,
) -> Result<bool, StateError> {
    if !matches_task(conn, root, decision, key)? {
        return Ok(false);
    }
    let path = key.path.key()?;
    let link:Option<(String,String,String,String,String,String,String)>=conn.query_row(
        "SELECT source_generation,source_pass,source_child,source_version,source_path,source_proof,proof_hash FROM provider_transfer_waits WHERE generation=?1 AND pass_key=?2 AND child=?3 AND version_size=?4 AND path=?5",
        params![key.generation,key.pass_key,key.child,key.version_size,path],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?,r.get(6)?))).optional()?;
    let Some((id, pass, child, version, source_path, source_proof, link_hash)) = link else {
        return Ok(false);
    };
    if pass != key.pass_key
        || child != key.child
        || version != key.version_size
        || source_path != path
    {
        return Ok(false);
    }
    let Some(observed) = observation(conn, root, decision)? else {
        return invalid();
    };
    let actual_hash = digest(&encode(&(
        "shared-transfer-wait-v1",
        &root.id,
        &key.pass_key,
        &key.child,
        &key.version_size,
        &path,
        &id,
        &pass,
        &child,
        &version,
        &source_path,
        &source_proof,
        &digest(&observed),
    ))?);
    if actual_hash != link_hash {
        return invalid();
    }
    let marker:Option<(String,i64,i64,String)>=conn.query_row(
        "SELECT url,attempt,deadline,proof_hash FROM provider_transfer_failures WHERE generation=?1 AND pass_key=?2 AND child=?3 AND version_size=?4 AND path=?5",
        params![id,pass,child,version,source_path],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).optional()?;
    let Some((url, attempt, deadline, marker_hash)) = marker else {
        return Ok(false);
    };
    if marker_hash != source_proof || deadline <= now {
        return Ok(false);
    }
    let exists: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM provider_active_generations WHERE id=?1)",
        [&id],
        |r| r.get(0),
    )?;
    if !exists {
        return Ok(false);
    }
    let source = load_generation(conn, &id)?;
    if source.spec.scope != root.spec.scope
        || source.spec.zone != root.spec.zone
        || source.spec.config_hash != root.spec.config_hash
        || source.spec.profile != root.spec.profile
        || source.spec.metadata_enabled != root.spec.metadata_enabled
        || source.spec.metadata_flags != root.spec.metadata_flags
    {
        return Ok(false);
    }
    let previous = load_decision(conn, &source, &pass, &child)?;
    if previous.state_id != key.state_id
        || previous.decision.master.as_deref() != Some(key.master.as_str())
        || previous.decision.confirmation != decision.decision.confirmation
    {
        return Ok(false);
    }
    let old_observed = observation(conn, &source, &previous)?;
    if old_observed.as_deref() != Some(observed.as_slice()) {
        return Ok(false);
    }
    let old = SelectionTransferKey {
        generation: id.clone(),
        pass_key: pass.clone(),
        child: child.clone(),
        state_id: key.state_id.clone(),
        master: key.master.clone(),
        version_size: version,
        path: key.path.clone(),
        checksum: key.checksum.clone(),
        size: key.size,
        url,
    };
    if old.path.key()? != source_path
        || !matches_task(conn, &source, &previous, &old)?
        || old.url != key.url
    {
        return Ok(false);
    }
    if failure_hash(&old, attempt, deadline, &observed)? != marker_hash {
        return invalid();
    }
    let actual:(i64,i64)=conn.query_row("SELECT attempts,next_retry_at FROM provider_active_decisions WHERE generation=?1 AND pass_key=?2 AND child=?3",params![id,pass,child],|r|Ok((r.get(0)?,r.get(1)?)))?;
    Ok(actual == (attempt, deadline) && deadline > now)
}

// A future child deadline may be bypassed only for a component whose
// never-dispatched state is durable and whose current epoch has a verified
// per-destination 410 owner. Missing/legacy/dispatched state is conservative.
fn never_dispatched_due(
    conn: &Connection,
    root: &ActiveGeneration,
    decision: &ActiveDecision,
    now: i64,
) -> Result<bool, StateError> {
    let Some(master) = decision.decision.master.as_deref() else {
        return Ok(false);
    };
    let mut owner_found = false;
    for dest in &decision.decision.destinations {
        if super::immutable_epochs::same_generation_failed_tx(conn, root, decision, dest, now)? {
            owner_found = true;
            break;
        }
        let path = dest.path.key()?;
        let url:Option<String>=conn.query_row("SELECT url FROM provider_transfer_failures WHERE generation=?1 AND pass_key=?2 AND child=?3 AND version_size=?4 AND path=?5",params![root.id,decision.decision.pass_key,decision.decision.child,dest.version_size,path],|r|r.get(0)).optional()?;
        if let Some(url) = url {
            let key = SelectionTransferKey {
                generation: root.id.clone(),
                pass_key: decision.decision.pass_key.clone(),
                child: decision.decision.child.clone(),
                state_id: decision.state_id.clone(),
                master: master.to_owned(),
                version_size: dest.version_size.clone(),
                path: dest.path.clone(),
                checksum: dest.checksum.clone(),
                size: dest.size,
                url,
            };
            if self_waiting(conn, root, decision, &key, now)? {
                owner_found = true;
                break;
            }
        }
    }
    if !owner_found {
        return Ok(false);
    }
    for dest in &decision.decision.destinations {
        let path = dest.path.key()?;
        let eligible:bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM provider_active_destinations WHERE generation=?1 AND pass_key=?2 AND child=?3 AND version_size=?4 AND path=?5 AND admitted=1 AND verified_media=0)",params![root.id,decision.decision.pass_key,decision.decision.child,dest.version_size,path],|r|r.get(0))?;
        if !eligible
            || dispatch_state(conn, root, decision, &dest.version_size, &path)?.as_deref()
                != Some("never")
        {
            continue;
        }
        let url:Option<String>=conn.query_row("SELECT f.url FROM provider_transfer_waits w JOIN provider_transfer_failures f ON f.generation=w.source_generation AND f.pass_key=w.source_pass AND f.child=w.source_child AND f.version_size=w.source_version AND f.path=w.source_path WHERE w.generation=?1 AND w.pass_key=?2 AND w.child=?3 AND w.version_size=?4 AND w.path=?5",params![root.id,decision.decision.pass_key,decision.decision.child,dest.version_size,path],|r|r.get(0)).optional()?;
        let Some(url) = url else { return Ok(true) };
        let key = SelectionTransferKey {
            generation: root.id.clone(),
            pass_key: decision.decision.pass_key.clone(),
            child: decision.decision.child.clone(),
            state_id: decision.state_id.clone(),
            master: master.to_owned(),
            version_size: dest.version_size.clone(),
            path: dest.path.clone(),
            checksum: dest.checksum.clone(),
            size: dest.size,
            url,
        };
        if !waiting(conn, root, decision, &key, now)? {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Whether a due child has independent runnable work. Shared waits are
/// revalidated individually; a due metadata obligation is never hidden.
pub(super) fn decision_has_due_work(
    conn: &Connection,
    root: &ActiveGeneration,
    decision: &ActiveDecision,
    now: i64,
) -> Result<bool, StateError> {
    let (admission, deadline): (String, i64) = conn.query_row(
        "SELECT admission,next_retry_at FROM provider_active_decisions WHERE generation=?1 AND pass_key=?2 AND child=?3",
        params![root.id, decision.decision.pass_key, decision.decision.child], |r| Ok((r.get(0)?, r.get(1)?)))?;
    if admission == "excluded" {
        return Ok(false);
    }
    if deadline > now {
        return if admission == "admitted" {
            never_dispatched_due(conn, root, decision, now)
        } else {
            Ok(false)
        };
    }
    if admission == "deferred" {
        return Ok(true);
    }
    let master = match decision.decision.master.as_deref() {
        Some(m) => m,
        None => return invalid(),
    };
    for dest in &decision.decision.destinations {
        let path = dest.path.key()?;
        let state: Option<(bool, bool, bool)> = conn.query_row(
            "SELECT admitted,verified_media,verified_metadata FROM provider_active_destinations WHERE generation=?1 AND pass_key=?2 AND child=?3 AND version_size=?4 AND path=?5",
            params![root.id,decision.decision.pass_key,decision.decision.child,dest.version_size,path],
            |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
        let Some((admitted, verified_media, verified_metadata)) = state else {
            return invalid();
        };
        if !admitted {
            continue;
        }
        if verified_media {
            if root.spec.metadata_enabled && !verified_metadata {
                return Ok(true);
            }
            continue;
        }
        let url: Option<String> = conn.query_row(
            "SELECT f.url FROM provider_transfer_waits w JOIN provider_transfer_failures f ON f.generation=w.source_generation AND f.pass_key=w.source_pass AND f.child=w.source_child AND f.version_size=w.source_version AND f.path=w.source_path WHERE w.generation=?1 AND w.pass_key=?2 AND w.child=?3 AND w.version_size=?4 AND w.path=?5",
            params![root.id,decision.decision.pass_key,decision.decision.child,dest.version_size,path], |r| r.get(0)).optional()?;
        let Some(url) = url else { return Ok(true) };
        let key = SelectionTransferKey {
            generation: root.id.clone(),
            pass_key: decision.decision.pass_key.clone(),
            child: decision.decision.child.clone(),
            state_id: decision.state_id.clone(),
            master: master.to_owned(),
            version_size: dest.version_size.clone(),
            path: dest.path.clone(),
            checksum: dest.checksum.clone(),
            size: dest.size,
            url,
        };
        if !waiting(conn, root, decision, &key, now)? {
            return Ok(true);
        }
    }
    Ok(false)
}

impl SqliteStateDb {
    /// Recheck the authenticated wait at the final dispatch boundary.
    pub(crate) async fn selection_transfer_waiting(
        &self,
        owner: account::AccountOwner,
        key: SelectionTransferKey,
    ) -> Result<bool, StateError> {
        self.with_conn("validating selected transfer wait", move |conn| {
            let tx = conn.unchecked_transaction()?;
            account::validate_authenticated(&tx, &owner)?;
            let root = load_generation(&tx, &key.generation)?;
            let decision = load_decision(&tx, &root, &key.pass_key, &key.child)?;
            let result = waiting(
                &tx,
                &root,
                &decision,
                &key,
                crate::state::db::retry_clock::now_for_connection(&tx),
            )?;
            tx.commit()?;
            Ok(result)
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::{
        DDL, PREDECESSOR_FAILURE_QUERY, PREDECESSOR_ROOT_QUERY, bounded_predecessor_roots,
        has_pair_tags, paired_records_equal,
    };

    #[test]
    fn paired_tags_require_two_unique_nonblank_string_tags() {
        let pair = br#"{"records":[{"recordName":"child","recordChangeTag":"c1"},{"recordName":"master","recordChangeTag":"m1"}]}"#;
        assert!(has_pair_tags(pair, "child", "master"));
        for bad in [
            &br#"{"records":[{"recordName":"child","recordChangeTag":" "},{"recordName":"master","recordChangeTag":"m1"}]}"#[..],
            &br#"{"records":[{"recordName":"child","recordChangeTag":"c1"},{"recordName":"master"}]}"#[..],
            &br#"{"records":[{"recordName":"child","recordChangeTag":"c1"},{"recordName":"master","recordChangeTag":1}]}"#[..],
            &br#"{"records":[{"recordName":"child","recordChangeTag":"c1"},{"recordName":"child","recordChangeTag":"c2"},{"recordName":"master","recordChangeTag":"m1"}]}"#[..],
            &br#"not json"#[..],
            &br#"{"records":[{"recordName":"child","recordChangeTag":"c","recordChangeTag":"c"},{"recordName":"master","recordChangeTag":"m"}]}"#[..],
        ] {
            assert!(!has_pair_tags(bad, "child", "master"));
        }
    }
    #[test]
    fn paired_records_compare_all_fields_not_envelope_order() {
        let observed = br#"{"records":[{"recordName":"master","recordChangeTag":"m","unknown":1},{"recordName":"child","recordChangeTag":"c","url":"https://example.invalid/a"}]}"#;
        let confirmation = br#"{ "records": [{"url":"https://example.invalid/a","recordChangeTag":"c","recordName":"child"},{"unknown":1,"recordChangeTag":"m","recordName":"master"}], "responseMarker": 7 }"#;
        assert!(paired_records_equal(
            observed,
            confirmation,
            "child",
            "master"
        ));
        let changed = br#"{"records":[{"recordName":"master","recordChangeTag":"m","unknown":2},{"recordName":"child","recordChangeTag":"c","url":"https://example.invalid/a"}]}"#;
        assert!(!paired_records_equal(
            changed,
            confirmation,
            "child",
            "master"
        ));
        let changed_url = br#"{"records":[{"recordName":"master","recordChangeTag":"m","unknown":1},{"recordName":"child","recordChangeTag":"c","url":"https://example.invalid/b"}]}"#;
        assert!(!paired_records_equal(
            changed_url,
            confirmation,
            "child",
            "master"
        ));
        let extra = br#"{"records":[{"recordName":"master","recordChangeTag":"m","unknown":1},{"recordName":"child","recordChangeTag":"c","url":"https://example.invalid/a"},{"recordName":"other","recordChangeTag":"x"}]}"#;
        assert!(!paired_records_equal(
            extra,
            confirmation,
            "child",
            "master"
        ));
    }

    #[test]
    fn retirement_removes_waits_even_without_foreign_key_enforcement() {
        let db = rusqlite::Connection::open_in_memory().unwrap();
        db.execute_batch("PRAGMA foreign_keys=OFF;
            CREATE TABLE provider_active_generations(id TEXT PRIMARY KEY,scope TEXT,config_hash TEXT);
            CREATE TABLE provider_active_sources(dummy INTEGER);
            CREATE TABLE provider_selection_rank_pages(dummy INTEGER);
            CREATE TABLE provider_selection_rank_records(dummy INTEGER);
            CREATE TABLE provider_active_decisions(generation TEXT,pass_key TEXT,child TEXT,PRIMARY KEY(generation,pass_key,child));
            CREATE TABLE provider_active_destinations(generation TEXT,pass_key TEXT,child TEXT,version_size TEXT,path TEXT,PRIMARY KEY(generation,pass_key,child,version_size,path));").unwrap();
        db.execute_batch(DDL).unwrap();
        db.execute_batch("INSERT INTO provider_active_generations VALUES('old','scope','config');
            INSERT INTO provider_active_destinations VALUES('old','pass','child','movie','path'),('new','pass','child','movie','path');
            INSERT INTO provider_transfer_failures VALUES('old','pass','child','movie','path','url',1,42,'proof',1);
            INSERT INTO provider_transfer_waits VALUES('new','pass','child','movie','path','old','pass','child','movie','path','proof','hash',1);
            DELETE FROM provider_active_destinations WHERE generation='old';").unwrap();
        let count: i64 = db
            .query_row("SELECT count(*) FROM provider_transfer_waits", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(count, 0);
    }
    #[test]
    fn predecessor_roots_are_scope_indexed_and_overflow_is_held() {
        let db = rusqlite::Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE provider_active_generations(id TEXT PRIMARY KEY,scope TEXT,config_hash TEXT);
            CREATE INDEX idx_transfer_failure_roots ON provider_active_generations(scope,config_hash,id);
            CREATE TABLE provider_transfer_failures(generation TEXT,pass_key TEXT,child TEXT,version_size TEXT,path TEXT,url TEXT,attempt INTEGER,deadline INTEGER,proof_hash TEXT,PRIMARY KEY(generation,pass_key,child,version_size,path)) WITHOUT ROWID;").unwrap();
        for i in 0..2000 {
            db.execute(
                "INSERT INTO provider_active_generations VALUES(?1,'other','config')",
                [format!("foreign-{i:05}")],
            )
            .unwrap();
        }
        for i in 0..64 {
            db.execute(
                "INSERT INTO provider_active_generations VALUES(?1,'wanted','config')",
                [format!("root-{i:05}")],
            )
            .unwrap();
        }
        let plan: String = db
            .query_row(
                &format!("EXPLAIN QUERY PLAN {PREDECESSOR_ROOT_QUERY}"),
                rusqlite::params!["wanted", "config", "current"],
                |r| r.get(3),
            )
            .unwrap();
        assert!(
            plan.contains("idx_transfer_failure_roots")
                && plan.contains("scope=?")
                && plan.contains("config_hash=?"),
            "{plan}"
        );
        let plan: String = db
            .query_row(
                &format!("EXPLAIN QUERY PLAN {PREDECESSOR_FAILURE_QUERY}"),
                rusqlite::params!["root", "pass", "child", "movie", "path", 0],
                |r| r.get(3),
            )
            .unwrap();
        assert!(
            plan.contains("PRIMARY KEY") && plan.contains("generation=?"),
            "{plan}"
        );
        assert_eq!(
            bounded_predecessor_roots(&db, "wanted", "config", "current")
                .unwrap()
                .len(),
            64
        );
        db.execute(
            "INSERT INTO provider_active_generations VALUES('root-overflow','wanted','config')",
            [],
        )
        .unwrap();
        assert!(matches!(
            bounded_predecessor_roots(&db, "wanted", "config", "current"),
            Err(crate::state::error::StateError::ProviderSelectionAmbiguous)
        ));
    }
}
