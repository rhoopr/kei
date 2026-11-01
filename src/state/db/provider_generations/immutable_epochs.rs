//! REVIEW ONLY. Never infer owner or authorize transfer from these locators.
//! Caller must validate exact receipt, owner, policy, selected resource, live
//! child epoch, deadline and destination in the final IMMEDIATE transaction.
use super::{
    ActiveDecision, ActiveGeneration, MAX_CHUNK_BYTES, digest, encode,
    transfer_wait::{self, SelectionTransferKey},
};
use crate::{download::CurrentPolicyEligibility, state::error::StateError};
use rusqlite::Connection;

pub(crate) const DDL: &str = include_str!("immutable_epochs.sql");

pub(super) fn exact_locator(
    conn: &Connection,
    root: &ActiveGeneration,
    decision: &ActiveDecision,
    key: &SelectionTransferKey,
    policy: &CurrentPolicyEligibility,
) -> Result<Option<String>, StateError> {
    if conn.is_autocommit()
        || root.id != key.generation
        || !decision
            .decision
            .confirmation
            .as_deref()
            .is_some_and(|body| policy.matches(root, &key.pass_key, &key.child, &key.master, body))
        || !transfer_wait::matches_task(conn, root, decision, key)?
    {
        return Ok(None);
    }
    let (account, provider) = super::owner_keys(conn)?;
    let path = key.path.key()?;
    let confirmation = decision
        .decision
        .confirmation
        .as_deref()
        .ok_or(StateError::ProviderSelectionInvalid)?;
    Ok(Some(digest(&encode(&(
        "expired-transfer-obligation-v1",
        (
            account,
            provider,
            &root.spec.scope,
            &root.spec.zone,
            &root.spec.config_hash,
            &root.spec.profile,
            root.spec.metadata_enabled,
            root.spec.metadata_flags,
            digest(policy.resolved_snapshot()),
            digest(confirmation),
        ),
        (
            &key.pass_key,
            &key.child,
            &key.state_id,
            &key.master,
            &key.version_size,
            path,
            &key.checksum,
            key.size,
            &key.url,
        ),
    ))?)))
}

/// Untrusted identity hint only; route is a locator, not source authority.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct EpochHint {
    pub(crate) generation: String,
    pub(crate) pass_key: String,
    pub(crate) child: String,
    pub(crate) version_size: String,
    pub(crate) path: String,
    pub(crate) attempt: i64,
    pub(crate) deadline: i64,
    pub(crate) marker_hash: String,
    pub(crate) route_id: String,
}

/// Full same-transaction selection, with a preflight length query before
/// persisted text allocation. Any overflow holds this exact obligation only.
pub(super) fn future_hints(
    conn: &Connection,
    locator: &str,
    now: i64,
) -> Result<Vec<EpochHint>, StateError> {
    if conn.is_autocommit()
        || locator.len() != 64
        || !locator
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(StateError::ProviderSelectionInvalid);
    }
    let sizes:Vec<(i64,i64,i64,i64,i64,i64,i64)>=conn.prepare("SELECT length(CAST(generation AS BLOB)),length(CAST(pass_key AS BLOB)),length(CAST(child AS BLOB)),length(CAST(version_size AS BLOB)),length(CAST(path AS BLOB)),length(CAST(marker_hash AS BLOB)),length(CAST(route_id AS BLOB)) FROM provider_410_failure_epochs WHERE obligation_digest=?1 AND deadline>?2 ORDER BY deadline,generation,pass_key,child,version_size,path,attempt,marker_hash LIMIT 65")?.query_map((locator,now),|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?,r.get(6)?)))?.collect::<Result<_,_>>()?;
    if sizes.len() > 64 {
        return Err(StateError::ProviderSelectionAmbiguous);
    }
    let mut bytes = 0usize;
    for (a, b, c, d, e, f, g) in sizes {
        for n in [a, b, c, d, e, f, g] {
            bytes = bytes
                .checked_add(
                    usize::try_from(n)
                        .map_err(|_redacted_error| StateError::ProviderSelectionInvalid)?,
                )
                .ok_or(StateError::ProviderSelectionFull)?;
        }
        if bytes > MAX_CHUNK_BYTES {
            return Err(StateError::ProviderSelectionFull);
        }
    }
    let rows:Vec<EpochHint>=conn.prepare("SELECT generation,pass_key,child,version_size,path,attempt,deadline,marker_hash,route_id FROM provider_410_failure_epochs WHERE obligation_digest=?1 AND deadline>?2 ORDER BY deadline,generation,pass_key,child,version_size,path,attempt,marker_hash LIMIT 65")?.query_map((locator,now),|r|Ok(EpochHint{generation:r.get(0)?,pass_key:r.get(1)?,child:r.get(2)?,version_size:r.get(3)?,path:r.get(4)?,attempt:r.get(5)?,deadline:r.get(6)?,marker_hash:r.get(7)?,route_id:r.get(8)?}))?.collect::<Result<_,_>>()?;
    if rows.len() > 64 {
        return Err(StateError::ProviderSelectionAmbiguous);
    }
    Ok(rows)
}

