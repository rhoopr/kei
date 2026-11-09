//! Compact observation proofs. Open obligations remain in the existing ledger.
use super::{
    ActiveGeneration, GenerationSpec, SqliteStateDb, account, digest, encode, guard_library_scope,
    load_decision, owner_keys, validate_spec,
};
use crate::state::db::sync_epoch;
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
    // Ephemeral epoch-attempt binding claimed by the cycle gate. It is never
    // part of durable observation evidence.
    epoch_attempt: Option<String>,
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
            epoch_attempt: None,
        })
    }
    pub(crate) fn delivery(receipt: AdmissionReceipt) -> Self {
        Self {
            base: vec![receipt],
            supplemental: Vec::new(),
            kind: Kind::Delivery,
            observed_at: chrono::Utc::now().timestamp(),
            fresh_inventory: false,
            epoch_attempt: None,
        }
    }
    /// Attach the per-library attempt claimed by the epoch gate. The value is
    /// in-memory only and is compared inside the staging transaction.
    pub(crate) fn with_epoch_attempt(mut self, attempt: Option<String>) -> Self {
        self.epoch_attempt = attempt;
        self
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
    /// A completed baseline for this exact zone, token and provider boundary.
    pub(crate) fn is_completed_baseline_for_boundary(
        &self,
        library: &str,
        token: &str,
        boundary: &str,
    ) -> bool {
        let Kind::Baseline { prior, next } = &self.kind else {
            return false;
        };
        *prior == digest(boundary.as_bytes())
            && *next == digest(token.as_bytes())
            && self.has_zone_receipt(library)
    }

    fn has_zone_receipt(&self, library: &str) -> bool {
        self.base.iter().any(|receipt| {
            receipt.plan.selector().is_some_and(|scope| {
                scope.pointer("/zone/zoneName").and_then(Value::as_str) == Some(library)
            })
        })
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    revalidation_round: Option<String>,
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
pub(in crate::state::db) fn revalidation_scope_current(
    conn: &Connection,
    library: &str,
    scope: &str,
    enum_hash: &str,
) -> Result<bool, StateError> {
    let Some(token) = metadata(conn, &format!("sync_token:v27:{library}"))? else {
        return Ok(false);
    };
    let Some(saved) = valid(conn, &format!("{ACTIVE}{library}"), &token)? else {
        return Ok(false);
    };
    Ok(saved.policy[0].as_deref() == Some(enum_hash)
        && saved.plans.iter().any(|p| p.scope == scope))
}
#[cfg(test)]
pub(crate) fn attach_revalidation_round_for_test(
    conn: &Connection,
    key: &str,
    token: &str,
    round: &str,
) -> Result<bool, StateError> {
    let Some(mut value) = valid(conn, key, token)? else {
        return Ok(false);
    };
    value.revalidation_round = Some(round.to_owned());
    store(conn, key, value)?;
    Ok(true)
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
impl Prepared {
    /// Do not advance a matching baseline cursor while its candidate round lacks
    /// the completed baseline proof that would make it retryable.
    pub(in crate::state::db) fn guard_revalidation(
        &self,
        conn: &Connection,
        updates: &[(String, String)],
        expected: &[(String, Option<String>)],
    ) -> Result<(), StateError> {
        for (key, _) in updates {
            let Some(zone) = super::super::legacy_preservation::checkpoint_library(key) else {
                continue;
            };
            for (key, value) in expected {
                if !key.starts_with("baseline_cursor:v27:") {
                    continue;
                }
                let Some(boundary) = value else {
                    continue;
                };
                let hash = digest(boundary.as_bytes());
                let candidates: Vec<String> = conn.prepare("SELECT r.id FROM source_revalidation_rounds r WHERE r.library=?1 AND r.boundary=?2 AND r.enum_hash=COALESCE((SELECT value FROM metadata WHERE key='pending_enum_config_hash'),(SELECT value FROM metadata WHERE key='enum_config_hash')) AND r.scope NOT LIKE 'catalog:v2:%' AND (r.phase='marking' OR (r.phase='observing' AND EXISTS(SELECT 1 FROM source_revalidation_debt d WHERE d.round=r.id)))")?
                    .query_map(params![zone,hash], |r| r.get(0))?.collect::<Result<_,_>>()?;
                if !candidates.is_empty()
                    && !self.0.iter().any(|(_, d)| {
                        d.revalidation_round
                            .as_ref()
                            .is_some_and(|id| candidates.contains(id))
                    })
                {
                    return Err(StateError::Invariant {
                        operation: "commit_checkpoint_transition",
                        detail: "baseline source revalidation lacks completed proof".to_owned(),
                    });
                }
            }
        }
        Ok(())
    }
}
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
    let revalidation_round: Option<String> = if baseline {
        conn.query_row("SELECT id FROM source_revalidation_rounds WHERE library=?1 AND scope=?2 AND enum_hash=?3 AND boundary=?4 AND phase='observing' ORDER BY id LIMIT 1", params![zone, first.plan.scope, first.policy[0], prior], |r| r.get(0)).optional()?
    } else {
        None
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
        let mut round = revalidation_round.clone();
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
                    let saved_round = saved.revalidation_round.clone();
                    if old_key == target
                        && target.starts_with(STAGED)
                        && let Some(saved_round) = saved_round.clone()
                    {
                        let catalog: bool = conn.query_row(
                            "SELECT EXISTS(SELECT 1 FROM source_revalidation_rounds WHERE id=?1 AND library=?2 AND scope LIKE 'catalog:v2:%')",
                            params![saved_round, zone],
                            |r| r.get(0),
                        )?;
                        eprintln!(
                            "B-PREPARE-STAGED saved_round={saved_round} old_key={old_key} target={target} catalog={catalog}"
                        );
                        if catalog {
                            if round.is_none() {
                                round = Some(saved_round);
                            }
                        } else {
                            let observing: bool = conn.query_row("SELECT EXISTS(SELECT 1 FROM source_revalidation_rounds WHERE id=?1 AND library=?2 AND phase='observing')", params![saved_round,zone], |r| r.get(0))?;
                            if !observing {
                                return Err(StateError::Invariant {
                                    operation: "commit_checkpoint_transition",
                                    detail: "staged baseline source revalidation round changed"
                                        .into(),
                                });
                            }
                            round = Some(saved_round);
                        }
                    }
                    if round.is_none()
                        && let Some(saved_round) = saved_round
                    {
                        let current: bool = conn.query_row(
                            "SELECT EXISTS(SELECT 1 FROM source_revalidation_rounds WHERE id=?1 AND library=?2 AND phase!='abandoned' AND scope LIKE 'catalog:v2:%')",
                            params![saved_round, zone],
                            |r| r.get(0),
                        )?;
                        if current {
                            round = Some(saved_round);
                        }
                    }
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
                revalidation_round: round,
            },
        ));
    }
    Ok(Prepared(out))
}
pub(in crate::state::db) fn apply(conn: &Connection, prepared: Prepared) -> Result<(), StateError> {
    for (key, mut value) in prepared.0 {
        if value.policy != policy(conn)? && value.revalidation_round.is_some() {
            return Err(StateError::Invariant {
                operation: "commit_checkpoint_transition",
                detail: "baseline source revalidation policy changed".into(),
            });
        }
        if value.policy == policy(conn)? {
            if key.starts_with(ACTIVE)
                && let Some(round) = &value.revalidation_round
            {
                let catalog: bool = conn.query_row(
                    "SELECT EXISTS(SELECT 1 FROM source_revalidation_rounds WHERE id=?1 AND library=?2 AND scope LIKE 'catalog:v2:%')",
                    params![round, value.library],
                    |row| row.get(0),
                )?;
                if catalog {
                    let phase: Option<String> = conn
                        .query_row(
                            "SELECT phase FROM source_revalidation_rounds WHERE id=?1 AND library=?2",
                            params![round, value.library],
                            |row| row.get(0),
                        )
                        .optional()?;
                    match phase.as_deref() {
                        Some("observing") => {
                            conn.execute("UPDATE source_revalidation_rounds SET phase='active' WHERE id=?1 AND library=?2 AND phase='observing'", params![round, value.library])?;
                        }
                        Some("active") => {}
                        Some(_) | None => {
                            // A known stale or missing catalog link is an
                            // optional consumer only. Do not veto the checkpoint.
                            value.revalidation_round = None;
                        }
                    }
                } else {
                    let activated = conn.execute("UPDATE source_revalidation_rounds SET phase='active' WHERE id=?1 AND library=?2 AND phase='observing'", params![round, value.library])?;
                    if activated != 1 {
                        return Err(StateError::Invariant {
                            operation: "commit_checkpoint_transition",
                            detail: "baseline source revalidation round changed".into(),
                        });
                    }
                }
            }
            store(conn, &key, value)?;
        }
    }
    Ok(())
}

