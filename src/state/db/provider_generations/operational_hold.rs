//! Source-scoped ownership evidence for operational reporting only.

use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde_json::Value;
use std::path::PathBuf;

use super::{
    CapturedSelectionRetry, DeferredMediaSuccessor, GenerationSpec, SqliteStateDb, account,
    guard_library_scope, load_decision_with_sources, load_generation, retained_profiles_compatible,
    validate_generation_headers,
};
use crate::state::error::StateError;

#[derive(Clone)]
pub(crate) struct SourceRequest {
    pub(crate) owner: account::AccountOwner,
    pub(crate) specification: GenerationSpec,
    pub(crate) baseline_scope: String,
    pub(crate) epoch: String,
    pub(crate) attempt: String,
    pub(crate) directory: PathBuf,
    pub(crate) catalog_plan: Option<String>,
    pub(crate) enum_policy: String,
    pub(crate) active_token_key: Option<String>,
    pub(crate) preservation_policy: Option<String>,
}

#[derive(Clone)]
struct FutureOwner {
    capture: CapturedSelectionRetry,
    reason: String,
    attempts: i64,
    deadline: i64,
}

#[derive(Clone, PartialEq, Eq)]
struct ProtectedRetry {
    asset_id: String,
    revision: i64,
    evidence: String,
    generation: i64,
    attempts: i64,
    last_attempt_at: i64,
    next_retry_at: i64,
}

#[derive(Clone)]
pub(crate) struct SourcePrework {
    request: SourceRequest,
    futures: Vec<FutureOwner>,
    protected_retries: Vec<ProtectedRetry>,
    active_observation: Option<super::observation::ActiveObservation>,
}

#[cfg(test)]
impl SourcePrework {
    /// Inspect the actual captured request without granting health authority.
    pub(crate) fn census_evidence_for_test(
        &self,
        conn: &Connection,
    ) -> Result<(bool, bool, bool, bool), StateError> {
        let request = &self.request;
        let Some((library, _, database)) =
            crate::icloud::photos::PhotoLibrary::provider_scope_projection(
                &request.specification.scope,
            )
        else {
            return Ok((false, false, false, false));
        };
        let source = super::observation::catalog_source_matches(
            conn,
            &library,
            &database,
            &request.specification.zone,
        )?;
        let plan = if let Some(plan) = &request.catalog_plan {
            super::observation::catalog_active_plan_matches(
                conn,
                &library,
                plan,
                &request.enum_policy,
            )?
        } else {
            false
        };
        Ok((
            source_valid(conn, request)?,
            source,
            plan,
            census_accounted(conn, request, &library)?,
        ))
    }
}

/// Paths returned here name only validated unfinished selection destinations.
/// They do not prove filesystem journal absence or provider completion.
pub(crate) struct SourceAccounting {
    pub(crate) paths: Vec<(PathBuf, PathBuf)>,
    protected_retries: Vec<ProtectedRetry>,
}

fn source_valid(conn: &Connection, request: &SourceRequest) -> Result<bool, StateError> {
    account::validate_authenticated(conn, &request.owner)?;
    let spec = &request.specification;
    let Some((library, owner, database)) =
        crate::icloud::photos::PhotoLibrary::provider_scope_projection(&spec.scope)
    else {
        return Ok(false);
    };
    if spec.zone.get("zoneName").and_then(Value::as_str) != Some(library.as_str())
        || spec.zone.get("ownerRecordName").and_then(Value::as_str) != Some(owner.as_str())
        || !matches!(database.as_str(), "private" | "shared")
        || request.baseline_scope.is_empty()
    {
        return Ok(false);
    }
    guard_library_scope(conn, &spec.scope, &library)?;
    let Some(epoch) = super::super::sync_epoch::validate_if_present(conn)? else {
        return Ok(false);
    };
    let Some(record) = super::super::sync_epoch::read_library(conn, &library)? else {
        return Ok(false);
    };
    Ok(epoch.epoch == request.epoch
        && record.epoch == request.epoch
        && record.attempt == request.attempt
        && record.state == "complete"
        && record.baseline_scope == request.baseline_scope)
}

fn same_capture(a: &CapturedSelectionRetry, b: &CapturedSelectionRetry) -> bool {
    a.generation == b.generation
        && a.specification_hash == b.specification_hash
        && a.pass_key == b.pass_key
        && a.child == b.child
        && a.state_id == b.state_id
        && a.manifest_hash == b.manifest_hash
        && a.destination == b.destination
}

