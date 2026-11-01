//! Selected-refresh state owner. Discovery returns hints only; the final
//! transaction must revalidate and claim the same live witness.
use super::immutable_epochs::EpochHint;
use super::refreshed_candidates::RefreshedHint;
use super::transfer_wait::SelectionTransferKey;
use super::{SqliteStateDb, account, load_decision, load_generation};
use crate::download::CurrentPolicyEligibility;
use crate::icloud::photos::session::SourceAuthority;
use crate::icloud::photos::{
    AuthenticatedRefresh, RefreshTarget, changes_json, current_asset, same_selected_facts,
};
use crate::state::{VersionSizeKey, error::StateError};

pub(crate) use super::refreshed_store::RefreshedDispatchTicket;

pub(crate) const DDL: &str = include_str!("refreshed_transfer.sql");

impl SqliteStateDb {
    /// Validate a live, unclaimed source and return bounded non-authoritative
    /// hints. Nothing here records a transfer or consumes the witness.
    pub(crate) async fn discover_authenticated_refreshed_wait_candidates(
        &self,
        owner: account::AccountOwner,
        authority: SourceAuthority,
        original: SelectionTransferKey,
        policy: CurrentPolicyEligibility,
        route: String,
        witness: AuthenticatedRefresh,
        provider_version: VersionSizeKey,
    ) -> Result<
        (
            AuthenticatedRefresh,
            CurrentPolicyEligibility,
            Vec<EpochHint>,
            Vec<RefreshedHint>,
        ),
        StateError,
    > {
        use rusqlite::TransactionBehavior;
        self.with_conn_mut("discovering selected refresh candidates", move |conn| {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            account::validate_authenticated(&tx, &owner)?;
            let root = load_generation(&tx, &original.generation)?;
            let decision = load_decision(&tx, &root, &original.pass_key, &original.child)?;
            if !super::transfer_wait::matches_task(&tx, &root, &decision, &original)? {
                return Err(StateError::ProviderWorkConflict);
            }
            if !super::compact_attestation::validate_one_compact_receipt_tx(
                &tx,
                &owner,
                &authority,
                &root.id,
                &original.pass_key,
                &original.child,
                &route,
                &policy,
            )? {
                return Err(StateError::ProviderSelectionAmbiguous);
            }
            let scope = changes_json::parse(root.spec.scope.as_bytes())
                .map_err(|_error| StateError::ProviderSelectionInvalid)?;
            let database = scope
                .get("database")
                .and_then(serde_json::Value::as_str)
                .ok_or(StateError::ProviderSelectionInvalid)?;
            let logical = VersionSizeKey::from_str(&original.version_size)
                .ok_or(StateError::ProviderSelectionInvalid)?;
            if !(logical == provider_version
                || matches!(
                    (logical, provider_version),
                    (VersionSizeKey::Original, VersionSizeKey::Alternative)
                        | (VersionSizeKey::Alternative, VersionSizeKey::Original)
                ))
            {
                return Err(StateError::ProviderWorkConflict);
            }
            let target = RefreshTarget {
                database,
                zone: &root.spec.zone,
                child: &original.child,
                master: &original.master,
                version: provider_version,
                checksum: &original.checksum,
                size: original.size,
            };
            let url = witness.url();
            let view = witness
                .peek_source(&owner, &authority, &target, url)
                .ok_or(StateError::ProviderSelectionAmbiguous)?;
            if !super::refreshed_source::validate_retained_lookup(
                &owner,
                &authority,
                database,
                &root.spec.zone,
                &original.child,
                &original.master,
                provider_version,
                &original.checksum,
                original.size,
                url,
                view.request_body,
                view.body,
                &view.request_binding,
                &view.authority_binding,
                view.invocation_id,
            ) {
                return Err(StateError::ProviderSelectionAmbiguous);
            }
            let original_body = decision
                .decision
                .confirmation
                .as_deref()
                .ok_or(StateError::ProviderSelectionAmbiguous)?;
            let original_asset = current_asset(
                original_body,
                &original.child,
                &original.master,
                &root.spec.zone,
            )
            .map_err(|_error| StateError::ProviderSelectionInvalid)?;
            let fresh_asset = current_asset(
                view.body,
                &original.child,
                &original.master,
                &root.spec.zone,
            )
            .map_err(|_error| StateError::ProviderSelectionInvalid)?;
            if !same_selected_facts(&original_asset, &fresh_asset) {
                return Err(StateError::ProviderWorkConflict);
            }
            let locator = super::refreshed_source::original_epoch_locator(
                &tx, &root, &decision, &original, &policy, view.body, url,
            )?
            .ok_or(StateError::ProviderSelectionAmbiguous)?;
            let now = super::super::retry_clock::now_for_connection(&tx);
            let original_hints = super::immutable_epochs::future_hints(&tx, &locator, now)?;
            let refreshed_hints = super::refreshed_candidates::discover(&tx, &locator, now)?;
            tx.commit()?;
            Ok((witness, policy, original_hints, refreshed_hints))
        })
        .await
    }
}

/// Complete lookup evidence retained once, at the authenticated transaction.
/// Signed URLs are present only inside the response and are never logged.
#[derive(serde::Serialize, serde::Deserialize)]
struct RetainedRefresh {
    request: Vec<u8>,
    response: Vec<u8>,
    request_binding: [u8; 32],
    authority_binding: [u8; 32],
    invocation: [u8; 16],
    provider_version: VersionSizeKey,
    route: String,
}

