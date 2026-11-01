//! Optional trusted-runtime observation attestations for the narrow date pass.
//!
//! This retains complete canonical record Values and causal identities, not the
//! original wire pages or independently verifiable page-inclusion proofs.
//! No live caller, migration, wait sharing, or checkpoint authority is wired.
use super::{
    ActiveGeneration, MAX_CHUNK_BYTES, NewRootPolicyPermit, SqliteStateDb, account, digest, encode,
    invalid, load_decision, load_generation, pass,
};
use crate::download::{CurrentPolicyEligibility, NewRootPolicy};
use crate::icloud::photos::asset::CausalRouteLease;
use crate::icloud::photos::session::SourceAuthority;
use crate::icloud::photos::source_causality::RouteWitness;
use crate::state::error::StateError;
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};

pub(crate) const DDL: &str = r"
CREATE TABLE provider_compact_root_policy (
 generation TEXT PRIMARY KEY REFERENCES provider_active_generations(id),
 preimage BLOB NOT NULL, snapshot BLOB NOT NULL, proof_hash TEXT NOT NULL,
 charged_bytes INTEGER NOT NULL CHECK(charged_bytes>0)
) WITHOUT ROWID;
CREATE TABLE provider_compact_observations (
 generation TEXT NOT NULL, pass_key TEXT NOT NULL, child TEXT NOT NULL,
 route_id TEXT NOT NULL UNIQUE, route_key BLOB NOT NULL, attestation BLOB NOT NULL,
 proof_hash TEXT NOT NULL, charged_bytes INTEGER NOT NULL CHECK(charged_bytes>0),
 PRIMARY KEY(generation,pass_key,child),
 FOREIGN KEY(generation,pass_key,child) REFERENCES provider_active_decisions(generation,pass_key,child)
) WITHOUT ROWID;
CREATE TRIGGER compact_policy_immutable BEFORE UPDATE ON provider_compact_root_policy BEGIN SELECT RAISE(ABORT,'retained compact policy'); END;
CREATE TRIGGER compact_observation_immutable BEFORE UPDATE ON provider_compact_observations BEGIN SELECT RAISE(ABORT,'retained compact observation'); END;
CREATE TRIGGER compact_policy_retain BEFORE DELETE ON provider_compact_root_policy BEGIN SELECT RAISE(ABORT,'retained compact policy'); END;
CREATE TRIGGER compact_observation_retain BEFORE DELETE ON provider_compact_observations BEGIN SELECT RAISE(ABORT,'retained compact observation'); END;
CREATE TRIGGER compact_policy_parent_retain BEFORE DELETE ON provider_active_generations WHEN EXISTS(SELECT 1 FROM provider_compact_root_policy WHERE generation=OLD.id) BEGIN SELECT RAISE(ABORT,'retained compact policy'); END;
CREATE TRIGGER compact_observation_parent_retain BEFORE DELETE ON provider_active_decisions WHEN EXISTS(SELECT 1 FROM provider_compact_observations WHERE generation=OLD.generation AND pass_key=OLD.pass_key AND child=OLD.child) BEGIN SELECT RAISE(ABORT,'retained compact observation'); END;
";

// A causal stage scope is an authenticated identity, not a byte-derived
// association. Compare the complete original scope before accepting a route.
fn exact_source_scope(source_scope: &str, root_scope: &str) -> bool {
    source_scope == root_scope
}

/// A policy marker loaded under authenticated state; private fields prevent a
/// caller from inventing an old-root marker from equal bytes.
pub(crate) struct StoredPolicyPreimage {
    generation: String,
    preimage: Vec<u8>,
    snapshot: Vec<u8>,
}
impl StoredPolicyPreimage {
    pub(crate) fn matches(
        &self,
        root: &ActiveGeneration,
        preimage: &[u8],
        snapshot: &[u8],
    ) -> bool {
        self.generation == root.id && self.preimage == preimage && self.snapshot == snapshot
    }
}

