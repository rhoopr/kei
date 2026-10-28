//! Compact observation proofs. Open obligations remain in the existing ledger.
use super::{
    ActiveGeneration, GenerationSpec, SqliteStateDb, account, digest, encode, guard_library_scope,
    load_decision, owner_keys, validate_spec,
};
use crate::state::error::StateError;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;

const ACTIVE: &str = "observed_selection:v27:";
const STAGED: &str = "observed_selection_staged:v27:";

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Plan {
    scope: String,
    config: String,
    profile: Value,
    metadata: bool,
    flags: u8,
}
impl Plan {
    fn of(spec: &GenerationSpec) -> Self {
        Self {
            scope: spec.scope.clone(),
            config: spec.config_hash.clone(),
            profile: spec.profile.clone(),
            metadata: spec.metadata_enabled,
            flags: spec.metadata_flags,
        }
    }
    fn selector(&self) -> Option<Value> {
        self.profile
            .pointer("/selector_binding/scope")
            .cloned()
            .or_else(|| serde_json::from_str(&self.scope).ok())
    }
}
#[derive(Clone, Serialize)]
pub(crate) struct AdmissionReceipt {
    account: String,
    provider: String,
    plan: Plan,
    policy: [Option<String>; 2],
    // A fresh empty delta has no admitted items; it cannot establish a baseline.
    seal: Option<String>,
}
impl AdmissionReceipt {
    pub(super) fn sealed(
        conn: &Connection,
        root: &ActiveGeneration,
        seal: String,
    ) -> Result<Option<Self>, StateError> {
        if root.spec.format != 2
            || !root.checkpoint_ready
            || root.checkpoint_veto.is_some()
            || root
                .spec
                .profile
                .get("current_download_metadata")
                .and_then(Value::as_u64)
                != Some(1)
        {
            return Ok(None);
        }
        let keys: Vec<(String, String)> = conn
            .prepare("SELECT pass_key,child FROM provider_active_decisions WHERE generation=?1")?
            .query_map([&root.id], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<Result<_, _>>()?;
        for (pass, child) in keys {
            load_decision(conn, root, &pass, &child)?;
        }
        let (account, provider) = owner_keys(conn)?;
        Ok(Some(Self {
            account,
            provider,
            plan: Plan::of(&root.spec),
            policy: policy(conn)?,
            seal: Some(seal),
        }))
    }
}
impl SqliteStateDb {
    /// Read-only selection check used to resume a completed zone while another
    /// zone still prevents global configuration promotion. The checkpoint
    /// transaction independently rechecks the same plans, policy and cursor.
    pub(crate) async fn current_observation_matches(
        &self,
        owner: account::AccountOwner,
        token_key: String,
        specs: Vec<GenerationSpec>,
    ) -> Result<bool, StateError> {
        self.with_conn("checking current observation selection", move |conn| {
            let tx = conn.unchecked_transaction()?;
            account::validate_authenticated(&tx, &owner)?;
            if specs.is_empty() || specs.iter().any(|spec| spec.format != 2) {
                return Ok(false);
            }
            let mut plans = specs.iter().map(Plan::of).collect::<Vec<_>>();
            plans.sort_by(|a, b| a.scope.cmp(&b.scope));
            if plans
                .windows(2)
                .any(|pair| matches!(pair, [left, right] if left.scope == right.scope))
            {
                return Ok(false);
            }
            let Some(key) = descriptor_key(&token_key) else {
                return Ok(false);
            };
            let Some(token) = metadata(&tx, &token_key)? else {
                return Ok(false);
            };
            Ok(valid(&tx, &key, &token)?.is_some_and(|saved| saved.plans == plans))
        })
        .await
    }