/// Call only after this exact dispatched task receives HTTP 410, within the
/// same IMMEDIATE transaction that records its newly deferred child epoch.
/// Route and policy are fresh authenticated capabilities, not source IDs.
/// The optional proof savepoint never rolls back ordinary child deferral.
pub(super) fn insert_real_410_tx(
    conn: &Connection,
    owner: &super::account::AccountOwner,
    authority: &crate::icloud::photos::session::SourceAuthority,
    root: &ActiveGeneration,
    decision: &ActiveDecision,
    key: &SelectionTransferKey,
    policy: &CurrentPolicyEligibility,
    route: &str,
    attempt: i64,
    deadline: i64,
    capacity: u64,
) -> Result<bool, StateError> {
    if conn.is_autocommit() {
        return Err(StateError::ProviderSelectionInvalid);
    }
    conn.execute_batch("SAVEPOINT immutable_410_proof")?;
    let result = insert_real_410_inner(
        conn, owner, authority, root, decision, key, policy, route, attempt, deadline, capacity,
    );
    match result {
        Ok(true) => {
            conn.execute_batch("RELEASE immutable_410_proof")?;
            Ok(true)
        }
        Ok(false) => {
            conn.execute_batch("ROLLBACK TO immutable_410_proof; RELEASE immutable_410_proof")?;
            Ok(false)
        }
        Err(error) => {
            conn.execute_batch("ROLLBACK TO immutable_410_proof; RELEASE immutable_410_proof")?;
            Err(error)
        }
    }
}
fn insert_real_410_inner(
    conn: &Connection,
    owner: &super::account::AccountOwner,
    authority: &crate::icloud::photos::session::SourceAuthority,
    root: &ActiveGeneration,
    decision: &ActiveDecision,
    key: &SelectionTransferKey,
    policy: &CurrentPolicyEligibility,
    route: &str,
    attempt: i64,
    deadline: i64,
    capacity: u64,
) -> Result<bool, StateError> {
    use rusqlite::params;
    super::account::validate_authenticated(conn, owner)?;
    let path = key.path.key()?;
    if transfer_wait::dispatch_state(conn, root, decision, &key.version_size, &path)?.as_deref()
        != Some("dispatched")
    {
        return Ok(false);
    }
    let epoch:(i64,i64)=conn.query_row("SELECT attempts,next_retry_at FROM provider_active_decisions WHERE generation=?1 AND pass_key=?2 AND child=?3",params![root.id,key.pass_key,key.child],|r|Ok((r.get(0)?,r.get(1)?)))?;
    if attempt <= 0
        || deadline <= crate::state::db::retry_clock::now_for_connection(conn)
        || epoch != (attempt, deadline)
    {
        return Err(StateError::ProviderSelectionInvalid);
    }
    if !super::compact_attestation::validate_one_compact_receipt_tx(
        conn,
        owner,
        authority,
        &root.id,
        &key.pass_key,
        &key.child,
        route,
        policy,
    )? {
        return Ok(false);
    }
    let Some(locator) = exact_locator(conn, root, decision, key, policy)? else {
        return Ok(false);
    };
    let marker = digest(&encode(&(
        "immutable-expired-transfer-410-v1",
        key,
        attempt,
        deadline,
        route,
        &locator,
    ))?);
    let charged =
        u64::try_from(encode(&(key, attempt, deadline, route, &locator, &marker))?.len() + 128)
            .map_err(|_redacted_error| StateError::ProviderSelectionFull)?;
    super::charge_optional_evidence(conn, charged, capacity, 2)?;
    conn.execute("INSERT INTO provider_410_failure_epochs(obligation_digest,deadline,generation,pass_key,child,version_size,path,attempt,marker_hash,route_id,state_id,master,checksum,size_bytes,url,charged_bytes) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16)",params![locator,deadline,root.id,key.pass_key,key.child,key.version_size,path,attempt,marker,route,key.state_id,key.master,key.checksum,i64::try_from(key.size).map_err(|_redacted_error|StateError::ProviderSelectionInvalid)?,key.url,i64::try_from(charged).map_err(|_redacted_error|StateError::ProviderSelectionFull)?])?;
    Ok(true)
}