/// Pairs the state owner's DB-bound new-root permit with filter-owner policy.
/// Neither capability is cloneable or reconstructible from a historical root.
pub(crate) struct NewRootPolicyOffer {
    permit: NewRootPolicyPermit,
    policy: NewRootPolicy,
}
impl NewRootPolicyOffer {
    pub(crate) fn new(
        permit: NewRootPolicyPermit,
        policy: NewRootPolicy,
        root: &ActiveGeneration,
    ) -> Option<Self> {
        if !policy.matches_spec(root) {
            return None;
        }
        Some(Self { permit, policy })
    }
    /// Provisional marker for the filter owner; state must recheck the DB-bound
    /// permit inside the admission transaction before persisting anything.
    pub(crate) fn marker(&self, root: &ActiveGeneration) -> Option<StoredPolicyPreimage> {
        if !self.policy.matches_spec(root) {
            return None;
        }
        Some(StoredPolicyPreimage {
            generation: root.id.clone(),
            preimage: self.policy.preimage().to_vec(),
            snapshot: self.policy.snapshot().to_vec(),
        })
    }
    fn matches(&self, conn: &Connection, root: &ActiveGeneration) -> bool {
        self.permit.matches(conn, root) && self.policy.matches_spec(root)
    }
}

/// Only called from the first-observation optional SAVEPOINT. On any later
/// failure the SAVEPOINT is rolled back so an empty marker never pins a root.
fn insert_policy_for_attestation(
    conn: &Connection,
    root: &ActiveGeneration,
    policy: &StoredPolicyPreimage,
    capacity: u64,
) -> Result<(), StateError> {
    if conn.is_autocommit() || policy.generation != root.id {
        return invalid();
    }
    let preimage = policy.preimage.as_slice();
    let snapshot = policy.snapshot.as_slice();
    if preimage.len() > MAX_CHUNK_BYTES || snapshot.len() > MAX_CHUNK_BYTES {
        return Err(StateError::ProviderSelectionFull);
    }
    let hash = digest(&encode(&(
        "compact-root-policy-v1",
        &root.id,
        preimage,
        snapshot,
    ))?);
    let charged = u64::try_from(root.id.len() + preimage.len() + snapshot.len() + hash.len() + 128)
        .map_err(|_redacted_error| StateError::ProviderSelectionFull)?;
    super::charge_optional_evidence(conn, charged, capacity, 2)?;
    conn.execute("INSERT INTO provider_compact_root_policy(generation,preimage,snapshot,proof_hash,charged_bytes) VALUES(?1,?2,?3,?4,?5)",params![root.id,preimage,snapshot,hash,i64::try_from(charged).map_err(|_redacted_error|StateError::ProviderSelectionFull)?])?;
    Ok(())
}