    /// Only the current delta producer uses this after routing an empty batch.
    /// It proves no new admission was required, never a complete inventory.
    pub(crate) async fn empty_observation_admission(
        &self,
        owner: account::AccountOwner,
        spec: GenerationSpec,
    ) -> Result<AdmissionReceipt, StateError> {
        self.with_conn("validating empty observation scope", move |conn| {
            account::validate_authenticated(conn, &owner)?;
            validate_spec(&spec)?;
            super::selector_binding::validate_owner(conn, &spec)?;
            guard_library_scope(
                conn,
                &spec.scope,
                spec.zone
                    .get("zoneName")
                    .and_then(Value::as_str)
                    .ok_or(StateError::ProviderSelectionInvalid)?,
            )?;
            let (account, provider) = owner_keys(conn)?;
            Ok(AdmissionReceipt {
                account,
                provider,
                plan: Plan::of(&spec),
                policy: policy(conn)?,
                seal: None,
            })
        })
        .await
    }
}

#[derive(Clone, PartialEq, Eq)]
enum Kind {
    Inventory,
    Delivery,
    Delta { prior: String, next: String },
    Baseline { prior: String, next: String },
}
#[derive(Clone)]
pub(crate) struct ObservationProof {
    base: Vec<AdmissionReceipt>,
    supplemental: Vec<AdmissionReceipt>,
    kind: Kind,
    observed_at: i64,
    fresh_inventory: bool,
}
impl std::fmt::Debug for ObservationProof {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ObservationProof(<redacted>)")
    }
}
impl ObservationProof {
    pub(crate) fn inventory(receipts: Vec<AdmissionReceipt>) -> Option<Self> {
        if receipts.is_empty() || receipts.iter().any(|r| r.seal.is_none()) {
            return None;
        }
        Some(Self {
            base: receipts,
            supplemental: Vec::new(),
            kind: Kind::Inventory,
            observed_at: chrono::Utc::now().timestamp(),
            fresh_inventory: true,
        })
    }
    pub(crate) fn delivery(receipt: AdmissionReceipt) -> Self {
        Self {
            base: vec![receipt],
            supplemental: Vec::new(),
            kind: Kind::Delivery,
            observed_at: chrono::Utc::now().timestamp(),
            fresh_inventory: false,
        }
    }
    pub(crate) fn combine_sources(mut self, other: Self) -> Option<Self> {
        if self.kind != Kind::Delivery || other.kind != Kind::Delivery {
            return None;
        }
        self.base.extend(other.base);
        self.supplemental.extend(other.supplemental);
        Some(self)
    }
    pub(crate) fn with_smart(mut self, other: Self) -> Option<Self> {
        if other.kind != Kind::Inventory {
            return None;
        }
        for receipt in &other.base {
            let parent = self.base.iter().find(|r| {
                r.plan.scope == receipt.plan.scope && r.plan.selector() == receipt.plan.selector()
            })?;
            let passes = parent.plan.profile.get("passes")?.as_array()?;
            if !receipt
                .plan
                .profile
                .get("passes")?
                .as_array()?
                .iter()
                .all(|p| passes.contains(p))
            {
                return None;
            }
        }
        self.supplemental.extend(other.base);
        self.supplemental.extend(other.supplemental);
        Some(self)
    }
    pub(crate) fn delta(mut self, prior: &str, next: &str) -> Option<Self> {
        if !matches!(self.kind, Kind::Delivery | Kind::Inventory)
            || prior.is_empty()
            || next.is_empty()
        {
            return None;
        }
        self.kind = Kind::Delta {
            prior: digest(prior.as_bytes()),
            next: digest(next.as_bytes()),
        };
        self.observed_at = chrono::Utc::now().timestamp();
        Some(self)
    }
    pub(crate) fn restarted_inventory(mut self) -> Option<Self> {
        if !self.fresh_inventory {
            return None;
        }
        self.kind = Kind::Inventory;
        Some(self)
    }
    pub(crate) fn bridge(self, mut delta: Self, boundary: &str) -> Option<Self> {
        if self.kind != Kind::Inventory || self.plans()? != delta.plans()? {
            return None;
        }
        let Kind::Delta { prior, next } = &delta.kind else {
            return None;
        };
        if *prior != digest(boundary.as_bytes()) {
            return None;
        }
        delta.kind = Kind::Baseline {
            prior: prior.clone(),
            next: next.clone(),
        };
        delta.supplemental.extend(self.base);
        delta.supplemental.extend(self.supplemental);
        Some(delta)
    }
    fn plans(&self) -> Option<Vec<Plan>> {
        let first = self.base.first()?;
        let selector = first.plan.selector()?;
        if self.base.iter().chain(&self.supplemental).any(|r| {
            r.policy != first.policy
                || r.account != first.account
                || r.provider != first.provider
                || r.plan.selector() != Some(selector.clone())
        }) {
            return None;
        }
        let mut plans = self.base.iter().map(|r| r.plan.clone()).collect::<Vec<_>>();
        plans.sort_by(|a, b| a.scope.cmp(&b.scope));
        if plans
            .windows(2)
            .any(|p| matches!(p, [left, right] if left.scope == right.scope))
        {
            return None;
        }
        Some(plans)
    }

