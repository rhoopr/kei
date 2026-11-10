//! Additive, baseline-bound source verification debt. It never owns media.
use super::{SqliteStateDb, account};
use crate::state::error::StateError;
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use sha2::{Digest, Sha256};

pub(crate) const CATALOG_MODE: &str = "catalog_v2";
#[allow(dead_code)] // used by the L1 schema44 migration and consumer audit.
pub(crate) const LEGACY_MODE: &str = "legacy_unfiled_v1";
pub(crate) const CATALOG_SCOPE_PREFIX: &str = "catalog:v2:";

#[cfg(test)]
pub(crate) fn prepare_existing_owned_database_for_test(
    path: &std::path::Path,
    owner: &account::AccountOwner,
) -> Result<(), StateError> {
    drop(account::open(path, owner)?);
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CatalogOrigin {
    pub(crate) plan_digest: String,
    pub(crate) epoch: String,
    pub(crate) attempt: String,
    pub(crate) nonce: String,
}

#[allow(dead_code)] // L1 catalog cycle integration is pending; focused owner tests exercise this API.
fn hex_64(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

#[allow(dead_code)] // L1 catalog cycle integration is pending; focused owner tests exercise this API.
fn uuid_text(value: &str) -> bool {
    uuid::Uuid::parse_str(value).is_ok()
}

#[allow(dead_code)] // L1 catalog cycle integration is pending; focused owner tests exercise this API.
impl CatalogOrigin {
    pub(crate) fn new(
        plan_digest: String,
        epoch: String,
        attempt: String,
        nonce: String,
    ) -> Result<Self, StateError> {
        if !hex_64(&plan_digest) || !uuid_text(&epoch) || !uuid_text(&attempt) || !uuid_text(&nonce)
        {
            return Err(StateError::Invariant {
                operation: "catalog_origin",
                detail: "catalog origin has an invalid plan digest or identity".into(),
            });
        }
        Ok(Self {
            plan_digest,
            epoch,
            attempt,
            nonce,
        })
    }

    pub(crate) fn parse(scope: &str) -> Result<Self, StateError> {
        let rest =
            scope
                .strip_prefix(CATALOG_SCOPE_PREFIX)
                .ok_or_else(|| StateError::Invariant {
                    operation: "catalog_origin",
                    detail: "unknown catalog scope version".into(),
                })?;
        let parts: Vec<&str> = rest.split(':').collect();
        let [plan_digest, epoch, attempt, nonce] = parts.as_slice() else {
            return Err(StateError::Invariant {
                operation: "catalog_origin",
                detail: "catalog scope must have four identity components".into(),
            });
        };
        Self::new(
            (*plan_digest).to_owned(),
            (*epoch).to_owned(),
            (*attempt).to_owned(),
            (*nonce).to_owned(),
        )
    }

    pub(crate) fn scope(&self) -> String {
        format!(
            "{CATALOG_SCOPE_PREFIX}{}:{}:{}:{}",
            self.plan_digest, self.epoch, self.attempt, self.nonce
        )
    }

    pub(crate) fn boundary(cursor: &str) -> String {
        format!("{:x}", Sha256::digest(cursor.as_bytes()))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FreshCatalogPass {
    pub(crate) key: String,
    pub(crate) kind: String,
    pub(crate) scope: serde_json::Value,
    pub(crate) excludes: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FreshCatalogPlan {
    pub(crate) account: String,
    pub(crate) provider: String,
    pub(crate) database: String,
    pub(crate) zone: serde_json::Value,
    pub(crate) scope: String,
    pub(crate) config_hash: String,
    pub(crate) metadata_enabled: bool,
    pub(crate) metadata_flags: u8,
    pub(crate) passes: Vec<FreshCatalogPass>,
    pub(crate) profile: serde_json::Value,
}

#[derive(Clone)]
pub(crate) struct FreshCatalogOriginRequest {
    pub(crate) library: String,
    pub(crate) plan: FreshCatalogPlan,
    pub(crate) baseline_key: String,
    pub(crate) preservation_config_hash: String,
    pub(crate) expected_baseline: Option<String>,
    pub(crate) fresh_cursor: String,
    pub(crate) enum_hash: String,
    pub(crate) expected_epoch: String,
    pub(crate) expected_attempt: String,
    pub(crate) expected_metadata: Vec<(String, Option<String>)>,
    pub(crate) expected_catalog_snapshot: String,
}

/// A pre-stream decision only. The checkpoint and catalog owner revalidate all
/// evidence after the provider stream has actually reached EOF.
#[must_use]
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum CatalogBootstrapEligibility {
    CurrentLink,
    CurrentLegacyLink,
    AttemptBootstrap,
}

#[must_use]
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum CatalogResumeRelation {
    /// Text equality only; this is not proof that a provider stream reached EOF.
    CurrentToken,
    SavedBoundary,
    PendingAttempt,
}

#[must_use]
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct CatalogResume {
    pub(crate) origin: CatalogOrigin,
    pub(crate) boundary: String,
    pub(crate) relation: CatalogResumeRelation,
}

impl std::fmt::Debug for CatalogResume {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CatalogResume")
            .field("relation", &self.relation)
            .finish_non_exhaustive()
    }
}

enum CatalogOriginOperation {
    Fresh,
    ObservedDelta(
        Box<(
            super::CheckpointTransition,
            super::provider_generations::observation::ObservationProof,
        )>,
    ),
    Eligibility,
    Resume,
}

enum CatalogOriginOperationResult {
    Installed(CatalogOrigin),
    Eligibility(CatalogBootstrapEligibility),
    Resume(Option<CatalogResume>),
}

fn catalog_snapshot(conn: &Connection, library: &str) -> Result<String, StateError> {
    let mut hash = Sha256::new();
    let mut statement = conn.prepare(
        "SELECT json_array(id,scope,enum_hash,boundary,high_id,after_id,phase,mode)
         FROM source_revalidation_rounds WHERE library=?1 ORDER BY id",
    )?;
    let rows = statement.query_map([library], |row| row.get::<_, String>(0))?;
    for row in rows {
        let row = row?;
        hash.update((row.len() as u64).to_be_bytes());
        hash.update(row.as_bytes());
    }
    Ok(format!("{:x}", hash.finalize()))
}

pub(crate) fn fresh_catalog_plan_digest(plan: &FreshCatalogPlan) -> String {
    let zone_name = plan
        .zone
        .get("zoneName")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    let plan_json = serde_json::json!({
        "scope": plan.scope,
        "config": plan.config_hash,
        "profile": plan.profile,
        "metadata": plan.metadata_enabled,
        "flags": plan.metadata_flags,
    });
    let canonical = serde_json::json!({
        "version": 1,
        "account": plan.account,
        "provider": plan.provider,
        "library": zone_name,
        "plans": [plan_json],
    });
    format!("{:x}", Sha256::digest(canonical.to_string().as_bytes()))
}

#[derive(Clone, Debug)]
#[allow(dead_code)] // L1 catalog cycle integration is pending; focused owner tests exercise this API.
pub(crate) struct CatalogBaselineClaim {
    pub(crate) library: String,
    pub(crate) baseline_key: String,
    pub(crate) expected_baseline: Option<String>,
    pub(crate) plan_digest: String,
    pub(crate) expected_enum_hash: String,
    pub(crate) expected_epoch: String,
    pub(crate) expected_epoch_json: String,
    pub(crate) expected_library_json: String,
    pub(crate) expected_metadata: Vec<(String, Option<String>)>,
    pub(crate) now: i64,
    pub(crate) watch_interval: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
#[allow(dead_code)] // L1 catalog cycle integration is pending; focused owner tests exercise this API.
pub(crate) enum CatalogClaimOutcome {
    Transferred {
        old_round: String,
        new_round: String,
        attempt: String,
        copied_debt: u64,
    },
    Baseline {
        attempt: String,
    },
    Promote {
        attempt: String,
    },
    Defer {
        reason: &'static str,
    },
    Blocked {
        reason: &'static str,
    },
    Open,
}

#[derive(Clone, Debug, PartialEq, Eq)]
#[allow(dead_code)] // L1 catalog precheck/report integration is pending.
pub(crate) struct CatalogPendingSignal {
    pub(crate) round: String,
    pub(crate) library: String,
    pub(crate) scope: String,
    pub(crate) phase: String,
    pub(crate) due_at: i64,
    pub(crate) pending_debt: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
#[allow(dead_code)] // L1 catalog report/hold integration is pending.
pub(crate) struct CatalogSourceOutcome {
    pub(crate) pending: bool,
    pub(crate) retained: u64,
    pub(crate) next_due_at: Option<i64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CatalogPairEvidence {
    pub(crate) database: String,
    pub(crate) zone: serde_json::Value,
    pub(crate) master: String,
    pub(crate) child: String,
    pub(crate) master_type: String,
    pub(crate) child_type: String,
    pub(crate) reference_exact: bool,
    pub(crate) explicit_deleted: bool,
}

#[derive(Clone, Debug)]
#[allow(dead_code)] // L1 catalog drain integration is pending; focused owner tests exercise this.
pub(crate) enum CatalogSettlementAuthority {
    Deleted {
        deleted_at: Option<chrono::DateTime<chrono::Utc>>,
        evidence: CatalogPairEvidence,
    },
    Live {
        evidence: CatalogPairEvidence,
    },
    Refused,
}

#[derive(Clone)]
#[allow(dead_code)] // L1 catalog cycle integration is pending; focused owner tests exercise this API.
struct ExistingCatalogRound {
    id: String,
    scope: String,
    origin: CatalogOrigin,
    enum_hash: String,
    boundary: String,
    high_id: String,
    after_id: String,
    phase: String,
}

#[cfg(test)]
pub(crate) fn ensure_catalog_mode_column_for_test(conn: &Connection) -> Result<(), StateError> {
    let exists: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info('source_revalidation_rounds') WHERE name='mode')",
        [],
        |row| row.get(0),
    )?;
    if !exists {
        conn.execute_batch(
            "ALTER TABLE source_revalidation_rounds ADD COLUMN mode TEXT NOT NULL DEFAULT 'legacy_unfiled_v1';",
        )?;
    }
    Ok(())
}

pub(crate) const DDL: &str = r"
CREATE TABLE IF NOT EXISTS source_revalidation_rounds (
 id TEXT PRIMARY KEY, library TEXT NOT NULL, scope TEXT NOT NULL,
 enum_hash TEXT NOT NULL, boundary TEXT NOT NULL, high_id TEXT NOT NULL,
 after_id TEXT NOT NULL, phase TEXT NOT NULL CHECK(phase IN ('marking','observing','active','abandoned'))
);
CREATE TABLE IF NOT EXISTS source_revalidation_debt (
 round TEXT NOT NULL REFERENCES source_revalidation_rounds(id),
 id TEXT NOT NULL, next_retry_at INTEGER NOT NULL DEFAULT 0,
 PRIMARY KEY(round,id)
);
CREATE INDEX IF NOT EXISTS source_revalidation_due ON source_revalidation_debt(next_retry_at,round,id);
";

/// Quarantine every historical source-revalidation round while retaining its
/// debt rows. This runs once at epoch creation, before any baseline guard.
/// Media, receipts, paths and all non-revalidation state are untouched.
pub(crate) fn quarantine_all_rounds(conn: &Connection) -> Result<(u64, i64), StateError> {
    let rounds = conn.execute(
        "UPDATE source_revalidation_rounds SET phase='abandoned' WHERE phase <> 'abandoned'",
        [],
    )? as u64;
    let debts: i64 =
        conn.query_row("SELECT COUNT(*) FROM source_revalidation_debt", [], |row| {
            row.get(0)
        })?;
    Ok((rounds, debts))
}

#[derive(Clone, Debug)]
pub(crate) struct SourceCandidate {
    pub(crate) round: String,
    pub(crate) library: String,
    pub(crate) id: String,
    pub(crate) master: String,
    pub(crate) child: Option<String>,
    pub(crate) owned_child: bool,
    pub(crate) snapshot: String,
}

fn snapshot(
    conn: &rusqlite::Connection,
    round: &str,
    id: &str,
    master: &str,
) -> Result<String, StateError> {
    Ok(conn.query_row(
        "SELECT json_array(
          (SELECT json_group_array(json_array(id,version_size,checksum,filename,size_bytes,status,local_path,local_checksum,download_checksum,metadata_hash,is_deleted,deleted_at,last_seen_at,downloaded_at))
           FROM (SELECT * FROM assets WHERE library=r.library AND (id=?2 OR id=?3 OR id IN (SELECT asset_record_name FROM asset_master_mappings WHERE library=r.library AND master_record_name=?3)) ORDER BY id,version_size)),
          (SELECT json_group_array(json_array(asset_record_name,master_record_name,updated_at))
           FROM (SELECT * FROM asset_master_mappings WHERE library=r.library AND (asset_record_name=?2 OR master_record_name=?3 OR asset_record_name IN (SELECT asset_record_name FROM legacy_master_state_owners WHERE library=r.library AND master_record_name=?3)) ORDER BY asset_record_name)),
          (SELECT json_group_array(json_array(asset_id,evidence_version,original_evidence,files,active_generation))
           FROM (SELECT * FROM unattributed_legacy WHERE library=r.library AND (asset_id=?2 OR asset_id=?3 OR asset_id IN (SELECT asset_record_name FROM asset_master_mappings WHERE library=r.library AND master_record_name=?3)) ORDER BY asset_id)),
          (SELECT json_group_array(json_array(id,version_size,state,reason,checked_at)) FROM (SELECT * FROM asset_verifications WHERE library=r.library AND (id=?2 OR id=?3 OR id IN (SELECT asset_record_name FROM asset_master_mappings WHERE library=r.library AND master_record_name=?3)) ORDER BY id,version_size)),
          (SELECT json_array(asset_record_name,claimed_at) FROM legacy_master_state_owners WHERE library=r.library AND master_record_name=?3),
          (SELECT next_retry_at FROM source_revalidation_debt WHERE round=r.id AND id=?2),
          r.scope,r.enum_hash,r.boundary,r.phase,r.mode,
          (SELECT value FROM metadata WHERE key='sync_token:v27:'||r.library),
          (SELECT value FROM metadata WHERE key='observed_selection:v27:'||r.library),
          (SELECT value FROM metadata WHERE key='enum_config_hash'),
          (SELECT value FROM metadata WHERE key='pending_enum_config_hash'),
          (SELECT value FROM metadata WHERE key='config_hash'),
          (SELECT value FROM metadata WHERE key='pending_download_config_hash')
         ) FROM source_revalidation_rounds r WHERE r.id=?1",
        params![round,id,master], |row| row.get(0),
    )?)
}

fn catalog_owner_protected(
    conn: &Connection,
    library: &str,
    ids_json: &str,
    ids: &std::collections::BTreeSet<String>,
) -> Result<bool, StateError> {
    let queries = [
        "SELECT EXISTS(SELECT 1 FROM assets WHERE library=?1 AND id IN (SELECT value FROM json_each(?2)) AND status<>'downloaded')",
        "SELECT EXISTS(SELECT 1 FROM unattributed_legacy WHERE library=?1 AND asset_id IN (SELECT value FROM json_each(?2)))",
        "SELECT EXISTS(SELECT 1 FROM asset_verifications WHERE library=?1 AND id IN (SELECT value FROM json_each(?2)))",
        "SELECT EXISTS(SELECT 1 FROM metadata_capture_retries WHERE library=?1 AND asset_id IN (SELECT value FROM json_each(?2)))",
        "SELECT EXISTS(SELECT 1 FROM assets WHERE library=?1 AND id IN (SELECT value FROM json_each(?2)) AND ((capture_repair_metadata_hash IS NOT NULL AND capture_repair_metadata_hash<>'') OR (capture_repair_output_checksum IS NOT NULL AND capture_repair_output_size IS NOT NULL) OR metadata_write_failed_at IS NOT NULL))",
        "SELECT EXISTS(SELECT 1 FROM asset_metadata_paths WHERE library=?1 AND id IN (SELECT value FROM json_each(?2)) AND ((capture_repair_metadata_hash IS NOT NULL AND capture_repair_metadata_hash<>'') OR (capture_repair_output_checksum IS NOT NULL AND capture_repair_output_size IS NOT NULL) OR metadata_write_failed_at IS NOT NULL))",
        "SELECT EXISTS(SELECT 1 FROM provider_active_destinations WHERE library=?1 AND (asset_id IN (SELECT value FROM json_each(?2)) OR master IN (SELECT value FROM json_each(?2))) AND ((admitted=1 AND (verified_media=0 OR verified_metadata=0)) OR prepared_hash IS NOT NULL OR prepared_checksum IS NOT NULL OR prepared_size IS NOT NULL OR frozen_metadata IS NOT NULL OR frozen_metadata_hash IS NOT NULL))",
        "SELECT EXISTS(SELECT 1 FROM provider_active_decisions WHERE child IN (SELECT value FROM json_each(?2)) AND (outcome='deferred' OR admission='deferred'))",
        "SELECT EXISTS(SELECT 1 FROM provider_work_obligations WHERE library=?1 AND asset_id IN (SELECT value FROM json_each(?2)))",
        "SELECT EXISTS(SELECT 1 FROM primary_layout_bindings WHERE library=?1 AND (family IN (SELECT value FROM json_each(?2)) OR child IN (SELECT value FROM json_each(?2))))",
        "SELECT EXISTS(SELECT 1 FROM primary_layout_operations WHERE (library=?1 OR source_library=?1) AND family IN (SELECT value FROM json_each(?2)) AND phase NOT IN ('committed','cancelled'))",
        "SELECT EXISTS(SELECT 1 FROM primary_layout_claims WHERE library=?1 AND (family IN (SELECT value FROM json_each(?2)) OR child IN (SELECT value FROM json_each(?2))))",
    ];
    for query in queries {
        let protected: bool =
            conn.query_row(query, params![library, ids_json], |row| row.get(0))?;
        if protected {
            return Ok(true);
        }
    }
    super::temp_files::PreparedPublication::protects_family(conn, library, ids)
}

fn catalog_signals(
    conn: &Connection,
    library: &str,
) -> Result<Vec<CatalogPendingSignal>, StateError> {
    let record = super::sync_epoch::read_library(conn, library)?;
    type SignalRow = (String, String, String, String, String, i64, Option<i64>);
    let rows: Vec<SignalRow> = conn
        .prepare(
            "SELECT r.id,r.library,r.scope,r.phase,r.enum_hash,COUNT(d.id),MIN(d.next_retry_at)
             FROM source_revalidation_rounds r
             LEFT JOIN source_revalidation_debt d ON d.round=r.id
             WHERE r.library=?1 AND r.mode=?2
               AND r.phase IN ('marking','observing','active')
              GROUP BY r.id,r.library,r.scope,r.phase,r.enum_hash
             ORDER BY r.id",
        )?
        .query_map(params![library, CATALOG_MODE], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
                row.get(6)?,
            ))
        })?
        .collect::<Result<_, _>>()?;
    let mut signals = Vec::new();
    for (round, library, scope, phase, policy, debt, next_retry) in rows {
        if let Some(record) = &record
            && record.state == "complete"
        {
            let Ok(origin) = CatalogOrigin::parse(&scope) else {
                continue;
            };
            if origin.nonce != round
                || origin.epoch != record.epoch
                || origin.attempt != record.attempt
                || !super::provider_generations::observation::catalog_active_plan_matches(
                    conn,
                    &library,
                    &origin.plan_digest,
                    &policy,
                )?
            {
                continue;
            }
            if phase == "active"
                && !super::provider_generations::observation::catalog_origin_current(
                    conn, &library, &origin, &policy,
                )?
            {
                continue;
            }
        }
        if phase == "active" && debt == 0 {
            continue;
        }
        let pending_debt =
            u64::try_from(debt).map_err(|_error| StateError::ProviderSelectionInvalid)?;
        let due_at = if phase == "marking" {
            0
        } else {
            next_retry.unwrap_or(0)
        };
        signals.push(CatalogPendingSignal {
            round,
            library,
            scope,
            phase,
            due_at,
            pending_debt,
        });
    }
    Ok(signals)
}

/// Validate an installed catalog census before resuming either marking or its
/// completed observing phase. No cursor text or row nonce alone is authority.
fn catalog_seed_phase(
    conn: &Connection,
    library: &str,
    scope: &str,
    enum_hash: &str,
    boundary: &str,
) -> Result<String, StateError> {
    let refuse = || StateError::Invariant {
        operation: "seed_source_revalidation",
        detail: "catalog origin is missing or no longer qualified".into(),
    };
    let origin = CatalogOrigin::parse(scope)?;
    let epoch = super::sync_epoch::validate(conn)?;
    let record = super::sync_epoch::read_library(conn, library)?.ok_or_else(refuse)?;
    if !matches!(record.state.as_str(), "pending" | "complete")
        || record.epoch != epoch.epoch
        || origin.epoch != epoch.epoch
        || origin.attempt != record.attempt
        || super::provider_generations::observation::policy(conn)?[0].as_deref() != Some(enum_hash)
    {
        return Err(refuse());
    }
    // A pending attempt has no ACTIVE descriptor. Its plan binding is the
    // exact catalog scope installed by the fenced claim for this attempt.
    // Complete attempts must also match the live ACTIVE source plan.
    if record.state == "complete"
        && !super::provider_generations::observation::catalog_active_plan_matches(
            conn,
            library,
            &origin.plan_digest,
            enum_hash,
        )?
    {
        return Err(refuse());
    }
    let prefix = format!(
        "baseline_cursor:v27:{}:{}:{}:",
        epoch.epoch, record.baseline_scope, enum_hash
    );
    let mut statement =
        conn.prepare("SELECT value FROM metadata WHERE substr(key,1,length(?1))=?1")?;
    let saved = statement.query_map([prefix], |row| row.get::<_, String>(0))?;
    let mut matching_boundary = false;
    for cursor in saved {
        let cursor = cursor?;
        if !cursor.trim().is_empty() && CatalogOrigin::boundary(&cursor) == boundary {
            matching_boundary = true;
        }
    }
    if !matching_boundary {
        return Err(refuse());
    }
    let phase: Option<String> = conn.query_row(
        "SELECT phase FROM source_revalidation_rounds WHERE id=?1 AND library=?2 AND scope=?3 AND enum_hash=?4 AND boundary=?5 AND mode=?6 AND phase IN ('marking','observing')",
        params![origin.nonce, library, scope, enum_hash, boundary, CATALOG_MODE], |row| row.get(0),
    ).optional()?;
    phase.ok_or_else(refuse)
}

pub(crate) fn has_due(conn: &rusqlite::Connection, library: &str) -> Result<bool, StateError> {
    let scopes: Vec<(String, String)> = conn.prepare("SELECT DISTINCT r.scope,r.enum_hash FROM source_revalidation_rounds r JOIN source_revalidation_debt d ON d.round=r.id WHERE r.library=?1 AND r.phase='active' AND r.mode=?3 AND r.enum_hash=COALESCE((SELECT value FROM metadata WHERE key='pending_enum_config_hash'),(SELECT value FROM metadata WHERE key='enum_config_hash')) AND d.next_retry_at<=?2")?
        .query_map(params![library, chrono::Utc::now().timestamp(), LEGACY_MODE], |r| Ok((r.get(0)?,r.get(1)?)))?.collect::<Result<_,_>>()?;
    for (scope, policy) in scopes {
        if super::provider_generations::observation::revalidation_scope_current(
            conn, library, &scope, &policy,
        )? {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Count active-round obligations separately from existing asset verification.
/// Deferred obligations remain eligible; unfinished rounds and stale scopes remain retained.
/// Superseded abandoned rounds remain durable but are not future obligations.
pub(crate) fn summary_counts(conn: &rusqlite::Connection) -> Result<(u64, u64), StateError> {
    let groups: Vec<(String, String, String, String, String, i64)> = conn.prepare(
        "SELECT r.library,r.mode,r.scope,r.enum_hash,r.phase,COUNT(*) FROM source_revalidation_rounds r JOIN source_revalidation_debt d ON d.round=r.id WHERE r.phase != 'abandoned' GROUP BY r.library,r.mode,r.scope,r.enum_hash,r.phase"
    )?.query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?)))?.collect::<Result<_,_>>()?;
    let effective: Option<String> = conn.query_row(
        "SELECT COALESCE((SELECT value FROM metadata WHERE key='pending_enum_config_hash'),(SELECT value FROM metadata WHERE key='enum_config_hash'))", [], |row| row.get(0)
    )?;
    let mut eligible = 0u64;
    let mut retained = 0u64;
    for (library, mode, scope, policy, phase, count) in groups {
        let count = u64::try_from(count).map_err(|_error| StateError::ProviderSelectionInvalid)?;
        if mode == LEGACY_MODE
            && phase == "active"
            && effective.as_deref() == Some(policy.as_str())
            && super::provider_generations::observation::revalidation_scope_current(
                conn, &library, &scope, &policy,
            )?
        {
            eligible = eligible.saturating_add(count);
        } else {
            retained = retained.saturating_add(count);
        }
    }
    Ok((eligible, retained))
}

impl SqliteStateDb {
    /// Capture the catalog state before obtaining the provider boundary. This
    /// also fences equal-cursor replacements against concurrent installations.
    #[allow(dead_code)] // L1 cycle integration is pending.
    pub(crate) async fn fresh_catalog_snapshot(
        &self,
        owner: account::AccountOwner,
        library: String,
    ) -> Result<String, StateError> {
        self.with_conn("reading fresh catalog fence", move |conn| {
            account::validate_authenticated(conn, &owner)?;
            catalog_snapshot(conn, &library)
        })
        .await
    }
    /// Claim an epoch baseline and, only for a validated pending-due old
    /// catalog origin, transfer census ownership to the new attempt in the same
    /// IMMEDIATE transaction as the unchanged epoch gate.
    #[allow(dead_code)] // L1 catalog cycle integration is pending; focused owner tests exercise this API.
    pub(crate) async fn claim_catalog_baseline(
        &self,
        owner: account::AccountOwner,
        claim: CatalogBaselineClaim,
    ) -> Result<CatalogClaimOutcome, StateError> {
        self.with_conn_mut("claiming catalog baseline", move |conn| {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            account::validate_authenticated(&tx, &owner)?;
            for (key, expected) in &claim.expected_metadata {
                let actual: Option<String> = tx
                    .query_row(
                        "SELECT value FROM metadata WHERE key=?1",
                        [key],
                        |row| row.get(0),
                    )
                    .optional()?;
                if actual != *expected {
                    return Err(StateError::Invariant {
                        operation: "claim_catalog_baseline",
                        detail: "catalog baseline metadata fence changed".into(),
                    });
                }
            }
            let actual_baseline: Option<String> = tx
                .query_row(
                    "SELECT value FROM metadata WHERE key=?1",
                    [&claim.baseline_key],
                    |row| row.get(0),
                )
                .optional()?;
            if actual_baseline != claim.expected_baseline {
                return Err(StateError::Invariant {
                    operation: "claim_catalog_baseline",
                    detail: "catalog baseline cursor changed".into(),
                });
            }
            let epoch = super::sync_epoch::validate(&tx)?;
            if epoch.epoch != claim.expected_epoch
                || serde_json::to_string(&epoch)
                    .map_err(|_invalid| StateError::ProviderSelectionInvalid)?
                    != claim.expected_epoch_json
            {
                return Err(StateError::Invariant {
                    operation: "claim_catalog_baseline",
                    detail: "catalog epoch fence changed".into(),
                });
            }
            let library_before = super::sync_epoch::read_library(&tx, &claim.library)?
                .ok_or(StateError::SyncEpochMismatch)?;
            if serde_json::to_string(&library_before)
                .map_err(|_invalid| StateError::ProviderSelectionInvalid)?
                != claim.expected_library_json
            {
                return Err(StateError::Invariant {
                    operation: "claim_catalog_baseline",
                    detail: "catalog library record fence changed".into(),
                });
            }
            let mut matching: Vec<ExistingCatalogRound> = Vec::new();
            if let Some(cursor) = claim.expected_baseline.as_deref() {
                let boundary = CatalogOrigin::boundary(cursor);
                let rows: Vec<(String, String, String, String, String, String, String)> = tx
                    .prepare(
                        "SELECT id,scope,enum_hash,boundary,high_id,after_id,phase
                         FROM source_revalidation_rounds
                         WHERE library=?1 AND mode=?2 AND phase!='abandoned'
                           AND boundary=?3 AND scope LIKE ?4
                         ORDER BY id",
                    )?
                    .query_map(
                        params![
                            &claim.library,
                            CATALOG_MODE,
                            boundary,
                            format!("{CATALOG_SCOPE_PREFIX}%")
                        ],
                        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?, row.get(6)?)),
                    )?
                    .collect::<Result<_, _>>()?;
                for (id, scope, enum_hash, boundary, high_id, after_id, phase) in rows {
                    let Ok(origin) = CatalogOrigin::parse(&scope) else { continue };
                    if !matches!(library_before.state.as_str(), "pending" | "ready")
                        || library_before.epoch != epoch.epoch
                        || origin.epoch != epoch.epoch
                        || origin.attempt != library_before.attempt
                        || origin.nonce != id
                        || origin.plan_digest != claim.plan_digest
                        || enum_hash != claim.expected_enum_hash
                    {
                        continue;
                    }
                    matching.push(ExistingCatalogRound { id, scope, origin, enum_hash, boundary, high_id, after_id, phase });
                }
            }
            if library_before.state != "ready" && matching.len() > 1 {
                return Err(StateError::Invariant {
                    operation: "claim_catalog_baseline",
                    detail: "multiple catalog origins share the baseline".into(),
                });
            }
            let old = matching.first().cloned();
            let gate = super::sync_epoch::gate(
                &tx,
                &claim.library,
                &library_before.baseline_scope,
                claim.now,
                claim.watch_interval,
            )?;
            let outcome = match gate {
                super::sync_epoch::SyncEpochGate::Baseline { attempt } => {
                    if library_before.state == "ready"
                        && library_before.binding.as_ref().is_none_or(|binding| {
                            binding.baseline_key != claim.baseline_key
                                || claim.expected_baseline.as_deref().is_none_or(|cursor| {
                                    CatalogOrigin::boundary(cursor) != binding.baseline_sha256
                                })
                        })
                    {
                        return Err(StateError::Invariant {
                            operation: "claim_catalog_baseline",
                            detail: "stale ready baseline binding changed".into(),
                        });
                    }
                    if matching.len() > 1 {
                        return Err(StateError::Invariant {
                            operation: "claim_catalog_baseline",
                            detail: "multiple catalog origins share the baseline".into(),
                        });
                    }
                    if library_before.state == "ready"
                        && old.as_ref().is_some_and(|round| !matches!(round.phase.as_str(), "marking" | "observing"))
                    {
                        return Err(StateError::Invariant {
                            operation: "claim_catalog_baseline",
                            detail: "stale ready catalog origin is not resumable".into(),
                        });
                    }
                    let pending_due = library_before.state == "pending"
                        && claim.now >= library_before.next_attempt_at;
                    if let Some(old) = &old
                        && (pending_due || library_before.state == "ready")
                        && old.origin.attempt == library_before.attempt
                        && matches!(old.phase.as_str(), "marking" | "observing")
                    {
                        let nonce = uuid::Uuid::new_v4().to_string();
                        let new_scope = CatalogOrigin::new(
                            claim.plan_digest.clone(),
                            epoch.epoch.clone(),
                            attempt.clone(),
                            nonce.clone(),
                        )?
                        .scope();
                        tx.execute(
                            "INSERT INTO source_revalidation_rounds(id,library,scope,enum_hash,boundary,high_id,after_id,phase,mode)
                             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)",
                            params![
                                nonce,
                                claim.library,
                                new_scope,
                                old.enum_hash,
                                old.boundary,
                                old.high_id,
                                old.after_id,
                                old.phase,
                                CATALOG_MODE
                            ],
                        )?;
                        let copied_debt = tx.execute(
                            "INSERT INTO source_revalidation_debt(round,id,next_retry_at)
                             SELECT ?1,id,next_retry_at FROM source_revalidation_debt WHERE round=?2",
                            params![nonce, old.id],
                        )? as u64;
                        tx.execute(
                            "UPDATE source_revalidation_rounds SET phase='abandoned' WHERE id=?1",
                            [&old.id],
                        )?;
                        tx.commit()?;
                        return Ok(CatalogClaimOutcome::Transferred {
                            old_round: old.id.clone(),
                            new_round: nonce,
                            attempt,
                            copied_debt,
                        });
                    }
                    for round in &matching {
                        tx.execute(
                            "UPDATE source_revalidation_rounds SET phase='abandoned' WHERE id=?1",
                            [&round.id],
                        )?;
                    }
                    CatalogClaimOutcome::Baseline { attempt }
                }
                super::sync_epoch::SyncEpochGate::Promote { attempt } => {
                    CatalogClaimOutcome::Promote { attempt }
                }
                super::sync_epoch::SyncEpochGate::Defer { reason } => {
                    CatalogClaimOutcome::Defer { reason }
                }
                super::sync_epoch::SyncEpochGate::Blocked { reason } => {
                    CatalogClaimOutcome::Blocked { reason }
                }
                super::sync_epoch::SyncEpochGate::Open => CatalogClaimOutcome::Open,
            };
            tx.commit()?;
            Ok(outcome)
        })
        .await
    }

    /// Install a fresh catalog origin and the fresh baseline cursor atomically.
    /// This grants no checkpoint, deletion, selection or promotion authority.
    #[allow(dead_code)] // L1 cycle integration is pending; focused owner tests exercise this API.
    pub(crate) async fn install_fresh_catalog_origin(
        &self,
        owner: account::AccountOwner,
        request: FreshCatalogOriginRequest,
    ) -> Result<CatalogOrigin, StateError> {
        match self
            .catalog_origin_operation(owner, request, CatalogOriginOperation::Fresh)
            .await?
        {
            CatalogOriginOperationResult::Installed(origin) => Ok(origin),
            _ => Err(StateError::SyncEpochMismatch),
        }
    }

    /// Commit a completed observed delta and install its successor as a new
    /// baseline in the same transaction. The proof is made by the existing
    /// completed-stream producer; a bare cursor cannot call this operation.
    #[allow(dead_code)] // L1 cycle integration is pending.
    pub(crate) async fn install_catalog_origin_after_delta(
        &self,
        owner: account::AccountOwner,
        request: FreshCatalogOriginRequest,
        transition: super::CheckpointTransition,
        proof: super::provider_generations::observation::ObservationProof,
    ) -> Result<CatalogOrigin, StateError> {
        match self
            .catalog_origin_operation(
                owner,
                request,
                CatalogOriginOperation::ObservedDelta(Box::new((transition, proof))),
            )
            .await?
        {
            CatalogOriginOperationResult::Installed(origin) => Ok(origin),
            _ => Err(StateError::SyncEpochMismatch),
        }
    }

    /// Read a pre-stream hint. `request.fresh_cursor` is the current token T,
    /// not an EOF boundary. A later bootstrap must replace it with actual D.
    /// An error means the caller must retain the checkpoint and work.
    #[allow(dead_code)] // L1 cycle integration is pending.
    pub(crate) async fn catalog_bootstrap_eligibility(
        &self,
        owner: account::AccountOwner,
        request: FreshCatalogOriginRequest,
    ) -> Result<CatalogBootstrapEligibility, StateError> {
        match self
            .catalog_origin_operation(owner, request, CatalogOriginOperation::Eligibility)
            .await?
        {
            CatalogOriginOperationResult::Eligibility(hint) => Ok(hint),
            _ => Err(StateError::SyncEpochMismatch),
        }
    }

    /// Read an exact saved census boundary and typed origin. This grants no
    /// checkpoint or provider-completeness authority. For a complete epoch,
    /// `fresh_cursor` is the current token; for pending it is unused.
    #[allow(dead_code)] // L1 cycle integration is pending.
    pub(crate) async fn catalog_resume(
        &self,
        owner: account::AccountOwner,
        request: FreshCatalogOriginRequest,
    ) -> Result<Option<CatalogResume>, StateError> {
        match self
            .catalog_origin_operation(owner, request, CatalogOriginOperation::Resume)
            .await?
        {
            CatalogOriginOperationResult::Resume(resume) => Ok(resume),
            _ => Err(StateError::SyncEpochMismatch),
        }
    }

    async fn catalog_origin_operation(
        &self,
        owner: account::AccountOwner,
        request: FreshCatalogOriginRequest,
        operation: CatalogOriginOperation,
    ) -> Result<CatalogOriginOperationResult, StateError> {
        self.with_conn_mut("installing fresh catalog origin", move |conn| {
            let behavior = match operation {
                CatalogOriginOperation::Eligibility | CatalogOriginOperation::Resume => TransactionBehavior::Deferred,
                _ => TransactionBehavior::Immediate,
            };
            let tx = conn.transaction_with_behavior(behavior)?;
            account::validate_authenticated(&tx, &owner)?;
            if catalog_snapshot(&tx, &request.library)? != request.expected_catalog_snapshot {
                return Err(StateError::Invariant { operation: "install_fresh_catalog_origin", detail: "catalog round fence changed".into() });
            }
            let (owner_account, owner_provider) = owner.authenticated_keys()?;
            let (account_key, provider_key): (String, String) = tx.query_row(
                "SELECT account_key,provider_key FROM account_owner WHERE singleton=1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?;
            if owner_account != account_key
                || owner_provider != provider_key
                || request.plan.account != account_key
                || request.plan.provider != provider_key
                || !matches!(request.plan.database.as_str(), "private" | "shared")
                || request.plan.zone.get("zoneName").and_then(serde_json::Value::as_str)
                    != Some(request.library.as_str())
                || request.plan.zone.get("ownerRecordName").and_then(serde_json::Value::as_str)
                    .is_none_or(|owner| owner.trim().is_empty())
            {
                return Err(StateError::Invariant {
                    operation: "install_fresh_catalog_origin",
                    detail: "authenticated source pin changed".into(),
                });
            }
            let plan_scope: serde_json::Value = serde_json::from_str(&request.plan.scope)
                .map_err(|_invalid| StateError::ProviderSelectionInvalid)?;
            if ["zoneName", "ownerRecordName"].iter().any(|field| plan_scope.pointer(&format!("/zone/{field}")) != request.plan.zone.get(*field))
                || plan_scope.get("database").and_then(serde_json::Value::as_str)
                    != Some(request.plan.database.as_str())
                || request.plan.profile.get("passes")
                    != Some(&serde_json::Value::Array(request.plan.passes.iter().map(|pass| {
                        serde_json::json!({"key": pass.key, "kind": pass.kind, "scope": pass.scope, "excludes": pass.excludes})
                    }).collect()))
            {
                return Err(StateError::Invariant {
                    operation: "install_fresh_catalog_origin",
                    detail: "catalog source scope changed".into(),
                });
            }
            let projection = crate::icloud::photos::PhotoLibrary::provider_scope_projection(&request.plan.scope)
                .ok_or(StateError::ProviderSelectionInvalid)?;
            if projection != (
                request.library.clone(),
                request.plan.zone.get("ownerRecordName").and_then(serde_json::Value::as_str).unwrap_or_default().to_owned(),
                request.plan.database.clone(),
            ) {
                return Err(StateError::ProviderSelectionInvalid);
            }
            let spec = super::provider_generations::GenerationSpec {
                format: 2,
                scope: request.plan.scope.clone(),
                zone: request.plan.zone.clone(),
                config_hash: request.plan.config_hash.clone(),
                basis: request.plan.config_hash.clone(),
                profile: request.plan.profile.clone(),
                metadata_enabled: request.plan.metadata_enabled,
                metadata_flags: request.plan.metadata_flags,
            };
            super::provider_generations::validate_catalog_plan(&tx, &spec, &request.library)?;
            if super::provider_generations::observation::policy(&tx)?[0].as_deref() != Some(request.enum_hash.as_str()) {
                return Err(StateError::ProviderSelectionInvalid);
            }
            let required = [
                super::sync_epoch::SYNC_EPOCH_KEY.to_owned(),
                super::sync_epoch::library_key(&request.library),
                "enum_config_hash".to_owned(),
                "pending_enum_config_hash".to_owned(),
                "config_hash".to_owned(),
                "pending_download_config_hash".to_owned(),
                format!("sync_token:v27:{}", request.library),
                format!("observed_selection:v27:{}", request.library),
                format!("download_ledger_scope:{}", request.library),
            ];
            if required.iter().any(|key| request.expected_metadata.iter().filter(|(saved, _)| saved == key).count() != 1) {
                return Err(StateError::Invariant { operation: "install_fresh_catalog_origin", detail: "missing or duplicate catalog metadata fence".into() });
            }
            for (key, expected) in &request.expected_metadata {
                let actual: Option<String> = tx
                    .query_row(
                        "SELECT value FROM metadata WHERE key=?1",
                        [key],
                        |row| row.get(0),
                    )
                    .optional()?;
                if actual != *expected {
                    return Err(StateError::Invariant {
                        operation: "install_fresh_catalog_origin",
                        detail: "catalog metadata fence changed".into(),
                    });
                }
            }
            let epoch = super::sync_epoch::validate(&tx)?;
            if epoch.epoch != request.expected_epoch {
                return Err(StateError::Invariant {
                    operation: "install_fresh_catalog_origin",
                    detail: "catalog epoch changed".into(),
                });
            }
            let library_record = super::sync_epoch::read_library(&tx, &request.library)?
                .ok_or(StateError::SyncEpochMismatch)?;
            if !matches!(library_record.state.as_str(), "complete" | "pending")
                || library_record.epoch != epoch.epoch
                || library_record.attempt != request.expected_attempt
                || library_record.baseline_scope != format!("{:x}", Sha256::digest(serde_json::json!({"database":request.plan.database,"zone":request.plan.zone}).to_string().as_bytes()))
            {
                return Err(StateError::Invariant {
                    operation: "install_fresh_catalog_origin",
                    detail: "catalog library attempt changed".into(),
                });
            }
            let expected_key = format!("baseline_cursor:v27:{}:{}:{}:{}", epoch.epoch, library_record.baseline_scope, request.enum_hash, request.preservation_config_hash);
            if request.baseline_key != expected_key
                || super::provider_generations::observation::has_valid_staged_for_library(&tx, &request.library)?
            {
                return Err(StateError::Invariant { operation: "install_fresh_catalog_origin", detail: "catalog baseline key or staged promotion changed".into() });
            }
            let epoch_raw = serde_json::to_string(&epoch)
                .map_err(|_invalid| StateError::ProviderSelectionInvalid)?;
            let library_raw = serde_json::to_string(&library_record)
                .map_err(|_invalid| StateError::ProviderSelectionInvalid)?;
            for (key, expected) in [
                (
                    super::sync_epoch::SYNC_EPOCH_KEY.to_owned(),
                    Some(epoch_raw),
                ),
                (
                    super::sync_epoch::library_key(&request.library),
                    Some(library_raw),
                ),
            ] {
                let actual: Option<String> = tx
                    .query_row(
                        "SELECT value FROM metadata WHERE key=?1",
                        [&key],
                        |row| row.get(0),
                    )
                    .optional()?;
                if actual != expected {
                    return Err(StateError::Invariant {
                        operation: "install_fresh_catalog_origin",
                        detail: "catalog raw epoch fence changed".into(),
                    });
                }
            }
            let actual_baseline: Option<String> = tx
                .query_row(
                    "SELECT value FROM metadata WHERE key=?1",
                    [&request.baseline_key],
                    |row| row.get(0),
                )
                .optional()?;
            if actual_baseline != request.expected_baseline || (!matches!(operation, CatalogOriginOperation::Resume) && request.fresh_cursor.trim().is_empty())
            {
                return Err(StateError::Invariant {
                    operation: "install_fresh_catalog_origin",
                    detail: "catalog baseline cursor fence changed".into(),
                });
            }
            let digest = fresh_catalog_plan_digest(&request.plan);
            if library_record.state == "complete" && !super::provider_generations::observation::catalog_active_plan_matches(
                &tx,
                &request.library,
                &digest,
                &request.enum_hash,
            )? {
                return Err(StateError::Invariant {
                    operation: "install_fresh_catalog_origin",
                    detail: "current ACTIVE source plan changed".into(),
                });
            }
            if matches!(operation, CatalogOriginOperation::Resume) {
                if library_record.state == "complete"
                    && !super::provider_generations::observation::catalog_resume_predecessor_matches(
                        &tx, &request.library, &request.fresh_cursor, &digest, &request.enum_hash,
                    )?
                {
                    return Err(StateError::SyncEpochMismatch);
                }
                let Some(boundary) = actual_baseline.filter(|value| !value.trim().is_empty()) else {
                    tx.commit()?;
                    return Ok(CatalogOriginOperationResult::Resume(None));
                };
                let hash = CatalogOrigin::boundary(&boundary);
                let rows: Vec<(String,String,String,String,String)> = tx.prepare(
                    "SELECT id,scope,enum_hash,mode,phase FROM source_revalidation_rounds WHERE library=?1 AND boundary=?2 AND phase!='abandoned' ORDER BY id"
                )?.query_map(params![request.library,hash], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?,row.get(4)?)))?.collect::<Result<_,_>>()?;
                let [(id, scope, enum_hash, mode, phase)] = rows.as_slice() else {
                    if rows.is_empty() {
                        tx.commit()?;
                        return Ok(CatalogOriginOperationResult::Resume(None));
                    }
                    return Err(StateError::SyncEpochMismatch);
                };
                let origin = CatalogOrigin::parse(scope)?;
                if mode != CATALOG_MODE || enum_hash != &request.enum_hash || origin.nonce != *id
                    || origin.plan_digest != digest || origin.epoch != epoch.epoch
                    || origin.attempt != library_record.attempt
                {
                    return Err(StateError::SyncEpochMismatch);
                }
                if phase == "active" {
                    tx.commit()?;
                    return Ok(CatalogOriginOperationResult::Resume(None));
                }
                if !matches!(phase.as_str(), "marking" | "observing") {
                    return Err(StateError::SyncEpochMismatch);
                }
                catalog_seed_phase(&tx, &request.library, scope, &request.enum_hash, &hash)?;
                let relation = if library_record.state == "pending" {
                    CatalogResumeRelation::PendingAttempt
                } else if request.fresh_cursor == boundary {
                    CatalogResumeRelation::CurrentToken
                } else {
                    CatalogResumeRelation::SavedBoundary
                };
                tx.commit()?;
                return Ok(CatalogOriginOperationResult::Resume(Some(CatalogResume { origin, boundary, relation })));
            }
            if matches!(operation, CatalogOriginOperation::Eligibility) {
                if library_record.state != "complete" {
                    return Err(StateError::SyncEpochMismatch);
                }
                let hint = super::provider_generations::observation::catalog_bootstrap_eligibility(
                    &tx, &request.library, &request.fresh_cursor, &digest, &request.enum_hash,
                )?;
                tx.commit()?;
                return Ok(CatalogOriginOperationResult::Eligibility(hint));
            }
            let required_delta = if let CatalogOriginOperation::ObservedDelta(delta) = operation {
                let (transition, proof) = *delta;
                if library_record.state != "complete" {
                    return Err(StateError::SyncEpochMismatch);
                }
                let required = super::provider_generations::observation::require_catalog_delta(
                    &tx, &transition, &proof, &request.library, &digest, &request.expected_attempt, &request.fresh_cursor,
                )?;
                super::checkpoints::commit_observation_transition_in_tx(&tx, transition, Some(proof), false)?;
                super::provider_generations::observation::verify_catalog_delta(&tx, &required)?;
                account::validate_authenticated(&tx, &owner)?;
                let token_key = format!("sync_token:v27:{}", request.library);
                let descriptor_key = format!("observed_selection:v27:{}", request.library);
                for (key, expected) in &request.expected_metadata {
                    if key == &token_key || key == &descriptor_key {
                        continue;
                    }
                    let actual: Option<String> = tx.query_row("SELECT value FROM metadata WHERE key=?1", [key], |row| row.get(0)).optional()?;
                    if actual != *expected {
                        return Err(StateError::SyncEpochMismatch);
                    }
                }
                let baseline: Option<String> = tx.query_row("SELECT value FROM metadata WHERE key=?1", [&request.baseline_key], |row| row.get(0)).optional()?;
                if baseline != request.expected_baseline
                    || catalog_snapshot(&tx, &request.library)? != request.expected_catalog_snapshot
                    || super::provider_generations::observation::has_valid_staged_for_library(&tx, &request.library)?
                {
                    return Err(StateError::SyncEpochMismatch);
                }
                Some(required)
            } else {
                None
            };
            let boundary = CatalogOrigin::boundary(&request.fresh_cursor);
            let high_id: Option<String> = tx.query_row(
                "SELECT MAX(id) FROM assets WHERE library=?1 AND (status='downloaded' OR downloaded_at IS NOT NULL) AND is_deleted=0",
                [&request.library],
                |row| row.get(0),
            )?;
            let high_id = high_id.unwrap_or_default();
            let nonce = uuid::Uuid::new_v4().to_string();
            let origin = CatalogOrigin::new(
                digest,
                request.expected_epoch.clone(),
                request.expected_attempt.clone(),
                nonce.clone(),
            )?;
            tx.execute(
                "INSERT INTO source_revalidation_rounds(id,library,scope,enum_hash,boundary,high_id,after_id,phase,mode)
                 VALUES(?1,?2,?3,?4,?5,?6,'','marking',?7)",
                params![
                    nonce,
                    request.library,
                    origin.scope(),
                    request.enum_hash,
                    boundary,
                    high_id,
                    CATALOG_MODE
                ],
            )?;
            // A replacement must not strand retry deadlines behind an older
            // ACTIVE descriptor. Keep the old rounds and debt as history, and
            // carry matching obligations into the new census without delaying
            // any earlier retry.
            let previous: Vec<(String, String)> = tx.prepare(
                "SELECT id,scope FROM source_revalidation_rounds WHERE library=?1 AND mode=?2 AND enum_hash=?3 AND id<>?4 AND phase!='abandoned'"
            )?.query_map(params![request.library, CATALOG_MODE, request.enum_hash, nonce], |row| Ok((row.get(0)?, row.get(1)?)))?.collect::<Result<_, _>>()?;
            for (id, scope) in previous {
                let Ok(old) = CatalogOrigin::parse(&scope) else { continue };
                if old.nonce != id || old.plan_digest != origin.plan_digest || old.epoch != origin.epoch || old.attempt != origin.attempt {
                    continue;
                }
                tx.execute(
                    "INSERT INTO source_revalidation_debt(round,id,next_retry_at)
                     SELECT ?1,id,next_retry_at FROM source_revalidation_debt WHERE round=?2
                     ON CONFLICT(round,id) DO UPDATE SET next_retry_at=MIN(next_retry_at,excluded.next_retry_at)",
                    params![nonce, id],
                )?;
                tx.execute(
                    "UPDATE source_revalidation_rounds SET phase='abandoned' WHERE id=?1",
                    [&id],
                )?;
            }
            tx.execute(
                "INSERT INTO metadata(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                params![request.baseline_key, request.fresh_cursor],
            )?;
            if let Some(required) = &required_delta {
                super::provider_generations::observation::verify_catalog_delta(&tx, required)?;
                account::validate_authenticated(&tx, &owner)?;
                let token_key = format!("sync_token:v27:{}", request.library);
                let descriptor_key = format!("observed_selection:v27:{}", request.library);
                for (key, expected) in &request.expected_metadata {
                    if key == &token_key || key == &descriptor_key || key == &request.baseline_key {
                        continue;
                    }
                    let actual: Option<String> = tx.query_row("SELECT value FROM metadata WHERE key=?1", [key], |row| row.get(0)).optional()?;
                    if actual != *expected {
                        return Err(StateError::SyncEpochMismatch);
                    }
                }
                let baseline: Option<String> = tx.query_row("SELECT value FROM metadata WHERE key=?1", [&request.baseline_key], |row| row.get(0)).optional()?;
                let exact: bool = tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM source_revalidation_rounds WHERE id=?1 AND library=?2 AND scope=?3 AND enum_hash=?4 AND boundary=?5 AND high_id=?6 AND after_id='' AND phase='marking' AND mode=?7)",
                    params![origin.nonce, request.library, origin.scope(), request.enum_hash, boundary, high_id, CATALOG_MODE], |row| row.get(0),
                )?;
                if baseline.as_deref() != Some(request.fresh_cursor.as_str()) || !exact
                    || super::provider_generations::observation::has_valid_staged_for_library(&tx, &request.library)?
                {
                    return Err(StateError::SyncEpochMismatch);
                }
            }
            tx.commit()?;
            Ok(CatalogOriginOperationResult::Installed(origin))
        })
        .await
    }

    /// Finish bounded candidate marking before the corresponding inventory begins.
    pub(crate) async fn seed_source_revalidation(
        &self,
        owner: account::AccountOwner,
        library: String,
        scope: String,
        enum_hash: String,
        boundary: String,
        shutdown: tokio_util::sync::CancellationToken,
    ) -> Result<(), StateError> {
        let initial_owner = owner.clone();
        let catalog = scope.starts_with("catalog:").then(|| {
            (
                library.clone(),
                scope.clone(),
                enum_hash.clone(),
                boundary.clone(),
            )
        });
        let round = self.with_conn_mut("starting source revalidation", move |conn| {
            let tx=conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            account::validate_authenticated(&tx,&initial_owner)?;
            if scope.starts_with("catalog:") {
                catalog_seed_phase(&tx, &library, &scope, &enum_hash, &boundary)?;
                let origin = CatalogOrigin::parse(&scope)?;
                tx.commit()?;
                return Ok(origin.nonce);
            }
            let existing:Option<String>=tx.query_row(
                "SELECT id FROM source_revalidation_rounds WHERE library=?1 AND scope=?2 AND enum_hash=?3 AND boundary=?4 AND phase='marking' ORDER BY id LIMIT 1",
                params![library,scope,enum_hash,boundary], |r| r.get(0)).optional()?;
            let id=if let Some(id)=existing {id} else {
                tx.execute("UPDATE source_revalidation_rounds SET phase='abandoned' WHERE library=?1 AND scope=?2 AND enum_hash=?3 AND boundary=?4 AND phase='observing'", params![library,scope,enum_hash,boundary])?;
                let id=uuid::Uuid::new_v4().to_string();
                let high:Option<String>=tx.query_row(
                    "SELECT MAX(id) FROM assets WHERE library=?1 AND (status='downloaded' OR downloaded_at IS NOT NULL) AND is_deleted=0",
                    [&library],|r|r.get(0))?;
                tx.execute("INSERT INTO source_revalidation_rounds(id,library,scope,enum_hash,boundary,high_id,after_id,phase) VALUES(?1,?2,?3,?4,?5,?6,'','marking')",
                    params![id,library,scope,enum_hash,boundary,high.unwrap_or_default()])?;
                id
            };
            tx.commit()?;Ok(id)
        }).await?;
        loop {
            if shutdown.is_cancelled() {
                return Ok(());
            }
            let id = round.clone();
            let owner = owner.clone();
            let catalog = catalog.clone();
            let done=self.with_conn_mut("marking source revalidation page",move |conn| {
                let tx=conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
                account::validate_authenticated(&tx, &owner)?;
                if let Some((library, scope, enum_hash, boundary)) = &catalog {
                    let phase = catalog_seed_phase(&tx, library, scope, enum_hash, boundary)?;
                    if phase == "observing" { tx.commit()?; return Ok(true); }
                }
                let (library,high,after,phase):(String,String,String,String)=tx.query_row(
                    "SELECT library,high_id,after_id,phase FROM source_revalidation_rounds WHERE id=?1",
                    [&id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?)))?;
                if phase!="marking" {tx.commit()?;return Ok(true)}
                let rows:Vec<String>=tx.prepare("SELECT DISTINCT id FROM assets WHERE library=?1 AND id>?2 AND id<=?3 AND (status='downloaded' OR downloaded_at IS NOT NULL) AND is_deleted=0 ORDER BY id LIMIT 64")?
                    .query_map(params![library,after,high],|r|r.get(0))?.collect::<Result<_,_>>()?;
                for child in &rows {
                    tx.execute("INSERT OR IGNORE INTO source_revalidation_debt(round,id) VALUES(?1,?2)",params![id,child])?;
                }
                if let Some(last)=rows.last() {
                    tx.execute("UPDATE source_revalidation_rounds SET after_id=?2 WHERE id=?1",params![id,last])?;
                } else {
                    tx.execute("UPDATE source_revalidation_rounds SET phase='observing' WHERE id=?1",[&id])?;
                }
                let done=rows.is_empty(); tx.commit()?;Ok(done)
            }).await?;
            if done {
                return Ok(());
            }
        }
    }

    pub(crate) async fn source_revalidation_seen(
        &self,
        owner: account::AccountOwner,
        library: String,
        scope: String,
        id: String,
    ) -> Result<(), StateError> {
        if scope.starts_with(CATALOG_SCOPE_PREFIX) {
            return Ok(());
        }
        self.with_conn_mut("settling observed source", move |conn| {
            let tx=conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            account::validate_authenticated(&tx,&owner)?;
            tx.execute("DELETE FROM source_revalidation_debt WHERE id=?1 AND round IN
                (SELECT id FROM source_revalidation_rounds WHERE library=?2 AND scope=?3 AND phase='observing')",
                params![id,library,scope])?;
            tx.commit()?;Ok(())
        }).await
    }

    /// Bounded catalog pending-origin signal for the watch precheck. Marking
    /// resumes at the next normal wake; observing/active debt reports its
    /// earliest saved deadline. The caller must still apply its configured
    /// cadence, so this is not an immediate rescan loop.
    #[allow(dead_code)] // L1 precheck integration is pending.
    pub(crate) async fn catalog_pending_signals(
        &self,
        owner: account::AccountOwner,
        library: String,
    ) -> Result<Vec<CatalogPendingSignal>, StateError> {
        self.with_conn("reading catalog pending signals", move |conn| {
            account::validate_authenticated(conn, &owner)?;
            catalog_signals(conn, &library)
        })
        .await
    }

    /// Typed per-source retained/pending outcome for the private report/hold
    /// seam. Catalog debt is visible but never a checkpoint veto or a fake
    /// failed-asset count.
    #[allow(dead_code)] // L1 report/hold integration is pending.
    pub(crate) async fn catalog_source_outcome(
        &self,
        owner: account::AccountOwner,
        library: String,
    ) -> Result<CatalogSourceOutcome, StateError> {
        self.with_conn("reading catalog source outcome", move |conn| {
            account::validate_authenticated(conn, &owner)?;
            let signals = catalog_signals(conn, &library)?;
            let retained: i64 = conn.query_row(
                "SELECT COUNT(*) FROM source_revalidation_debt d JOIN source_revalidation_rounds r ON r.id=d.round WHERE r.library=?1 AND r.mode=?2 AND r.phase!='abandoned'",
                params![library, CATALOG_MODE], |row| row.get(0),
            )?;
            let retained = u64::try_from(retained).map_err(|_error| StateError::ProviderSelectionInvalid)?;
            let next_due_at = signals.iter().map(|signal| signal.due_at).min();
            Ok(CatalogSourceOutcome {
                pending: !signals.is_empty() || retained != 0,
                retained,
                next_due_at,
            })
        })
        .await
    }

    #[allow(dead_code)] // L1 catalog drain integration is pending; focused owner tests exercise this.
    pub(crate) async fn due_catalog_candidates(
        &self,
        owner: account::AccountOwner,
        library: String,
        origin: CatalogOrigin,
        enum_hash: String,
    ) -> Result<Vec<SourceCandidate>, StateError> {
        self.with_conn("reading due catalog candidates", move |conn| {
            account::validate_authenticated(conn, &owner)?;
            if !super::provider_generations::observation::catalog_origin_current(
                conn,
                &library,
                &origin,
                &enum_hash,
            )? {
                return Ok(Vec::new());
            }
            let pairs: Vec<String> = conn
                .prepare(
                    "SELECT d.id FROM source_revalidation_debt d
                     JOIN source_revalidation_rounds r ON r.id=d.round
                     WHERE r.id=?1 AND r.library=?2 AND r.scope=?3
                       AND r.phase='active' AND r.mode=?4 AND d.next_retry_at<=?5
                     ORDER BY d.next_retry_at,d.id LIMIT 64",
                )?
                .query_map(
                    params![
                        origin.nonce,
                        library,
                        origin.scope(),
                        CATALOG_MODE,
                        chrono::Utc::now().timestamp()
                    ],
                    |row| row.get(0),
                )?
                .collect::<Result<_, _>>()?;
            let mut output = Vec::new();
            for id in pairs {
                let mapped: Option<String> = conn
                    .query_row(
                        "SELECT master_record_name FROM asset_master_mappings WHERE library=?1 AND asset_record_name=?2",
                        params![library, id],
                        |row| row.get(0),
                    )
                    .optional()?;
                let master = mapped.clone().unwrap_or_else(|| id.clone());
                let (child, owned_child) = if mapped.is_some() {
                    (Some(id.clone()), false)
                } else {
                    let known: Option<(String, Option<String>)> = conn
                        .query_row(
                            "SELECT o.asset_record_name,m.master_record_name FROM legacy_master_state_owners o LEFT JOIN asset_master_mappings m ON m.library=o.library AND m.asset_record_name=o.asset_record_name WHERE o.library=?1 AND o.master_record_name=?2",
                            params![library, master],
                            |row| Ok((row.get(0)?, row.get(1)?)),
                        )
                        .optional()?;
                    match known {
                        Some((child, Some(mapped_master))) if mapped_master == master => {
                            (Some(child), true)
                        }
                        _ => (None, false),
                    }
                };
                output.push(SourceCandidate {
                    snapshot: snapshot(conn, &origin.nonce, &id, &master)?,
                    round: origin.nonce.clone(),
                    library: library.clone(),
                    id,
                    master,
                    child,
                    owned_child,
                });
            }
            Ok(output)
        })
        .await
    }

    pub(crate) async fn due_source_revalidation(
        &self,
        owner: account::AccountOwner,
        library: String,
        scope: String,
    ) -> Result<Vec<SourceCandidate>, StateError> {
        self.with_conn("reading due source revalidation",move |conn| {
            account::validate_authenticated(conn,&owner)?;
            let pairs:Vec<(String,String,String)>=conn.prepare(
                "SELECT d.round,d.id,r.enum_hash FROM source_revalidation_debt d JOIN source_revalidation_rounds r ON r.id=d.round
                 WHERE r.library=?1 AND r.scope=?2 AND r.phase='active' AND r.mode=?4 AND r.enum_hash=COALESCE((SELECT value FROM metadata WHERE key='pending_enum_config_hash'),(SELECT value FROM metadata WHERE key='enum_config_hash')) AND d.next_retry_at<=?3 ORDER BY d.next_retry_at,d.round,d.id LIMIT 64")?
                .query_map(params![library,scope,chrono::Utc::now().timestamp(),LEGACY_MODE],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?.collect::<Result<_,_>>()?;
            let mut output=Vec::new();
            for (round,id,enum_hash) in pairs {
                if !super::provider_generations::observation::revalidation_scope_current(conn, &library, &scope, &enum_hash)? { continue; }
                let mapped:Option<String>=conn.query_row("SELECT master_record_name FROM asset_master_mappings WHERE library=?1 AND asset_record_name=?2",params![library,id],|r|r.get(0)).optional()?;
                let master=mapped.clone().unwrap_or_else(||id.clone());
                let (child,owned_child)=if mapped.is_some() {
                    (Some(id.clone()), false)
                } else {
                    let owner:Option<(String,Option<String>)>=conn.query_row(
                        "SELECT o.asset_record_name,m.master_record_name FROM legacy_master_state_owners o LEFT JOIN asset_master_mappings m ON m.library=o.library AND m.asset_record_name=o.asset_record_name WHERE o.library=?1 AND o.master_record_name=?2",
                        params![library,master],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
                    match owner {
                        Some((child,Some(mapped_master))) if mapped_master==master => (Some(child),true),
                        _ => (None,false),
                    }
                };
                output.push(SourceCandidate{snapshot:snapshot(conn,&round,&id,&master)?,round,library:library.clone(),id,master,child,owned_child});
            }
            Ok(output)
        }).await
    }

    pub(crate) async fn settle_source_revalidation(
        &self,
        owner: account::AccountOwner,
        candidate: SourceCandidate,
        deleted: Option<(bool, Option<chrono::DateTime<chrono::Utc>>)>,
        live: bool,
    ) -> Result<bool, StateError> {
        self.with_conn_mut("settling confirmed source revalidation", move |conn| {
            let tx=conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            account::validate_authenticated(&tx,&owner)?;
            let exists:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM source_revalidation_debt d JOIN source_revalidation_rounds r ON r.id=d.round WHERE d.round=?1 AND d.id=?2 AND r.library=?3 AND r.mode=?4 AND r.phase='active')",params![candidate.round,candidate.id,candidate.library,LEGACY_MODE],|r|r.get(0))?;
            if !exists { tx.commit()?; return Ok(false); }
            if snapshot(&tx,&candidate.round,&candidate.id,&candidate.master)? != candidate.snapshot {
                tx.execute("UPDATE source_revalidation_debt SET next_retry_at=?3 WHERE round=?1 AND id=?2",params![candidate.round,candidate.id,chrono::Utc::now().timestamp()+3600])?;
                tx.commit()?;return Ok(false)
            }
            let deleted = deleted.filter(|(family, _)| *family || candidate.id != candidate.master || candidate.owned_child);
            if let Some((family,date))=deleted {
                let guard = if family {
                    super::asset_writes::ensure_master_family_has_no_prepared_capture_repair(&tx,&candidate.library,&candidate.master,"source_revalidation")
                } else {
                    super::asset_writes::ensure_asset_has_no_prepared_capture_repair(&tx,&candidate.library,&candidate.id,"source_revalidation")
                };
                match guard {
                    Ok(()) => (),
                    Err(StateError::Invariant { operation: "source_revalidation", .. }) => {
                        tx.execute("UPDATE source_revalidation_debt SET next_retry_at=?3 WHERE round=?1 AND id=?2",params![candidate.round,candidate.id,chrono::Utc::now().timestamp()+3600])?;
                        tx.commit()?; return Ok(false);
                    },
                    Err(error) => return Err(error),
                }
                if family {
                    tx.execute("UPDATE assets SET is_deleted=1,deleted_at=COALESCE(?3,deleted_at) WHERE library=?1 AND NOT EXISTS(SELECT 1 FROM unattributed_legacy p WHERE p.library=assets.library AND p.asset_id=assets.id) AND (id=?2 OR id IN (SELECT asset_record_name FROM asset_master_mappings WHERE library=?1 AND master_record_name=?2))",params![candidate.library,candidate.master,date.map(|v|v.timestamp())])?;
                } else {
                    tx.execute("UPDATE assets SET is_deleted=1,deleted_at=COALESCE(?3,deleted_at) WHERE library=?1 AND id=?2 AND NOT EXISTS(SELECT 1 FROM unattributed_legacy p WHERE p.library=assets.library AND p.asset_id=assets.id)",params![candidate.library,candidate.id,date.map(|v|v.timestamp())])?;
                }
                let remaining:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM assets WHERE library=?1 AND id=?2 AND is_deleted=0)",params![candidate.library,candidate.id],|r|r.get(0))?;
                if remaining {
                    tx.execute("UPDATE source_revalidation_debt SET next_retry_at=?3 WHERE round=?1 AND id=?2",params![candidate.round,candidate.id,chrono::Utc::now().timestamp()+3600])?;
                    tx.commit()?;return Ok(false)
                }
            }
            let live = live && (candidate.id != candidate.master || candidate.owned_child);
            if deleted.is_some() || live {
                tx.execute("DELETE FROM source_revalidation_debt WHERE round=?1 AND id=?2",params![candidate.round,candidate.id])?;
            } else {
                tx.execute("UPDATE source_revalidation_debt SET next_retry_at=?3 WHERE round=?1 AND id=?2",params![candidate.round,candidate.id,chrono::Utc::now().timestamp()+3600])?;
            }
            tx.commit()?;Ok(true)
        }).await
    }

    #[allow(dead_code)] // L1 catalog drain integration is pending; focused owner tests exercise this.
    pub(crate) async fn settle_catalog_candidate(
        &self,
        owner: account::AccountOwner,
        library: String,
        origin: CatalogOrigin,
        enum_hash: String,
        candidate: SourceCandidate,
        authority: CatalogSettlementAuthority,
    ) -> Result<bool, StateError> {
        self.with_conn_mut("settling catalog candidate", move |conn| {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            account::validate_authenticated(&tx, &owner)?;
            if candidate.library != library {
                return Err(StateError::Invariant {
                    operation: "settle_catalog_candidate",
                    detail: "catalog candidate source changed".into(),
                });
            }
            if !super::provider_generations::observation::catalog_origin_current(
                &tx,
                &library,
                &origin,
                &enum_hash,
            )? {
                tx.commit()?;
                return Ok(false);
            }
            let round_matches: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM source_revalidation_rounds WHERE id=?1 AND library=?2 AND scope=?3 AND phase='active' AND mode=?4)",
                params![origin.nonce, library, origin.scope(), CATALOG_MODE],
                |row| row.get(0),
            )?;
            if !round_matches
                || candidate.round != origin.nonce
                || snapshot(&tx, &candidate.round, &candidate.id, &candidate.master)?
                    != candidate.snapshot
            {
                tx.commit()?;
                return Ok(false);
            }
            let evidence = match &authority {
                CatalogSettlementAuthority::Deleted { evidence, .. }
                | CatalogSettlementAuthority::Live { evidence } => evidence.clone(),
                CatalogSettlementAuthority::Refused => {
                    tx.execute(
                        "UPDATE source_revalidation_debt SET next_retry_at=?3 WHERE round=?1 AND id=?2",
                        params![
                            candidate.round,
                            candidate.id,
                            chrono::Utc::now().timestamp() + 3600
                        ],
                    )?;
                    tx.commit()?;
                    return Ok(false);
                }
            };
            if evidence.master != candidate.master
                || evidence.zone.get("zoneName").and_then(serde_json::Value::as_str)
                    != Some(library.as_str())
                || !matches!(evidence.database.as_str(), "private" | "shared")
                || !super::provider_generations::observation::catalog_source_matches(
                    &tx, &library, &evidence.database, &evidence.zone,
                )?
                || evidence.master_type != "CPLMaster"
                || evidence.child_type != "CPLAsset"
                || !evidence.reference_exact
            {
                tx.execute(
                    "UPDATE source_revalidation_debt SET next_retry_at=?3 WHERE round=?1 AND id=?2",
                    params![
                        candidate.round,
                        candidate.id,
                        chrono::Utc::now().timestamp() + 3600
                    ],
                )?;
                tx.commit()?;
                return Ok(false);
            }
            let expected_child = candidate
                .child
                .clone()
                .unwrap_or_else(|| candidate.id.clone());
            if evidence.child != expected_child {
                tx.execute(
                    "UPDATE source_revalidation_debt SET next_retry_at=?3 WHERE round=?1 AND id=?2",
                    params![
                        candidate.round,
                        candidate.id,
                        chrono::Utc::now().timestamp() + 3600
                    ],
                )?;
                tx.commit()?;
                return Ok(false);
            }
            // The strict provider pair authenticates one child. The mutable
            // state identity must be that child, or an explicitly owned legacy
            // master-state row bound to it. Other historical mappings protect
            // the family but do not expand the deletion target.
            let identity_matches: bool = if candidate.id == candidate.master {
                candidate.owned_child && candidate.child.is_some() && tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM legacy_master_state_owners o JOIN asset_master_mappings m ON m.library=o.library AND m.asset_record_name=o.asset_record_name AND m.master_record_name=o.master_record_name WHERE o.library=?1 AND o.master_record_name=?2 AND o.asset_record_name=?3)",
                    params![library,candidate.master,expected_child], |row| row.get(0),
                )?
            } else {
                !candidate.owned_child && candidate.child.as_deref() == Some(candidate.id.as_str()) && tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM asset_master_mappings WHERE library=?1 AND asset_record_name=?2 AND master_record_name=?3)",
                    params![library,candidate.id,candidate.master], |row| row.get(0),
                )?
            };
            if !identity_matches {
                tx.commit()?;
                return Ok(false);
            }
            match authority {
                CatalogSettlementAuthority::Deleted { deleted_at, .. }
                    if evidence.explicit_deleted =>
                {
                    let mut ids = std::collections::BTreeSet::new();
                    ids.insert(candidate.master.clone());
                    ids.insert(candidate.id.clone());
                    ids.insert(evidence.child.clone());
                    let mapped: Vec<String> = tx
                        .prepare(
                            "SELECT asset_record_name FROM asset_master_mappings WHERE library=?1 AND master_record_name=?2",
                        )?
                        .query_map(params![library, candidate.master], |row| row.get(0))?
                        .collect::<Result<_, _>>()?;
                    ids.extend(mapped);
                    let ids_json = serde_json::to_string(&ids)
                        .map_err(|_invalid| StateError::ProviderSelectionInvalid)?;
                    if catalog_owner_protected(&tx, &library, &ids_json, &ids)? {
                        tx.execute(
                            "UPDATE source_revalidation_debt SET next_retry_at=?3 WHERE round=?1 AND id=?2",
                            params![
                                candidate.round,
                                candidate.id,
                                chrono::Utc::now().timestamp() + 3600
                            ],
                        )?;
                        tx.commit()?;
                        return Ok(false);
                    }
                    tx.execute(
                        "UPDATE assets SET is_deleted=1,deleted_at=COALESCE(?3,deleted_at) WHERE library=?1 AND id=?2",
                        params![library, candidate.id, deleted_at.map(|value| value.timestamp())],
                    )?;
                    tx.execute(
                        "DELETE FROM source_revalidation_debt WHERE round=?1 AND id=?2",
                        params![candidate.round, candidate.id],
                    )?;
                    tx.commit()?;
                    Ok(true)
                }
                CatalogSettlementAuthority::Live { .. } if !evidence.explicit_deleted => {
                    tx.execute(
                        "DELETE FROM source_revalidation_debt WHERE round=?1 AND id=?2",
                        params![candidate.round, candidate.id],
                    )?;
                    tx.commit()?;
                    Ok(true)
                }
                _ => {
                    tx.execute(
                        "UPDATE source_revalidation_debt SET next_retry_at=?3 WHERE round=?1 AND id=?2",
                        params![
                            candidate.round,
                            candidate.id,
                            chrono::Utc::now().timestamp() + 3600
                        ],
                    )?;
                    tx.commit()?;
                    Ok(false)
                }
            }
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::VersionSizeKey;
    use serde_json::json;

    fn owner() -> account::AccountOwner {
        account::AccountOwner::authenticated(
            "source-revalidation@example.invalid",
            "com",
            &serde_json::from_value(json!({"dsInfo":{"dsid":"synthetic"}})).unwrap(),
        )
        .unwrap()
    }

    #[tokio::test]
    async fn blocked_or_changed_candidate_is_deferred_without_losing_other_debt() {
        let dir = tempfile::tempdir().unwrap();
        let db = SqliteStateDb::open_owned(&dir.path().join("db"), &owner())
            .await
            .unwrap();
        let record = crate::test_helpers::TestAssetRecord::new("item")
            .library("PrimarySync")
            .version_size(VersionSizeKey::Original)
            .build();
        db.upsert_seen(&record).await.unwrap();
        db.mark_downloaded(
            "PrimarySync",
            "item",
            "original",
            &dir.path().join("item"),
            "synthetic",
            None,
        )
        .await
        .unwrap();
        let mut candidate = {
            let conn = db.acquire_lock("synthetic source debt").unwrap();
            conn.execute("INSERT INTO source_revalidation_rounds(id,library,scope,enum_hash,boundary,high_id,after_id,phase) VALUES('round','PrimarySync','scope','policy','boundary','item','item','active')", []).unwrap();
            conn.execute(
                "INSERT INTO source_revalidation_debt(round,id) VALUES('round','item')",
                [],
            )
            .unwrap();
            conn.execute("INSERT INTO asset_verifications VALUES('PrimarySync','item','original','unknown','other debt',1)", []).unwrap();
            let snapshot = snapshot(&conn, "round", "item", "item").unwrap();
            conn.execute("UPDATE assets SET last_error='local reconcile' WHERE library='PrimarySync' AND id='item'", []).unwrap();
            // status is part of the snapshot, unlike the informational last_error.
            conn.execute(
                "UPDATE assets SET status='failed' WHERE library='PrimarySync' AND id='item'",
                [],
            )
            .unwrap();
            SourceCandidate {
                round: "round".into(),
                library: "PrimarySync".into(),
                id: "item".into(),
                master: "item".into(),
                child: None,
                owned_child: false,
                snapshot,
            }
        };
        let mut wrong_library = candidate.clone();
        wrong_library.library = "OtherLibrary".into();
        assert!(
            !db.settle_source_revalidation(owner(), wrong_library, Some((true, None)), false)
                .await
                .unwrap()
        );
        {
            let conn = db
                .acquire_lock("verify legacy wrong-library refusal")
                .unwrap();
            let deadline: i64 = conn.query_row("SELECT next_retry_at FROM source_revalidation_debt WHERE round='round' AND id='item'", [], |row| row.get(0)).unwrap();
            assert_eq!(deadline, 0);
        }
        assert!(
            !db.settle_source_revalidation(owner(), candidate.clone(), Some((true, None)), false)
                .await
                .unwrap()
        );
        {
            let conn = db.acquire_lock("verify deferred stale candidate").unwrap();
            let (when, deleted):(i64,i64) = conn.query_row("SELECT d.next_retry_at,a.is_deleted FROM source_revalidation_debt d JOIN assets a ON a.library='PrimarySync' AND a.id=d.id", [], |r| Ok((r.get(0)?,r.get(1)?))).unwrap();
            assert!(when > chrono::Utc::now().timestamp());
            assert_eq!(deleted, 0);
            conn.execute("UPDATE source_revalidation_debt SET next_retry_at=0", [])
                .unwrap();
            conn.execute("INSERT INTO unattributed_legacy VALUES('PrimarySync','item',1,'evidence','[]',1,NULL)", []).unwrap();
            candidate.snapshot = snapshot(&conn, "round", "item", "item").unwrap();
        }
        assert!(
            !db.settle_source_revalidation(owner(), candidate, Some((true, None)), false)
                .await
                .unwrap()
        );
        let conn = db
            .acquire_lock("verify deferred protected candidate")
            .unwrap();
        let (when, deleted, other):(i64,i64,i64) = conn.query_row("SELECT d.next_retry_at,a.is_deleted,(SELECT COUNT(*) FROM asset_verifications WHERE reason='other debt') FROM source_revalidation_debt d JOIN assets a ON a.library='PrimarySync' AND a.id=d.id", [], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?))).unwrap();
        assert!(when > chrono::Utc::now().timestamp());
        assert_eq!((deleted, other), (0, 1));
    }

    async fn pending_catalog_claim(
        db: &SqliteStateDb,
        dir: &std::path::Path,
    ) -> (CatalogBaselineClaim, String, String, SourceCandidate) {
        db.set_metadata("enum_config_hash", "policy").await.unwrap();
        db.enroll_sync_epoch(vec![("PrimarySync".into(), "scope".into())])
            .await
            .unwrap();
        let asset_path = dir.join("asset1");
        std::fs::write(&asset_path, b"catalog asset").unwrap();
        let record = crate::test_helpers::TestAssetRecord::new("asset1")
            .library("PrimarySync")
            .build();
        db.upsert_seen(&record).await.unwrap();
        db.mark_downloaded(
            "PrimarySync",
            "asset1",
            "original",
            &asset_path,
            "synthetic",
            None,
        )
        .await
        .unwrap();
        let plan = "a".repeat(64);
        let old_attempt = "22222222-2222-2222-2222-222222222222";
        let old_nonce = "44444444-4444-4444-4444-444444444444";
        let baseline_key = "baseline_cursor:v27:synthetic:policy:";
        let cursor = "same-cursor";
        let candidate;
        let epoch;
        let library_json;
        {
            let conn = db.acquire_lock("prepare pending catalog claim").unwrap();
            ensure_catalog_mode_column_for_test(&conn).unwrap();
            conn.execute(
                "UPDATE metadata SET value=json_set(value,'$.state','pending','$.attempt',?2,'$.next_attempt_at',0,'$.binding',NULL,'$.completed_at',NULL,'$.last_outcome','baseline_claimed') WHERE key=?1",
                rusqlite::params![
                    crate::state::db::sync_epoch::library_key("PrimarySync"),
                    old_attempt
                ],
            )
            .unwrap();
            conn.execute(
                "INSERT OR REPLACE INTO metadata(key,value) VALUES(?1,?2)",
                rusqlite::params![baseline_key, cursor],
            )
            .unwrap();
            epoch = crate::state::db::sync_epoch::validate(&conn).unwrap();
            let library = crate::state::db::sync_epoch::read_library(&conn, "PrimarySync")
                .unwrap()
                .unwrap();
            library_json = serde_json::to_string(&library).unwrap();
            let scope = CatalogOrigin::new(
                plan.clone(),
                epoch.epoch.clone(),
                old_attempt.into(),
                old_nonce.into(),
            )
            .unwrap()
            .scope();
            conn.execute(
                "INSERT INTO source_revalidation_rounds(id,library,scope,enum_hash,boundary,high_id,after_id,phase,mode)
                 VALUES(?1,'PrimarySync',?2,'policy',?3,'high-100','after-040','marking',?4)",
                rusqlite::params![
                    old_nonce,
                    scope,
                    CatalogOrigin::boundary(cursor),
                    CATALOG_MODE
                ],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO source_revalidation_debt(round,id,next_retry_at) VALUES(?1,'asset1',10),(?1,'asset2',20)",
                [old_nonce],
            )
            .unwrap();
            let snapshot = snapshot(&conn, old_nonce, "asset1", "asset1").unwrap();
            candidate = SourceCandidate {
                round: old_nonce.into(),
                library: "PrimarySync".into(),
                id: "asset1".into(),
                master: "asset1".into(),
                child: None,
                owned_child: false,
                snapshot,
            };
        }
        let expected_epoch = epoch.epoch.clone();
        let expected_epoch_json = serde_json::to_string(&epoch).unwrap();
        (
            CatalogBaselineClaim {
                library: "PrimarySync".into(),
                baseline_key: baseline_key.into(),
                expected_baseline: Some(cursor.into()),
                plan_digest: plan,
                expected_enum_hash: "policy".into(),
                expected_epoch,
                expected_epoch_json,
                expected_library_json: library_json,
                expected_metadata: vec![],
                now: chrono::Utc::now().timestamp(),
                watch_interval: Some(3600),
            },
            old_nonce.into(),
            old_attempt.into(),
            candidate,
        )
    }

    #[tokio::test]
    async fn catalog_mode_seen_due_summary_unknown_and_quarantine_are_isolated() {
        let dir = tempfile::tempdir().unwrap();
        let db = SqliteStateDb::open_owned(&dir.path().join("db"), &owner())
            .await
            .unwrap();
        let catalog = format!(
            "catalog:v2:{}:11111111-1111-1111-1111-111111111111:22222222-2222-2222-2222-222222222222:66666666-6666-6666-6666-666666666666",
            "d".repeat(64)
        );
        {
            let conn = db.acquire_lock("seed mode isolation").unwrap();
            ensure_catalog_mode_column_for_test(&conn).unwrap();
            conn.execute(
                "INSERT INTO source_revalidation_rounds(id,library,scope,enum_hash,boundary,high_id,after_id,phase) VALUES('legacy','PrimarySync','ordinary-scope','policy','legacy-boundary','','','observing')",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO source_revalidation_debt(round,id) VALUES('legacy','legacy-asset')",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO source_revalidation_rounds(id,library,scope,enum_hash,boundary,high_id,after_id,phase,mode) VALUES('catalog','PrimarySync',?1,'policy','catalog-boundary','','','active','catalog_v2')",
                [&catalog],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO source_revalidation_debt(round,id,next_retry_at) VALUES('catalog','catalog-asset',0)",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO source_revalidation_rounds(id,library,scope,enum_hash,boundary,high_id,after_id,phase,mode) VALUES('unknown','PrimarySync','future-scope','policy','future-boundary','','','active','future_mode')",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO source_revalidation_debt(round,id) VALUES('unknown','future-asset')",
                [],
            )
            .unwrap();
        }
        assert!(
            !has_due(
                &db.acquire_lock("catalog due isolation").unwrap(),
                "PrimarySync"
            )
            .unwrap()
        );
        assert!(
            db.due_source_revalidation(owner(), "PrimarySync".into(), catalog.clone())
                .await
                .unwrap()
                .is_empty()
        );
        db.source_revalidation_seen(
            owner(),
            "PrimarySync".into(),
            catalog,
            "catalog-asset".into(),
        )
        .await
        .unwrap();
        // Unknown mode is not eligible, but the debt is retained.
        assert!(
            !has_due(
                &db.acquire_lock("unknown due isolation").unwrap(),
                "PrimarySync"
            )
            .unwrap()
        );
        // Legacy Unfiled behavior is unchanged: ordinary observing scope can
        // still retire its own debt.
        db.source_revalidation_seen(
            owner(),
            "PrimarySync".into(),
            "ordinary-scope".into(),
            "legacy-asset".into(),
        )
        .await
        .unwrap();
        {
            let conn = db.acquire_lock("mode summary isolation").unwrap();
            let (legacy, catalog_debt, unknown_debt): (i64, i64, i64) = conn
                .query_row(
                    "SELECT (SELECT COUNT(*) FROM source_revalidation_debt WHERE round='legacy'),
                            (SELECT COUNT(*) FROM source_revalidation_debt WHERE round='catalog'),
                            (SELECT COUNT(*) FROM source_revalidation_debt WHERE round='unknown')",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .unwrap();
            assert_eq!((legacy, catalog_debt, unknown_debt), (0, 1, 1));
            let (eligible, retained) = summary_counts(&conn).unwrap();
            assert_eq!((eligible, retained), (0, 2));
            let quarantined = quarantine_all_rounds(&conn).unwrap();
            assert_eq!(quarantined.0, 3);
            let phases: (String, String, String) = conn
                .query_row(
                    "SELECT (SELECT phase FROM source_revalidation_rounds WHERE id='legacy'),
                            (SELECT phase FROM source_revalidation_rounds WHERE id='catalog'),
                            (SELECT phase FROM source_revalidation_rounds WHERE id='unknown')",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .unwrap();
            assert_eq!(
                (phases.0.as_str(), phases.1.as_str(), phases.2.as_str()),
                ("abandoned", "abandoned", "abandoned")
            );
            let debts: i64 = conn
                .query_row("SELECT COUNT(*) FROM source_revalidation_debt", [], |row| {
                    row.get(0)
                })
                .unwrap();
            assert_eq!(debts, 2);
        }
    }

    #[tokio::test]
    async fn catalog_pending_signals_expose_marking_and_saved_deadlines() {
        let dir = tempfile::tempdir().unwrap();
        let db = SqliteStateDb::open_owned(&dir.path().join("db"), &owner())
            .await
            .unwrap();
        let plan = "e".repeat(64);
        let epoch = "11111111-1111-1111-1111-111111111111";
        let attempt = "22222222-2222-2222-2222-222222222222";
        let marking =
            format!("catalog:v2:{plan}:{epoch}:{attempt}:77777777-7777-7777-7777-777777777777");
        let observing =
            format!("catalog:v2:{plan}:{epoch}:{attempt}:88888888-8888-8888-8888-888888888888");
        {
            let conn = db.acquire_lock("seed catalog pending signals").unwrap();
            conn.execute(
                "INSERT INTO source_revalidation_rounds(id,library,scope,enum_hash,boundary,high_id,after_id,phase,mode) VALUES('marking-origin','PrimarySync',?1,'policy','boundary','high','after','marking','catalog_v2')",
                [&marking],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO source_revalidation_rounds(id,library,scope,enum_hash,boundary,high_id,after_id,phase,mode) VALUES('observing-origin','PrimarySync',?1,'policy','boundary','','','observing','catalog_v2')",
                [&observing],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO source_revalidation_debt(round,id,next_retry_at) VALUES('observing-origin','asset',123)",
                [],
            )
            .unwrap();
        }
        let signals = db
            .catalog_pending_signals(owner(), "PrimarySync".into())
            .await
            .unwrap();
        assert_eq!(signals.len(), 2);
        let marking = signals
            .iter()
            .find(|signal| signal.phase == "marking")
            .unwrap();
        assert_eq!((marking.due_at, marking.pending_debt), (0, 0));
        let observing = signals
            .iter()
            .find(|signal| signal.phase == "observing")
            .unwrap();
        assert_eq!((observing.due_at, observing.pending_debt), (123, 1));
        let outcome = db
            .catalog_source_outcome(owner(), "PrimarySync".into())
            .await
            .unwrap();
        assert!(outcome.pending);
        assert_eq!(outcome.retained, 1);
        assert_eq!(outcome.next_due_at, Some(0));
    }

    #[tokio::test]
    async fn pending_catalog_claim_transfers_marking_progress_to_new_attempt_atomically() {
        let dir = tempfile::tempdir().unwrap();
        let db = SqliteStateDb::open_owned(&dir.path().join("db"), &owner())
            .await
            .unwrap();
        let (claim, old_round, old_attempt, candidate) =
            pending_catalog_claim(&db, dir.path()).await;
        let outcome = db
            .claim_catalog_baseline(owner(), claim.clone())
            .await
            .unwrap();
        let CatalogClaimOutcome::Transferred {
            old_round: abandoned,
            new_round,
            attempt,
            copied_debt,
        } = outcome
        else {
            panic!("pending-due catalog baseline must transfer ownership");
        };
        assert_eq!(abandoned, old_round);
        assert_eq!(copied_debt, 2);
        assert_ne!(attempt, old_attempt);
        assert_ne!(new_round, old_round);
        {
            let conn = db
                .acquire_lock("verify transferred catalog origin")
                .unwrap();
            let (mode, scope, high, after, phase, boundary): (
                String,
                String,
                String,
                String,
                String,
                String,
            ) = conn
                .query_row(
                    "SELECT mode,scope,high_id,after_id,phase,boundary FROM source_revalidation_rounds WHERE id=?1",
                    [&new_round],
                    |row| {
                        Ok((
                            row.get(0)?,
                            row.get(1)?,
                            row.get(2)?,
                            row.get(3)?,
                            row.get(4)?,
                            row.get(5)?,
                        ))
                    },
                )
                .unwrap();
            let origin = CatalogOrigin::parse(&scope).unwrap();
            assert_eq!(mode, CATALOG_MODE);
            assert_eq!(origin.attempt, attempt);
            assert_eq!(origin.nonce, new_round);
            assert_eq!(
                (high.as_str(), after.as_str(), phase.as_str()),
                ("high-100", "after-040", "marking")
            );
            assert_eq!(boundary, CatalogOrigin::boundary("same-cursor"));
            let debt: Vec<(String, i64)> = conn
                .prepare("SELECT id,next_retry_at FROM source_revalidation_debt WHERE round=?1 ORDER BY id")
                .unwrap()
                .query_map([&new_round], |row| Ok((row.get(0)?, row.get(1)?)))
                .unwrap()
                .collect::<Result<_, _>>()
                .unwrap();
            assert_eq!(
                debt,
                vec![("asset1".to_string(), 10), ("asset2".to_string(), 20)]
            );
            let (old_phase, old_debt): (String, i64) = conn
                .query_row(
                    "SELECT (SELECT phase FROM source_revalidation_rounds WHERE id=?1),
                            (SELECT COUNT(*) FROM source_revalidation_debt WHERE round=?1)",
                    [&old_round],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .unwrap();
            assert_eq!((old_phase.as_str(), old_debt), ("abandoned", 2));
            let cursor: String = conn
                .query_row(
                    "SELECT value FROM metadata WHERE key=?1",
                    [&claim.baseline_key],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(cursor, "same-cursor");
            let record = crate::state::db::sync_epoch::read_library(&conn, "PrimarySync")
                .unwrap()
                .unwrap();
            assert_eq!(record.attempt, attempt);
            assert_eq!(record.state, "pending");
            assert!(record.next_attempt_at > claim.now);
        }
        assert!(
            !db.settle_source_revalidation(owner(), candidate, Some((true, None)), false)
                .await
                .unwrap()
        );
        {
            let conn = db.acquire_lock("old finisher cannot delete").unwrap();
            let deleted: i64 = conn
                .query_row(
                    "SELECT is_deleted FROM assets WHERE library='PrimarySync' AND id='asset1'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(deleted, 0);
        }
        drop(db);
        let reopened = SqliteStateDb::open_owned(&dir.path().join("db"), &owner())
            .await
            .unwrap();
        let conn = reopened
            .acquire_lock("reopen transferred catalog origin")
            .unwrap();
        let (phase, debt): (String, i64) = conn
            .query_row(
                "SELECT (SELECT phase FROM source_revalidation_rounds WHERE id=?1),
                        (SELECT COUNT(*) FROM source_revalidation_debt WHERE round=?1)",
                [&new_round],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!((phase.as_str(), debt), ("marking", 2));
    }

    #[tokio::test]
    async fn complete_epoch_catalog_claim_is_open_without_transfer_or_attempt_rotation() {
        let dir = tempfile::tempdir().unwrap();
        let db = SqliteStateDb::open_owned(&dir.path().join("db"), &owner())
            .await
            .unwrap();
        db.set_metadata("enum_config_hash", "policy").await.unwrap();
        db.enroll_sync_epoch(vec![("PrimarySync".into(), "scope".into())])
            .await
            .unwrap();
        let plan = "f".repeat(64);
        let old_attempt = "99999999-9999-9999-9999-999999999999";
        let catalog = format!(
            "catalog:v2:{plan}:11111111-1111-1111-1111-111111111111:{old_attempt}:aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa"
        );
        let cursor = "complete-cursor";
        let baseline_key = "baseline_cursor:v27:synthetic:policy:";
        let (epoch_json, library_json, epoch_id) = {
            let conn = db.acquire_lock("prepare complete epoch claim").unwrap();
            conn.execute(
                "UPDATE metadata SET value=json_set(value,'$.state','complete','$.attempt',?2,'$.next_attempt_at',0,'$.binding',NULL,'$.completed_at',1,'$.last_outcome','qualified') WHERE key=?1",
                rusqlite::params![
                    crate::state::db::sync_epoch::library_key("PrimarySync"),
                    old_attempt
                ],
            )
            .unwrap();
            conn.execute(
                "INSERT OR REPLACE INTO metadata(key,value) VALUES(?1,?2)",
                rusqlite::params![baseline_key, cursor],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO source_revalidation_rounds(id,library,scope,enum_hash,boundary,high_id,after_id,phase,mode) VALUES('complete-origin','PrimarySync',?1,'policy',?2,'high','after','active','catalog_v2')",
                rusqlite::params![catalog, CatalogOrigin::boundary(cursor)],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO source_revalidation_debt(round,id,next_retry_at) VALUES('complete-origin','asset',0)",
                [],
            )
            .unwrap();
            let epoch = crate::state::db::sync_epoch::validate(&conn).unwrap();
            let library = crate::state::db::sync_epoch::read_library(&conn, "PrimarySync")
                .unwrap()
                .unwrap();
            (
                serde_json::to_string(&epoch).unwrap(),
                serde_json::to_string(&library).unwrap(),
                epoch.epoch,
            )
        };
        let outcome = db
            .claim_catalog_baseline(
                owner(),
                CatalogBaselineClaim {
                    library: "PrimarySync".into(),
                    baseline_key: baseline_key.into(),
                    expected_baseline: Some(cursor.into()),
                    plan_digest: plan,
                    expected_enum_hash: "policy".into(),
                    expected_epoch: epoch_id,
                    expected_epoch_json: epoch_json,
                    expected_library_json: library_json,
                    expected_metadata: vec![],
                    now: chrono::Utc::now().timestamp(),
                    watch_interval: Some(3600),
                },
            )
            .await
            .unwrap();
        assert_eq!(outcome, CatalogClaimOutcome::Open);
        let conn = db
            .acquire_lock("verify complete epoch no transfer")
            .unwrap();
        let (phase, debt, attempt): (String, i64, String) = conn
            .query_row(
                "SELECT (SELECT phase FROM source_revalidation_rounds WHERE id='complete-origin'),
                        (SELECT COUNT(*) FROM source_revalidation_debt WHERE round='complete-origin'),
                        (SELECT json_extract(value,'$.attempt') FROM metadata WHERE key=?1)",
                [crate::state::db::sync_epoch::library_key("PrimarySync")],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!((phase.as_str(), debt), ("active", 1));
        assert_eq!(attempt, old_attempt);
    }

    #[tokio::test]
    async fn catalog_claim_copy_failure_rolls_back_epoch_claim_and_origin() {
        let dir = tempfile::tempdir().unwrap();
        let db = SqliteStateDb::open_owned(&dir.path().join("db"), &owner())
            .await
            .unwrap();
        let (claim, old_round, old_attempt, _candidate) =
            pending_catalog_claim(&db, dir.path()).await;
        {
            let conn = db.acquire_lock("install debt copy failure").unwrap();
            conn.execute_batch(
                "CREATE TRIGGER fail_catalog_debt_copy BEFORE INSERT ON source_revalidation_debt \
                 WHEN NEW.round <> '44444444-4444-4444-4444-444444444444' \
                 BEGIN SELECT RAISE(ABORT,'synthetic catalog debt copy'); END;",
            )
            .unwrap();
        }
        assert!(
            db.claim_catalog_baseline(owner(), claim.clone())
                .await
                .is_err()
        );
        let conn = db.acquire_lock("verify claim rollback").unwrap();
        let record = crate::state::db::sync_epoch::read_library(&conn, "PrimarySync")
            .unwrap()
            .unwrap();
        assert_eq!(record.attempt, old_attempt);
        let (phase, rounds): (String, i64) = conn
            .query_row(
                "SELECT (SELECT phase FROM source_revalidation_rounds WHERE id=?1),
                        (SELECT COUNT(*) FROM source_revalidation_rounds WHERE mode=?2)",
                rusqlite::params![old_round, CATALOG_MODE],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!((phase.as_str(), rounds), ("marking", 1));
        let debt: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM source_revalidation_debt WHERE round=?1",
                [old_round],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(debt, 2);
    }

    #[tokio::test]
    async fn repeated_boundary_new_attempt_abandons_old_origin_and_stale_settle_cannot_delete() {
        let dir = tempfile::tempdir().unwrap();
        let db = SqliteStateDb::open_owned(&dir.path().join("db"), &owner())
            .await
            .unwrap();
        let path = dir.path().join("catalog");
        std::fs::write(&path, b"catalog bytes").unwrap();
        let record = crate::test_helpers::TestAssetRecord::new("catalog-asset")
            .library("PrimarySync")
            .build();
        db.upsert_seen(&record).await.unwrap();
        db.mark_downloaded(
            "PrimarySync",
            "catalog-asset",
            "original",
            &path,
            "synthetic",
            None,
        )
        .await
        .unwrap();
        let plan = "a".repeat(64);
        let epoch = "11111111-1111-1111-1111-111111111111";
        let attempt_a = "22222222-2222-2222-2222-222222222222";
        let attempt_b = "33333333-3333-3333-3333-333333333333";
        let nonce_a = "44444444-4444-4444-4444-444444444444";
        let nonce_b = "55555555-5555-5555-5555-555555555555";
        let boundary = "b".repeat(64);
        let scope_a = format!("catalog:v2:{plan}:{epoch}:{attempt_a}:{nonce_a}");
        let scope_b = format!("catalog:v2:{plan}:{epoch}:{attempt_b}:{nonce_b}");
        let candidate = {
            let conn = db.acquire_lock("seed old catalog origin").unwrap();
            conn.execute(
                "INSERT INTO source_revalidation_rounds(id,library,scope,enum_hash,boundary,high_id,after_id,phase,mode) VALUES(?1,'PrimarySync',?2,'policy',?3,'catalog-asset','catalog-asset','active','catalog_v2')",
                rusqlite::params![nonce_a, scope_a, boundary],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO source_revalidation_debt(round,id) VALUES(?1,'catalog-asset')",
                [nonce_a],
            )
            .unwrap();
            let snapshot = snapshot(&conn, nonce_a, "catalog-asset", "catalog-asset").unwrap();
            SourceCandidate {
                round: nonce_a.into(),
                library: "PrimarySync".into(),
                id: "catalog-asset".into(),
                master: "catalog-asset".into(),
                child: None,
                owned_child: false,
                snapshot,
            }
        };
        {
            let conn = db
                .acquire_lock("install repeated-boundary new attempt origin")
                .unwrap();
            // Same provider boundary text, new epoch attempt. The old origin is
            // superseded and retained; the new origin has a new nonce round id.
            conn.execute(
                "UPDATE source_revalidation_rounds SET phase='abandoned' WHERE id=?1",
                [nonce_a],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO source_revalidation_rounds(id,library,scope,enum_hash,boundary,high_id,after_id,phase,mode) VALUES(?1,'PrimarySync',?2,'policy',?3,'catalog-asset','catalog-asset','observing','catalog_v2')",
                rusqlite::params![nonce_b, scope_b, boundary],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO source_revalidation_debt(round,id) VALUES(?1,'catalog-asset')",
                [nonce_b],
            )
            .unwrap();
        }
        assert!(
            !db.settle_source_revalidation(owner(), candidate, Some((true, None)), false)
                .await
                .unwrap(),
            "a stale nonce from the prior attempt cannot settle after a repeated boundary"
        );
        let conn = db.acquire_lock("verify old origin did not delete").unwrap();
        let (deleted, old_phase, old_debt, new_debt): (i64, String, i64, i64) = conn
            .query_row(
                "SELECT (SELECT is_deleted FROM assets WHERE library='PrimarySync' AND id='catalog-asset'),
                        (SELECT phase FROM source_revalidation_rounds WHERE id=?1),
                        (SELECT COUNT(*) FROM source_revalidation_debt WHERE round=?1),
                        (SELECT COUNT(*) FROM source_revalidation_debt WHERE round=?2)",
                rusqlite::params![nonce_a, nonce_b],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        assert_eq!(
            (deleted, old_phase.as_str(), old_debt, new_debt),
            (0, "abandoned", 1, 1)
        );
        assert_eq!(std::fs::read(&path).unwrap(), b"catalog bytes");
    }
}