/// Read the epoch-scoped baseline boundary from the transition evidence. A
/// reset-zone baseline must bind the exact fresh boundary, not an old cursor.
fn epoch_baseline(
    transition: &crate::state::db::CheckpointTransition,
    epoch: &str,
) -> Option<(String, String)> {
    transition
        .expected_metadata
        .iter()
        .find_map(|(key, value)| {
            if sync_epoch::is_epoch_baseline_key(key, epoch) {
                value
                    .clone()
                    .filter(|value| !value.trim().is_empty())
                    .map(|value| (key.clone(), value))
            } else {
                None
            }
        })
}

/// The stored descriptor remains the owner of policy, plans, admission and
/// round. The binding pins that exact descriptor by hash and binds it to one
/// token and one fresh boundary.
fn descriptor_matches_binding(
    descriptor: &Descriptor,
    binding: &sync_epoch::BaselineBinding,
    token: &str,
    descriptor_proof_hash: &str,
) -> bool {
    descriptor.cursor_hash == binding.token_sha256
        && digest(token.as_bytes()) == binding.token_sha256
        && binding.descriptor_proof_hash == descriptor_proof_hash
}

fn staged_zone(key: &str) -> Option<&str> {
    let rest = key.strip_prefix("pending_sync_token:v27:")?;
    let (_, zone) = rest.split_once(':')?;
    Some(zone)
}