/// Revalidates a proposed owner pair at the final state boundary. `hint` is
/// only a discovery hint; all facts and the current wall clock are read again.
pub(super) fn exact_pair_waiting_tx(
    conn: &Connection,
    owner: &super::account::AccountOwner,
    authority: &crate::icloud::photos::session::SourceAuthority,
    target: &SelectionTransferKey,
    target_route: &str,
    target_policy: &CurrentPolicyEligibility,
    hint: &EpochHint,
    source_policy: &CurrentPolicyEligibility,
) -> Result<bool, StateError> {
    use rusqlite::{OptionalExtension, params};
    if conn.is_autocommit() {
        return Err(StateError::ProviderSelectionInvalid);
    }
    super::account::validate_authenticated(conn, owner)?;
    let now = crate::state::db::retry_clock::now_for_connection(conn);
    if hint.generation == target.generation
        || hint.pass_key != target.pass_key
        || hint.child != target.child
        || hint.version_size != target.version_size
        || hint.path != target.path.key()?
    {
        return Ok(false);
    }
    let new_root = super::load_generation(conn, &target.generation)?;
    let new_decision = super::load_decision(conn, &new_root, &target.pass_key, &target.child)?;
    let Some(locator) = exact_locator(conn, &new_root, &new_decision, target, target_policy)?
    else {
        return Ok(false);
    };
    let old_root = super::load_generation(conn, &hint.generation)?;
    let old_decision = super::load_decision(conn, &old_root, &hint.pass_key, &hint.child)?;
    let old = SelectionTransferKey {
        generation: hint.generation.clone(),
        pass_key: target.pass_key.clone(),
        child: target.child.clone(),
        state_id: target.state_id.clone(),
        master: target.master.clone(),
        version_size: target.version_size.clone(),
        path: target.path.clone(),
        checksum: target.checksum.clone(),
        size: target.size,
        url: target.url.clone(),
    };
    if exact_locator(conn, &old_root, &old_decision, &old, source_policy)?.as_deref()
        != Some(locator.as_str())
    {
        return Ok(false);
    }
    let marker = digest(&encode(&(
        "immutable-expired-transfer-410-v1",
        &old,
        hint.attempt,
        hint.deadline,
        &hint.route_id,
        &locator,
    ))?);
    if marker != hint.marker_hash {
        return Err(StateError::ProviderSelectionInvalid);
    }
    let present:bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM provider_410_failure_epochs WHERE obligation_digest=?1 AND deadline=?2 AND generation=?3 AND pass_key=?4 AND child=?5 AND version_size=?6 AND path=?7 AND attempt=?8 AND marker_hash=?9 AND route_id=?10 AND state_id=?11 AND master=?12 AND checksum=?13 AND size_bytes=?14 AND url=?15)",params![locator,hint.deadline,hint.generation,hint.pass_key,hint.child,hint.version_size,hint.path,hint.attempt,hint.marker_hash,hint.route_id,target.state_id,target.master,target.checksum,i64::try_from(target.size).map_err(|_redacted_error|StateError::ProviderSelectionInvalid)?,target.url],|r|r.get(0))?;
    if !present {
        return Ok(false);
    }
    let epoch:Option<(i64,i64)>=conn.query_row("SELECT attempts,next_retry_at FROM provider_active_decisions WHERE generation=?1 AND pass_key=?2 AND child=?3",params![hint.generation,hint.pass_key,hint.child],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
    if epoch != Some((hint.attempt, hint.deadline)) || hint.deadline <= now {
        return Ok(false);
    }
    super::compact_attestation::revalidate_compact_pair_tx(
        conn,
        owner,
        authority,
        (
            &hint.generation,
            &hint.pass_key,
            &hint.child,
            &hint.route_id,
            source_policy,
        ),
        (
            &target.generation,
            &target.pass_key,
            &target.child,
            target_route,
            target_policy,
        ),
    )
}