/// Authenticate the existing row, preflight lengths, and recheck the exact
/// root fingerprint before providing the filter owner its private marker.
pub(super) fn load_policy_tx(
    conn: &Connection,
    owner: &account::AccountOwner,
    generation: &str,
) -> Result<Option<StoredPolicyPreimage>, StateError> {
    if conn.is_autocommit() {
        return invalid();
    }
    account::validate_authenticated(conn, owner)?;
    let root = load_generation(conn, generation)?;
    let sizes:Option<(i64,i64,i64)>=conn.query_row("SELECT length(preimage),length(snapshot),length(proof_hash) FROM provider_compact_root_policy WHERE generation=?1",[generation],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
    let Some((a, b, h)) = sizes else {
        return Ok(None);
    };
    if [a, b]
        .into_iter()
        .any(|n| usize::try_from(n).map_or(true, |n| n > MAX_CHUNK_BYTES))
        || h != 64
    {
        return invalid();
    }
    let (preimage, snapshot, hash): (Vec<u8>, Vec<u8>, String) = conn.query_row(
        "SELECT preimage,snapshot,proof_hash FROM provider_compact_root_policy WHERE generation=?1",
        [generation],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;
    let value: Value = serde_json::from_slice(&preimage)
        .map_err(|_redacted_error| StateError::ProviderSelectionInvalid)?;
    if digest(&encode(&(
        "compact-root-policy-v1",
        generation,
        preimage.as_slice(),
        snapshot.as_slice(),
    ))?) != hash
        || digest(&preimage) != root.spec.config_hash
        || value.get("profile") != Some(&root.spec.profile)
        || value.get("scope").and_then(Value::as_str) != Some(root.spec.scope.as_str())
        || value.get("zone") != Some(&root.spec.zone)
    {
        return invalid();
    }
    Ok(Some(StoredPolicyPreimage {
        generation: generation.to_owned(),
        preimage,
        snapshot,
    }))
}

impl SqliteStateDb {
    /// Return no marker for old roots or for roots whose optional policy
    /// evidence was never recorded. This cannot backfill historical state.
    pub(crate) async fn compact_root_policy(
        &self,
        owner: account::AccountOwner,
        generation: String,
    ) -> Result<Option<StoredPolicyPreimage>, StateError> {
        self.with_conn("loading compact root policy", move |conn| {
            let tx = conn.unchecked_transaction()?;
            let marker = load_policy_tx(&tx, &owner, &generation)?;
            tx.commit()?;
            Ok(marker)
        })
        .await
    }
}

/// Insert a compact attestation only inside the already authenticated
/// admission transaction. The actual OwnedMutexGuard-backed lease must be
/// moved into the enclosing blocking closure and held through commit/rollback;
/// checking `is_leased` on a detached candidate is not sufficient.
///
/// This row is a trusted runtime attestation, not stored wire-page inclusion.
pub(super) fn insert_compact_attestation_tx(
    conn: &Connection,
    owner: &account::AccountOwner,
    authority: &SourceAuthority,
    root: &ActiveGeneration,
    key: &str,
    child: &str,
    lease: &CausalRouteLease,
    eligibility: &CurrentPolicyEligibility,
    permit: Option<&NewRootPolicyOffer>,
    capacity: u64,
) -> Result<Option<String>, StateError> {
    if conn.is_autocommit() {
        return invalid();
    }
    conn.execute_batch("SAVEPOINT optional_compact_attestation")?;
    let result = insert_compact_attestation_inner(
        conn,
        owner,
        authority,
        root,
        key,
        child,
        lease,
        eligibility,
        permit,
        capacity,
    );
    match result {
        Ok(Some(id)) => {
            conn.execute_batch("RELEASE optional_compact_attestation")?;
            Ok(Some(id))
        }
        Ok(None) | Err(StateError::ProviderSelectionFull) => {
            conn.execute_batch(
                "ROLLBACK TO optional_compact_attestation; RELEASE optional_compact_attestation",
            )?;
            Ok(None)
        }
        Err(error) => {
            conn.execute_batch(
                "ROLLBACK TO optional_compact_attestation; RELEASE optional_compact_attestation",
            )?;
            Err(error)
        }
    }
}

fn insert_compact_attestation_inner(
    conn: &Connection,
    owner: &account::AccountOwner,
    authority: &SourceAuthority,
    root: &ActiveGeneration,
    key: &str,
    child: &str,
    lease: &CausalRouteLease,
    eligibility: &CurrentPolicyEligibility,
    permit: Option<&NewRootPolicyOffer>,
    capacity: u64,
) -> Result<Option<String>, StateError> {
    if conn.is_autocommit() {
        return invalid();
    }
    account::validate_authenticated(conn, owner)?;
    let current = load_generation(conn, &root.id)?;
    if current.spec != root.spec || root.spec.format != 2 {
        return invalid();
    }
    let decision = load_decision(conn, root, key, child)?;
    let Some(master) = decision.decision.master.as_deref() else {
        return Ok(None);
    };
    let Some(confirmation) = decision.decision.confirmation.as_deref() else {
        return Ok(None);
    };
    if !eligibility.matches(root, key, child, master, confirmation) {
        return Ok(None);
    }
    let (policy, needs_marker) = match load_policy_tx(conn, owner, &root.id)? {
        Some(policy) => (policy, false),
        None => match permit
            .filter(|permit| permit.matches(conn, root))
            .and_then(|permit| permit.marker(root))
        {
            Some(policy) => (policy, true),
            None => return Ok(None),
        },
    };
    if !policy.matches(
        root,
        eligibility.policy_preimage(),
        eligibility.resolved_snapshot(),
    ) {
        return Ok(None);
    }
    let pass_spec = pass(root, key)?;
    if pass_spec.get("kind").and_then(Value::as_str) != Some("Unfiled") {
        return Ok(None);
    }
    let Some(candidate) = lease.candidate() else {
        return Ok(None);
    };
    let (candidate_owner, zone, candidate_pass, witness, _, _, _, _) = candidate.source();
    if candidate_owner != owner
        || zone != &root.spec.zone
        || !exact_source_scope(candidate.source_scope(), &root.spec.scope)
        || candidate_pass != key
        || !matches!(witness, RouteWitness::DirectZone)
    {
        return Ok(None);
    }
    let Some((master_page, child_page)) = candidate.page_attestations() else {
        return Ok(None);
    };
    let (
        master_inv,
        master_ord,
        master_record,
        master_request,
        master_request_binding,
        master_authority,
    ) = master_page.fields();
    let (child_inv, child_ord, child_record, child_request, child_request_binding, child_authority) =
        child_page.fields();
    let scope: Value = serde_json::from_str(&root.spec.scope)
        .map_err(|_redacted_error| StateError::ProviderSelectionInvalid)?;
    let database = scope
        .get("database")
        .and_then(Value::as_str)
        .ok_or(StateError::ProviderSelectionInvalid)?;
    let Some(current_authority) = authority.binding(owner, database, "changes/zone") else {
        return Ok(None);
    };
    if !master_request.valid_changes_zone_request(&root.spec.zone)
        || !child_request.valid_changes_zone_request(&root.spec.zone)
        || current_authority != master_authority
        || current_authority != child_authority
        || !authority.validate_retained_request(
            owner,
            database,
            "changes/zone",
            master_request,
            &master_request_binding,
        )
        || !authority.validate_retained_request(
            owner,
            database,
            "changes/zone",
            child_request,
            &child_request_binding,
        )
    {
        return Ok(None);
    }
    if master_record.get("recordName").and_then(Value::as_str) != Some(master)
        || master_record.get("recordType").and_then(Value::as_str) != Some("CPLMaster")
        || child_record.get("recordName").and_then(Value::as_str) != Some(child)
        || child_record.get("recordType").and_then(Value::as_str) != Some("CPLAsset")
        || child_record
            .pointer("/fields/masterRef/value/recordName")
            .and_then(Value::as_str)
            != Some(master)
        || [master_record, child_record].into_iter().any(|r| {
            r.get("recordChangeTag")
                .and_then(Value::as_str)
                .is_none_or(|s| s.trim().is_empty())
        })
    {
        return Ok(None);
    }
    crate::icloud::photos::current_asset(confirmation, child, master, &root.spec.zone)
        .map_err(|_redacted_error| StateError::ProviderSelectionInvalid)?;
    let confirmed: Value = serde_json::from_slice(confirmation)
        .map_err(|_redacted_error| StateError::ProviderSelectionInvalid)?;
    let Some(records) = confirmed
        .get("records")
        .and_then(Value::as_array)
        .filter(|v| v.len() == 2)
    else {
        return Ok(None);
    };
    if records.iter().filter(|v| *v == master_record).count() != 1
        || records.iter().filter(|v| *v == child_record).count() != 1
    {
        return Ok(None);
    }
    // Bound the two complete canonical records before building the combined
    // attestation; original page sizes and in-flight permit are provider-owned.
    let master_bytes = encode(master_record)?;
    let child_bytes = encode(child_record)?;
    if master_bytes.len() > MAX_CHUNK_BYTES
        || child_bytes.len() > MAX_CHUNK_BYTES
        || master_bytes
            .len()
            .checked_add(child_bytes.len())
            .is_none_or(|n| n > MAX_CHUNK_BYTES)
    {
        return Ok(None);
    }
    let epoch = candidate.stage_epoch();
    let route_key = encode(&(
        "compact-route-v1",
        epoch.to_string(),
        master_inv.to_string(),
        master_ord,
        child_inv.to_string(),
        child_ord,
        key,
    ))?;
    let route_id = digest(&route_key);
    let body = encode(
        &json!({"version":1,"evidence":"trusted-runtime-canonical-records","generation":root.id,"scope":root.spec.scope,"zone":root.spec.zone,"pass":key,"child":child,"master":master,"epoch":epoch.to_string(),"master_invocation":master_inv.to_string(),"master_ordinal":master_ord,"master_record":master_record,"master_request_binding":data_encoding::HEXLOWER.encode(&master_request_binding),"child_invocation":child_inv.to_string(),"child_ordinal":child_ord,"child_record":child_record,"child_request_binding":data_encoding::HEXLOWER.encode(&child_request_binding),"authority_binding":data_encoding::HEXLOWER.encode(&current_authority),"confirmation_hash":digest(confirmation),"policy_preimage_hash":digest(eligibility.policy_preimage()),"resolved_policy_hash":digest(eligibility.resolved_snapshot())}),
    )?;
    if route_key.len() > MAX_CHUNK_BYTES || body.len() > MAX_CHUNK_BYTES {
        return Ok(None);
    }
    let hash = digest(&encode(&(
        "compact-attestation-v1",
        &root.id,
        key,
        child,
        &route_id,
        &route_key,
        &body,
    ))?);
    let sizes:Option<(i64,i64,i64,i64,i64,i64)>=conn.query_row("SELECT length(generation),length(pass_key),length(child),length(route_key),length(attestation),length(proof_hash) FROM provider_compact_observations WHERE route_id=?1",[&route_id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?))).optional()?;
    if let Some((g, p, c, k, b, h)) = sizes
        && ([g, p, c, k, b]
            .into_iter()
            .any(|n| usize::try_from(n).map_or(true, |n| n > MAX_CHUNK_BYTES))
            || h != 64)
    {
        return invalid();
    }
    type CompactObservationRow = (String, String, String, Vec<u8>, Vec<u8>, String);
    let duplicate:Option<CompactObservationRow>=conn.query_row("SELECT generation,pass_key,child,route_key,attestation,proof_hash FROM provider_compact_observations WHERE route_id=?1",[&route_id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?))).optional()?;
    if let Some((g, p, c, k, b, h)) = duplicate {
        if needs_marker {
            return invalid();
        }
        if g != root.id || p != key || c != child || k != route_key || b != body || h != hash {
            return invalid();
        }
        return Ok(Some(route_id));
    }
    if needs_marker {
        insert_policy_for_attestation(conn, root, &policy, capacity)?;
    }
    let charged = u64::try_from(
        root.id.len()
            + key.len()
            + child.len()
            + route_id.len()
            + route_key.len()
            + body.len()
            + hash.len()
            + 128,
    )
    .map_err(|_redacted_error| StateError::ProviderSelectionFull)?;
    super::charge_optional_evidence(conn, charged, capacity, 2)?;
    conn.execute("INSERT INTO provider_compact_observations(generation,pass_key,child,route_id,route_key,attestation,proof_hash,charged_bytes) VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",params![root.id,key,child,route_id,route_key,body,hash,i64::try_from(charged).map_err(|_redacted_error|StateError::ProviderSelectionFull)?])?;
    Ok(Some(route_id))
}