/// Classify the entire census, not just due debt or owner pending signals.
fn census_accounted(
    conn: &Connection,
    request: &SourceRequest,
    library: &str,
) -> Result<bool, StateError> {
    use super::super::source_revalidation::{CATALOG_MODE, CatalogOrigin, LEGACY_MODE};
    use super::observation::{
        catalog_active_plan_matches, catalog_origin_current, catalog_source_matches,
        revalidation_scope_current,
    };

    let rows: Vec<(String, String, String, String, String, String, bool)> = conn
        .prepare(
            "SELECT r.id,r.scope,r.enum_hash,r.boundary,r.phase,r.mode,\
             EXISTS(SELECT 1 FROM source_revalidation_debt d WHERE d.round=r.id) \
             FROM source_revalidation_rounds r WHERE r.library=?1 ORDER BY r.id",
        )?
        .query_map([library], |row| {
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
    if rows.is_empty() {
        return Ok(true);
    }
    let Some(plan) = &request.catalog_plan else {
        return Ok(false);
    };
    let policy = &request.enum_policy;
    let Some((_, _, database)) = crate::icloud::photos::PhotoLibrary::provider_scope_projection(
        &request.specification.scope,
    ) else {
        return Ok(false);
    };
    if !catalog_source_matches(conn, library, &database, &request.specification.zone)?
        || !catalog_active_plan_matches(conn, library, plan, policy)?
    {
        return Ok(false);
    }
    let mut active_catalog = false;
    let mut abandoned_catalog = false;
    for (id, scope, enum_hash, boundary, phase, mode, debt) in rows {
        if enum_hash != *policy {
            return Ok(false);
        }
        match (phase.as_str(), mode.as_str()) {
            ("abandoned", CATALOG_MODE) | ("active", CATALOG_MODE) => {
                let Ok(origin) = CatalogOrigin::parse(&scope) else {
                    return Ok(false);
                };
                if origin.nonce != id
                    || origin.plan_digest != *plan
                    || boundary.len() != 64
                    || !boundary.bytes().all(|byte| byte.is_ascii_hexdigit())
                {
                    return Ok(false);
                }
                if phase == "abandoned" {
                    abandoned_catalog = true;
                } else if debt || !catalog_origin_current(conn, library, &origin, policy)? {
                    return Ok(false);
                } else {
                    active_catalog = true;
                }
            }
            ("active", LEGACY_MODE) if !debt => {
                if !revalidation_scope_current(conn, library, &scope, policy)? {
                    return Ok(false);
                }
            }
            _ => return Ok(false),
        }
    }
    Ok(!abandoned_catalog || active_catalog)
}

/// All inner work uses the one supplied transaction. No bounded retry window is
/// treated as a census. SQL/owner failures remain errors, not empty results.
fn inspect(
    conn: &Connection,
    request: &SourceRequest,
    prework: Option<&SourcePrework>,
    successors: &[DeferredMediaSuccessor],
    sparse: Option<&crate::state::db::SparseCurrentWitness>,
    preservation: bool,
) -> Result<Option<(Vec<FutureOwner>, SourceAccounting)>, StateError> {
    if !source_valid(conn, request)? {
        return Ok(None);
    }
    if let Some(token_key) = &request.active_token_key {
        let Some(library) = request
            .specification
            .zone
            .get("zoneName")
            .and_then(Value::as_str)
        else {
            return Ok(None);
        };
        let hold_key = format!(
            "{}{library}",
            crate::sync_cycle::RETAINED_CHECKPOINT_HOLD_PREFIX
        );
        let hold: Option<String> = conn
            .query_row(
                "SELECT value FROM metadata WHERE key=?1",
                [&hold_key],
                |row| row.get(0),
            )
            .optional()?;
        let token: Option<String> = conn
            .query_row(
                "SELECT value FROM metadata WHERE key=?1",
                [token_key],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(hold) = hold {
            let Some(token) = token else {
                return Ok(None);
            };
            if crate::sync_cycle::retained_checkpoint_blocks_health(
                &hold,
                &token,
                &request.enum_policy,
                request.preservation_policy.as_deref(),
            ) {
                return Ok(None);
            }
        }
    }
    validate_generation_headers(conn)?;
    let library = request
        .specification
        .zone
        .get("zoneName")
        .and_then(Value::as_str)
        .ok_or(StateError::ProviderSelectionInvalid)?;
    if !census_accounted(conn, request, library)? {
        return Ok(None);
    }
    if prework.is_some() && sparse.is_none() {
        let unresolved: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM unresolved_sparse_identities WHERE library=?1) OR EXISTS(SELECT 1 FROM metadata WHERE key=?2)",
            params![library, crate::state::unresolved_identity_key(library)],
            |row| row.get(0),
        )?;
        if unresolved {
            return Ok(None);
        }
    }
    if prework.is_some() && !preservation {
        let unresolved: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM unattributed_legacy WHERE library=?1)",
            [library],
            |row| row.get(0),
        )?;
        if unresolved {
            return Ok(None);
        }
    }
    if let Some(witness) = sparse {
        match super::super::sparse_identity::attest_current_sparse(conn, witness) {
            Ok(()) => {}
            Err(super::super::sparse_identity::SparseAttestationRefusal::ReadFailed) => {
                return Err(StateError::Invariant {
                    operation: "operational_sparse",
                    detail: "sparse owner read failed".into(),
                });
            }
            Err(_) => return Ok(None),
        }
    }
    let blocked: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM primary_layout_operations WHERE source_library=?1 AND phase NOT IN ('committed','cancelled')) OR EXISTS(SELECT 1 FROM primary_layout_bindings WHERE source_library=?1) OR EXISTS(SELECT 1 FROM primary_layout_claims WHERE library=?1) OR EXISTS(SELECT 1 FROM owned_temp_files WHERE publication IS NOT NULL AND (json_type(publication,'$.library') IS NOT 'text' OR json_extract(publication,'$.library')=?1))",
        [library], |row| row.get(0),
    )?;
    if blocked {
        return Ok(None);
    }
    let metadata_pending: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM assets a WHERE a.library=?1 AND (a.metadata_write_failed_at IS NOT NULL OR a.capture_repair_metadata_hash IS NOT NULL OR a.capture_repair_output_checksum IS NOT NULL OR a.capture_repair_output_size IS NOT NULL)) OR EXISTS(SELECT 1 FROM asset_metadata_paths a WHERE a.library=?1 AND (a.metadata_write_failed_at IS NOT NULL OR a.capture_repair_metadata_hash IS NOT NULL OR a.capture_repair_output_checksum IS NOT NULL OR a.capture_repair_output_size IS NOT NULL))",
        [library],
        |row| row.get(0),
    )?;
    if metadata_pending {
        return Ok(None);
    }
    let other_pending: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM metadata_capture_retries m WHERE m.library=?1 AND NOT EXISTS(SELECT 1 FROM unattributed_legacy p WHERE p.library=m.library AND p.asset_id=m.asset_id)) OR EXISTS(SELECT 1 FROM source_revalidation_rounds r WHERE r.library=?1 AND r.phase<>'abandoned' AND (r.phase<>'active' OR EXISTS(SELECT 1 FROM source_revalidation_debt d WHERE d.round=r.id))) OR EXISTS(SELECT 1 FROM provider_work_obligations w LEFT JOIN assets a ON a.library=w.library AND a.id=w.asset_id AND a.version_size=w.version_size WHERE w.library=?1 AND (a.id IS NULL OR a.status<>'downloaded' OR a.checksum<>w.checksum OR a.size_bytes<>w.size_bytes OR a.metadata_hash IS NOT w.metadata_hash))",
        [library],
        |row| row.get(0),
    )?;
    if other_pending {
        return Ok(None);
    }
    let retained_provider_work: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM provider_work_retries r JOIN provider_shadow_pages p ON p.id=r.page_id WHERE p.scope=?1) OR EXISTS(SELECT 1 FROM provider_work_receipts WHERE scope=?1 AND state='deferred')",
        [&request.specification.scope],
        |row| row.get(0),
    )?;
    if retained_provider_work {
        return Ok(None);
    }
    let protected_retries: Vec<ProtectedRetry> = conn
        .prepare(
            "SELECT m.asset_id,m.target_revision,m.evidence,m.generation,m.attempts,m.last_attempt_at,m.next_retry_at \
             FROM metadata_capture_retries m JOIN unattributed_legacy p \
             ON p.library=m.library AND p.asset_id=m.asset_id WHERE m.library=?1 \
             ORDER BY m.asset_id,m.target_revision",
        )?
        .query_map([library], |row| {
            Ok(ProtectedRetry {
                asset_id: row.get(0)?,
                revision: row.get(1)?,
                evidence: row.get(2)?,
                generation: row.get(3)?,
                attempts: row.get(4)?,
                last_attempt_at: row.get(5)?,
                next_retry_at: row.get(6)?,
            })
        })?
        .collect::<Result<_, _>>()?;
    if prework.is_some_and(|before| before.protected_retries != protected_retries) {
        return Ok(None);
    }
    let ids: Vec<String> = conn
        .prepare("SELECT id FROM provider_active_generations WHERE scope=?1 ORDER BY id")?
        .query_map([&request.specification.scope], |row| row.get(0))?
        .collect::<Result<_, _>>()?;
    let now = chrono::Utc::now().timestamp();
    let mut futures = Vec::new();
    let mut paths = Vec::new();
    let mut used = vec![false; successors.len()];
    for id in ids {
        let root = load_generation(conn, &id)?;
        let spec_hash: String = conn.query_row(
            "SELECT specification_hash FROM provider_active_generations WHERE id=?1",
            [&id],
            |row| row.get(0),
        )?;
        let keys: Vec<(String, String)> = conn.prepare("SELECT pass_key,child FROM provider_active_decisions WHERE generation=?1 ORDER BY pass_key,child")?
            .query_map([&id], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<Result<_, _>>()?;
        let compatible = root.spec.format == 2
            && root.spec.scope == request.specification.scope
            && root.spec.zone == request.specification.zone
            && ((root.spec.config_hash == request.specification.config_hash
                && root.spec.profile == request.specification.profile)
                || retained_profiles_compatible(
                    &root.spec.profile,
                    &request.specification.profile,
                ));
        if prework.is_some() && (!compatible || (keys.is_empty() && !root.sealed)) {
            return Ok(None);
        }
        for (key, child) in keys {
            let manifest = load_decision_with_sources(conn, &root, &key, &child, false)?;
            let (manifest_hash, admission, reason, attempts, deadline, admission_hash, compacted):
                (String, String, String, i64, i64, Option<String>, bool) = conn.query_row(
                "SELECT manifest_hash,admission,reason,attempts,next_retry_at,admission_hash,compacted_hash IS NOT NULL FROM provider_active_decisions WHERE generation=?1 AND pass_key=?2 AND child=?3",
                params![id,key,child], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?,row.get(4)?,row.get(5)?,row.get(6)?)),
            )?;
            if admission == "deferred" && prework.is_some() {
                return Ok(None);
            }
            for destination in &manifest.decision.destinations {
                let (admitted, media, metadata, prepared, frozen): (bool,bool,bool,bool,bool) = conn.query_row(
                    "SELECT admitted,verified_media,verified_metadata,prepared_hash IS NOT NULL,frozen_metadata IS NOT NULL OR frozen_metadata_hash IS NOT NULL FROM provider_active_destinations WHERE generation=?1 AND pass_key=?2 AND child=?3 AND version_size=?4 AND path=?5",
                    params![id,key,child,destination.version_size,destination.path.key()?],
                    |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?,row.get(4)?)),
                )?;
                if prework.is_some() && (prepared || frozen) {
                    return Ok(None);
                }
                if prework.is_some() && admitted {
                    paths.push((request.directory.clone(), destination.path.to_path()));
                }
                if !admitted || (media && (!root.spec.metadata_enabled || metadata)) {
                    continue;
                }
                if !compatible
                    || compacted
                    || prepared
                    || frozen
                    || media
                    || admission != "admitted"
                {
                    if prework.is_some() {
                        return Ok(None);
                    }
                    continue;
                }
                let Some(admission_hash) = admission_hash.as_ref() else {
                    return Ok(None);
                };
                let capture = CapturedSelectionRetry {
                    generation: id.clone(),
                    specification_hash: spec_hash.clone(),
                    pass_key: key.clone(),
                    child: child.clone(),
                    state_id: manifest.state_id.clone(),
                    manifest_hash: manifest_hash.clone(),
                    admission_hash: admission_hash.clone(),
                    destination: destination.clone(),
                };
                if prework.is_none() {
                    if deadline > now {
                        futures.push(FutureOwner {
                            capture,
                            reason: reason.clone(),
                            attempts,
                            deadline,
                        });
                    }
                    continue;
                }
                let mut matching = successors.iter().enumerate().filter(|(_, successor)| {
                    same_capture(&capture, &successor.captured)
                        && successor.committed_admission_hash == *admission_hash
                        && successor.admission == admission
                        && successor.reason == reason
                        && successor.attempts == attempts
                        && successor.next_retry_at == deadline
                });
                if let Some((index, _)) = matching.next() {
                    let Some(matched) = used.get_mut(index) else {
                        return Ok(None);
                    };
                    if matching.next().is_some() || *matched || deadline <= now {
                        return Ok(None);
                    }
                    *matched = true;
                } else {
                    let known = prework.is_some_and(|pre| {
                        pre.futures.iter().any(|future| {
                            same_capture(&capture, &future.capture)
                                && capture.admission_hash == future.capture.admission_hash
                                && reason == future.reason
                                && attempts == future.attempts
                                && deadline == future.deadline
                                && deadline > now
                        })
                    });
                    if !known {
                        return Ok(None);
                    }
                }
            }
        }
    }
    if prework.is_some() && used.iter().any(|used| !used) {
        return Ok(None);
    }
    if prework.is_some() {
        // Source recovery also visits recorded paths outside the current root.
        // Include all of them in the journal observation, not only the failed
        // destination or paths selected for this wake.
        let mut statement = conn.prepare(
            "SELECT local_path FROM assets WHERE library=?1 AND local_path IS NOT NULL \
             UNION SELECT local_path FROM asset_metadata_paths WHERE library=?1",
        )?;
        let rows = statement.query_map([library], |row| row.get::<_, String>(0))?;
        for row in rows {
            let path = PathBuf::from(row?);
            let Some(parent) = path.parent() else {
                return Ok(None);
            };
            if crate::fs_util::confined_path_key(&path).is_err() {
                return Ok(None);
            }
            paths.push((parent.to_path_buf(), path));
        }
    }
    if preservation {
        let Some((_, owner_record, database)) =
            crate::icloud::photos::PhotoLibrary::provider_scope_projection(
                &request.specification.scope,
            )
        else {
            return Ok(None);
        };
        let preservation = super::super::legacy_preservation::PreservationHoldRequest {
            library: library.to_owned(),
            expected_owner: request.owner.clone(),
            expected_epoch: request.epoch.clone(),
            expected_attempt: request.attempt.clone(),
            scope: request.specification.scope.clone(),
            expected_zone_name: library.to_owned(),
            expected_owner_record_name: owner_record,
            expected_database: database,
            baseline_scope: request.baseline_scope.clone(),
        };
        match super::super::legacy_preservation::preservation_hold_snapshot(conn, &preservation)? {
            super::super::legacy_preservation::PreservationHoldOutcome::Accounted(proof) => {
                paths.extend(proof.protected_paths)
            }
            super::super::legacy_preservation::PreservationHoldOutcome::Refused(_) => {
                return Ok(None);
            }
        }
    }
    Ok(Some((
        futures,
        SourceAccounting {
            paths,
            protected_retries,
        },
    )))
}