#[allow(clippy::too_many_arguments)]
fn validate_refreshed_history_tx(
    conn: &rusqlite::Connection,
    owner: &account::AccountOwner,
    authority: &SourceAuthority,
    target: &SelectionTransferKey,
    target_policy: &CurrentPolicyEligibility,
    locator: &str,
    fresh_body: &[u8],
    fresh_url: &str,
    hint: &RefreshedHint,
    source_policy: &CurrentPolicyEligibility,
) -> Result<(), StateError> {
    use rusqlite::OptionalExtension;
    if conn.is_autocommit() {
        return Err(StateError::ProviderSelectionInvalid);
    }
    account::validate_authenticated(conn, owner)?;
    let sizes: Option<(i64,i64,i64,i64)> = conn.query_row(
        "SELECT length(receipt),length(policy_snapshot),length(CAST(native_slot AS BLOB)),length(CAST(proof_hash AS BLOB)) FROM provider_refreshed_transfer WHERE id=?1",
        [&hint.id], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?)),
    ).optional()?;
    let Some((receipt_len, policy_len, slot_len, proof_len)) = sizes else {
        return Err(StateError::ProviderSelectionAmbiguous);
    };
    if [receipt_len, policy_len, slot_len]
        .into_iter()
        .any(|n| n <= 0 || usize::try_from(n).map_or(true, |n| n > super::MAX_CHUNK_BYTES))
        || proof_len != 64
    {
        return Err(StateError::ProviderSelectionInvalid);
    }
    type Row = (
        String,
        String,
        String,
        String,
        String,
        String,
        String,
        Vec<u8>,
        String,
        u32,
        Vec<u8>,
        i64,
        i64,
        i64,
        String,
        i64,
    );
    let row: Row = conn.query_row(
        "SELECT obligation_digest,generation,pass_key,child,version_size,path,native_slot,receipt,proof_hash,prior_attempts,policy_snapshot,retry_delay,attempt,deadline,outcome_proof,charged_bytes FROM provider_refreshed_transfer WHERE id=?1 AND phase='expired'",
        [&hint.id], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?,r.get(6)?,r.get(7)?,r.get(8)?,r.get(9)?,r.get(10)?,r.get(11)?,r.get(12)?,r.get(13)?,r.get(14)?,r.get(15)?)),
    )?;
    let (
        saved_locator,
        generation,
        pass,
        child,
        version,
        path,
        slot,
        receipt,
        proof,
        prior,
        snapshot,
        delay,
        attempt,
        deadline,
        outcome,
        charged,
    ) = row;
    if saved_locator != locator
        || generation != hint.generation
        || pass != hint.pass
        || child != hint.child
        || version != hint.version
        || path != hint.path
        || pass != target.pass_key
        || child != target.child
        || version != target.version_size
        || path != target.path.key()?
        || attempt != hint.attempt
        || deadline != hint.deadline
        || attempt != i64::from(prior.saturating_add(1).min(32))
        || !(1..=86400).contains(&delay)
        || deadline <= 0
        || snapshot != target_policy.resolved_snapshot()
        || snapshot != source_policy.resolved_snapshot()
        || target_policy.policy_preimage() != source_policy.policy_preimage()
    {
        return Err(StateError::ProviderSelectionAmbiguous);
    }
    let expected_slot = super::super::primary_layout::path_key(&target.path.to_path())?;
    if slot != expected_slot {
        return Err(StateError::ProviderWorkConflict);
    }
    let expected_proof = super::digest(&super::encode(&(
        "authenticated-refreshed-dispatch-v1",
        &hint.id,
        &saved_locator,
        &generation,
        &pass,
        &child,
        &version,
        &path,
        &slot,
        &receipt,
        prior,
        delay,
        &snapshot,
    ))?);
    if proof != expected_proof {
        return Err(StateError::ProviderSelectionInvalid);
    }
    let expected_charge = u64::try_from(
        super::encode(&(
            &hint.id,
            &saved_locator,
            &generation,
            &pass,
            &child,
            &version,
            &path,
            &slot,
            &receipt,
            &proof,
            prior,
            delay,
            &snapshot,
        ))?
        .len()
        .checked_add(512)
        .ok_or(StateError::ProviderSelectionFull)?,
    )
    .map_err(|_error| StateError::ProviderSelectionFull)?;
    if u64::try_from(charged).ok() != Some(expected_charge) {
        return Err(StateError::ProviderSelectionInvalid);
    }
    if outcome
        != super::digest(&super::encode(&(
            "authenticated-refreshed-410-v1",
            &hint.id,
            &proof,
            prior.saturating_add(1).min(32),
            deadline,
        ))?)
    {
        return Err(StateError::ProviderSelectionInvalid);
    }
    let retained: RetainedRefresh =
        serde_json::from_slice(&receipt).map_err(|_error| StateError::ProviderSelectionInvalid)?;
    if retained.response != fresh_body {
        return Err(StateError::ProviderWorkConflict);
    }
    let root = load_generation(conn, &generation)?;
    let decision = load_decision(conn, &root, &pass, &child)?;
    let original_body = decision
        .decision
        .confirmation
        .as_deref()
        .ok_or(StateError::ProviderSelectionAmbiguous)?;
    let original_asset = current_asset(original_body, &child, &target.master, &root.spec.zone)
        .map_err(|_error| StateError::ProviderSelectionInvalid)?;
    let mut candidates = original_asset
        .versions()
        .iter()
        .filter(|(version, resource)| {
            VersionSizeKey::from(*version) == retained.provider_version
                && resource.checksum.as_ref() == target.checksum
                && resource.size == target.size
        });
    let Some((_, resource)) = candidates.next() else {
        return Err(StateError::ProviderWorkConflict);
    };
    if candidates.next().is_some() {
        return Err(StateError::ProviderSelectionAmbiguous);
    }
    let mut original = target.clone();
    original.generation = generation.clone();
    original.url = resource.url.to_string();
    if !super::transfer_wait::matches_task(conn, &root, &decision, &original)? {
        return Err(StateError::ProviderWorkConflict);
    }
    let scope = changes_json::parse(root.spec.scope.as_bytes())
        .map_err(|_error| StateError::ProviderSelectionInvalid)?;
    let database = scope
        .get("database")
        .and_then(serde_json::Value::as_str)
        .ok_or(StateError::ProviderSelectionInvalid)?;
    if !super::refreshed_source::validate_retained_lookup(
        owner,
        authority,
        database,
        &root.spec.zone,
        &child,
        &target.master,
        retained.provider_version,
        &target.checksum,
        target.size,
        fresh_url,
        &retained.request,
        &retained.response,
        &retained.request_binding,
        &retained.authority_binding,
        uuid::Uuid::from_bytes(retained.invocation),
    ) {
        return Err(StateError::ProviderSelectionAmbiguous);
    }
    if !super::compact_attestation::validate_one_compact_receipt_tx(
        conn,
        owner,
        authority,
        &generation,
        &pass,
        &child,
        &retained.route,
        source_policy,
    )? {
        return Err(StateError::ProviderSelectionAmbiguous);
    }
    Ok(())
}

impl SqliteStateDb {
    /// Load inputs for reminting the source policy. This remains a hint; the
    /// final transaction revalidates the receipt, row, owner and policy.
    pub(crate) async fn current_refreshed_epoch_context(
        &self,
        owner: account::AccountOwner,
        hint: RefreshedHint,
    ) -> Result<(RefreshedHint, Option<super::immutable_epochs::EpochContext>), StateError> {
        use rusqlite::{TransactionBehavior, params};
        self.with_conn_mut("loading refreshed retry owner", move |conn| {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Deferred)?;
            account::validate_authenticated(&tx, &owner)?;
            let exists: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM provider_refreshed_transfer WHERE id=?1 AND generation=?2 AND pass_key=?3 AND child=?4 AND version_size=?5 AND path=?6 AND attempt=?7 AND deadline=?8 AND phase='expired')",
                params![hint.id,hint.generation,hint.pass,hint.child,hint.version,hint.path,hint.attempt,hint.deadline],
                |r| r.get(0),
            )?;
            if !exists { tx.commit()?; return Ok((hint,None)); }
            let root = load_generation(&tx, &hint.generation)?;
            let decision = load_decision(&tx, &root, &hint.pass, &hint.child)?;
            let policy = super::compact_attestation::load_policy_tx(&tx, &owner, &root.id)?;
            let context = policy.map(|policy| super::immutable_epochs::EpochContext { root,decision,policy });
            tx.commit()?;
            Ok((hint,context))
        }).await
    }
}

/// Every policy is freshly minted by the actual owning selection run. Hints
/// remain untrusted until revalidated together with the consumed witness.
pub(crate) struct RefreshedBegin {
    pub(crate) owner: account::AccountOwner,
    pub(crate) authority: SourceAuthority,
    pub(crate) original: SelectionTransferKey,
    pub(crate) policy: CurrentPolicyEligibility,
    pub(crate) route: String,
    pub(crate) witness: AuthenticatedRefresh,
    pub(crate) provider_version: VersionSizeKey,
    pub(crate) original_hints: Vec<EpochHint>,
    pub(crate) original_policies: Vec<(EpochHint, CurrentPolicyEligibility)>,
    pub(crate) refreshed_hints: Vec<RefreshedHint>,
    pub(crate) refreshed_policies: Vec<(RefreshedHint, CurrentPolicyEligibility)>,
    pub(crate) capacity: u64,
}

