//! Unlinked transaction primitives, not a source-ownership or dispatch API.
//! Caller must prove the complete revision-stable predecessor audit, current
//! owner/policies/route/lookup, and destination guards in the SAME transaction.
use super::super::provider_selection::SelectionPath;
use super::{MAX_CHUNK_BYTES, digest, encode};
use crate::state::error::StateError;
use rusqlite::{Connection, params};

/// Kept private to the state owner and moved with the actual held guard.
/// It proves only that this transaction inserted an in-flight reservation.
// Must eventually be minted by the policy/audit owner only after exact
// retained snapshot and real same-component history are validated. No public
// constructor exists in this prototype.
pub(super) struct ValidatedComponentHistory {
    prior_attempts: u32,
    retry_delay: i64,
    policy_snapshot: Vec<u8>,
}

pub(crate) struct RefreshedDispatchTicket {
    pub(super) id: String,
    pub(super) proof: String,
    pub(super) prior_attempts: u32,
    pub(super) retry_delay: i64,
    pub(super) capacity: u64,
    pub(super) owner: Option<super::account::AccountOwner>,
    pub(super) authority: Option<crate::icloud::photos::session::SourceAuthority>,
    pub(super) original: Option<super::transfer_wait::SelectionTransferKey>,
    pub(super) policy: Option<crate::download::CurrentPolicyEligibility>,
    pub(super) route: Option<String>,
    pub(super) fresh_url: Option<String>,
}

pub(super) fn validated_history(
    prior_attempts: u32,
    policy: &crate::download::CurrentPolicyEligibility,
) -> Result<ValidatedComponentHistory, StateError> {
    if prior_attempts > 32 {
        return Err(StateError::ProviderSelectionInvalid);
    }
    let retry_delay = (3600_i64 * (1_i64 << prior_attempts.min(5))).min(86400);
    Ok(ValidatedComponentHistory {
        prior_attempts,
        retry_delay,
        policy_snapshot: policy.resolved_snapshot().to_vec(),
    })
}

/// Only call after full authority, complete candidate and policy revalidation.
/// The row itself is NOT evidence that any HTTP request took place.
pub(super) fn insert_inflight_tx(
    conn: &Connection,
    generation: &str,
    pass: &str,
    child: &str,
    version: &str,
    path: &SelectionPath,
    obligation_digest: &str,
    receipt: &[u8],
    history: ValidatedComponentHistory,
    capacity: u64,
) -> Result<RefreshedDispatchTicket, StateError> {
    if conn.is_autocommit()
        || receipt.is_empty()
        || receipt.len() > MAX_CHUNK_BYTES
        || obligation_digest.len() != 64
        || !obligation_digest
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        || history.prior_attempts > 32
        || !(1..=86400).contains(&history.retry_delay)
        || (history.policy_snapshot.is_empty() || history.policy_snapshot.len() > MAX_CHUNK_BYTES)
    {
        return Err(StateError::ProviderSelectionInvalid);
    }
    let native = path.to_path();
    if SelectionPath::from_path(&native) != *path {
        return Err(StateError::ProviderSelectionInvalid);
    }
    crate::fs_util::confined_path_key(&native)
        .map_err(|_redacted_error| StateError::ProviderSelectionInvalid)?;
    let path_key = path.key()?;
    let slot = super::super::primary_layout::path_key(&native)?;
    let id = uuid::Uuid::new_v4().to_string();
    let proof = digest(&encode(&(
        "authenticated-refreshed-dispatch-v1",
        &id,
        obligation_digest,
        generation,
        pass,
        child,
        version,
        &path_key,
        &slot,
        receipt,
        history.prior_attempts,
        history.retry_delay,
        &history.policy_snapshot,
    ))?);
    let charged = u64::try_from(
        encode(&(
            &id,
            obligation_digest,
            generation,
            pass,
            child,
            version,
            &path_key,
            &slot,
            receipt,
            &proof,
            history.prior_attempts,
            history.retry_delay,
            &history.policy_snapshot,
        ))?
        .len()
        .checked_add(512)
        .ok_or(StateError::ProviderSelectionFull)?,
    )
    .map_err(|_redacted_error| StateError::ProviderSelectionFull)?;
    super::charge_optional_evidence(conn, charged, capacity, 2)?;
    let busy: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM provider_refreshed_transfer WHERE native_slot=?1 AND phase='inflight')",
        [&slot], |r| r.get(0),
    )?;
    if busy {
        return Err(StateError::ProviderWorkConflict);
    }
    conn.execute(
        "INSERT INTO provider_refreshed_transfer(id,obligation_digest,generation,pass_key,child,version_size,path,native_slot,receipt,proof_hash,prior_attempts,policy_snapshot,retry_delay,phase,charged_bytes) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,'inflight',?14)",
        params![id,obligation_digest,generation,pass,child,version,path_key,slot,receipt,proof,history.prior_attempts,history.policy_snapshot,history.retry_delay,i64::try_from(charged).map_err(|_redacted_error| StateError::ProviderSelectionFull)?],
    )?;
    Ok(RefreshedDispatchTicket {
        id,
        proof,
        prior_attempts: history.prior_attempts,
        retry_delay: history.retry_delay,
        capacity,
        owner: None,
        authority: None,
        original: None,
        policy: None,
        route: None,
        fresh_url: None,
    })
}

/// Called solely after this ticket's actual HTTP 410, under the same held
/// destination owner. The attempt is created only at this boundary.
pub(super) fn record_410_tx(
    conn: &Connection,
    ticket: RefreshedDispatchTicket,
) -> Result<(), StateError> {
    if conn.is_autocommit() {
        return Err(StateError::ProviderSelectionInvalid);
    }
    let attempt = ticket.prior_attempts.saturating_add(1).min(32);
    let delay = ticket.retry_delay;
    let now = super::super::retry_clock::now_for_connection(conn);
    let deadline = now
        .checked_add(delay)
        .ok_or(StateError::ProviderSelectionInvalid)?;
    if deadline <= now {
        return Err(StateError::ProviderSelectionInvalid);
    }
    let outcome = digest(&encode(&(
        "authenticated-refreshed-410-v1",
        &ticket.id,
        &ticket.proof,
        attempt,
        deadline,
    ))?);
    let updated = conn.execute(
        "UPDATE provider_refreshed_transfer SET phase='expired',attempt=?1,deadline=?2,outcome_proof=?3 WHERE id=?4 AND proof_hash=?5 AND phase='inflight'",
        params![attempt,deadline,outcome,ticket.id,ticket.proof],
    )?;
    if updated != 1 {
        return Err(StateError::ProviderWorkConflict);
    }
    Ok(())
}