    /// A fresh full inventory may authorize skipping only the duplicate
    /// recent inventory when it matches the current authenticated scope,
    /// selection plans, owner, and provider policy.
    fn matches_current_inventory(
        &self,
        conn: &Connection,
        specs: &[GenerationSpec],
    ) -> Result<bool, StateError> {
        if self.kind != Kind::Inventory || !self.fresh_inventory {
            return Ok(false);
        }
        let Some(first) = self.base.first() else {
            return Ok(false);
        };
        if owner_keys(conn)? != (first.account.clone(), first.provider.clone())
            || first.policy != policy(conn)?
        {
            return Ok(false);
        }
        let Some(plans) = self.plans() else {
            return Ok(false);
        };
        let mut expected = specs.iter().map(Plan::of).collect::<Vec<_>>();
        expected.sort_by(|left, right| left.scope.cmp(&right.scope));
        Ok(plans == expected)
    }
}

impl SqliteStateDb {
    /// Read-only validation of a transient full-inventory proof before the
    /// incremental bridge may omit its duplicate recent inventory.
    pub(crate) async fn inventory_proof_matches(
        &self,
        owner: account::AccountOwner,
        proof: ObservationProof,
        specs: Vec<GenerationSpec>,
    ) -> Result<bool, StateError> {
        self.with_conn("validating prior inventory proof", move |conn| {
            let Ok((account, provider)) = owner.authenticated_keys() else {
                return Ok(false);
            };
            if owner_keys(conn)? != (account.to_owned(), provider.to_owned()) {
                return Ok(false);
            }
            proof.matches_current_inventory(conn, &specs)
        })
        .await
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct Descriptor {
    version: u8,
    account: String,
    provider: String,
    library: String,
    plans: Vec<Plan>,
    cursor_hash: String,
    observed_at: i64,
    policy: [Option<String>; 2],
    admission_hash: String,
    proof_hash: String,
}
fn metadata(conn: &Connection, key: &str) -> Result<Option<String>, StateError> {
    Ok(conn
        .query_row("SELECT value FROM metadata WHERE key=?1", [key], |r| {
            r.get(0)
        })
        .optional()?)
}
pub(in crate::state::db) fn policy(conn: &Connection) -> Result<[Option<String>; 2], StateError> {
    Ok([
        metadata(conn, crate::sync_cycle::PENDING_ENUM_CONFIG_HASH_KEY)?
            .or(metadata(conn, crate::sync_cycle::ENUM_CONFIG_HASH_KEY)?),
        metadata(conn, crate::sync_cycle::PENDING_DOWNLOAD_CONFIG_HASH_KEY)?
            .or(metadata(conn, crate::download::DOWNLOAD_CONFIG_HASH_KEY)?),
    ])
}
fn descriptor_key(token: &str) -> Option<String> {
    token
        .strip_prefix("sync_token:v27:")
        .map(|s| format!("{ACTIVE}{s}"))
        .or_else(|| {
            token
                .strip_prefix("pending_sync_token:v27:")
                .map(|s| format!("{STAGED}{s}"))
        })
}
fn valid(conn: &Connection, key: &str, token: &str) -> Result<Option<Descriptor>, StateError> {
    let Some(raw) = metadata(conn, key)? else {
        return Ok(None);
    };
    let Ok(mut value) = serde_json::from_str::<Descriptor>(&raw) else {
        return Ok(None);
    };
    let library = key.strip_prefix(ACTIVE).or_else(|| {
        key.strip_prefix(STAGED)?
            .split_once(':')
            .map(|(_, zone)| zone)
    });
    let selector = value.plans.first().and_then(Plan::selector);
    if library != Some(value.library.as_str())
        || selector
            .as_ref()
            .and_then(|scope| scope.pointer("/zone/zoneName"))
            .and_then(Value::as_str)
            != Some(value.library.as_str())
        || value.plans.iter().any(|plan| plan.selector() != selector)
    {
        return Ok(None);
    }
    let saved = std::mem::take(&mut value.proof_hash);
    if value.version != 1
        || digest(&encode(&value)?) != saved
        || owner_keys(conn)? != (value.account.clone(), value.provider.clone())
        || value.cursor_hash != digest(token.as_bytes())
        || value.policy != policy(conn)?
    {
        return Ok(None);
    }
    value.proof_hash = saved;
    Ok(Some(value))
}
fn store(conn: &Connection, key: &str, mut value: Descriptor) -> Result<(), StateError> {
    value.proof_hash.clear();
    value.proof_hash = digest(&encode(&value)?);
    conn.execute("INSERT INTO metadata(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value",params![key,serde_json::to_string(&value).map_err(|_invalid|StateError::ProviderSelectionInvalid)?])?;
    Ok(())
}
/// Generic mutations invalidate matching observations, even for equal tokens.
/// Policy promotion preserves proof only when effective pending/active policy
/// is unchanged; an actual policy change invalidates active and staged claims.
pub(in crate::state::db) fn invalidate(
    conn: &Connection,
    keys: &[String],
    before: [Option<String>; 2],
) -> Result<(), StateError> {
    for key in keys {
        if let Some(proof) = descriptor_key(key) {
            conn.execute("DELETE FROM metadata WHERE key=?1", [proof])?;
        }
    }
    if before != policy(conn)? {
        conn.execute("DELETE FROM metadata WHERE key LIKE 'observed_selection:v27:%' OR key LIKE 'observed_selection_staged:v27:%'",[])?;
    }
    Ok(())
}

/// Prepared before token writes; applied inside that same transaction afterward.
pub(in crate::state::db) struct Prepared(Vec<(String, Descriptor)>);
pub(in crate::state::db) fn prepare(
    conn: &Connection,
    updates: &[(String, String)],
    expected: &[(String, Option<String>)],
    proof: Option<&ObservationProof>,
    promote: bool,
) -> Result<Prepared, StateError> {
    let mut out = Vec::new();
    let required = [
        crate::sync_cycle::ENUM_CONFIG_HASH_KEY,
        crate::sync_cycle::PENDING_ENUM_CONFIG_HASH_KEY,
        crate::download::DOWNLOAD_CONFIG_HASH_KEY,
        crate::sync_cycle::PENDING_DOWNLOAD_CONFIG_HASH_KEY,
    ];
    if (proof.is_some() || promote)
        && !required
            .iter()
            .all(|key| expected.iter().any(|(saved, _)| saved == key))
    {
        return Ok(Prepared(out));
    }
    if promote {
        for (key, token) in updates {
            let Some(zone) = key.strip_prefix("sync_token:v27:") else {
                continue;
            };
            let Some(hash) = updates
                .iter()
                .find(|(k, _)| k == "enum_config_hash")
                .map(|(_, v)| v)
            else {
                continue;
            };
            let staged_token = format!("pending_sync_token:v27:{hash}:{zone}");
            if metadata(conn, &staged_token)?.as_deref() != Some(token) {
                continue;
            }
            if let Some(value) = valid(conn, &format!("{STAGED}{hash}:{zone}"), token)? {
                out.push((format!("{ACTIVE}{zone}"), value));
            }
        }
        return Ok(Prepared(out));
    }
    let Some(proof) = proof else {
        return Ok(Prepared(out));
    };
    let Some(plans) = proof.plans() else {
        return Ok(Prepared(out));
    };
    let Some(first) = proof.base.first() else {
        return Ok(Prepared(out));
    };
    if owner_keys(conn)? != (first.account.clone(), first.provider.clone())
        || first.policy != policy(conn)?
    {
        return Ok(Prepared(out));
    }
    let Some(selector) = first.plan.selector() else {
        return Ok(Prepared(out));
    };
    let Some(zone) = selector.pointer("/zone/zoneName").and_then(Value::as_str) else {
        return Ok(Prepared(out));
    };
    let (prior, next, baseline) = match &proof.kind {
        Kind::Delta { prior, next } => (prior, next, false),
        Kind::Baseline { prior, next } => (prior, next, true),
        _ => return Ok(Prepared(out)),
    };
    for (key, token) in updates {
        let Some(target) = descriptor_key(key) else {
            continue;
        };
        if super::super::legacy_preservation::checkpoint_library(key) != Some(zone)
            || digest(token.as_bytes()) != *next
        {
            continue;
        }
        if !baseline {
            let mut predecessor = false;
            for token_key in [key.clone(), format!("sync_token:v27:{zone}")] {
                if let Some(old) = metadata(conn, &token_key)?
                    && digest(old.as_bytes()) == *prior
                    && let Some(old_key) = descriptor_key(&token_key)
                    && let Some(saved) = valid(conn, &old_key, &old)?
                    && saved.plans == plans
                {
                    predecessor = true;
                }
            }
            if !predecessor {
                continue;
            }
        }
        out.push((
            target,
            Descriptor {
                version: 1,
                account: first.account.clone(),
                provider: first.provider.clone(),
                library: zone.to_owned(),
                plans: plans.clone(),
                cursor_hash: next.clone(),
                observed_at: proof.observed_at,
                policy: first.policy.clone(),
                admission_hash: digest(&encode(&(&proof.base, &proof.supplemental))?),
                proof_hash: String::new(),
            },
        ));
    }
    Ok(Prepared(out))
}
pub(in crate::state::db) fn apply(conn: &Connection, prepared: Prepared) -> Result<(), StateError> {
    for (key, value) in prepared.0 {
        if value.policy == policy(conn)? {
            store(conn, &key, value)?;
        }
    }
    Ok(())
}

pub(crate) struct ObservedStatus {
    pub(crate) selector_label: &'static str,
    pub(crate) plan_id: String,
    pub(crate) observed_at: i64,
    pub(crate) pending_media: bool,
    pub(crate) pending_metadata: bool,
    pub(crate) unresolved: bool,
    pub(crate) settlement_unknown: bool,
}
impl SqliteStateDb {
    pub(crate) async fn observed_selection_status(
        &self,
    ) -> Result<Vec<ObservedStatus>, StateError> {
        self.with_conn("reading checkpoint observation proofs",|conn|{
            let tx=conn.unchecked_transaction()?;
            let keys:Vec<String>=tx.prepare("SELECT key FROM metadata WHERE key LIKE 'observed_selection:v27:%' ORDER BY key")?.query_map([],|r|r.get(0))?.collect::<Result<_,_>>()?;
            let mut output=Vec::new();
            for key in keys {
                let Some(zone)=key.strip_prefix(ACTIVE) else {continue;};
                let Some(token)=metadata(&tx,&format!("sync_token:v27:{zone}"))? else {continue;};
                let Some(saved)=valid(&tx,&key,&token)? else {continue;};
                let selector_label = if zone == "PrimarySync" { "Primary library" } else if zone.starts_with("SharedSync") { "Shared library" } else { "Provider library" };
                let plan_id=digest(&encode(&saved.plans)?).chars().take(12).collect();
                let mut status=ObservedStatus {selector_label,plan_id,observed_at:saved.observed_at,pending_media:false,pending_metadata:false,unresolved:false,settlement_unknown:false};
                let scopes=saved.plans.iter().map(|p|p.scope.clone()).collect::<Vec<_>>();
                let roots:Vec<(String,String)>=tx.prepare("SELECT id,scope FROM provider_active_generations")?.query_map([],|r|Ok((r.get(0)?,r.get(1)?)))?.collect::<Result<_,_>>()?;
                for (id,scope) in roots {
                    if !scopes.contains(&scope) {continue;}
                    let root=match super::load_generation(&tx,&id) {Ok(root)=>root,Err(_)=>{status.settlement_unknown=true;continue;}};
                    if root.spec.format==1 {continue;}
                    let Some((media,metadata,deferred))=super::coverage::observation_pending(&tx,&root)? else {status.settlement_unknown=true;continue;};
                    if (media>0 || metadata>0 || deferred>0) && !saved.plans.contains(&Plan::of(&root.spec)) {
                        // Old selected work is retained, but offline status cannot
                        // assert that it is still eligible for this observation.
                        status.settlement_unknown=true;
                        continue;
                    }
                    status.pending_media |= media>0;status.pending_metadata |= metadata>0;status.unresolved |= deferred>0;
                }
                for plan in &saved.plans {
                    let scope:Value=serde_json::from_str(&plan.scope).map_err(|_invalid|StateError::ProviderSelectionInvalid)?;
                    let Some(library)=scope.pointer("/zone/zoneName").and_then(Value::as_str) else {status.settlement_unknown=true;continue;};
                    let sources:i64=tx.query_row("SELECT count(*) FROM unresolved_sparse_identities WHERE library=?1 AND epoch=1",[library],|r|r.get(0))?;
                    if sources>0 || metadata(&tx,&crate::state::unresolved_identity_key(library))?.is_some() {status.settlement_unknown=true;}
                    let publishing:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM provider_active_destinations w JOIN provider_active_generations g ON g.id=w.generation WHERE g.scope=?1 AND EXISTS(SELECT 1 FROM owned_temp_files t WHERE json_extract(t.publication,'$.library')=w.library AND json_extract(t.publication,'$.id')=w.asset_id AND json_extract(t.publication,'$.version')=w.version_size))",[&plan.scope],|r|r.get(0))?;
                    if publishing {status.settlement_unknown=true;}
                }
                output.push(status);
            }
            tx.commit()?;Ok(output)
        }).await
    }
}