#[allow(clippy::too_many_arguments)]
fn insert_refreshed_wait_tx(
    conn: &rusqlite::Connection,
    target: &SelectionTransferKey,
    source_kind: &str,
    source_id: &str,
    deadline: i64,
    receipt: &[u8],
    snapshot: &[u8],
    capacity: u64,
) -> Result<(), StateError> {
    use rusqlite::{OptionalExtension, params};
    if conn.is_autocommit()
        || !matches!(source_kind, "original" | "refreshed")
        || deadline <= super::super::retry_clock::now_for_connection(conn)
        || receipt.is_empty()
        || snapshot.is_empty()
    {
        return Err(StateError::ProviderSelectionInvalid);
    }
    let path = target.path.key()?;
    let proof = super::digest(&super::encode(&(
        "authenticated-refreshed-wait-v1",
        &target.generation,
        &target.pass_key,
        &target.child,
        &target.version_size,
        &path,
        source_kind,
        source_id,
        deadline,
        receipt,
        snapshot,
    ))?);
    let charged = u64::try_from(
        super::encode(&(
            &target.generation,
            &target.pass_key,
            &target.child,
            &target.version_size,
            &path,
            source_kind,
            source_id,
            deadline,
            receipt,
            snapshot,
            &proof,
        ))?
        .len()
        .checked_add(256)
        .ok_or(StateError::ProviderSelectionFull)?,
    )
    .map_err(|_error| StateError::ProviderSelectionFull)?;
    type StoredRefreshedWait = (i64, Vec<u8>, Vec<u8>, String, i64);
    let existing: Option<StoredRefreshedWait> = conn.query_row(
        "SELECT deadline,receipt,policy_snapshot,proof_hash,charged_bytes FROM provider_refreshed_waits WHERE generation=?1 AND pass_key=?2 AND child=?3 AND version_size=?4 AND path=?5 AND source_kind=?6 AND source_id=?7",
        params![target.generation,target.pass_key,target.child,target.version_size,path,source_kind,source_id],
        |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?)),
    ).optional()?;
    if let Some((saved_deadline, saved_receipt, saved_snapshot, saved_proof, saved_charged)) =
        existing
    {
        if saved_deadline != deadline
            || saved_receipt != receipt
            || saved_snapshot != snapshot
            || saved_proof != proof
            || u64::try_from(saved_charged).ok() != Some(charged)
        {
            return Err(StateError::ProviderSelectionAmbiguous);
        }
        return Ok(());
    }
    super::charge_optional_evidence(conn, charged, capacity, 2)?;
    conn.execute(
        "INSERT INTO provider_refreshed_waits(generation,pass_key,child,version_size,path,source_kind,source_id,deadline,receipt,policy_snapshot,proof_hash,charged_bytes) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)",
        params![target.generation,target.pass_key,target.child,target.version_size,path,source_kind,source_id,deadline,receipt,snapshot,proof,i64::try_from(charged).map_err(|_error| StateError::ProviderSelectionFull)?],
    )?;
    Ok(())
}