fn any_enrolled_update(
    conn: &Connection,
    transition: &crate::state::db::CheckpointTransition,
) -> Result<bool, StateError> {
    for (key, _) in &transition.metadata_updates {
        let zone = key
            .strip_prefix("sync_token:v27:")
            .or_else(|| staged_zone(key));
        if let Some(zone) = zone
            && sync_epoch::read_library(conn, zone)?.is_some()
        {
            return Ok(true);
        }
    }
    Ok(false)
}

/// A fresh observation may stage or directly complete an epoch baseline only
/// for the attempt the gate claimed before that baseline ran. The field is
/// ephemeral; missing evidence fails closed for an unfinished epoch.
fn observation_attempt_matches(proof: &ObservationProof, attempt: &str) -> bool {
    proof.epoch_attempt.as_deref() == Some(attempt)
}

/// Validate a durable staged proof before writing a direct active token or while
/// staging a token under config drift. Direct completion returns the exact epoch
/// data that must be removed atomically with the active-token write.
pub(in crate::state::db) fn record_epoch_stage(
    conn: &Connection,
    proof: Option<&ObservationProof>,
    prepared: &Prepared,
    transition: &crate::state::db::CheckpointTransition,
) -> Result<Vec<sync_epoch::EpochCompletion>, StateError> {
    let Some(epoch) = sync_epoch::validate_if_present(conn)? else {
        return Ok(Vec::new());
    };
    if !any_enrolled_update(conn, transition)? {
        return Ok(Vec::new());
    }
    let mut completions = Vec::new();
    for (key, token) in &transition.metadata_updates {
        if let Some(zone) = staged_zone(key) {
            let Some(record) = sync_epoch::read_library(conn, zone)? else {
                continue;
            };
            // Owner presence and uniqueness stay part of the epoch guard.
            let (_account, _provider) = owner_keys(conn)?;
            if record.epoch != epoch.epoch {
                return Err(StateError::SyncEpochMismatch);
            }
            if record.state == "complete" {
                continue;
            }
            if record.state == "blocked" {
                return Err(StateError::Invariant {
                    operation: "commit_checkpoint_transition",
                    detail: "sync epoch library is blocked".to_owned(),
                });
            }
            if record.state == "ready" {
                if let Some(proof) = proof
                    && !observation_attempt_matches(proof, &record.attempt)
                {
                    return Err(StateError::SyncEpochMismatch);
                }
                if let Some(binding) = &record.binding
                    && binding.token_sha256 == digest(token.as_bytes())
                {
                    continue;
                }
                return Err(StateError::SyncEpochMismatch);
            }
            let Some(proof) = proof else {
                return Err(StateError::Invariant {
                    operation: "commit_checkpoint_transition",
                    detail: "sync epoch staged token lacks a completed baseline".to_owned(),
                });
            };
            if !observation_attempt_matches(proof, &record.attempt) {
                return Err(StateError::SyncEpochMismatch);
            }
            let Some((baseline_key, baseline_value)) = epoch_baseline(transition, &epoch.epoch)
            else {
                return Err(StateError::Invariant {
                    operation: "commit_checkpoint_transition",
                    detail: "sync epoch staged token lacks a fresh baseline boundary".to_owned(),
                });
            };
            if !proof.is_completed_baseline_for_boundary(zone, token, &baseline_value) {
                return Err(StateError::Invariant {
                    operation: "commit_checkpoint_transition",
                    detail: "sync epoch staged token lacks a completed baseline".to_owned(),
                });
            }
            let descriptor_key = format!(
                "{STAGED}{}",
                key.strip_prefix("pending_sync_token:v27:")
                    .unwrap_or_default()
            );
            let Some(descriptor) = prepared
                .0
                .iter()
                .find(|(key, _)| key == &descriptor_key)
                .map(|(_, descriptor)| descriptor)
            else {
                return Err(StateError::Invariant {
                    operation: "commit_checkpoint_transition",
                    detail: "sync epoch staged token lacks a validated descriptor".to_owned(),
                });
            };
            let mut finalized = descriptor.clone();
            finalized.proof_hash.clear();
            let descriptor_proof_hash = digest(&encode(&finalized)?);
            let binding = sync_epoch::BaselineBinding {
                token_key: key.clone(),
                token_sha256: digest(token.as_bytes()),
                descriptor_key,
                descriptor_proof_hash: descriptor_proof_hash.clone(),
                baseline_key,
                baseline_sha256: digest(baseline_value.as_bytes()),
            };
            if !descriptor_matches_binding(descriptor, &binding, token, &descriptor_proof_hash) {
                return Err(StateError::Invariant {
                    operation: "commit_checkpoint_transition",
                    detail: "sync epoch staged descriptor does not match its binding".to_owned(),
                });
            }
            sync_epoch::record_ready(conn, &epoch, zone, &record.attempt, binding)?;
            continue;
        }
        let Some(zone) = key.strip_prefix("sync_token:v27:") else {
            continue;
        };
        let Some(record) = sync_epoch::read_library(conn, zone)? else {
            continue;
        };
        // Owner presence and uniqueness stay part of the epoch guard.
        let (_account, _provider) = owner_keys(conn)?;
        let current_policy = policy(conn)?;
        if record.epoch != epoch.epoch {
            return Err(StateError::SyncEpochMismatch);
        }
        if record.state == "complete" {
            continue;
        }
        if record.state == "blocked" {
            return Err(StateError::Invariant {
                operation: "commit_checkpoint_transition",
                detail: "sync epoch library is blocked".to_owned(),
            });
        }
        if record.state != "pending" {
            return Err(StateError::SyncEpochMismatch);
        }
        let Some(proof) = proof else {
            return Err(StateError::Invariant {
                operation: "commit_checkpoint_transition",
                detail: "sync epoch active token lacks a completed baseline".to_owned(),
            });
        };
        if !observation_attempt_matches(proof, &record.attempt) {
            return Err(StateError::SyncEpochMismatch);
        }
        let Some((baseline_key, baseline_value)) = epoch_baseline(transition, &epoch.epoch) else {
            return Err(StateError::Invariant {
                operation: "commit_checkpoint_transition",
                detail: "sync epoch active token lacks a fresh baseline boundary".to_owned(),
            });
        };
        if !proof.is_completed_baseline_for_boundary(zone, token, &baseline_value) {
            return Err(StateError::Invariant {
                operation: "commit_checkpoint_transition",
                detail: "sync epoch active token lacks a completed baseline".to_owned(),
            });
        }
        let descriptor_key = format!("{ACTIVE}{zone}");
        let Some(descriptor) = prepared
            .0
            .iter()
            .find(|(key, _)| key == &descriptor_key)
            .map(|(_, descriptor)| descriptor)
        else {
            return Err(StateError::Invariant {
                operation: "commit_checkpoint_transition",
                detail: "sync epoch active token lacks a validated descriptor".to_owned(),
            });
        };
        let mut finalized = descriptor.clone();
        finalized.proof_hash.clear();
        let descriptor_proof_hash = digest(&encode(&finalized)?);
        let binding = sync_epoch::BaselineBinding {
            token_key: key.clone(),
            token_sha256: digest(token.as_bytes()),
            descriptor_key,
            descriptor_proof_hash: descriptor_proof_hash.clone(),
            baseline_key: baseline_key.clone(),
            baseline_sha256: digest(baseline_value.as_bytes()),
        };
        if descriptor.policy != current_policy {
            return Err(StateError::Invariant {
                operation: "commit_checkpoint_transition",
                detail: "sync epoch active descriptor policy changed".to_owned(),
            });
        }
        if !descriptor_matches_binding(descriptor, &binding, token, &descriptor_proof_hash) {
            return Err(StateError::Invariant {
                operation: "commit_checkpoint_transition",
                detail: "sync epoch active descriptor does not match its binding".to_owned(),
            });
        }
        completions.push(sync_epoch::EpochCompletion {
            library: zone.to_owned(),
            attempt: record.attempt,
            baseline_key,
        });
    }
    Ok(completions)
}