fn wait_proof(
    target_key_hash: &str,
    target_route: &str,
    hint: &EpochHint,
    binding: &(String, String, String, String, String),
) -> Result<String, StateError> {
    Ok(digest(&encode(&(
        "immutable-shared-410-wait-v2",
        target_key_hash,
        target_route,
        &hint.generation,
        &hint.pass_key,
        &hint.child,
        &hint.version_size,
        &hint.path,
        hint.attempt,
        hint.deadline,
        &hint.marker_hash,
        &hint.route_id,
        binding,
    ))?))
}
fn decision_binding(
    conn: &Connection,
    target: &SelectionTransferKey,
    hint: &EpochHint,
    policy: &CurrentPolicyEligibility,
) -> Result<(String, String, String, String, String), StateError> {
    use rusqlite::params;
    let (tm,ta):(String,String)=conn.query_row("SELECT manifest_hash,admission_hash FROM provider_active_decisions WHERE generation=?1 AND pass_key=?2 AND child=?3",params![target.generation,target.pass_key,target.child],|r|Ok((r.get(0)?,r.get(1)?)))?;
    let (sm,sa):(String,String)=conn.query_row("SELECT manifest_hash,admission_hash FROM provider_active_decisions WHERE generation=?1 AND pass_key=?2 AND child=?3",params![hint.generation,hint.pass_key,hint.child],|r|Ok((r.get(0)?,r.get(1)?)))?;
    if [&tm, &ta, &sm, &sa].iter().any(|s| s.len() != 64) {
        return Err(StateError::ProviderSelectionInvalid);
    }
    Ok((tm, ta, sm, sa, digest(policy.resolved_snapshot())))
}
/// Persist a shared wait without inventing any successor attempt. Candidate
/// ownership must be revalidated again immediately before physical dispatch.
pub(super) fn insert_shared_wait_tx(
    conn: &Connection,
    owner: &super::account::AccountOwner,
    authority: &crate::icloud::photos::session::SourceAuthority,
    target: &SelectionTransferKey,
    target_route: &str,
    target_policy: &CurrentPolicyEligibility,
    hint: &EpochHint,
    source_policy: &CurrentPolicyEligibility,
    capacity: u64,
) -> Result<bool, StateError> {
    use rusqlite::{OptionalExtension, params};
    if !exact_pair_waiting_tx(
        conn,
        owner,
        authority,
        target,
        target_route,
        target_policy,
        hint,
        source_policy,
    )? {
        return Ok(false);
    }
    let binding = decision_binding(conn, target, hint, target_policy)?;
    let target_hash = digest(&encode(target)?);
    let proof = wait_proof(&target_hash, target_route, hint, &binding)?;
    let path = target.path.key()?;
    let saved:Option<String>=conn.query_row("SELECT proof_hash FROM provider_410_shared_waits WHERE generation=?1 AND pass_key=?2 AND child=?3 AND version_size=?4 AND path=?5 AND source_generation=?6 AND source_attempt=?7 AND source_deadline=?8 AND source_marker=?9",params![target.generation,target.pass_key,target.child,target.version_size,path,hint.generation,hint.attempt,hint.deadline,hint.marker_hash],|r|r.get(0)).optional()?;
    if let Some(saved) = saved {
        return Ok(saved == proof);
    }
    let charged = u64::try_from(
        encode(&(
            target,
            target_route,
            &hint.generation,
            hint.attempt,
            hint.deadline,
            &hint.marker_hash,
            &hint.route_id,
            &binding,
            target_policy.policy_preimage(),
            target_policy.resolved_snapshot(),
            &proof,
        ))?
        .len()
            + 128,
    )
    .map_err(|_redacted_error| StateError::ProviderSelectionFull)?;
    super::charge_optional_evidence(conn, charged, capacity, 2)?;
    conn.execute("INSERT INTO provider_410_shared_waits(generation,pass_key,child,version_size,path,source_generation,source_attempt,source_deadline,source_marker,source_route,target_route,target_key_hash,target_manifest,target_admission,source_manifest,source_admission,policy_hash,policy_preimage,policy_snapshot,proof_hash,charged_bytes) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21)",params![target.generation,target.pass_key,target.child,target.version_size,path,hint.generation,hint.attempt,hint.deadline,hint.marker_hash,hint.route_id,target_route,target_hash,binding.0,binding.1,binding.2,binding.3,binding.4,target_policy.policy_preimage(),target_policy.resolved_snapshot(),proof,i64::try_from(charged).map_err(|_redacted_error|StateError::ProviderSelectionFull)?])?;
    Ok(true)
}
/// A retained link never authorizes publication; the receipt/path owner still
/// runs normal checks after a wait expires or its source becomes stale.
pub(super) fn revalidate_shared_wait_tx(
    conn: &Connection,
    owner: &super::account::AccountOwner,
    authority: &crate::icloud::photos::session::SourceAuthority,
    target: &SelectionTransferKey,
    target_route: &str,
    target_policy: &CurrentPolicyEligibility,
    hint: &EpochHint,
    source_policy: &CurrentPolicyEligibility,
) -> Result<bool, StateError> {
    use rusqlite::{OptionalExtension, params};
    if conn.is_autocommit() {
        return Err(StateError::ProviderSelectionInvalid);
    }
    let path = target.path.key()?;
    type WaitProofLengths = (i64, i64, i64, i64, i64, i64, i64, i64, i64);
    let sizes:Option<WaitProofLengths>=conn.query_row("SELECT length(source_route),length(target_route),length(proof_hash),length(target_key_hash),length(target_manifest),length(target_admission),length(source_manifest),length(source_admission),length(policy_hash) FROM provider_410_shared_waits WHERE generation=?1 AND pass_key=?2 AND child=?3 AND version_size=?4 AND path=?5 AND source_generation=?6 AND source_attempt=?7 AND source_deadline=?8 AND source_marker=?9",params![target.generation,target.pass_key,target.child,target.version_size,path,hint.generation,hint.attempt,hint.deadline,hint.marker_hash],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?,r.get(6)?,r.get(7)?,r.get(8)?))).optional()?;
    let Some(sizes) = sizes else { return Ok(false) };
    if sizes != (64, 64, 64, 64, 64, 64, 64, 64, 64) {
        return Err(StateError::ProviderSelectionInvalid);
    }
    type SavedWaitProof = (
        String,
        String,
        String,
        String,
        String,
        String,
        String,
        String,
        String,
    );
    let saved:Option<SavedWaitProof>=conn.query_row("SELECT source_route,target_route,proof_hash,target_key_hash,target_manifest,target_admission,source_manifest,source_admission,policy_hash FROM provider_410_shared_waits WHERE generation=?1 AND pass_key=?2 AND child=?3 AND version_size=?4 AND path=?5 AND source_generation=?6 AND source_attempt=?7 AND source_deadline=?8 AND source_marker=?9",params![target.generation,target.pass_key,target.child,target.version_size,path,hint.generation,hint.attempt,hint.deadline,hint.marker_hash],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?,r.get(6)?,r.get(7)?,r.get(8)?))).optional()?;
    let Some((source_route, saved_target_route, proof, saved_target_hash, tm, ta, sm, sa, ph)) =
        saved
    else {
        return Ok(false);
    };
    let saved_binding = (tm, ta, sm, sa, ph);
    if source_route != hint.route_id
        || saved_target_route != target_route
        || proof != wait_proof(&saved_target_hash, target_route, hint, &saved_binding)?
    {
        return Err(StateError::ProviderSelectionInvalid);
    }
    let binding = decision_binding(conn, target, hint, target_policy)?;
    if saved_binding != binding || saved_target_hash != digest(&encode(target)?) {
        return Ok(false);
    }
    exact_pair_waiting_tx(
        conn,
        owner,
        authority,
        target,
        target_route,
        target_policy,
        hint,
        source_policy,
    )
}