impl SqliteStateDb {
    /// Atomically consume the exact live witness, revalidate all bounded hints,
    /// and either retain an honest shared wait or reserve actual dispatch.
    /// Caller moves the SAME held destination guard into this owned operation.
    pub(crate) async fn begin_authenticated_refreshed_transfer(
        &self,
        input: RefreshedBegin,
    ) -> Result<Option<RefreshedDispatchTicket>, StateError> {
        use rusqlite::TransactionBehavior;
        self.with_conn_mut("beginning selected authenticated refresh", move |conn| {
            let RefreshedBegin {
                owner,
                authority,
                original,
                policy,
                route,
                mut witness,
                provider_version,
                original_hints,
                original_policies,
                refreshed_hints,
                refreshed_policies,
                capacity,
            } = input;
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            account::validate_authenticated(&tx, &owner)?;
            let root = load_generation(&tx, &original.generation)?;
            let decision = load_decision(&tx, &root, &original.pass_key, &original.child)?;
            if !super::transfer_wait::matches_task(&tx, &root, &decision, &original)?
                || !super::compact_attestation::validate_one_compact_receipt_tx(
                    &tx,
                    &owner,
                    &authority,
                    &root.id,
                    &original.pass_key,
                    &original.child,
                    &route,
                    &policy,
                )?
            {
                return Err(StateError::ProviderSelectionAmbiguous);
            }
            let scope = changes_json::parse(root.spec.scope.as_bytes())
                .map_err(|_error| StateError::ProviderSelectionInvalid)?;
            let database = scope
                .get("database")
                .and_then(serde_json::Value::as_str)
                .ok_or(StateError::ProviderSelectionInvalid)?;
            let logical = VersionSizeKey::from_str(&original.version_size)
                .ok_or(StateError::ProviderSelectionInvalid)?;
            if !(logical == provider_version
                || matches!(
                    (logical, provider_version),
                    (VersionSizeKey::Original, VersionSizeKey::Alternative)
                        | (VersionSizeKey::Alternative, VersionSizeKey::Original)
                ))
            {
                return Err(StateError::ProviderWorkConflict);
            }
            let target = RefreshTarget {
                database,
                zone: &root.spec.zone,
                child: &original.child,
                master: &original.master,
                version: provider_version,
                checksum: &original.checksum,
                size: original.size,
            };
            let fresh_url = witness.url().to_owned();
            if !witness.claim(&owner, &authority, &target, &fresh_url) {
                return Err(StateError::ProviderSelectionAmbiguous);
            }
            let source = witness
                .claimed_source(&owner, &authority, &target, &fresh_url)
                .ok_or(StateError::ProviderSelectionAmbiguous)?;
            if !super::refreshed_source::validate_retained_lookup(
                &owner,
                &authority,
                database,
                &root.spec.zone,
                &original.child,
                &original.master,
                provider_version,
                &original.checksum,
                original.size,
                &fresh_url,
                source.request_body,
                source.body,
                &source.request_binding,
                &source.authority_binding,
                source.invocation_id,
            ) {
                return Err(StateError::ProviderSelectionAmbiguous);
            }
            let old = decision
                .decision
                .confirmation
                .as_deref()
                .ok_or(StateError::ProviderSelectionAmbiguous)?;
            let old_asset = current_asset(old, &original.child, &original.master, &root.spec.zone)
                .map_err(|_error| StateError::ProviderSelectionInvalid)?;
            let fresh_asset = current_asset(
                source.body,
                &original.child,
                &original.master,
                &root.spec.zone,
            )
            .map_err(|_error| StateError::ProviderSelectionInvalid)?;
            if !same_selected_facts(&old_asset, &fresh_asset) {
                return Err(StateError::ProviderWorkConflict);
            }
            let locator = super::refreshed_source::original_epoch_locator(
                &tx,
                &root,
                &decision,
                &original,
                &policy,
                source.body,
                &fresh_url,
            )?
            .ok_or(StateError::ProviderSelectionAmbiguous)?;
            let now = super::super::retry_clock::now_for_connection(&tx);
            let current_original = super::immutable_epochs::future_hints(&tx, &locator, now)?;
            let current_refreshed = super::refreshed_candidates::discover(&tx, &locator, now)?;
            if current_original != original_hints
                || current_refreshed != refreshed_hints
                || original_hints.len() != original_policies.len()
                || refreshed_hints.len() != refreshed_policies.len()
            {
                return Err(StateError::ProviderWorkConflict);
            }
            let mut wait: Option<(&str, String, i64)> = None;
            for (hint, (policy_hint, source_policy)) in
                original_hints.iter().zip(original_policies.iter())
            {
                if hint != policy_hint {
                    return Err(StateError::ProviderSelectionAmbiguous);
                }
                let validated = super::refreshed_source::original_epoch_waiting_tx(
                    &tx,
                    &owner,
                    &authority,
                    &original,
                    &route,
                    &policy,
                    source.body,
                    &fresh_url,
                    hint,
                    source_policy,
                )?;
                let check_now = super::super::retry_clock::now_for_connection(&tx);
                if !validated && hint.deadline > check_now {
                    return Err(StateError::ProviderSelectionAmbiguous);
                }
                if validated
                    && hint.deadline > check_now
                    && wait.as_ref().is_none_or(|w| w.2 < hint.deadline)
                {
                    wait = Some(("original", hint.marker_hash.clone(), hint.deadline));
                }
            }
            let mut prior_attempts = 0_u32;
            for (hint, (policy_hint, source_policy)) in
                refreshed_hints.iter().zip(refreshed_policies.iter())
            {
                if hint != policy_hint {
                    return Err(StateError::ProviderSelectionAmbiguous);
                }
                validate_refreshed_history_tx(
                    &tx,
                    &owner,
                    &authority,
                    &original,
                    &policy,
                    &locator,
                    source.body,
                    &fresh_url,
                    hint,
                    source_policy,
                )?;
                if hint.generation == original.generation {
                    prior_attempts = prior_attempts.max(
                        u32::try_from(hint.attempt)
                            .map_err(|_error| StateError::ProviderSelectionInvalid)?,
                    );
                }
                if hint.deadline > super::super::retry_clock::now_for_connection(&tx)
                    && wait.as_ref().is_none_or(|w| w.2 < hint.deadline)
                {
                    wait = Some(("refreshed", hint.id.clone(), hint.deadline));
                }
            }
            let retained = RetainedRefresh {
                request: source.request_body.to_vec(),
                response: source.body.to_vec(),
                request_binding: source.request_binding,
                authority_binding: source.authority_binding,
                invocation: *source.invocation_id.as_bytes(),
                provider_version,
                route: route.clone(),
            };
            let receipt = super::encode(&retained)?;
            if let Some((kind, id, deadline)) = wait {
                // Validation may cross the predecessor deadline. Recheck after
                // all receipts and do not persist a stale shared wait.
                if deadline > super::super::retry_clock::now_for_connection(&tx) {
                    let wait_id = format!("{id}:{}", uuid::Uuid::from_bytes(retained.invocation));
                    insert_refreshed_wait_tx(
                        &tx,
                        &original,
                        kind,
                        &wait_id,
                        deadline,
                        &receipt,
                        policy.resolved_snapshot(),
                        capacity,
                    )?;
                    if deadline <= super::super::retry_clock::now_for_connection(&tx) {
                        return Err(StateError::ProviderWorkConflict);
                    }
                    tx.commit()?;
                    return Ok(None);
                }
            }
            guard_asset_attempt_limit(
                &tx,
                policy.resolved_snapshot(),
                &root.spec.zone,
                &original.state_id,
            )?;
            let history = super::refreshed_store::validated_history(prior_attempts, &policy)?;
            let mut ticket = super::refreshed_store::insert_inflight_tx(
                &tx,
                &original.generation,
                &original.pass_key,
                &original.child,
                &original.version_size,
                &original.path,
                &locator,
                &receipt,
                history,
                capacity,
            )?;
            ticket.owner = Some(owner);
            ticket.authority = Some(authority);
            ticket.original = Some(original);
            ticket.policy = Some(policy);
            ticket.route = Some(route);
            ticket.fresh_url = Some(fresh_url);
            tx.commit()?;
            Ok(Some(ticket))
        })
        .await
    }
}