/// Validate reset-zone active-token promotions against the immutable staged
/// proof. Non-epoch zones are not inspected.
pub(in crate::state::db) fn validate_epoch_promotion(
    conn: &Connection,
    prepared: &Prepared,
    transition: &crate::state::db::CheckpointTransition,
) -> Result<Vec<sync_epoch::EpochCompletion>, StateError> {
    let Some(epoch) = sync_epoch::validate_if_present(conn)? else {
        return Ok(Vec::new());
    };
    if !any_enrolled_update(conn, transition)? {
        return Ok(Vec::new());
    }
    let mut completions = Vec::new();
    for (key, token) in &transition.metadata_updates {
        let Some(zone) = key.strip_prefix("sync_token:v27:") else {
            continue;
        };
        let Some(record) = sync_epoch::read_library(conn, zone)? else {
            continue;
        };
        // Owner presence and uniqueness stay part of the epoch guard.
        let (_account, _provider) = owner_keys(conn)?;
        let _current_policy = policy(conn)?;
        if record.epoch != epoch.epoch {
            return Err(StateError::SyncEpochMismatch);
        }
        if record.state == "complete" {
            continue;
        }
        if record.state != "ready" {
            return Err(StateError::Invariant {
                operation: "commit_checkpoint_transition",
                detail: "sync epoch promotion is not ready".to_owned(),
            });
        }
        let Some(binding) = record.binding else {
            return Err(StateError::Invariant {
                operation: "commit_checkpoint_transition",
                detail: "sync epoch promotion lacks a durable baseline binding".to_owned(),
            });
        };
        let token_hash = digest(token.as_bytes());
        let staged_rest = binding
            .token_key
            .strip_prefix("pending_sync_token:v27:")
            .and_then(|rest| rest.split_once(':'));
        let Some((enum_hash, staged_zone)) = staged_rest else {
            return Err(StateError::Invariant {
                operation: "commit_checkpoint_transition",
                detail: "sync epoch promotion binding does not name a staged token".to_owned(),
            });
        };
        if staged_zone != zone
            || binding.descriptor_key != format!("{STAGED}{enum_hash}:{zone}")
            || binding.token_sha256 != token_hash
            || !sync_epoch::is_epoch_baseline_key(&binding.baseline_key, &epoch.epoch)
            || binding.baseline_sha256.is_empty()
            || !baseline_binding_matches(conn, &binding)?
        {
            return Err(StateError::Invariant {
                operation: "commit_checkpoint_transition",
                detail: "sync epoch promotion binding does not match current state".to_owned(),
            });
        }
        let descriptor_key = format!("{ACTIVE}{zone}");
        let Some(descriptor) = prepared
            .0
            .iter()
            .find(|(key, _)| key == &descriptor_key)
            .map(|(_, descriptor)| descriptor)
        else {
            return Err(StateError::Invariant {
                operation: "commit_checkpoint_transition",
                detail: "sync epoch promotion lacks a validated active descriptor".to_owned(),
            });
        };
        if !descriptor_matches_binding(descriptor, &binding, token, &descriptor.proof_hash) {
            return Err(StateError::Invariant {
                operation: "commit_checkpoint_transition",
                detail: "sync epoch promotion descriptor does not match its binding".to_owned(),
            });
        }
        completions.push(sync_epoch::EpochCompletion {
            library: zone.to_owned(),
            attempt: record.attempt,
            baseline_key: binding.baseline_key,
        });
    }
    Ok(completions)
}