impl super::SqliteStateDb {
    /// Only the HTTP consumer that observed this task's actual 410 may call
    /// this. An already-future sibling retry is never attributed to this task.
    pub(crate) async fn record_real_expired_transfer(
        &self,
        owner: super::account::AccountOwner,
        authority: crate::icloud::photos::session::SourceAuthority,
        key: SelectionTransferKey,
        policy: CurrentPolicyEligibility,
        route: String,
        capacity: u64,
    ) -> Result<bool, StateError> {
        use rusqlite::{TransactionBehavior, params};
        self.with_conn_mut("recording authenticated HTTP 410",move|conn|{
            let tx=conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            super::account::validate_authenticated(&tx,&owner)?;
            let root=super::load_generation(&tx,&key.generation)?;
            let decision=super::load_decision(&tx,&root,&key.pass_key,&key.child)?;
            if !transfer_wait::matches_task(&tx,&root,&decision,&key)? {return Err(StateError::ProviderWorkConflict)}
            let path=key.path.key()?;
            if transfer_wait::dispatch_state(&tx,&root,&decision,&key.version_size,&path)?.as_deref()!=Some("dispatched"){return Err(StateError::ProviderWorkConflict)}
            let previous:i64=tx.query_row("SELECT next_retry_at FROM provider_active_decisions WHERE generation=?1 AND pass_key=?2 AND child=?3",params![key.generation,key.pass_key,key.child],|r|r.get(0))?;
            if previous>crate::state::db::retry_clock::now_for_connection(&tx){tx.commit()?;return Ok(false)}
            super::defer_attempt(&tx,&root,&key.pass_key,&key.child)?;
            let (attempt,deadline):(i64,i64)=tx.query_row("SELECT attempts,next_retry_at FROM provider_active_decisions WHERE generation=?1 AND pass_key=?2 AND child=?3",params![key.generation,key.pass_key,key.child],|r|Ok((r.get(0)?,r.get(1)?)))?;
            let fresh=super::load_decision(&tx,&root,&key.pass_key,&key.child)?;
            let recorded=match insert_real_410_tx(&tx,&owner,&authority,&root,&fresh,&key,&policy,&route,attempt,deadline,capacity){
                Ok(recorded)=>recorded,
                // The optional proof savepoint is already rolled back. Commit
                // real child retry debt, then return Full so the exact source
                // checkpoint remains held until proof can be safely recorded.
                Err(StateError::ProviderSelectionFull)=>{tx.commit()?;return Err(StateError::ProviderSelectionFull)},
                Err(error)=>return Err(error),
            };
            tx.commit()?;
            // A newly observed 410 without an authenticated compact epoch must
            // retain its actual retry debt but cannot safely advance the source
            // checkpoint as though the work were shareable. The early return
            // above is different: an existing future child deadline belongs to
            // prior work and must not invent an attempt for this destination.
            if !recorded { return Err(StateError::ProviderSelectionAmbiguous); }
            #[cfg(all(test,target_os="linux"))]
            crate::test_helpers::process_death_point("selection-expired-transfer-recorded");
            Ok(true)
        }).await
    }
}