/// Revalidate a reserved dispatch inside the terminal SQLite transaction.
fn validate_inflight_ticket_tx(
    conn: &rusqlite::Connection,
    ticket: &RefreshedDispatchTicket,
    completed: bool,
) -> Result<(), StateError> {
    use rusqlite::OptionalExtension;
    let owner = ticket
        .owner
        .as_ref()
        .ok_or(StateError::ProviderSelectionAmbiguous)?;
    let authority = ticket
        .authority
        .as_ref()
        .ok_or(StateError::ProviderSelectionAmbiguous)?;
    let original = ticket
        .original
        .as_ref()
        .ok_or(StateError::ProviderSelectionAmbiguous)?;
    let policy = ticket
        .policy
        .as_ref()
        .ok_or(StateError::ProviderSelectionAmbiguous)?;
    let route = ticket
        .route
        .as_deref()
        .ok_or(StateError::ProviderSelectionAmbiguous)?;
    let fresh_url = ticket
        .fresh_url
        .as_deref()
        .ok_or(StateError::ProviderSelectionAmbiguous)?;
    account::validate_authenticated(conn, owner)?;
    let root = load_generation(conn, &original.generation)?;
    let decision = load_decision(conn, &root, &original.pass_key, &original.child)?;
    if (!completed && !super::transfer_wait::matches_task(conn, &root, &decision, original)?)
        || !super::compact_attestation::validate_one_compact_receipt_tx(
            conn,
            owner,
            authority,
            &root.id,
            &original.pass_key,
            &original.child,
            route,
            policy,
        )?
    {
        return Err(StateError::ProviderSelectionAmbiguous);
    }
    let lengths:Option<(i64,i64,i64)>=conn.query_row(
                "SELECT length(receipt),length(policy_snapshot),length(CAST(native_slot AS BLOB)) FROM provider_refreshed_transfer WHERE id=?1 AND phase='inflight'",
                [&ticket.id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))
            ).optional()?;
    let Some((receipt_len, snapshot_len, slot_len)) = lengths else {
        return Err(StateError::ProviderSelectionAmbiguous);
    };
    if [receipt_len, snapshot_len, slot_len]
        .into_iter()
        .any(|n| n <= 0 || usize::try_from(n).map_or(true, |n| n > super::MAX_CHUNK_BYTES))
    {
        return Err(StateError::ProviderSelectionInvalid);
    }
    type Row = (
        String,
        String,
        String,
        String,
        String,
        String,
        String,
        Vec<u8>,
        String,
        u32,
        Vec<u8>,
        i64,
        i64,
    );
    let row:Row=conn.query_row(
                "SELECT obligation_digest,generation,pass_key,child,version_size,path,native_slot,receipt,proof_hash,prior_attempts,policy_snapshot,retry_delay,charged_bytes FROM provider_refreshed_transfer WHERE id=?1 AND phase='inflight'",
                [&ticket.id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?,r.get(6)?,r.get(7)?,r.get(8)?,r.get(9)?,r.get(10)?,r.get(11)?,r.get(12)?))
            )?;
    let (
        locator,
        generation,
        pass,
        child,
        version,
        path,
        slot,
        receipt,
        proof,
        prior,
        snapshot,
        delay,
        charged,
    ) = row;
    if generation != original.generation
        || pass != original.pass_key
        || child != original.child
        || version != original.version_size
        || path != original.path.key()?
        || slot != super::super::primary_layout::path_key(&original.path.to_path())?
        || proof != ticket.proof
        || prior != ticket.prior_attempts
        || delay != ticket.retry_delay
        || snapshot != policy.resolved_snapshot()
        || delay != (3600_i64 * (1_i64 << prior.min(5))).min(86400)
    {
        return Err(StateError::ProviderSelectionAmbiguous);
    }
    let computed = super::digest(&super::encode(&(
        "authenticated-refreshed-dispatch-v1",
        &ticket.id,
        &locator,
        &generation,
        &pass,
        &child,
        &version,
        &path,
        &slot,
        &receipt,
        prior,
        delay,
        &snapshot,
    ))?);
    let expected_charge = u64::try_from(
        super::encode(&(
            &ticket.id,
            &locator,
            &generation,
            &pass,
            &child,
            &version,
            &path,
            &slot,
            &receipt,
            &proof,
            prior,
            delay,
            &snapshot,
        ))?
        .len()
        .checked_add(512)
        .ok_or(StateError::ProviderSelectionFull)?,
    )
    .map_err(|_error| StateError::ProviderSelectionFull)?;
    if proof != computed || u64::try_from(charged).ok() != Some(expected_charge) {
        return Err(StateError::ProviderSelectionInvalid);
    }
    let retained: RetainedRefresh =
        serde_json::from_slice(&receipt).map_err(|_error| StateError::ProviderSelectionInvalid)?;
    if retained.route != route {
        return Err(StateError::ProviderSelectionAmbiguous);
    }
    let scope = changes_json::parse(root.spec.scope.as_bytes())
        .map_err(|_error| StateError::ProviderSelectionInvalid)?;
    let database = scope
        .get("database")
        .and_then(serde_json::Value::as_str)
        .ok_or(StateError::ProviderSelectionInvalid)?;
    if !super::refreshed_source::validate_retained_lookup(
        owner,
        authority,
        database,
        &root.spec.zone,
        &original.child,
        &original.master,
        retained.provider_version,
        &original.checksum,
        original.size,
        fresh_url,
        &retained.request,
        &retained.response,
        &retained.request_binding,
        &retained.authority_binding,
        uuid::Uuid::from_bytes(retained.invocation),
    ) {
        return Err(StateError::ProviderSelectionAmbiguous);
    }
    let actual = if completed {
        super::refreshed_source::completed_epoch_locator(
            conn,
            &root,
            &decision,
            original,
            policy,
            &retained.response,
            fresh_url,
        )?
    } else {
        super::refreshed_source::original_epoch_locator(
            conn,
            &root,
            &decision,
            original,
            policy,
            &retained.response,
            fresh_url,
        )?
    }
    .ok_or(StateError::ProviderSelectionAmbiguous)?;
    if actual != locator {
        return Err(StateError::ProviderWorkConflict);
    }
    let old = decision
        .decision
        .confirmation
        .as_deref()
        .ok_or(StateError::ProviderSelectionAmbiguous)?;
    let original_asset = current_asset(old, &original.child, &original.master, &root.spec.zone)
        .map_err(|_error| StateError::ProviderSelectionInvalid)?;
    let fresh_asset = current_asset(
        &retained.response,
        &original.child,
        &original.master,
        &root.spec.zone,
    )
    .map_err(|_error| StateError::ProviderSelectionInvalid)?;
    if !same_selected_facts(&original_asset, &fresh_asset) {
        return Err(StateError::ProviderWorkConflict);
    }
    Ok(())
}

impl SqliteStateDb {
    /// Record only an actually observed 410. Caller retains the same destination
    /// guard throughout; failure leaves the reservation unresolved.
    pub(crate) async fn record_authenticated_refreshed_410(
        &self,
        ticket: RefreshedDispatchTicket,
    ) -> Result<(), StateError> {
        use rusqlite::TransactionBehavior;
        self.with_conn_mut(
            "recording selected authenticated refresh 410",
            move |conn| {
                let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
                validate_inflight_ticket_tx(&tx, &ticket, false)?;
                super::refreshed_store::record_410_tx(&tx, ticket)?;
                tx.commit()?;
                Ok(())
            },
        )
        .await
    }
}

/// Match the producer's per-asset limit using current rows, not selection
/// generation counts. A zero limit retains the existing unlimited semantics.
fn guard_asset_attempt_limit(
    conn: &rusqlite::Connection,
    snapshot: &[u8],
    zone: &serde_json::Value,
    state_id: &str,
) -> Result<(), StateError> {
    use rusqlite::params;
    if conn.is_autocommit() || snapshot.len() > super::MAX_CHUNK_BYTES || state_id.is_empty() {
        return Err(StateError::ProviderSelectionInvalid);
    }
    let value =
        changes_json::parse(snapshot).map_err(|_error| StateError::ProviderSelectionInvalid)?;
    let library = value
        .get("library")
        .and_then(serde_json::Value::as_str)
        .filter(|v| !v.is_empty())
        .ok_or(StateError::ProviderSelectionInvalid)?;
    if zone.get("zoneName").and_then(serde_json::Value::as_str) != Some(library) {
        return Err(StateError::ProviderWorkConflict);
    }
    let max = value
        .pointer("/retry/max_download_attempts")
        .and_then(serde_json::Value::as_u64)
        .and_then(|v| u32::try_from(v).ok())
        .ok_or(StateError::ProviderSelectionInvalid)?;
    if max == 0 {
        return Ok(());
    }
    let count: i64 = conn.query_row(
        "SELECT COALESCE(MAX(download_attempts),0) FROM assets WHERE library=?1 AND id=?2 AND download_attempts>0",
        params![library,state_id], |r| r.get(0),
    )?;
    let count = u32::try_from(count).unwrap_or(u32::MAX);
    if count >= max {
        return Err(StateError::ProviderWorkConflict);
    }
    Ok(())
}

#[cfg(test)]
mod attempt_limit_tests {
    use super::*;

    #[test]
    fn uses_current_asset_max_and_honors_reset_and_unlimited() {
        let mut conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE assets(library TEXT NOT NULL,id TEXT NOT NULL,version_size TEXT NOT NULL,download_attempts INTEGER NOT NULL,PRIMARY KEY(library,id,version_size)); INSERT INTO assets VALUES('PrimarySync','asset','original',1),('PrimarySync','asset','live_original',3),('Other','asset','original',99);").unwrap();
        let tx = conn.transaction().unwrap();
        let zone = serde_json::json!({"zoneName":"PrimarySync"});
        let snapshot = |max| {
            serde_json::to_vec(
                &serde_json::json!({"library":"PrimarySync","retry":{"max_download_attempts":max}}),
            )
            .unwrap()
        };
        assert!(guard_asset_attempt_limit(&tx, &snapshot(4), &zone, "asset").is_ok());
        assert!(matches!(
            guard_asset_attempt_limit(&tx, &snapshot(3), &zone, "asset"),
            Err(StateError::ProviderWorkConflict)
        ));
        assert!(guard_asset_attempt_limit(&tx, &snapshot(0), &zone, "asset").is_ok());
        assert!(guard_asset_attempt_limit(&tx, &snapshot(1), &zone, "other").is_ok());
        assert!(
            guard_asset_attempt_limit(
                &tx,
                &snapshot(4),
                &serde_json::json!({"zoneName":"Other"}),
                "asset"
            )
            .is_err()
        );
        tx.execute(
            "UPDATE assets SET download_attempts=0 WHERE library='PrimarySync' AND id='asset'",
            [],
        )
        .unwrap();
        assert!(guard_asset_attempt_limit(&tx, &snapshot(3), &zone, "asset").is_ok());
    }
}