impl SqliteStateDb {
    pub(crate) async fn operational_prework(
        &self,
        request: SourceRequest,
    ) -> Result<Option<SourcePrework>, StateError> {
        self.with_conn_mut("capturing source operational ownership", move |conn| {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Deferred)?;
            let active_observation = if let Some(key) = &request.active_token_key {
                let Some(observation) = super::observation::capture_active_observation(
                    &tx,
                    key,
                    &request.specification,
                    &request.enum_policy,
                )?
                else {
                    return Ok(None);
                };
                Some(observation)
            } else {
                None
            };
            Ok(
                inspect(&tx, &request, None, &[], None, false)?.map(|(futures, accounting)| {
                    SourcePrework {
                        request,
                        futures,
                        protected_retries: accounting.protected_retries,
                        active_observation,
                    }
                }),
            )
        })
        .await
    }

    pub(crate) async fn operational_accounting(
        &self,
        prework: SourcePrework,
        successors: Vec<DeferredMediaSuccessor>,
        sparse: Option<crate::state::db::SparseCurrentWitness>,
        preservation: bool,
    ) -> Result<Option<SourceAccounting>, StateError> {
        self.with_conn_mut("revalidating source operational ownership", move |conn| {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Deferred)?;
            if let Some(key) = &prework.request.active_token_key {
                let current = super::observation::capture_active_observation(
                    &tx,
                    key,
                    &prework.request.specification,
                    &prework.request.enum_policy,
                )?;
                if current.as_ref() != prework.active_observation.as_ref() || current.is_none() {
                    return Ok(None);
                }
            }
            Ok(inspect(
                &tx,
                &prework.request,
                Some(&prework),
                &successors,
                sparse.as_ref(),
                preservation,
            )?
            .map(|(_, accounting)| accounting))
        })
        .await
    }
}