#[cfg(test)]
mod tests {
    use super::DDL;
    use rusqlite::Connection;

    #[test]
    fn route_identity_is_one_shot_and_captured_rows_are_retained() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys=OFF; CREATE TABLE provider_scheduler_revision(singleton INTEGER PRIMARY KEY,value INTEGER NOT NULL); INSERT INTO provider_scheduler_revision VALUES(1,0); CREATE TABLE provider_scheduler_scope_revisions(scope TEXT,format INTEGER,value INTEGER,PRIMARY KEY(scope,format)); INSERT INTO provider_scheduler_scope_revisions VALUES('scope',2,0); CREATE TABLE provider_active_generations(id TEXT PRIMARY KEY,scope TEXT); INSERT INTO provider_active_generations VALUES('g1','scope'),('g2','scope'); CREATE TABLE provider_active_decisions(generation TEXT,pass_key TEXT,child TEXT,PRIMARY KEY(generation,pass_key,child)); INSERT INTO provider_active_decisions VALUES('g1','p','c'),('g2','p','c');").unwrap();
        conn.execute_batch(DDL).unwrap();
        conn.execute_batch("INSERT INTO provider_compact_root_policy VALUES('g1',X'7B7D',X'7B7D','h',1); INSERT INTO provider_compact_observations VALUES('g1','p','c','route',X'01',X'02','h',1);").unwrap();
        assert!(conn.execute("INSERT INTO provider_compact_observations VALUES('g2','p','c','route',X'01',X'02','h',1)",[]).is_err());
        assert!(
            conn.execute("DELETE FROM provider_compact_observations", [])
                .is_err()
        );
        assert!(
            conn.execute(
                "DELETE FROM provider_active_decisions WHERE generation='g1'",
                []
            )
            .is_err()
        );
        assert!(
            conn.execute("DELETE FROM provider_active_generations WHERE id='g1'", [])
                .is_err()
        );
        assert_eq!(
            conn.query_row(
                "SELECT COUNT(*) FROM provider_compact_observations",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            1
        );
    }
}