/// Stream the original v1 JSON tuples into a bounded counting hash sink. The
/// archived BLOB stays byte-for-byte unchanged, and the proof and charge are
/// exactly those produced by serde_json::to_vec on the same v1 tuples.
fn legacy_success_proof_and_charge(
    dispatch_id: &str,
    receipt_id: &str,
    archived: &[u8],
) -> Result<(String, u64), StateError> {
    use sha2::{Digest, Sha256};
    use std::io::{self, Write};

    // Match `super::encode`: each outer v1 tuple has its own 16 MiB limit.
    // No additional bound is imposed on IDs or other individual fields.
    let limit = super::MAX_CHUNK_BYTES;
    struct Sink {
        hash: Sha256,
        count: usize,
        limit: usize,
        overflow: bool,
    }
    impl Write for Sink {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            let Some(next) = self.count.checked_add(bytes.len()) else {
                self.overflow = true;
                return Err(io::Error::other("bounded serialization overflow"));
            };
            if next > self.limit {
                self.overflow = true;
                return Err(io::Error::other("bounded serialization overflow"));
            }
            self.hash.update(bytes);
            self.count = next;
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    fn stream<T: serde::Serialize>(value: &T, limit: usize) -> Result<(String, usize), StateError> {
        let mut sink = Sink {
            hash: Sha256::new(),
            count: 0,
            limit,
            overflow: false,
        };
        if serde_json::to_writer(&mut sink, value).is_err() {
            return Err(if sink.overflow {
                StateError::ProviderSelectionFull
            } else {
                StateError::ProviderSelectionInvalid
            });
        }
        Ok((format!("{:x}", sink.hash.finalize()), sink.count))
    }
    let (proof, _) = stream(
        &(
            "authenticated-refreshed-success-receipt-v1",
            dispatch_id,
            receipt_id,
            archived,
        ),
        limit,
    )?;
    let (_, size) = stream(&(dispatch_id, receipt_id, archived, &proof), limit)?;
    let charge = u64::try_from(
        size.checked_add(128)
            .ok_or(StateError::ProviderSelectionFull)?,
    )
    .map_err(|_error| StateError::ProviderSelectionFull)?;
    Ok((proof, charge))
}

fn finish_refreshed_success_tx<F>(
    conn: &rusqlite::Connection,
    ticket: &RefreshedDispatchTicket,
    receipt_id: &str,
    validate_file: F,
) -> Result<(), StateError>
where
    F: FnOnce(&super::super::download_receipts::DownloadReceipt) -> Result<(), StateError>,
{
    use rusqlite::params;
    validate_inflight_ticket_tx(conn, ticket, true)?;
    let owner = ticket
        .owner
        .as_ref()
        .ok_or(StateError::ProviderSelectionAmbiguous)?;
    let original = ticket
        .original
        .as_ref()
        .ok_or(StateError::ProviderSelectionAmbiguous)?;
    account::validate_authenticated(conn, owner)?;
    let root = load_generation(conn, &original.generation)?;
    let receipt = super::super::download_receipts::load(conn, receipt_id)?
        .ok_or(StateError::ProviderSelectionAmbiguous)?;
    let (account_key, provider_key) = super::owner_keys(conn)?;
    let library = root
        .spec
        .zone
        .get("zoneName")
        .and_then(serde_json::Value::as_str)
        .ok_or(StateError::ProviderSelectionInvalid)?;
    if receipt.account_key != account_key
        || receipt.provider_key != provider_key
        || receipt.scope != root.spec.scope
        || receipt.library != library
        || receipt.child != original.child
        || receipt.master != original.master
        || receipt.state_id != original.state_id
        || !receipt.fulfills_pass(&original.pass_key)
        || receipt.version_size != original.version_size
        || receipt.provider_checksum != original.checksum
        || receipt.provider_size != original.size
        || receipt.native_path != original.path
    {
        return Err(StateError::ProviderWorkConflict);
    }
    let (local,source):(String,String)=conn.query_row(
                "SELECT local_checksum,source_checksum FROM provider_active_destinations WHERE generation=?1 AND pass_key=?2 AND child=?3 AND version_size=?4 AND path=?5 AND admitted=1 AND verified_media=1",
                params![original.generation,original.pass_key,original.child,original.version_size,original.path.key()?],
                |r|Ok((r.get(0)?,r.get(1)?)),
            )?;
    if local != receipt.local_checksum || source != receipt.source_checksum {
        return Err(StateError::ProviderWorkConflict);
    }
    // The callback owns the no-follow file proof and validates its
    // current identity/content while the transaction and guard live.
    validate_file(&receipt)?;
    let archived = super::encode(&receipt)?;
    let (archive_proof, charged) =
        legacy_success_proof_and_charge(&ticket.id, receipt_id, &archived)?;
    super::charge_optional_evidence(conn, charged, ticket.capacity, 2)?;
    conn.execute("INSERT INTO provider_refreshed_success_receipts(dispatch_id,receipt_id,receipt,proof_hash,charged_bytes) VALUES(?1,?2,?3,?4,?5)",
                params![ticket.id,receipt_id,archived,archive_proof,i64::try_from(charged).map_err(|_error|StateError::ProviderSelectionFull)?])?;
    let outcome = super::digest(&super::encode(&(
        "authenticated-refreshed-success-v2",
        &ticket.id,
        &ticket.proof,
        &receipt_id,
        &archive_proof,
    ))?);
    let updated=conn.execute(
                "UPDATE provider_refreshed_transfer SET phase='published',outcome_proof=?1,success_receipt_id=?2 WHERE id=?3 AND proof_hash=?4 AND phase='inflight'",
                params![outcome,receipt_id,ticket.id,ticket.proof],
            )?;
    if updated != 1 {
        return Err(StateError::ProviderWorkConflict);
    }
    Ok(())
}

impl SqliteStateDb {
    /// Complete a refreshed dispatch only after a persisted, authenticated
    /// source-backed receipt and a currently held file both validate. The
    /// caller must keep the same destination guard alive through this commit.
    pub(crate) async fn record_authenticated_refreshed_success<F>(
        &self,
        ticket: RefreshedDispatchTicket,
        receipt_id: String,
        validate_file: F,
    ) -> Result<(), StateError>
    where
        F: FnOnce(&super::super::download_receipts::DownloadReceipt) -> Result<(), StateError>
            + Send
            + 'static,
    {
        use rusqlite::TransactionBehavior;
        self.with_conn_mut(
            "confirming selected authenticated refresh publication",
            move |conn| {
                let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
                finish_refreshed_success_tx(&tx, &ticket, &receipt_id, validate_file)?;
                tx.commit()?;
                Ok(())
            },
        )
        .await
    }
}

impl SqliteStateDb {
    /// Complete an actually observed non-410 terminal HTTP failure. The caller
    /// must carry the same destination guard and supply only the status from
    /// the finished typed DownloadError::HttpStatus. The caller derives
    /// replace_truncated from the actual task publication policy. Abort,
    /// timeout, transport interruption and unknown outcomes must not call.
    ///
    /// The reservation transition and the ordinary per-asset failed-attempt
    /// update commit together. On success the caller must not also call the
    /// normal finalize_failed path for this same transfer.
    pub(crate) async fn record_authenticated_refreshed_http_failure(
        &self,
        ticket: RefreshedDispatchTicket,
        status: u16,
        error: String,
        replace_truncated: bool,
    ) -> Result<(), StateError> {
        use rusqlite::{OptionalExtension, TransactionBehavior, params};
        if !(status == 429 || (500..=599).contains(&status)) || error.len() > 65_536 {
            return Err(StateError::ProviderSelectionInvalid);
        }
        self.with_conn_mut("recording selected refreshed HTTP failure",move|conn|{
            let tx=conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            validate_inflight_ticket_tx(&tx,&ticket,false)?;
            let owner=ticket.owner.as_ref().ok_or(StateError::ProviderSelectionAmbiguous)?;
            let original=ticket.original.as_ref().ok_or(StateError::ProviderSelectionAmbiguous)?;
            let policy=ticket.policy.as_ref().ok_or(StateError::ProviderSelectionAmbiguous)?;
            account::validate_authenticated(&tx,owner)?;
            let root=load_generation(&tx,&original.generation)?;
            let snapshot=changes_json::parse(policy.resolved_snapshot()).map_err(|_error|StateError::ProviderSelectionInvalid)?;
            let library=snapshot.get("library").and_then(serde_json::Value::as_str)
                .filter(|s|!s.is_empty()).ok_or(StateError::ProviderSelectionInvalid)?;
            if root.spec.zone.get("zoneName").and_then(serde_json::Value::as_str)!=Some(library)
            {return Err(StateError::ProviderWorkConflict)}
            let prior:Option<Option<String>>=tx.query_row(
                "SELECT last_error FROM assets WHERE library=?1 AND id=?2 AND version_size=?3 AND checksum=?4 AND size_bytes=?5 AND is_deleted=0 AND status<>'downloaded'",
                params![library,original.state_id,original.version_size,original.checksum,i64::try_from(original.size).map_err(|_error|StateError::ProviderSelectionInvalid)?],
                |r|r.get(0),
            ).optional()?;
            let Some(prior)=prior else {return Err(StateError::ProviderWorkConflict)};
            // Match finalize_failed's existing reconciliation sentinel for a
            // truncated-file replacement; all other publications retain the
            // exact original transfer error.
            let durable_error=if replace_truncated {
                crate::commands::reconcile::FILE_TRUNCATED_REASON
            } else { error.as_str() };
            let merged=super::super::match_diagnostics::MatchDiagnostics::with_transfer_error(prior.as_deref(),durable_error);
            let error_hash=super::digest(durable_error.as_bytes());
            let outcome=super::digest(&super::encode(&(
                "authenticated-refreshed-http-failure-v1",&ticket.id,&ticket.proof,status,&error_hash,
            ))?);
            let updated=tx.execute(
                "UPDATE provider_refreshed_transfer SET phase='failed',outcome_proof=?1,failure_status=?2,failure_error_hash=?3 WHERE id=?4 AND proof_hash=?5 AND phase='inflight'",
                params![outcome,status,error_hash,ticket.id,ticket.proof],
            )?;
            if updated!=1 {return Err(StateError::ProviderWorkConflict)}
            let changed=tx.execute(
                "UPDATE assets SET status='failed',download_attempts=download_attempts+1,last_error=?1 WHERE library=?2 AND id=?3 AND version_size=?4 AND checksum=?5 AND size_bytes=?6 AND is_deleted=0 AND status<>'downloaded'",
                params![merged,library,original.state_id,original.version_size,original.checksum,i64::try_from(original.size).map_err(|_error|StateError::ProviderSelectionInvalid)?],
            )?;
            if changed!=1 {return Err(StateError::ProviderWorkConflict)}
            tx.commit()?;
            Ok(())
        }).await
    }
}

/// A bounded scheduling hint, not evidence of transfer or publication.
#[derive(Clone, Debug)]
pub(crate) struct RefreshedRecoveryHint {
    pub(crate) id: String,
    pub(crate) pass: String,
    pub(crate) child: String,
    pub(crate) version: String,
    pub(crate) path: super::super::provider_selection::SelectionPath,
}

impl SqliteStateDb {
    /// Reserve at most one bounded page of recovery checks. The scheduling
    /// delay is not a transfer attempt or an expiry decision.
    pub(crate) async fn due_refreshed_recovery_page(
        &self,
        owner: account::AccountOwner,
        generation: String,
    ) -> Result<(Vec<RefreshedRecoveryHint>, bool), StateError> {
        use rusqlite::{TransactionBehavior, params};
        self.with_conn_mut("scheduling selected refresh recovery",move|conn|{
            let tx=conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            account::validate_authenticated(&tx,&owner)?;
            let root=load_generation(&tx,&generation)?;
            if root.spec.format!=2 {return Err(StateError::ProviderSelectionInvalid)}
            let now=super::super::retry_clock::now_for_connection(&tx);
            if now<0 {return Err(StateError::ProviderSelectionInvalid)}
            let next=now.checked_add(60).ok_or(StateError::ProviderSelectionInvalid)?;
            let batch=uuid::Uuid::new_v4().to_string();
            let scheduled=tx.execute(
                "UPDATE provider_refreshed_transfer SET recovery_not_before=?2,recovery_batch=?3 WHERE id IN (SELECT id FROM provider_refreshed_transfer WHERE generation=?1 AND phase='inflight' AND recovery_not_before<=?4 ORDER BY recovery_not_before,id LIMIT 64)",
                params![generation,next,batch,now],
            )?;
            let rows:Vec<(String,i64,i64,i64,i64)>=tx.prepare(
                "SELECT id,length(CAST(pass_key AS BLOB)),length(CAST(child AS BLOB)),length(CAST(version_size AS BLOB)),length(CAST(path AS BLOB)) FROM provider_refreshed_transfer WHERE generation=?1 AND phase='inflight' AND recovery_batch=?2 AND length(CAST(id AS BLOB))=36 ORDER BY id LIMIT 64",
            )?.query_map(params![generation,batch],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?)))?.collect::<Result<_,_>>()?;
            // The indexed probe is limited to one additional due row. A
            // full page does not imply that the durable scan is complete.
            let more_due:bool=tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM provider_refreshed_transfer WHERE generation=?1 AND phase='inflight' AND recovery_not_before<=?2 LIMIT 1)",
                params![generation,now],|r|r.get(0))?;
            let mut total=0_usize;
            let mut result=Vec::new();
            let mut incomplete=more_due || scheduled!=rows.len();
            for (id,p,c,v,path_len) in rows {
                if uuid::Uuid::parse_str(&id).is_err() {incomplete=true;continue}
                let mut bytes=0_usize;
                for n in [p,c,v,path_len] {
                    let Ok(n)=usize::try_from(n) else{bytes=super::MAX_CHUNK_BYTES.saturating_add(1);break};
                    bytes=bytes.saturating_add(n);
                }
                if bytes==0 || bytes>super::MAX_CHUNK_BYTES || total.saturating_add(bytes)>super::MAX_CHUNK_BYTES {incomplete=true;continue}
                let (pass,child,version,path):(String,String,String,String)=tx.query_row(
                    "SELECT pass_key,child,version_size,path FROM provider_refreshed_transfer WHERE id=?1 AND generation=?2 AND phase='inflight' AND recovery_batch=?3",
                    params![id,generation,batch],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?)))?;
                let Ok(path)=serde_json::from_str::<super::super::provider_selection::SelectionPath>(&path) else{incomplete=true;continue};
                if pass.is_empty() || child.is_empty() || VersionSizeKey::from_str(&version).is_none()
                    || super::super::provider_selection::SelectionPath::from_path(&path.to_path())!=path
                {incomplete=true;continue}
                let after_processed=next.checked_add(60).ok_or(StateError::ProviderSelectionInvalid)?;
                tx.execute("UPDATE provider_refreshed_transfer SET recovery_not_before=?2 WHERE id=?1 AND generation=?3 AND phase='inflight' AND recovery_batch=?4",
                    params![id,after_processed,generation,batch])?;
                total+=bytes;
                result.push(RefreshedRecoveryHint{id,pass,child,version,path});
            }
            tx.commit()?;
            Ok((result,incomplete))
        }).await
    }
}