impl super::SqliteStateDb {
    /// Return bounded non-authoritative hints and the moved target policy. The
    /// download owner must mint each predecessor policy and the eventual
    /// admission or dispatch transaction rechecks all facts and current time.
    pub(crate) async fn discover_410_wait_candidates(
        &self,
        owner: super::account::AccountOwner,
        key: SelectionTransferKey,
        policy: CurrentPolicyEligibility,
    ) -> Result<(CurrentPolicyEligibility, Vec<EpochHint>), StateError> {
        use rusqlite::TransactionBehavior;
        self.with_conn_mut("discovering authenticated 410 candidates", move |conn| {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            super::account::validate_authenticated(&tx, &owner)?;
            let root = super::load_generation(&tx, &key.generation)?;
            let decision = super::load_decision(&tx, &root, &key.pass_key, &key.child)?;
            let Some(locator) = exact_locator(&tx, &root, &decision, &key, &policy)? else {
                tx.commit()?;
                return Ok((policy, Vec::new()));
            };
            let rows = future_hints(
                &tx,
                &locator,
                crate::state::db::retry_clock::now_for_connection(&tx),
            )?;
            tx.commit()?;
            Ok((policy, rows))
        })
        .await
    }
}

/// Within one already-owned decision, prove that this exact component created
/// the child's future epoch. This never certifies a different generation.
pub(super) fn same_generation_failed_tx(
    conn: &Connection,
    root: &ActiveGeneration,
    decision: &ActiveDecision,
    dest: &crate::state::db::provider_selection::SelectionDestination,
    now: i64,
) -> Result<bool, StateError> {
    use rusqlite::{OptionalExtension, params};
    let (attempt,deadline):(i64,i64)=conn.query_row("SELECT attempts,next_retry_at FROM provider_active_decisions WHERE generation=?1 AND pass_key=?2 AND child=?3",params![root.id,decision.decision.pass_key,decision.decision.child],|r|Ok((r.get(0)?,r.get(1)?)))?;
    if deadline <= now || attempt <= 0 {
        return Ok(false);
    }
    let path = dest.path.key()?;
    let sizes:Option<(i64,i64,i64,i64)>=conn.query_row("SELECT length(CAST(url AS BLOB)),length(CAST(route_id AS BLOB)),length(CAST(obligation_digest AS BLOB)),length(CAST(marker_hash AS BLOB)) FROM provider_410_failure_epochs WHERE generation=?1 AND pass_key=?2 AND child=?3 AND version_size=?4 AND path=?5 AND attempt=?6 AND deadline=?7 LIMIT 1",params![root.id,decision.decision.pass_key,decision.decision.child,dest.version_size,path,attempt,deadline],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).optional()?;
    let Some((url_size, route_size, locator_size, marker_size)) = sizes else {
        return Ok(false);
    };
    if usize::try_from(url_size).map_or(true, |n| n > MAX_CHUNK_BYTES)
        || route_size != 64
        || locator_size != 64
        || marker_size != 64
    {
        return Err(StateError::ProviderSelectionInvalid);
    }
    let row:Option<(String,String,String,String)>=conn.query_row("SELECT url,route_id,obligation_digest,marker_hash FROM provider_410_failure_epochs WHERE generation=?1 AND pass_key=?2 AND child=?3 AND version_size=?4 AND path=?5 AND attempt=?6 AND deadline=?7 LIMIT 1",params![root.id,decision.decision.pass_key,decision.decision.child,dest.version_size,path,attempt,deadline],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).optional()?;
    let Some((url, route, locator, marker)) = row else {
        return Err(StateError::ProviderSelectionInvalid);
    };
    let Some(master) = decision.decision.master.as_deref() else {
        return Err(StateError::ProviderSelectionInvalid);
    };
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
    if !transfer_wait::matches_task(conn, root, decision, &key)? {
        return Ok(false);
    }
    if marker
        != digest(&encode(&(
            "immutable-expired-transfer-410-v1",
            &key,
            attempt,
            deadline,
            &route,
            &locator,
        ))?)
    {
        return Err(StateError::ProviderSelectionInvalid);
    }
    Ok(true)
}