fn baseline_binding_matches(
    conn: &Connection,
    binding: &sync_epoch::BaselineBinding,
) -> Result<bool, StateError> {
    // The exact boundary must still be present. A successful epoch promotion
    // removes the baseline and binding together in one transaction, so a
    // binding that survives while its boundary is absent is not evidence.
    match metadata(conn, &binding.baseline_key)? {
        Some(value) => Ok(digest(value.as_bytes()) == binding.baseline_sha256),
        None => Ok(false),
    }
}

/// Read-only readiness check used by the normal-cycle gate. A deterministic
/// mismatch is stale (a new full may recover); malformed storage is an error.
pub(in crate::state::db) fn epoch_proof_ready(
    conn: &Connection,
    epoch: &str,
    record: &sync_epoch::SyncEpochLibrary,
) -> Result<bool, StateError> {
    if record.epoch != epoch {
        return Ok(false);
    }
    let Some(binding) = record.binding.as_ref() else {
        return Ok(false);
    };
    if !sync_epoch::is_epoch_baseline_key(&binding.baseline_key, epoch)
        || binding.baseline_sha256.is_empty()
        || !baseline_binding_matches(conn, binding)?
    {
        return Ok(false);
    }
    let Some(token) = metadata(conn, &binding.token_key)? else {
        return Ok(false);
    };
    if digest(token.as_bytes()) != binding.token_sha256 {
        return Ok(false);
    }
    let Some(descriptor) = valid(conn, &binding.descriptor_key, &token)? else {
        return Ok(false);
    };
    Ok(descriptor_matches_binding(
        &descriptor,
        binding,
        &token,
        &descriptor.proof_hash,
    ))
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

#[cfg(test)]
pub(in crate::state::db) fn seed_valid_staged_epoch_for_test(
    conn: &Connection,
    zone: &str,
    token: &str,
) -> Result<(), StateError> {
    let epoch = sync_epoch::validate(conn)?;
    let (account, provider) = owner_keys(conn)?;
    let current_policy = policy(conn)?;
    let enum_hash = current_policy[0]
        .clone()
        .ok_or(StateError::SyncEpochMismatch)?;
    let attempt = format!("attempt-{zone}");
    let scope = format!("scope-{zone}");
    let selector = serde_json::json!({"zone": {"zoneName": zone}});
    let plans = vec![Plan {
        scope: scope.clone(),
        config: "config".to_owned(),
        profile: serde_json::json!({"selector_binding": {"scope": selector}}),
        metadata: false,
        flags: 0,
    }];
    let token_key = format!("pending_sync_token:v27:{enum_hash}:{zone}");
    let descriptor_key = format!("{STAGED}{enum_hash}:{zone}");
    let baseline_key = format!("baseline_cursor:v27:{}:{scope}:{enum_hash}:", epoch.epoch);
    let baseline_value = "synthetic-boundary";
    let mut descriptor = Descriptor {
        version: 1,
        account: account.clone(),
        provider: provider.clone(),
        library: zone.to_owned(),
        plans: plans.clone(),
        cursor_hash: digest(token.as_bytes()),
        observed_at: 0,
        policy: current_policy.clone(),
        admission_hash: "admission".to_owned(),
        proof_hash: String::new(),
        revalidation_round: None,
    };
    let descriptor_proof_hash = digest(&encode(&descriptor)?);
    descriptor.proof_hash = descriptor_proof_hash.clone();
    store(conn, &descriptor_key, descriptor)?;
    conn.execute(
        "INSERT OR REPLACE INTO metadata(key,value) VALUES(?1,?2)",
        params![token_key, token],
    )?;
    conn.execute(
        "INSERT OR REPLACE INTO metadata(key,value) VALUES(?1,?2)",
        params![baseline_key, baseline_value],
    )?;
    let record = serde_json::json!({
        "version": 2,
        "epoch": epoch.epoch,
        "library": zone,
        "state": "ready",
        "reason": "existing_upgrade",
        "attempt": attempt,
        "attempts": 1,
        "next_attempt_at": 0,
        "last_outcome": "baseline_staged",
        "baseline_scope": scope,
        "enrolled_at": 0,
        "completed_at": null,
        "binding": {
            "token_key": token_key,
            "token_sha256": digest(token.as_bytes()),
            "descriptor_key": descriptor_key,
            "descriptor_proof_hash": descriptor_proof_hash,
            "baseline_key": baseline_key,
            "baseline_sha256": digest(baseline_value.as_bytes()),
        }
    });
    conn.execute(
        "INSERT OR REPLACE INTO metadata(key,value) VALUES(?1,?2)",
        params![sync_epoch::library_key(zone), record.to_string()],
    )?;
    Ok(())
}

#[cfg(test)]
mod baseline_tests {
    use super::*;
    use rusqlite::params;

    fn binding() -> sync_epoch::BaselineBinding {
        sync_epoch::BaselineBinding {
            token_key: "pending_sync_token:v27:enum:PrimarySync".into(),
            token_sha256: "token".into(),
            descriptor_key: "observed_selection_staged:v27:enum:PrimarySync".into(),
            descriptor_proof_hash: "descriptor".into(),
            baseline_key: "baseline_cursor:v27:epoch:scope:enum:".into(),
            baseline_sha256: digest(b"boundary"),
        }
    }
    #[test]
    fn ready_epoch_binding_requires_the_current_attempt_before_idempotent_token_continue() {
        let db = SqliteStateDb::open_in_memory().unwrap();
        let conn = db.acquire_lock("ready epoch attempt test").unwrap();
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS account_owner(\
             singleton INTEGER PRIMARY KEY CHECK(singleton=1), \
             version INTEGER NOT NULL, account_key TEXT NOT NULL, \
             provider_key TEXT NOT NULL);\
             INSERT OR REPLACE INTO account_owner VALUES(1,1,'test-account','test-provider');",
        )
        .unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO metadata(key,value) VALUES(?1,'enum'),(?2,'download')",
            params![
                crate::sync_cycle::ENUM_CONFIG_HASH_KEY,
                crate::download::DOWNLOAD_CONFIG_HASH_KEY
            ],
        )
        .unwrap();
        seed_valid_staged_epoch_for_test(&conn, "PrimarySync", "ready-token").unwrap();
        let transition = crate::state::CheckpointTransition {
            expected_metadata: Vec::new(),
            legacy_preservation_proofs: Vec::new(),
            legacy_config_hash: None,
            sparse_identity_proofs: Vec::new(),
            metadata_updates: vec![(
                "pending_sync_token:v27:enum:PrimarySync".to_owned(),
                "ready-token".to_owned(),
            )],
            metadata_deletes: Vec::new(),
        };
        let receipt = AdmissionReceipt {
            account: "test-account".to_owned(),
            provider: "test-provider".to_owned(),
            plan: Plan {
                scope: "scope".to_owned(),
                config: "download".to_owned(),
                profile: serde_json::json!({
                    "selector_binding": {"scope": {"zone": {"zoneName": "PrimarySync"}}}
                }),
                metadata: false,
                flags: 0,
            },
            policy: [Some("enum".to_owned()), Some("download".to_owned())],
            seal: Some("seal".to_owned()),
        };
        let stale = ObservationProof::delivery(receipt.clone())
            .with_epoch_attempt(Some("attempt-old".to_owned()));
        assert!(matches!(
            record_epoch_stage(&conn, Some(&stale), &Prepared(Vec::new()), &transition),
            Err(StateError::SyncEpochMismatch)
        ));
        let current = ObservationProof::delivery(receipt.clone())
            .with_epoch_attempt(Some("attempt-PrimarySync".to_owned()));
        assert!(
            record_epoch_stage(&conn, Some(&current), &Prepared(Vec::new()), &transition)
                .unwrap()
                .is_empty(),
            "the current attempt keeps the ready token idempotent"
        );
        assert!(
            record_epoch_stage(&conn, None, &Prepared(Vec::new()), &transition)
                .unwrap()
                .is_empty(),
            "an absent fresh proof preserves ready promotion-only behavior"
        );
        let record = sync_epoch::read_library(&conn, "PrimarySync")
            .unwrap()
            .unwrap();
        assert_eq!(record.state, "ready");
        assert_eq!(record.attempt, "attempt-PrimarySync");
        assert!(record.binding.is_some());

        let library_key = sync_epoch::library_key("PrimarySync");
        let raw: String = conn
            .query_row(
                "SELECT value FROM metadata WHERE key=?1",
                [&library_key],
                |row| row.get(0),
            )
            .unwrap();
        let mut pending: serde_json::Value = serde_json::from_str(&raw).unwrap();
        pending["state"] = serde_json::json!("pending");
        let _ = pending.as_object_mut().unwrap().remove("binding");
        conn.execute(
            "UPDATE metadata SET value=?2 WHERE key=?1",
            params![library_key, pending.to_string()],
        )
        .unwrap();
        let missing = ObservationProof::delivery(receipt);
        assert!(matches!(
            record_epoch_stage(&conn, Some(&missing), &Prepared(Vec::new()), &transition),
            Err(StateError::SyncEpochMismatch)
        ));
        let matching = missing.with_epoch_attempt(Some("attempt-PrimarySync".to_owned()));
        assert!(matches!(
            record_epoch_stage(&conn, Some(&matching), &Prepared(Vec::new()), &transition),
            Err(StateError::Invariant { .. })
        ));
    }

    #[test]
    fn baseline_binding_requires_the_exact_present_boundary() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE metadata(key TEXT PRIMARY KEY, value TEXT NOT NULL);")
            .unwrap();
        let binding = binding();
        assert!(
            !baseline_binding_matches(&conn, &binding).unwrap(),
            "a missing baseline must fail closed"
        );
        conn.execute(
            "INSERT INTO metadata(key,value) VALUES(?1,'boundary')",
            [&binding.baseline_key],
        )
        .unwrap();
        assert!(baseline_binding_matches(&conn, &binding).unwrap());
        conn.execute(
            "UPDATE metadata SET value='changed' WHERE key=?1",
            [&binding.baseline_key],
        )
        .unwrap();
        assert!(
            !baseline_binding_matches(&conn, &binding).unwrap(),
            "a changed present boundary must fail"
        );
    }
}