/// Owner-minted recovery inputs. An in-flight row is only a hint: it is never
/// evidence that a GET completed, failed, or produced a particular file.
pub(crate) struct RefreshedRecovery {
    pub(crate) owner: account::AccountOwner,
    pub(crate) authority: SourceAuthority,
    pub(crate) original: SelectionTransferKey,
    pub(crate) policy: CurrentPolicyEligibility,
    pub(crate) route: String,
    pub(crate) id: String,
    pub(crate) capacity: u64,
}

impl SqliteStateDb {
    /// Close only a previously completed, source-receipted publication. The
    /// caller retains the destination guard and no-follow file handle through
    /// this transaction. Missing/unverified work remains in-flight.
    pub(crate) async fn recover_verified_refreshed_transfer<F>(
        &self,
        input: RefreshedRecovery,
        validate_file: F,
    ) -> Result<bool, StateError>
    where
        F: FnOnce(&super::super::download_receipts::DownloadReceipt) -> Result<(), StateError>
            + Send
            + 'static,
    {
        use rusqlite::{OptionalExtension, TransactionBehavior, params};
        self.with_conn_mut("recovering verified selected refresh",move|conn|{
            let RefreshedRecovery{owner,authority,original,policy,route,id,capacity}=input;
            let tx=conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            account::validate_authenticated(&tx,&owner)?;
            let root=load_generation(&tx,&original.generation)?;
            load_decision(&tx,&root,&original.pass_key,&original.child)?;
            let path=original.path.key()?;
            let verified:bool=tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM provider_active_destinations WHERE generation=?1 AND pass_key=?2 AND child=?3 AND version_size=?4 AND path=?5 AND admitted=1 AND verified_media=1 AND length(local_checksum)=64 AND length(source_checksum)=64)",
                params![original.generation,original.pass_key,original.child,original.version_size,path],|r|r.get(0))?;
            if !verified {tx.commit()?;return Ok(false)}
            let lengths:Option<(i64,i64,i64)>=tx.query_row(
                "SELECT length(receipt),length(policy_snapshot),length(CAST(proof_hash AS BLOB)) FROM provider_refreshed_transfer WHERE id=?1 AND generation=?2 AND pass_key=?3 AND child=?4 AND version_size=?5 AND path=?6 AND phase='inflight'",
                params![id,original.generation,original.pass_key,original.child,original.version_size,path],
                |r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
            let Some((receipt_len,snapshot_len,proof_len))=lengths else{tx.commit()?;return Ok(false)};
            if proof_len!=64 || [receipt_len,snapshot_len].into_iter().any(|n|n<=0 || usize::try_from(n).map_or(true,|n|n>super::MAX_CHUNK_BYTES))
            {return Err(StateError::ProviderSelectionInvalid)}
            let (receipt,proof,prior,delay):(Vec<u8>,String,u32,i64)=tx.query_row(
                "SELECT receipt,proof_hash,prior_attempts,retry_delay FROM provider_refreshed_transfer WHERE id=?1 AND generation=?2 AND pass_key=?3 AND child=?4 AND version_size=?5 AND path=?6 AND phase='inflight'",
                params![id,original.generation,original.pass_key,original.child,original.version_size,path],
                |r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?)))?;
            let retained:RetainedRefresh=serde_json::from_slice(&receipt).map_err(|_error|StateError::ProviderSelectionInvalid)?;
            if retained.route!=route {return Err(StateError::ProviderSelectionAmbiguous)}
            let fresh=current_asset(&retained.response,&original.child,&original.master,&root.spec.zone)
                .map_err(|_error|StateError::ProviderSelectionInvalid)?;
            let mut resources=fresh.versions().iter().filter(|(v,r)|
                VersionSizeKey::from(*v)==retained.provider_version && r.checksum.as_ref()==original.checksum && r.size==original.size);
            let Some((_,resource))=resources.next() else{return Err(StateError::ProviderSelectionAmbiguous)};
            if resources.next().is_some(){return Err(StateError::ProviderSelectionAmbiguous)}
            let fresh_url=resource.url.to_string();
            let (local,source):(String,String)=tx.query_row(
                "SELECT local_checksum,source_checksum FROM provider_active_destinations WHERE generation=?1 AND pass_key=?2 AND child=?3 AND version_size=?4 AND path=?5 AND admitted=1 AND verified_media=1",
                params![original.generation,original.pass_key,original.child,original.version_size,path],
                |r|Ok((r.get(0)?,r.get(1)?)))?;
            let (account_key,provider_key)=super::owner_keys(&tx)?;
            let library=root.spec.zone.get("zoneName").and_then(serde_json::Value::as_str).ok_or(StateError::ProviderSelectionInvalid)?;
            let expected=super::super::download_receipts::DownloadReceipt{
                account_key,provider_key,scope:root.spec.scope.clone(),library:library.to_owned(),
                child:original.child.clone(),master:original.master.clone(),state_id:original.state_id.clone(),
                origin_pass:original.pass_key.clone(),version_size:original.version_size.clone(),
                provider_checksum:original.checksum.clone(),provider_size:original.size,native_path:original.path.clone(),
                source_checksum:source,local_checksum:local,
            };
            let receipt_id=expected.id()?;
            let ticket=RefreshedDispatchTicket{id,proof,prior_attempts:prior,retry_delay:delay,capacity,
                owner:Some(owner),authority:Some(authority),original:Some(original),policy:Some(policy),route:Some(route),fresh_url:Some(fresh_url)};
            // This repeats all recorded lookup, compact owner, policy, exact
            // destination, source-backed receipt and held-file checks.
            finish_refreshed_success_tx(&tx,&ticket,&receipt_id,validate_file)?;
            tx.commit()?;
            Ok(true)
        }).await
    }
}