impl super::SqliteStateDb {
    /// After compact admission, record a wait for a fully revalidated pair.
    /// The caller retains its normal checkpoint/dispatch guard until completion.
    pub(crate) async fn record_immutable_shared_wait(
        &self,
        owner: super::account::AccountOwner,
        authority: crate::icloud::photos::session::SourceAuthority,
        target: SelectionTransferKey,
        target_route: String,
        target_policy: CurrentPolicyEligibility,
        hint: EpochHint,
        source_policy: CurrentPolicyEligibility,
        capacity: u64,
    ) -> Result<bool, StateError> {
        use rusqlite::TransactionBehavior;
        self.with_conn_mut("recording immutable shared wait", move |conn| {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let waiting = insert_shared_wait_tx(
                &tx,
                &owner,
                &authority,
                &target,
                &target_route,
                &target_policy,
                &hint,
                &source_policy,
                capacity,
            )?;
            tx.commit()?;
            Ok(waiting)
        })
        .await
    }
    /// Must run under the existing destination guard after current planning and
    /// immediately before dispatch. False is not publication authorization.
    pub(crate) async fn revalidate_immutable_shared_wait(
        &self,
        owner: super::account::AccountOwner,
        authority: crate::icloud::photos::session::SourceAuthority,
        target: SelectionTransferKey,
        target_route: String,
        target_policy: CurrentPolicyEligibility,
        hint: EpochHint,
        source_policy: CurrentPolicyEligibility,
    ) -> Result<bool, StateError> {
        use rusqlite::TransactionBehavior;
        self.with_conn_mut("revalidating immutable shared wait", move |conn| {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let waiting = revalidate_shared_wait_tx(
                &tx,
                &owner,
                &authority,
                &target,
                &target_route,
                &target_policy,
                &hint,
                &source_policy,
            )?;
            tx.commit()?;
            Ok(waiting)
        })
        .await
    }
}

/// Snapshot of actual admitted pending components for pre-confirm planning.
/// It is not ownership, publication, or transfer authorization.
pub(crate) struct PendingComponents {
    pub(crate) media: Vec<crate::state::db::provider_selection::SelectionDestination>,
    pub(crate) ready_metadata: bool,
    pub(crate) unadmitted: bool,
    pub(crate) decision: ActiveDecision,
}
impl super::SqliteStateDb {
    pub(crate) async fn pending_410_components(
        &self,
        owner: super::account::AccountOwner,
        generation: String,
        pass: String,
        child: String,
    ) -> Result<PendingComponents, StateError> {
        use rusqlite::{TransactionBehavior, params};
        self.with_conn_mut("reading owned pending transfer components",move|conn|{
            let tx=conn.transaction_with_behavior(TransactionBehavior::Deferred)?;
            super::account::validate_authenticated(&tx,&owner)?;
            let root=super::load_generation(&tx,&generation)?;
            let decision=super::load_decision(&tx,&root,&pass,&child)?;
            let mut media=Vec::new();let mut ready_metadata=false;let mut unadmitted=false;
            for dest in &decision.decision.destinations {
                let path=dest.path.key()?;
                let (admitted,verified_media,verified_metadata):(bool,bool,bool)=tx.query_row("SELECT admitted,verified_media,verified_metadata FROM provider_active_destinations WHERE generation=?1 AND pass_key=?2 AND child=?3 AND version_size=?4 AND path=?5",params![generation,pass,child,dest.version_size,path],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?;
                if !admitted {unadmitted=true}
                if admitted && !verified_media {media.push(dest.clone())}
                if admitted && verified_media && root.spec.metadata_enabled && !verified_metadata {ready_metadata=true}
            }
            tx.commit()?;Ok(PendingComponents{media,ready_metadata,unadmitted,decision})
        }).await
    }
}

/// Authenticated retained inputs for current filter/planner evaluation. They
/// are only a hint: final dispatch repeats full owner and receipt validation.
pub(crate) struct EpochContext {
    pub(crate) root: ActiveGeneration,
    pub(crate) decision: ActiveDecision,
    pub(crate) policy: super::compact_attestation::StoredPolicyPreimage,
}
impl super::SqliteStateDb {
    pub(crate) async fn current_410_epoch_context(
        &self,
        owner: super::account::AccountOwner,
        hint: EpochHint,
    ) -> Result<(EpochHint, Option<EpochContext>), StateError> {
        use rusqlite::{TransactionBehavior, params};
        self.with_conn_mut("loading retained retry owner",move|conn|{
            let tx=conn.transaction_with_behavior(TransactionBehavior::Deferred)?;
            super::account::validate_authenticated(&tx,&owner)?;
            if hint.deadline<=crate::state::db::retry_clock::now_for_connection(&tx){tx.commit()?;return Ok((hint,None))}
            let exists:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM provider_410_failure_epochs e JOIN provider_active_generations g ON g.id=e.generation WHERE e.generation=?1 AND e.pass_key=?2 AND e.child=?3 AND e.version_size=?4 AND e.path=?5 AND e.attempt=?6 AND e.deadline=?7 AND e.marker_hash=?8 AND e.route_id=?9)",params![hint.generation,hint.pass_key,hint.child,hint.version_size,hint.path,hint.attempt,hint.deadline,hint.marker_hash,hint.route_id],|r|r.get(0))?;
            if !exists {tx.commit()?;return Ok((hint,None))}
            let root=super::load_generation(&tx,&hint.generation)?;
            let decision=super::load_decision(&tx,&root,&hint.pass_key,&hint.child)?;
            let policy=super::compact_attestation::load_policy_tx(&tx,&owner,&root.id)?;
            let context=policy.map(|policy|EpochContext{root,decision,policy});
            tx.commit()?;Ok((hint,context))
        }).await
    }
}