struct ValidatedCompact {
    policy: StoredPolicyPreimage,
    confirmation: Vec<u8>,
}

/// Load a previously admitted runtime attestation. This verifies its durable
/// owner and contents but does not claim to reconstruct the original wire page.
fn validate_receipt_tx(
    conn: &Connection,
    owner: &account::AccountOwner,
    authority: &SourceAuthority,
    generation: &str,
    key: &str,
    child: &str,
    route_id: &str,
    eligibility: &CurrentPolicyEligibility,
) -> Result<Option<ValidatedCompact>, StateError> {
    account::validate_authenticated(conn, owner)?;
    let root = load_generation(conn, generation)?;
    let decision = load_decision(conn, &root, key, child)?;
    let Some(master) = decision.decision.master.as_deref() else {
        return Ok(None);
    };
    let Some(confirmation) = decision.decision.confirmation.as_deref() else {
        return Ok(None);
    };
    if !eligibility.matches(&root, key, child, master, confirmation) {
        return Ok(None);
    }
    let Some(policy) = load_policy_tx(conn, owner, generation)? else {
        return Ok(None);
    };
    if !policy.matches(
        &root,
        eligibility.policy_preimage(),
        eligibility.resolved_snapshot(),
    ) {
        return Ok(None);
    }
    let sizes:Option<(i64,i64,i64)>=conn.query_row("SELECT length(route_key),length(attestation),length(proof_hash) FROM provider_compact_observations WHERE generation=?1 AND pass_key=?2 AND child=?3 AND route_id=?4",params![generation,key,child,route_id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
    let Some((key_len, body_len, hash_len)) = sizes else {
        return Ok(None);
    };
    if [key_len, body_len]
        .into_iter()
        .any(|n| usize::try_from(n).map_or(true, |n| n > MAX_CHUNK_BYTES))
        || hash_len != 64
    {
        return invalid();
    }
    let (route_key,body,hash):(Vec<u8>,Vec<u8>,String)=conn.query_row("SELECT route_key,attestation,proof_hash FROM provider_compact_observations WHERE generation=?1 AND pass_key=?2 AND child=?3 AND route_id=?4",params![generation,key,child,route_id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?;
    if digest(&route_key) != route_id
        || digest(&encode(&(
            "compact-attestation-v1",
            generation,
            key,
            child,
            route_id,
            &route_key,
            &body,
        ))?) != hash
    {
        return invalid();
    }
    let value: Value = serde_json::from_slice(&body)
        .map_err(|_redacted_error| StateError::ProviderSelectionInvalid)?;
    if value.get("version").and_then(Value::as_u64) != Some(1)
        || value.get("evidence").and_then(Value::as_str)
            != Some("trusted-runtime-canonical-records")
        || value.get("generation").and_then(Value::as_str) != Some(generation)
        || value.get("scope").and_then(Value::as_str) != Some(root.spec.scope.as_str())
        || value.get("zone") != Some(&root.spec.zone)
        || value.get("pass").and_then(Value::as_str) != Some(key)
        || value.get("child").and_then(Value::as_str) != Some(child)
        || value.get("master").and_then(Value::as_str) != Some(master)
        || value.get("confirmation_hash").and_then(Value::as_str)
            != Some(digest(confirmation).as_str())
        || value.get("policy_preimage_hash").and_then(Value::as_str)
            != Some(digest(&policy.preimage).as_str())
        || value.get("resolved_policy_hash").and_then(Value::as_str)
            != Some(digest(&policy.snapshot).as_str())
    {
        return invalid();
    }
    let scope: Value = serde_json::from_str(&root.spec.scope)
        .map_err(|_redacted_error| StateError::ProviderSelectionInvalid)?;
    let database = scope
        .get("database")
        .and_then(Value::as_str)
        .ok_or(StateError::ProviderSelectionInvalid)?;
    let Some(binding) = authority.binding(owner, database, "changes/zone") else {
        return Ok(None);
    };
    if value.get("authority_binding").and_then(Value::as_str)
        != Some(data_encoding::HEXLOWER.encode(&binding).as_str())
    {
        return Ok(None);
    }
    let epoch = value
        .get("epoch")
        .and_then(Value::as_str)
        .ok_or(StateError::ProviderSelectionInvalid)?;
    let mi = value
        .get("master_invocation")
        .and_then(Value::as_str)
        .ok_or(StateError::ProviderSelectionInvalid)?;
    let ci = value
        .get("child_invocation")
        .and_then(Value::as_str)
        .ok_or(StateError::ProviderSelectionInvalid)?;
    let mo = value
        .get("master_ordinal")
        .and_then(Value::as_u64)
        .ok_or(StateError::ProviderSelectionInvalid)?;
    let co = value
        .get("child_ordinal")
        .and_then(Value::as_u64)
        .ok_or(StateError::ProviderSelectionInvalid)?;
    if [epoch, mi, ci]
        .into_iter()
        .any(|s| uuid::Uuid::parse_str(s).is_err())
        || encode(&("compact-route-v1", epoch, mi, mo, ci, co, key))? != route_key
    {
        return invalid();
    }
    for name in ["master_request_binding", "child_request_binding"] {
        if value
            .get(name)
            .and_then(Value::as_str)
            .is_none_or(|s| s.len() != 64 || data_encoding::HEXLOWER.decode(s.as_bytes()).is_err())
        {
            return invalid();
        }
    }
    let master_record = value
        .get("master_record")
        .ok_or(StateError::ProviderSelectionInvalid)?;
    let child_record = value
        .get("child_record")
        .ok_or(StateError::ProviderSelectionInvalid)?;
    if master_record.get("recordName").and_then(Value::as_str) != Some(master)
        || child_record.get("recordName").and_then(Value::as_str) != Some(child)
        || master_record.get("recordType").and_then(Value::as_str) != Some("CPLMaster")
        || child_record.get("recordType").and_then(Value::as_str) != Some("CPLAsset")
        || child_record
            .pointer("/fields/masterRef/value/recordName")
            .and_then(Value::as_str)
            != Some(master)
        || [master_record, child_record].into_iter().any(|r| {
            r.get("recordChangeTag")
                .and_then(Value::as_str)
                .is_none_or(|v| v.trim().is_empty())
        })
    {
        return invalid();
    }
    crate::icloud::photos::current_asset(confirmation, child, master, &root.spec.zone)
        .map_err(|_redacted_error| StateError::ProviderSelectionInvalid)?;
    let confirmed: Value = serde_json::from_slice(confirmation)
        .map_err(|_redacted_error| StateError::ProviderSelectionInvalid)?;
    let Some(records) = confirmed
        .get("records")
        .and_then(Value::as_array)
        .filter(|v| v.len() == 2)
    else {
        return invalid();
    };
    if records.iter().filter(|v| *v == master_record).count() != 1
        || records.iter().filter(|v| *v == child_record).count() != 1
    {
        return invalid();
    }
    Ok(Some(ValidatedCompact {
        policy,
        confirmation: confirmation.to_vec(),
    }))
}

/// Revalidate one retained receipt under the current owner, authority and
/// filter policy. This does not authorize a transfer, retire an epoch, or
/// establish resource, path, deadline, or sibling state.
pub(super) fn validate_one_compact_receipt_tx(
    conn: &Connection,
    owner: &account::AccountOwner,
    authority: &SourceAuthority,
    generation: &str,
    key: &str,
    child: &str,
    route_id: &str,
    eligibility: &CurrentPolicyEligibility,
) -> Result<bool, StateError> {
    if conn.is_autocommit() {
        return invalid();
    }
    if route_id.len() != 64 {
        return Ok(false);
    }
    Ok(validate_receipt_tx(
        conn,
        owner,
        authority,
        generation,
        key,
        child,
        route_id,
        eligibility,
    )?
    .is_some())
}

/// Compare both durable attestations and fresh filter-owner capabilities. This
/// is only the compact-evidence portion: the caller must still revalidate the
/// exact task/path/resource and live predecessor retry epoch at dispatch.
pub(super) fn revalidate_compact_pair_tx(
    conn: &Connection,
    owner: &account::AccountOwner,
    authority: &SourceAuthority,
    old: (&str, &str, &str, &str, &CurrentPolicyEligibility),
    new: (&str, &str, &str, &str, &CurrentPolicyEligibility),
) -> Result<bool, StateError> {
    if conn.is_autocommit() {
        return invalid();
    }
    if old.0 == new.0 || old.3 == new.3 || old.1 != new.1 || old.2 != new.2 {
        return Ok(false);
    }
    let Some(a) = validate_receipt_tx(conn, owner, authority, old.0, old.1, old.2, old.3, old.4)?
    else {
        return Ok(false);
    };
    let Some(b) = validate_receipt_tx(conn, owner, authority, new.0, new.1, new.2, new.3, new.4)?
    else {
        return Ok(false);
    };
    Ok(a.confirmation == b.confirmation
        && a.policy.preimage == b.policy.preimage
        && a.policy.snapshot == b.policy.snapshot)
}

#[cfg(test)]
mod source_scope_tests {
    use super::exact_source_scope;

    #[test]
    fn same_owner_and_zone_do_not_authorize_a_different_causal_scope() {
        // Owner and zone are held equal; only the causal scope differs.
        let owner = "same-owner";
        let zone = "same-zone";
        let candidate = (
            owner,
            zone,
            "{\"database\":\"private\",\"partition\":\"a\"}",
        );
        let root = (
            owner,
            zone,
            "{\"database\":\"private\",\"partition\":\"b\"}",
        );
        assert_eq!((candidate.0, candidate.1), (root.0, root.1));
        assert!(!exact_source_scope(candidate.2, root.2));
        assert!(exact_source_scope(root.2, root.2));
    }
}

/// Moves both real guards into the blocking admission closure. A permit is
/// consumed only after marker+observation commit has succeeded.
pub(crate) struct OwnedCompactAdmission {
    pub(super) lease: crate::icloud::photos::asset::CausalRouteLease,
    pub(super) offer: tokio::sync::OwnedMutexGuard<Option<NewRootPolicyOffer>>,
    pub(super) authority: SourceAuthority,
    pub(super) eligibility: CurrentPolicyEligibility,
}
impl OwnedCompactAdmission {
    pub(crate) fn new(
        lease: crate::icloud::photos::asset::CausalRouteLease,
        offer: tokio::sync::OwnedMutexGuard<Option<NewRootPolicyOffer>>,
        authority: SourceAuthority,
        eligibility: CurrentPolicyEligibility,
    ) -> Self {
        Self {
            lease,
            offer,
            authority,
            eligibility,
        }
    }
}