impl super::SqliteStateDb {
    /// Authenticated lookup hint, never evidence by itself. Receipt validation
    /// always reopens and verifies the full retained route in the final tx.
    pub(crate) async fn compact_410_route_hint(
        &self,
        owner: super::account::AccountOwner,
        generation: String,
        pass: String,
        child: String,
    ) -> Result<Option<String>, StateError> {
        use rusqlite::{OptionalExtension, TransactionBehavior, params};
        self.with_conn_mut("loading compact retry route hint",move|conn|{
            let tx=conn.transaction_with_behavior(TransactionBehavior::Deferred)?;
            super::account::validate_authenticated(&tx,&owner)?;
            let root=super::load_generation(&tx,&generation)?;
            super::load_decision(&tx,&root,&pass,&child)?;
            let size:Option<i64>=tx.query_row("SELECT length(CAST(route_id AS BLOB)) FROM provider_compact_observations WHERE generation=?1 AND pass_key=?2 AND child=?3",params![generation,pass,child],|r|r.get(0)).optional()?;
            let Some(size)=size else {tx.commit()?;return Ok(None)};
            if size!=64 {return Err(StateError::ProviderSelectionInvalid)}
            let route:String=tx.query_row("SELECT route_id FROM provider_compact_observations WHERE generation=?1 AND pass_key=?2 AND child=?3",params![generation,pass,child],|r|r.get(0))?;
            tx.commit()?;Ok(Some(route))
        }).await
    }
}

/// Owned, freshly minted planning inputs. The fields are not transferable
/// publication authority; every part is checked again in one state snapshot.
pub(crate) struct PreconfirmWaitEvidence {
    pub(crate) target: SelectionTransferKey,
    pub(crate) target_route: String,
    pub(crate) target_policy: CurrentPolicyEligibility,
    pub(crate) hint: EpochHint,
    pub(crate) source_policy: CurrentPolicyEligibility,
}
impl super::SqliteStateDb {
    pub(crate) async fn all_410_components_waiting(
        &self,
        owner: super::account::AccountOwner,
        authority: crate::icloud::photos::session::SourceAuthority,
        generation: String,
        pass: String,
        child: String,
        evidence: Vec<PreconfirmWaitEvidence>,
    ) -> Result<bool, StateError> {
        use rusqlite::{TransactionBehavior, params};
        self.with_conn_mut("revalidating all pending shared waits",move|conn|{
            let tx=conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            super::account::validate_authenticated(&tx,&owner)?;
            let root=super::load_generation(&tx,&generation)?;
            let decision=super::load_decision(&tx,&root,&pass,&child)?;
            let mut count=0usize;
            for dest in &decision.decision.destinations {
                let path=dest.path.key()?;
                let (admitted,verified_media,verified_metadata):(bool,bool,bool)=tx.query_row("SELECT admitted,verified_media,verified_metadata FROM provider_active_destinations WHERE generation=?1 AND pass_key=?2 AND child=?3 AND version_size=?4 AND path=?5",params![generation,pass,child,dest.version_size,path],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?;
                if !admitted || (verified_media && root.spec.metadata_enabled && !verified_metadata) {tx.commit()?;return Ok(false)}
                if verified_media {continue}
                count+=1;
                let mut matching=evidence.iter().filter(|e|e.target.generation==generation && e.target.pass_key==pass && e.target.child==child && e.target.version_size==dest.version_size && e.target.path==dest.path && e.target.checksum==dest.checksum && e.target.size==dest.size);
                let Some(e)=matching.next() else{tx.commit()?;return Ok(false)};
                if matching.next().is_some() || !revalidate_shared_wait_tx(&tx,&owner,&authority,&e.target,&e.target_route,&e.target_policy,&e.hint,&e.source_policy)? {tx.commit()?;return Ok(false)}
            }
            if count==0 || count!=evidence.len() {tx.commit()?;return Ok(false)}
            let final_now=crate::state::db::retry_clock::now_for_connection(&tx);
            if evidence.iter().any(|e|e.hint.deadline<=final_now){tx.commit()?;return Ok(false)}
            tx.commit()?;Ok(true)
        }).await
    }
}
