//! Single-cycle sync runner.
//!
//! This module owns the per-cycle boundary that sits between the outer
//! process/watch loop and the download engine: mode choice, enumeration config
//! hash handling, grouping preload, download dispatch, zone-token writes, and
//! the aggregate cycle result.

use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use crate::auth;
use crate::config;
use crate::download;
use crate::state;

/// Per-library state: zone name, sync token key, and resolved album plan.
pub(crate) struct LibraryState {
    pub(crate) library: crate::icloud::photos::PhotoLibrary,
    pub(crate) cross_zone_libraries: Vec<crate::icloud::photos::PhotoLibrary>,
    pub(crate) pass_scope: crate::commands::PassScope,
    pub(crate) zone_name: String,
    pub(crate) sync_token_key: String,
    /// Ordered list of download passes. Each pass carries its own
    /// exclude-asset-ids set. See [`crate::commands::AlbumPlan`].
    pub(crate) plan: crate::commands::AlbumPlan,
    /// True when `resolve_passes` failed at the end of the prior cycle and
    /// the plan above is the previous cycle's stale snapshot. Album
    /// membership data captured under a stale plan can route assets to the
    /// wrong pass (e.g. an asset added to a newly-created album shows up in
    /// the unfiled pass), so any cycle that consumes a stale plan must not
    /// advance the sync token for any zone -- doing so would skip the
    /// change events those assets generated and leave `asset_albums`
    /// permanently incomplete.
    pub(crate) plan_is_stale: bool,
    /// True after an idle watch sleep. Refreshing only when a later
    /// `changes/database` pre-check finds relevant work avoids burning album
    /// listing calls on quiet watch cycles.
    pub(crate) plan_needs_refresh: bool,
}

/// Metadata key holding the SHA-256 of the enumeration-affecting subset of
/// the user's download config. Distinct from the path-affecting
/// `config_hash` consumed by the download pipeline; using a single key for
/// both would cause each cycle to overwrite the other's value and
/// permanently invalidate incremental sync.
pub(crate) const ENUM_CONFIG_HASH_KEY: &str = "enum_config_hash";
pub(crate) const PENDING_ENUM_CONFIG_HASH_KEY: &str = "pending_enum_config_hash";
pub(crate) const PENDING_DOWNLOAD_CONFIG_HASH_KEY: &str = "pending_download_config_hash";
const PENDING_ZONE_TOKEN_PREFIX: &str = "pending_sync_token:";
const LAST_CHECKPOINT_STATUS_KEY: &str = "last_checkpoint_status";
const LAST_RECOVERY_ACTION_KEY: &str = "last_recovery_action";
const LAST_FULL_ENUMERATION_REASON_KEY: &str = "last_full_enumeration_reason";

/// Prefix for every per-zone CloudKit sync token row in the metadata table.
pub(crate) const SYNC_TOKEN_PREFIX: &str = "sync_token:";
const INVENTORY_ANCHOR_PREFIX: &str = "inventory_anchor:";
const INVENTORY_DROP_THRESHOLD_PERCENT: f64 = 5.0;

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
struct InventoryAnchor {
    api_total_at_start: u64,
    assets_seen: u64,
    completed_at: i64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct InventoryDrop {
    previous_total: u64,
    current_total: u64,
    drop_assets: u64,
    drop_percent: f64,
}

/// Return the metadata key that stores the CloudKit sync token for a zone.
pub(crate) fn sync_token_key(zone_name: &str) -> String {
    format!("{SYNC_TOKEN_PREFIX}{zone_name}")
}

fn inventory_anchor_key(enum_config_hash: &str, zone_name: &str) -> String {
    format!("{INVENTORY_ANCHOR_PREFIX}{enum_config_hash}:{zone_name}")
}

pub(crate) fn pending_zone_token_key(enum_config_hash: &str, zone_name: &str) -> String {
    format!("{PENDING_ZONE_TOKEN_PREFIX}{enum_config_hash}:{zone_name}")
}

#[allow(
    clippy::cast_precision_loss,
    reason = "inventory totals are operator diagnostics; f64 percent is only for warning/report context"
)]
fn classify_inventory_drop(previous_total: u64, current_total: u64) -> Option<InventoryDrop> {
    if previous_total == 0 || current_total >= previous_total {
        return None;
    }

    let drop_assets = previous_total - current_total;
    let drop_percent = (drop_assets as f64 / previous_total as f64) * 100.0;
    if drop_percent < INVENTORY_DROP_THRESHOLD_PERCENT {
        return None;
    }

    Some(InventoryDrop {
        previous_total,
        current_total,
        drop_assets,
        drop_percent,
    })
}

/// Outcome of a single sync cycle across all libraries.
#[derive(Debug)]
pub(crate) struct CycleResult {
    pub(crate) failed_count: usize,
    pub(crate) session_expired: bool,
    pub(crate) stats: download::SyncStats,
    pub(crate) db_sync_token_advance_safe: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CheckpointBasis {
    IncrementalDelta,
    CompleteInventory,
    InventoryWithDeltaBridge,
}

impl CheckpointBasis {
    const fn as_str(self) -> &'static str {
        match self {
            Self::IncrementalDelta => "incremental_delta",
            Self::CompleteInventory => "complete_inventory",
            Self::InventoryWithDeltaBridge => "inventory_with_delta_bridge",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CheckpointHoldReason {
    DryRun,
    StalePassPlan,
    Interrupted,
    SessionExpired,
    EnumerationIncomplete,
    StateNotDurable,
    TokenProofIncomplete,
}

impl CheckpointHoldReason {
    const fn as_str(self) -> &'static str {
        match self {
            Self::DryRun => "dry_run",
            Self::StalePassPlan => "stale_pass_plan",
            Self::Interrupted => "interrupted",
            Self::SessionExpired => "session_expired",
            Self::EnumerationIncomplete => "enumeration_incomplete",
            Self::StateNotDurable => "state_not_durable",
            Self::TokenProofIncomplete => "token_proof_incomplete",
        }
    }
}

pub(crate) use download::RecoveryAction;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SourceCheckpointDecision {
    Advance {
        token: String,
        basis: CheckpointBasis,
    },
    Preserve {
        reason: CheckpointHoldReason,
        recovery: RecoveryAction,
    },
}

fn source_checkpoint_decision(
    result: &download::SyncResult,
    dry_run: bool,
    stale_pass_plan: bool,
    basis: CheckpointBasis,
) -> SourceCheckpointDecision {
    // CONTRACT: SOURCE_CHECKPOINT_REQUIRES_DURABLE_RECOVERY
    let preserve = |reason, recovery| SourceCheckpointDecision::Preserve { reason, recovery };
    if dry_run {
        return preserve(CheckpointHoldReason::DryRun, RecoveryAction::Stop);
    }
    if stale_pass_plan {
        return preserve(
            CheckpointHoldReason::StalePassPlan,
            RecoveryAction::ReplayFromPriorToken,
        );
    }
    if matches!(
        result.outcome,
        download::DownloadOutcome::SessionExpired { .. }
    ) {
        return preserve(
            CheckpointHoldReason::SessionExpired,
            RecoveryAction::Reauthenticate,
        );
    }
    if result.stats.interrupted {
        return preserve(
            CheckpointHoldReason::Interrupted,
            RecoveryAction::ReplayFromPriorToken,
        );
    }
    if result.stats.enumeration_errors > 0 || result.stats.enumeration_incomplete {
        return preserve(
            CheckpointHoldReason::EnumerationIncomplete,
            RecoveryAction::ContinueTail,
        );
    }
    if result.stats.state_write_failures > 0 {
        return preserve(
            CheckpointHoldReason::StateNotDurable,
            RecoveryAction::ReplayFromPriorToken,
        );
    }
    let Some(token) = result
        .sync_token
        .as_ref()
        .filter(|token| !token.trim().is_empty())
    else {
        let recovery = if !result.stats.checkpoint_retry_passes.is_empty() {
            RecoveryAction::RetryPasses(result.stats.checkpoint_retry_passes.clone())
        } else if !result.stats.checkpoint_revalidate_records.is_empty() {
            RecoveryAction::RevalidateRecords(result.stats.checkpoint_revalidate_records.clone())
        } else {
            RecoveryAction::ReplayFromPriorToken
        };
        return preserve(CheckpointHoldReason::TokenProofIncomplete, recovery);
    };
    if result.stats.sync_token_blocked {
        let recovery = if result.stats.checkpoint_retry_passes.is_empty() {
            RecoveryAction::ReplayFromPriorToken
        } else {
            RecoveryAction::RetryPasses(result.stats.checkpoint_retry_passes.clone())
        };
        return preserve(CheckpointHoldReason::TokenProofIncomplete, recovery);
    }
    SourceCheckpointDecision::Advance {
        token: token.clone(),
        basis,
    }
}

fn merge_download_outcomes(
    first: &download::DownloadOutcome,
    second: &download::DownloadOutcome,
) -> download::DownloadOutcome {
    match (first, second) {
        (download::DownloadOutcome::SessionExpired { auth_error_count }, _)
        | (_, download::DownloadOutcome::SessionExpired { auth_error_count }) => {
            download::DownloadOutcome::SessionExpired {
                auth_error_count: *auth_error_count,
            }
        }
        (
            download::DownloadOutcome::PartialFailure {
                failed_count: first,
            },
            download::DownloadOutcome::PartialFailure {
                failed_count: second,
            },
        ) => download::DownloadOutcome::PartialFailure {
            failed_count: first.saturating_add(*second),
        },
        (download::DownloadOutcome::PartialFailure { failed_count }, _)
        | (_, download::DownloadOutcome::PartialFailure { failed_count }) => {
            download::DownloadOutcome::PartialFailure {
                failed_count: *failed_count,
            }
        }
        (download::DownloadOutcome::Success, download::DownloadOutcome::Success) => {
            download::DownloadOutcome::Success
        }
    }
}

/// Decide whether the per-zone `sync_token` should be persisted to the state
/// DB after a download pass.
///
/// The contract is "advance only on full success and not in dry-run":
/// - On `PartialFailure`, a stored token would skip the failed assets on the
///   next incremental sync (they'd never appear in the delta again -- silent
///   data loss).
/// - On `SessionExpired`, the cycle aborts mid-stream; the token may be
///   stale or only reflect a subset of the work.
/// - In `--dry-run`, we promise to make no DB writes that survive the run
///   (apart from the `sync_runs` ledger). Advancing the token here would
///   silently break the next real sync.
///
/// The returned bool is the gate: callers still check that `sync_token` is
/// `Some(_)` and that a state DB is configured before persisting.
#[cfg(test)]
pub(crate) fn should_store_sync_token(outcome: &download::DownloadOutcome, dry_run: bool) -> bool {
    matches!(outcome, download::DownloadOutcome::Success) && !dry_run
}

/// Library-level gate that combines the per-library outcome check with the
/// stale-plan flag for the same zone.
///
/// A reused album plan can route assets created or moved between cycles to
/// the wrong pass, so the affected zone must not advance. Unaffected zones
/// can still store their own zone tokens; the broader database pre-check
/// token remains conservative until every selected zone is clean.
#[cfg(test)]
pub(crate) fn should_store_sync_token_for_cycle(
    outcome: &download::DownloadOutcome,
    dry_run: bool,
    library_plan_is_stale: bool,
) -> bool {
    should_store_sync_token(outcome, dry_run) && !library_plan_is_stale
}

fn has_active_passes(lib_state: &LibraryState) -> bool {
    !lib_state.plan.passes.is_empty()
}

fn should_warn_zero_assets(
    sync_result: &download::SyncResult,
    library_completed_without_errors: bool,
    run_mode: download::DownloadRunMode,
    is_retry_failed: bool,
    lib_state: &LibraryState,
) -> bool {
    sync_result.full_enumeration_ran
        && library_completed_without_errors
        && run_mode.downloads_files()
        && !is_retry_failed
        && sync_result.stats.assets_seen == 0
        && has_active_passes(lib_state)
}

async fn update_inventory_anchor_for_cycle<D>(
    db: &D,
    enum_config_hash: &str,
    zone_name: &str,
    stats: &mut download::SyncStats,
) where
    D: state::SyncTokenStore + ?Sized,
{
    let Some(current_total) = stats.api_total_at_start else {
        return;
    };
    let key = inventory_anchor_key(enum_config_hash, zone_name);
    match db.get_metadata(&key).await {
        Ok(Some(raw)) => match serde_json::from_str::<InventoryAnchor>(&raw) {
            Ok(anchor) => {
                if let Some(drop) =
                    classify_inventory_drop(anchor.api_total_at_start, current_total)
                {
                    tracing::warn!(
                        library = zone_name,
                        previous_api_total = drop.previous_total,
                        current_api_total = drop.current_total,
                        drop_assets = drop.drop_assets,
                        drop_percent = drop.drop_percent,
                        threshold_percent = INVENTORY_DROP_THRESHOLD_PERCENT,
                        strict_inventory = false,
                        enum_config_hash,
                        "Inventory dropped below previous comparable full run"
                    );
                    stats.inventory_drop_warnings = stats.inventory_drop_warnings.saturating_add(1);
                    stats.inventory_drop_assets = drop.drop_assets;
                    stats.inventory_drop_percent = Some(drop.drop_percent);
                    stats.inventory_drop_previous_total = Some(drop.previous_total);
                    stats.inventory_drop_current_total = Some(drop.current_total);
                    stats.inventory_drop_library = Some(zone_name.to_string());
                }
            }
            Err(e) => {
                tracing::debug!(
                    error = %e,
                    library = zone_name,
                    "Ignoring unreadable inventory anchor"
                );
            }
        },
        Ok(None) => {}
        Err(e) => {
            tracing::debug!(
                error = %e,
                library = zone_name,
                "Failed to read inventory anchor"
            );
        }
    }

    let anchor = InventoryAnchor {
        api_total_at_start: current_total,
        assets_seen: stats.assets_seen,
        completed_at: chrono::Utc::now().timestamp(),
    };
    match serde_json::to_string(&anchor) {
        Ok(value) => {
            if let Err(e) = db.set_metadata(&key, &value).await {
                tracing::debug!(
                    error = %e,
                    library = zone_name,
                    "Failed to persist inventory anchor"
                );
            }
        }
        Err(e) => {
            tracing::debug!(
                error = %e,
                library = zone_name,
                "Failed to serialize inventory anchor"
            );
        }
    }
}

/// Closure shape used to derive a per-library `DownloadConfig` from the
/// shared base config. Boxed dyn so `run_cycle` can accept a single
/// reference instead of a generic parameter (avoids reuse-by-monomorphization
/// blow-up in error messages).
pub(crate) type BuildDownloadConfigFn<'a> = dyn Fn(
        download::SyncMode,
        Arc<rustc_hash::FxHashSet<String>>,
        Arc<download::AssetGroupings>,
        Arc<str>,
    ) -> Arc<download::DownloadConfig>
    + 'a;

/// Outcome of [`check_and_persist_enum_config_hash`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EnumConfigHashOutcome {
    /// No prior hash; current hash persisted. Sync tokens left alone:
    /// a first-run DB must not invalidate tokens another process may
    /// have written.
    Initial,
    Unchanged,
    /// Hash drifted; the candidate hash was staged while active tokens and
    /// the active hash remain unchanged until reconciliation succeeds.
    Changed,
    /// Hash drift was detected, but the pending reconciliation hash could
    /// not be staged.
    ChangedTokenPurgeFailed,
    /// The stored hash could not be read, so the cycle cannot prove whether
    /// existing sync tokens still match the current config.
    ReadFailed,
}

impl EnumConfigHashOutcome {
    fn must_force_full_sync(self) -> bool {
        matches!(
            self,
            Self::Changed | Self::ChangedTokenPurgeFailed | Self::ReadFailed
        )
    }
}

/// State of the path-affecting download config hash before a cycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DownloadConfigHashOutcome {
    Unchanged,
    Changed,
    ReadFailed,
    TokenPurgeFailed,
}

impl DownloadConfigHashOutcome {
    const fn must_force_full_sync(self) -> bool {
        matches!(self, Self::ReadFailed | Self::TokenPurgeFailed)
    }

    const fn token_purge_failed(self) -> bool {
        matches!(self, Self::TokenPurgeFailed)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SyncModeDecision {
    pub(crate) mode: download::SyncMode,
    pub(crate) full_enumeration_reason: Option<download::FullEnumerationReason>,
}

fn metadata_refresh_mode_decision(refresh_metadata: bool) -> Option<SyncModeDecision> {
    refresh_metadata.then_some(SyncModeDecision {
        mode: download::SyncMode::Full,
        full_enumeration_reason: Some(download::FullEnumerationReason::MetadataBackfill),
    })
}

/// Compare the current enumeration-config hash against the active value.
/// Drift is staged without deleting the last safe provider checkpoint.
pub(crate) async fn check_and_persist_enum_config_hash<D>(
    db: &D,
    current_hash: &str,
) -> EnumConfigHashOutcome
where
    D: state::SyncTokenStore + ?Sized,
{
    let stored_hash = match db.get_metadata(ENUM_CONFIG_HASH_KEY).await {
        Ok(hash) => hash,
        Err(e) => {
            tracing::warn!(error = %e, "Failed to read enum_config_hash");
            return EnumConfigHashOutcome::ReadFailed;
        }
    };
    let outcome = match stored_hash.as_deref() {
        Some(h) if h == current_hash => {
            if let Err(e) = db
                .delete_metadata_by_prefix(PENDING_ENUM_CONFIG_HASH_KEY)
                .await
            {
                tracing::debug!(error = %e, "Failed to clear reverted pending enum config hash");
            }
            if let Err(e) = db
                .delete_metadata_by_prefix(PENDING_ZONE_TOKEN_PREFIX)
                .await
            {
                tracing::debug!(error = %e, "Failed to clear reverted pending zone checkpoints");
            }
            return EnumConfigHashOutcome::Unchanged;
        }
        Some(_) => EnumConfigHashOutcome::Changed,
        None => EnumConfigHashOutcome::Initial,
    };

    let key = if matches!(outcome, EnumConfigHashOutcome::Changed) {
        tracing::info!(
            "Enumeration config changed since last sync; preserving active checkpoints and staging reconciliation"
        );
        match db.get_metadata(PENDING_ENUM_CONFIG_HASH_KEY).await {
            Ok(Some(pending_hash)) if pending_hash == current_hash => {}
            Ok(_) => {
                if let Err(e) = db
                    .delete_metadata_by_prefix(PENDING_ZONE_TOKEN_PREFIX)
                    .await
                {
                    tracing::warn!(error = %e, "Failed to clear checkpoints from superseded config reconciliation");
                    return EnumConfigHashOutcome::ChangedTokenPurgeFailed;
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "Failed to read pending enum config hash");
                return EnumConfigHashOutcome::ReadFailed;
            }
        }
        PENDING_ENUM_CONFIG_HASH_KEY
    } else {
        ENUM_CONFIG_HASH_KEY
    };
    if let Err(e) = db.set_metadata(key, current_hash).await {
        tracing::warn!(error = %e, key, "Failed to persist enum config hash state");
        if matches!(outcome, EnumConfigHashOutcome::Changed) {
            return EnumConfigHashOutcome::ChangedTokenPurgeFailed;
        }
    }
    outcome
}

/// Check the path-affecting download config hash without deleting provider
/// cursors. Reconciliation promotes the pending hash only after it succeeds.
///
/// This hash does not prove CloudKit cursor safety. It proves local
/// path/state trust: after a directory or filename-template change, an
/// incremental `/changes/zone` cycle would only see new deltas and could miss
/// already-known assets that must be written to the new local paths.
pub(crate) async fn check_download_config_hash_for_cycle<D>(
    db: &D,
    current_hash: &str,
    legacy_hash: &str,
) -> DownloadConfigHashOutcome
where
    D: state::SyncTokenStore + ?Sized,
{
    match db.get_metadata(download::DOWNLOAD_CONFIG_HASH_KEY).await {
        Ok(Some(stored)) if stored == current_hash => {
            if let Err(e) = db
                .delete_metadata_by_prefix(PENDING_DOWNLOAD_CONFIG_HASH_KEY)
                .await
            {
                tracing::debug!(error = %e, "Failed to clear reverted pending download config hash");
            }
            DownloadConfigHashOutcome::Unchanged
        }
        Ok(Some(stored)) if stored == legacy_hash => {
            if let Err(e) = db
                .commit_checkpoint_transition(state::CheckpointTransition {
                    metadata_updates: vec![(
                        download::DOWNLOAD_CONFIG_HASH_KEY.to_owned(),
                        current_hash.to_owned(),
                    )],
                    metadata_deletes: vec![PENDING_DOWNLOAD_CONFIG_HASH_KEY.to_owned()],
                })
                .await
            {
                tracing::warn!(error = %e, "Failed to migrate legacy download config_hash");
                return DownloadConfigHashOutcome::ReadFailed;
            }
            tracing::info!("Migrated legacy download path config hash");
            DownloadConfigHashOutcome::Unchanged
        }
        Ok(None) => {
            if let Err(e) = db
                .set_metadata(download::DOWNLOAD_CONFIG_HASH_KEY, current_hash)
                .await
            {
                tracing::warn!(error = %e, "Failed to persist download config_hash");
                return DownloadConfigHashOutcome::ReadFailed;
            }
            DownloadConfigHashOutcome::Unchanged
        }
        Ok(Some(_stored)) => {
            tracing::info!(
                "Download path config changed since last sync; preserving provider checkpoints and staging local reconciliation"
            );
            if let Err(e) = db
                .set_metadata(PENDING_DOWNLOAD_CONFIG_HASH_KEY, current_hash)
                .await
            {
                tracing::warn!(error = %e, "Failed to stage pending download config_hash");
                DownloadConfigHashOutcome::TokenPurgeFailed
            } else {
                if let Err(e) = db
                    .set_metadata(LAST_RECOVERY_ACTION_KEY, "reconcile_local_paths")
                    .await
                {
                    tracing::debug!(error = %e, "Failed to persist local path reconciliation state");
                }
                DownloadConfigHashOutcome::Changed
            }
        }
        Err(e) => {
            tracing::warn!(error = %e, "Failed to read download config_hash");
            DownloadConfigHashOutcome::ReadFailed
        }
    }
}

/// Run one sync cycle: iterate all libraries, download photos, store sync tokens.
pub(crate) async fn run_cycle(
    library_states: &[&LibraryState],
    config: &config::Config,
    state_db: Option<&dyn download::DownloadStore>,
    is_retry_failed: bool,
    build_download_config: &BuildDownloadConfigFn<'_>,
    download_controls: download::DownloadControls,
    shared_session: &auth::SharedSession,
    shutdown_token: &CancellationToken,
) -> anyhow::Result<CycleResult> {
    let mut cycle_failed_count = 0usize;
    let mut cycle_session_expired = false;
    let mut cycle_stats = download::SyncStats::default();

    let cycle_has_stale_plan = library_states.iter().any(|s| s.plan_is_stale);
    if cycle_has_stale_plan {
        tracing::warn!(
            "One or more libraries are running on a stale album plan; sync \
             database pre-check token will not advance this cycle"
        );
    }
    // CONTRACT: SYNC_TOKEN_ADVANCE_REQUIRES_CLEAN_CYCLE
    let mut db_sync_token_advance_safe = !config.runtime.dry_run && !cycle_has_stale_plan;
    let mut force_full_for_config_hash = false;
    let mut force_full_for_download_config_hash = false;
    let mut checkpoint_transition_state_safe = true;
    let mut enum_config_hash_outcome = EnumConfigHashOutcome::Unchanged;
    let mut pending_download_config_hash = None;
    let mut path_reconciliation_complete = true;
    let mut checkpoint_hold_action: Option<&'static str> = None;
    let enum_config_hash = download::compute_config_hash(config);

    // Check if token-unsafe eligibility config changed since last sync. If
    // so, clear sync tokens and force full enumeration for this cycle -- the
    // stored incremental token would miss assets that are newly eligible
    // under the changed config (e.g. a user switching [photos].resolution or
    // changing [filters].media). The hash is
    // cycle-invariant across libraries,
    // so this runs once per cycle, not once per library.
    //
    // The metadata key `enum_config_hash` is distinct from the download
    // pipeline's `config_hash` (which tracks path-affecting fields only).
    // Using a single key for both would cause the two hashes to overwrite
    // each other every cycle, permanently preventing incremental sync.
    if !config.runtime.dry_run
        && let Some(db) = state_db
    {
        enum_config_hash_outcome = check_and_persist_enum_config_hash(db, &enum_config_hash).await;
        force_full_for_config_hash = enum_config_hash_outcome.must_force_full_sync();
        if matches!(enum_config_hash_outcome, EnumConfigHashOutcome::Changed) {
            let recovery = RecoveryAction::ReconcileInventory(
                download::FullEnumerationReason::EnumConfigHashDrift,
            );
            if let Err(e) = db
                .set_metadata(LAST_RECOVERY_ACTION_KEY, recovery.as_str())
                .await
            {
                tracing::debug!(error = %e, "Failed to persist inventory reconciliation action");
            }
        }
        if matches!(
            enum_config_hash_outcome,
            EnumConfigHashOutcome::ChangedTokenPurgeFailed | EnumConfigHashOutcome::ReadFailed
        ) {
            checkpoint_transition_state_safe = false;
            db_sync_token_advance_safe = false;
        }
    }

    // A path-affecting config drift does not make the CloudKit token itself
    // unsafe, but it does make an incremental cycle insufficient: with no
    // changed assets, `/changes/zone` would never re-plan already-known media
    // into the new directory/template. Detect that before choosing
    // incremental mode. Path-only drift keeps source tracking incremental;
    // the pending marker drives separate catalog/targeted rehydration work.
    if !config.runtime.dry_run
        && let (Some(db), Some(first_library)) = (state_db, library_states.first())
    {
        let probe_config = build_download_config(
            download::SyncMode::Full,
            Arc::new(rustc_hash::FxHashSet::default()),
            Arc::new(download::AssetGroupings::default()),
            Arc::from(first_library.zone_name.as_str()),
        );
        let download_config_hash = download::hash_download_config(&probe_config);
        let legacy_download_config_hash = download::hash_legacy_download_config(&probe_config);
        let outcome = check_download_config_hash_for_cycle(
            db,
            &download_config_hash,
            &legacy_download_config_hash,
        )
        .await;
        force_full_for_download_config_hash = outcome.must_force_full_sync();
        if matches!(outcome, DownloadConfigHashOutcome::Changed) {
            pending_download_config_hash = Some(download_config_hash);
        }
        if outcome.token_purge_failed() {
            checkpoint_transition_state_safe = false;
            db_sync_token_advance_safe = false;
        }
    }

    for lib_state in library_states
        .iter()
        .copied()
        .filter(|state| has_active_passes(state))
    {
        if shutdown_token.is_cancelled() {
            break;
        }

        // Determine source-enumeration mode per library. Failed transfers are
        // rehydrated from durable pending state by the download engine and do
        // not require replaying the provider inventory.
        let pending_zone_token =
            if matches!(enum_config_hash_outcome, EnumConfigHashOutcome::Changed) {
                if let Some(db) = state_db {
                    db.get_metadata(&pending_zone_token_key(
                        &enum_config_hash,
                        &lib_state.zone_name,
                    ))
                    .await?
                    .filter(|token| !token.trim().is_empty())
                } else {
                    None
                }
            } else {
                None
            };
        let sync_mode_decision = if let Some(refresh_decision) =
            metadata_refresh_mode_decision(config.runtime.refresh_metadata)
        {
            refresh_decision
        } else if let Some(zone_sync_token) = pending_zone_token {
            tracing::debug!(
                zone = %lib_state.zone_name,
                "Continuing config reconciliation from the completed zone checkpoint"
            );
            SyncModeDecision {
                mode: download::SyncMode::Incremental { zone_sync_token },
                full_enumeration_reason: None,
            }
        } else if force_full_for_config_hash {
            let reason = download::FullEnumerationReason::EnumConfigHashDrift;
            tracing::debug!(
                zone = %lib_state.zone_name,
                full_enumeration_reason = reason.as_str(),
                "Forcing full sync because config-hash validation invalidated stored sync tokens"
            );
            SyncModeDecision {
                mode: download::SyncMode::Full,
                full_enumeration_reason: Some(reason),
            }
        } else if force_full_for_download_config_hash {
            let reason = download::FullEnumerationReason::DownloadConfigHashDrift;
            tracing::debug!(
                zone = %lib_state.zone_name,
                full_enumeration_reason = reason.as_str(),
                "Forcing full sync because download config hash drift requires local path reconciliation"
            );
            SyncModeDecision {
                mode: download::SyncMode::Full,
                full_enumeration_reason: Some(reason),
            }
        } else {
            determine_sync_mode_decision(
                is_retry_failed,
                library_states.len(),
                state_db,
                &lib_state.sync_token_key,
                &lib_state.zone_name,
            )
            .await
        };
        let sync_mode = sync_mode_decision.mode;

        let sync_mode_label = match &sync_mode {
            download::SyncMode::Full => "full",
            download::SyncMode::Incremental { .. } => "incremental",
        };
        if let Some(reason) = sync_mode_decision.full_enumeration_reason {
            if let Some(db) = state_db
                && let Err(e) = db
                    .set_metadata(LAST_FULL_ENUMERATION_REASON_KEY, reason.as_str())
                    .await
            {
                tracing::debug!(error = %e, "Failed to persist full-enumeration reason");
            }
            tracing::info!(
                sync_mode = sync_mode_label,
                zone = %lib_state.zone_name,
                full_enumeration_reason = reason.as_str(),
                "Starting full-enumeration sync cycle"
            );
        } else {
            tracing::debug!(sync_mode = sync_mode_label, zone = %lib_state.zone_name, "Starting sync cycle");
        }

        // Skip the DB scan entirely when nothing downstream will read it.
        #[cfg(feature = "xmp")]
        let asset_groupings = if config.metadata.embed_xmp || config.metadata.xmp_sidecar {
            preload_asset_groupings(state_db, &lib_state.zone_name).await
        } else {
            Arc::new(download::AssetGroupings::default())
        };
        #[cfg(not(feature = "xmp"))]
        let asset_groupings = Arc::new(download::AssetGroupings::default());
        // Each pass carries its own exclude-asset-ids, so the config built
        // here starts with an empty set; download_photos_with_sync derives
        // per-pass configs internally via `with_exclude_ids`.
        let download_config = build_download_config(
            sync_mode,
            Arc::new(rustc_hash::FxHashSet::default()),
            Arc::clone(&asset_groupings),
            Arc::from(lib_state.zone_name.as_str()),
        );
        let download_client = shared_session.read().await.download_client().clone();
        if pending_download_config_hash.is_some() {
            let reconciliation = download::reconcile_catalog_paths(
                &lib_state.plan.passes,
                Arc::clone(&download_config),
                shutdown_token.clone(),
            )
            .await?;
            path_reconciliation_complete = path_reconciliation_complete && reconciliation.complete;
            let reconciliation_failures = reconciliation
                .stats
                .failed
                .saturating_add(reconciliation.stats.exif_failures)
                .saturating_add(reconciliation.stats.state_write_failures);
            cycle_failed_count = cycle_failed_count.saturating_add(reconciliation_failures);
            cycle_stats.accumulate(&reconciliation.stats);
            if reconciliation_failures > 0 {
                // A normal download/adoption pass must not move state past a
                // rejected reconciliation destination or failed finalization.
                db_sync_token_advance_safe = false;
                continue;
            }
        }
        let mut sync_result = download::download_photos_with_sync(
            &download_client,
            &lib_state.plan.passes,
            download_config,
            download_controls,
            shutdown_token.clone(),
        )
        .await?;

        let mut checkpoint_basis = if sync_result.full_enumeration_ran {
            CheckpointBasis::CompleteInventory
        } else {
            CheckpointBasis::IncrementalDelta
        };

        // Eligibility reconciliation begins from a trusted prior cursor when
        // one exists. The inventory covers unchanged newly eligible assets;
        // replaying the prior cursor then bridges changes that occurred while
        // that inventory was running. Only the bridged token may replace the
        // active checkpoint.
        if matches!(enum_config_hash_outcome, EnumConfigHashOutcome::Changed)
            && sync_result.full_enumeration_ran
            && checkpoint_transition_state_safe
        {
            let prior_token = if let Some(db) = state_db {
                db.get_metadata(&lib_state.sync_token_key).await?
            } else {
                None
            };
            if let Some(prior_token) = prior_token.filter(|token| !token.trim().is_empty()) {
                let inventory_decision = source_checkpoint_decision(
                    &sync_result,
                    config.runtime.dry_run,
                    lib_state.plan_is_stale,
                    CheckpointBasis::CompleteInventory,
                );
                if matches!(inventory_decision, SourceCheckpointDecision::Advance { .. }) {
                    let bridge_config = build_download_config(
                        download::SyncMode::Incremental {
                            zone_sync_token: prior_token,
                        },
                        Arc::new(rustc_hash::FxHashSet::default()),
                        Arc::clone(&asset_groupings),
                        Arc::from(lib_state.zone_name.as_str()),
                    );
                    tracing::info!(
                        zone = %lib_state.zone_name,
                        "Inventory reconciliation complete; bridging changes from the preserved provider checkpoint"
                    );
                    let bridge_result = download::download_photos_with_sync(
                        &download_client,
                        &lib_state.plan.passes,
                        bridge_config,
                        download_controls,
                        shutdown_token.clone(),
                    )
                    .await?;
                    let bridge_decision = source_checkpoint_decision(
                        &bridge_result,
                        config.runtime.dry_run,
                        lib_state.plan_is_stale,
                        CheckpointBasis::IncrementalDelta,
                    );
                    let bridge_outcome =
                        merge_download_outcomes(&sync_result.outcome, &bridge_result.outcome);
                    sync_result.stats.accumulate(&bridge_result.stats);
                    sync_result.outcome = bridge_outcome;
                    if !bridge_result.full_enumeration_ran {
                        if let SourceCheckpointDecision::Advance { token, .. } = bridge_decision {
                            sync_result.sync_token = Some(token);
                            checkpoint_basis = CheckpointBasis::InventoryWithDeltaBridge;
                        } else {
                            sync_result.sync_token = None;
                            sync_result.stats.sync_token_blocked = true;
                            sync_result.stats.sync_token_blocked_reason =
                                Some("inventory_delta_bridge_failed");
                            sync_result.stats.sync_token_blocked_source = Some("kei");
                            sync_result.stats.sync_token_blocked_explanation = Some(
                                "the inventory completed, but replay from the preserved provider checkpoint did not finish safely",
                            );
                        }
                    } else {
                        sync_result.sync_token = None;
                        sync_result.stats.sync_token_blocked = true;
                        sync_result.stats.sync_token_blocked_reason =
                            Some("inventory_delta_bridge_failed");
                        sync_result.stats.sync_token_blocked_source = Some("kei");
                        sync_result.stats.sync_token_blocked_explanation = Some(
                            "the prior provider checkpoint could not be replayed incrementally after inventory reconciliation",
                        );
                    }
                }
            }
        }

        if sync_result.full_enumeration_ran && sync_result.stats.full_enumeration_reason.is_none() {
            sync_result.stats.full_enumeration_reason = sync_mode_decision.full_enumeration_reason;
        }

        let library_completed_without_errors =
            matches!(&sync_result.outcome, download::DownloadOutcome::Success)
                && !sync_result.stats.interrupted
                && sync_result.stats.enumeration_errors == 0
                && !shutdown_token.is_cancelled();
        if should_warn_zero_assets(
            &sync_result,
            library_completed_without_errors,
            download_controls.run_mode,
            is_retry_failed,
            lib_state,
        ) {
            tracing::warn!(
                library = %lib_state.zone_name,
                library_count = library_states.len(),
                assets_seen = sync_result.stats.assets_seen,
                "Sync completed after enumerating zero assets; check iCloud library \
                 access and filters if this was unexpected"
            );
        }
        if library_completed_without_errors
            && !cycle_has_stale_plan
            && download_controls.run_mode.downloads_files()
            && let Some(db) = state_db
        {
            update_inventory_anchor_for_cycle(
                db,
                &enum_config_hash,
                &lib_state.zone_name,
                &mut sync_result.stats,
            )
            .await;
        }

        // Provider cursor safety is independent from transfer completion.
        // The download pipeline persists every planned work item before this
        // boundary, so failed transfers may advance while state, identity,
        // enumeration, auth, and token-proof failures preserve the old cursor.
        let checkpoint_decision =
            if checkpoint_transition_state_safe && !shutdown_token.is_cancelled() {
                source_checkpoint_decision(
                    &sync_result,
                    config.runtime.dry_run,
                    lib_state.plan_is_stale,
                    checkpoint_basis,
                )
            } else if shutdown_token.is_cancelled() {
                SourceCheckpointDecision::Preserve {
                    reason: CheckpointHoldReason::Interrupted,
                    recovery: RecoveryAction::ReplayFromPriorToken,
                }
            } else {
                SourceCheckpointDecision::Preserve {
                    reason: CheckpointHoldReason::StateNotDurable,
                    recovery: RecoveryAction::ReplayFromPriorToken,
                }
            };

        match checkpoint_decision {
            SourceCheckpointDecision::Advance { token, basis } => {
                crate::metrics::record_checkpoint_decision("advanced", basis.as_str());
                crate::metrics::record_deferred_transfers(
                    sync_result
                        .stats
                        .failed
                        .saturating_add(sync_result.stats.exif_failures),
                );
                if let Some(db) = state_db {
                    let reconciliation_active = force_full_for_config_hash;
                    if reconciliation_active {
                        let candidate_key =
                            pending_zone_token_key(&enum_config_hash, &lib_state.zone_name);
                        if let Err(e) = db.set_metadata(&candidate_key, &token).await {
                            checkpoint_hold_action =
                                Some(RecoveryAction::ReplayFromPriorToken.as_str());
                            db_sync_token_advance_safe = false;
                            tracing::warn!(
                                zone = %lib_state.zone_name,
                                error = %e,
                                "Failed to retain completed zone reconciliation checkpoint"
                            );
                        }
                    } else {
                        let mut metadata_updates = vec![(lib_state.sync_token_key.clone(), token)];
                        if let Some(recovery_action) = checkpoint_hold_action {
                            metadata_updates.push((
                                LAST_CHECKPOINT_STATUS_KEY.to_owned(),
                                "preserved".to_owned(),
                            ));
                            metadata_updates.push((
                                LAST_RECOVERY_ACTION_KEY.to_owned(),
                                recovery_action.to_owned(),
                            ));
                        } else {
                            metadata_updates.push((
                                LAST_CHECKPOINT_STATUS_KEY.to_owned(),
                                "current".to_owned(),
                            ));
                            metadata_updates
                                .push((LAST_RECOVERY_ACTION_KEY.to_owned(), "none".to_owned()));
                        }
                        if let Err(e) = db
                            .commit_checkpoint_transition(state::CheckpointTransition {
                                metadata_updates,
                                metadata_deletes: Vec::new(),
                            })
                            .await
                        {
                            checkpoint_hold_action =
                                Some(RecoveryAction::ReplayFromPriorToken.as_str());
                            db_sync_token_advance_safe = false;
                            tracing::warn!(error = %e, "Failed to store provider checkpoint");
                        } else {
                            if cycle_has_stale_plan && !lib_state.plan_is_stale {
                                tracing::warn!(
                                    zone = %lib_state.zone_name,
                                    diagnostic = "stale_plan_unaffected_zone",
                                    "Stored clean zone checkpoint despite another selected zone's stale plan"
                                );
                            }
                            if sync_result.stats.failed > 0 || sync_result.stats.exif_failures > 0 {
                                tracing::info!(
                                    zone = %lib_state.zone_name,
                                    basis = ?basis,
                                    deferred_transfers = sync_result.stats.failed,
                                    metadata_failures = sync_result.stats.exif_failures,
                                    "Provider checkpoint advanced: incomplete local work is durably queued for targeted retry"
                                );
                            } else {
                                tracing::debug!(zone = %lib_state.zone_name, basis = ?basis, "Stored provider checkpoint for next incremental sync");
                            }
                        }
                    }
                } else {
                    db_sync_token_advance_safe = false;
                    tracing::debug!(
                        zone = %lib_state.zone_name,
                        "Provider checkpoint available but no state DB is configured"
                    );
                }
            }
            SourceCheckpointDecision::Preserve { reason, recovery } => {
                checkpoint_hold_action = Some(recovery.as_str());
                crate::metrics::record_checkpoint_decision("preserved", reason.as_str());
                db_sync_token_advance_safe = false;
                if let Some(db) = state_db
                    && let Err(e) = db
                        .commit_checkpoint_transition(state::CheckpointTransition {
                            metadata_updates: vec![
                                (
                                    LAST_CHECKPOINT_STATUS_KEY.to_owned(),
                                    "preserved".to_owned(),
                                ),
                                (
                                    LAST_RECOVERY_ACTION_KEY.to_owned(),
                                    recovery.as_str().to_owned(),
                                ),
                            ],
                            metadata_deletes: Vec::new(),
                        })
                        .await
                {
                    tracing::debug!(error = %e, "Failed to persist checkpoint hold status");
                }
                let diagnostic = sync_result
                    .stats
                    .sync_token_blocked_reason
                    .unwrap_or("provider_checkpoint_preserved");
                let explanation = sync_result
                    .stats
                    .sync_token_blocked_explanation
                    .unwrap_or_else(|| download::sync_token_blocked_explanation(diagnostic));
                tracing::warn!(
                    zone = %lib_state.zone_name,
                    reason = ?reason,
                    recovery = ?recovery,
                    diagnostic,
                    explanation,
                    "Provider checkpoint preserved; recovery will use the narrowest safe action"
                );
            }
        }

        if sync_result.stats.sync_token_blocked
            && sync_result.stats.sync_token_blocked_zone.is_none()
        {
            sync_result.stats.sync_token_blocked_zone = Some(lib_state.zone_name.clone());
        }

        // Accumulate stats across libraries.
        cycle_stats.accumulate(&sync_result.stats);

        match sync_result.outcome {
            download::DownloadOutcome::Success => {}
            download::DownloadOutcome::SessionExpired { auth_error_count } => {
                tracing::warn!(
                    auth_error_count,
                    zone = %lib_state.zone_name,
                    "Session expired during library sync"
                );
                cycle_session_expired = true;
                break; // Stop iterating libraries -- need re-auth
            }
            download::DownloadOutcome::PartialFailure { failed_count } => {
                cycle_failed_count += failed_count;
            }
        }
    }

    let reconciliation_active = force_full_for_config_hash;
    if reconciliation_active
        && db_sync_token_advance_safe
        && !cycle_session_expired
        && !shutdown_token.is_cancelled()
        && let Some(db) = state_db
    {
        let mut metadata_updates = Vec::new();
        let mut metadata_deletes = Vec::new();
        if force_full_for_config_hash {
            for lib_state in library_states
                .iter()
                .copied()
                .filter(|state| has_active_passes(state))
            {
                let candidate_key = pending_zone_token_key(&enum_config_hash, &lib_state.zone_name);
                let Some(token) = db
                    .get_metadata(&candidate_key)
                    .await?
                    .filter(|token| !token.trim().is_empty())
                else {
                    db_sync_token_advance_safe = false;
                    tracing::warn!(
                        zone = %lib_state.zone_name,
                        "Config reconciliation has no completed checkpoint for selected zone"
                    );
                    break;
                };
                metadata_updates.push((lib_state.sync_token_key.clone(), token));
                metadata_deletes.push(candidate_key);
            }
            if db_sync_token_advance_safe {
                metadata_updates.push((ENUM_CONFIG_HASH_KEY.to_owned(), enum_config_hash));
                metadata_deletes.push(PENDING_ENUM_CONFIG_HASH_KEY.to_owned());
            }
        }
        if db_sync_token_advance_safe {
            metadata_updates.push((LAST_CHECKPOINT_STATUS_KEY.to_owned(), "current".to_owned()));
            metadata_updates.push((LAST_RECOVERY_ACTION_KEY.to_owned(), "none".to_owned()));
            if let Err(e) = db
                .commit_checkpoint_transition(state::CheckpointTransition {
                    metadata_updates,
                    metadata_deletes,
                })
                .await
            {
                db_sync_token_advance_safe = false;
                tracing::warn!(error = %e, "Failed to atomically promote checkpoint reconciliation");
            }
        }
    }

    if path_reconciliation_complete
        && !cycle_session_expired
        && !shutdown_token.is_cancelled()
        && let (Some(db), Some(download_config_hash)) = (state_db, pending_download_config_hash)
        && let Err(e) = db
            .commit_checkpoint_transition(state::CheckpointTransition {
                metadata_updates: vec![(
                    download::DOWNLOAD_CONFIG_HASH_KEY.to_owned(),
                    download_config_hash,
                )],
                metadata_deletes: vec![PENDING_DOWNLOAD_CONFIG_HASH_KEY.to_owned()],
            })
            .await
    {
        tracing::warn!(error = %e, "Failed to promote completed local path reconciliation");
    }

    Ok(CycleResult {
        failed_count: cycle_failed_count,
        session_expired: cycle_session_expired,
        stats: cycle_stats,
        db_sync_token_advance_safe,
    })
}

/// Bulk-load `asset_albums` + `asset_people` into an in-memory index so the
/// filter phase can enrich payloads without per-asset DB hits. Scoped to a
/// single library so multi-library accounts don't cross-attribute album /
/// person memberships across zones (the v9 schema scopes both join tables
/// per library; this reader honours that scope).
#[cfg(feature = "xmp")]
pub(crate) async fn preload_asset_groupings<D>(
    state_db: Option<&D>,
    library: &str,
) -> Arc<download::AssetGroupings>
where
    D: state::MembershipStore + ?Sized,
{
    let Some(db) = state_db else {
        return Arc::new(download::AssetGroupings::default());
    };
    let albums = db.get_all_asset_albums(library).await;
    let people = db.get_all_asset_people(library).await;
    let mut groupings = download::AssetGroupings::default();
    match albums {
        Ok(rows) => {
            for (asset_id, album) in rows {
                groupings.albums.entry(asset_id).or_default().push(album);
            }
        }
        Err(e) => tracing::warn!(error = %e, library, "Failed to preload asset_albums"),
    }
    match people {
        Ok(rows) => {
            for (asset_id, person) in rows {
                groupings.people.entry(asset_id).or_default().push(person);
            }
        }
        Err(e) => tracing::warn!(error = %e, library, "Failed to preload asset_people"),
    }
    Arc::new(groupings)
}

/// Determine the sync mode for a library: full enumeration or incremental.
pub(crate) async fn determine_sync_mode_decision<D>(
    _is_retry_failed: bool,
    _library_count: usize,
    state_db: Option<&D>,
    sync_token_key: &str,
    zone_name: &str,
) -> SyncModeDecision
where
    D: state::SyncTokenStore + ?Sized,
{
    if let Some(db) = state_db {
        match db.get_metadata(sync_token_key).await {
            Ok(Some(ref token)) if !token.is_empty() => {
                tracing::debug!(zone = %zone_name, "Stored sync token found, using incremental sync");
                SyncModeDecision {
                    mode: download::SyncMode::Incremental {
                        zone_sync_token: token.clone(),
                    },
                    full_enumeration_reason: None,
                }
            }
            Ok(_) => {
                let reason = download::FullEnumerationReason::NoStoredToken;
                tracing::debug!(
                    zone = %zone_name,
                    full_enumeration_reason = reason.as_str(),
                    "No sync token found, performing full enumeration"
                );
                SyncModeDecision {
                    mode: download::SyncMode::Full,
                    full_enumeration_reason: Some(reason),
                }
            }
            Err(e) => {
                let reason = download::FullEnumerationReason::OtherStaticReason;
                tracing::warn!(
                    error = %e,
                    full_enumeration_reason = reason.as_str(),
                    "Failed to load sync token, falling back to full enumeration"
                );
                SyncModeDecision {
                    mode: download::SyncMode::Full,
                    full_enumeration_reason: Some(reason),
                }
            }
        }
    } else {
        SyncModeDecision {
            mode: download::SyncMode::Full,
            full_enumeration_reason: Some(download::FullEnumerationReason::NoStoredToken),
        }
    }
}

#[cfg(test)]
pub(crate) async fn determine_sync_mode<D>(
    is_retry_failed: bool,
    library_count: usize,
    state_db: Option<&D>,
    sync_token_key: &str,
    zone_name: &str,
) -> download::SyncMode
where
    D: state::SyncTokenStore + ?Sized,
{
    determine_sync_mode_decision(
        is_retry_failed,
        library_count,
        state_db,
        sync_token_key,
        zone_name,
    )
    .await
    .mode
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::PassKind;

    fn make_library_state(has_passes: bool) -> LibraryState {
        LibraryState {
            library: crate::icloud::photos::PhotoLibrary::new_stub_with_zone(
                Box::new(crate::test_helpers::MockPhotosSession::new()),
                "PrimarySync",
            ),
            cross_zone_libraries: Vec::new(),
            pass_scope: crate::commands::PassScope {
                include_albums: false,
                include_smart_folders: false,
                include_unfiled: has_passes,
            },
            zone_name: "PrimarySync".to_string(),
            sync_token_key: "sync_token:PrimarySync".to_string(),
            plan: crate::commands::AlbumPlan {
                passes: if has_passes {
                    vec![crate::commands::AlbumPass {
                        kind: PassKind::Unfiled,
                        album: crate::icloud::photos::PhotoAlbum::stub_for_test(Arc::from(
                            "PrimarySync",
                        )),
                        exclude_ids: Arc::new(rustc_hash::FxHashSet::default()),
                    }]
                } else {
                    Vec::new()
                },
            },
            plan_is_stale: false,
            plan_needs_refresh: false,
        }
    }

    #[test]
    fn should_warn_zero_assets_requires_active_passes() {
        let sync_result = download::SyncResult {
            outcome: download::DownloadOutcome::Success,
            sync_token: Some("zone-token".to_string()),
            stats: download::SyncStats {
                assets_seen: 0,
                ..download::SyncStats::default()
            },
            full_enumeration_ran: true,
        };
        assert!(!should_warn_zero_assets(
            &sync_result,
            true,
            download::DownloadRunMode::Download,
            false,
            &make_library_state(false),
        ));
    }

    #[test]
    fn should_warn_zero_assets_when_all_gates_are_true() {
        let sync_result = download::SyncResult {
            outcome: download::DownloadOutcome::Success,
            sync_token: Some("zone-token".to_string()),
            stats: download::SyncStats {
                assets_seen: 0,
                ..download::SyncStats::default()
            },
            full_enumeration_ran: true,
        };
        assert!(should_warn_zero_assets(
            &sync_result,
            true,
            download::DownloadRunMode::Download,
            false,
            &make_library_state(true),
        ));
    }

    #[test]
    fn durable_transfer_failure_can_advance_source_checkpoint() {
        let result = download::SyncResult {
            outcome: download::DownloadOutcome::PartialFailure { failed_count: 1 },
            sync_token: Some("zone-token-next".to_string()),
            stats: download::SyncStats {
                failed: 1,
                ..download::SyncStats::default()
            },
            full_enumeration_ran: false,
        };

        assert_eq!(
            source_checkpoint_decision(&result, false, false, CheckpointBasis::IncrementalDelta,),
            SourceCheckpointDecision::Advance {
                token: "zone-token-next".to_string(),
                basis: CheckpointBasis::IncrementalDelta,
            }
        );
    }

    #[test]
    fn failed_retry_state_write_preserves_source_checkpoint() {
        let result = download::SyncResult {
            outcome: download::DownloadOutcome::PartialFailure { failed_count: 1 },
            sync_token: Some("zone-token-next".to_string()),
            stats: download::SyncStats {
                failed: 1,
                state_write_failures: 1,
                ..download::SyncStats::default()
            },
            full_enumeration_ran: false,
        };

        assert_eq!(
            source_checkpoint_decision(&result, false, false, CheckpointBasis::IncrementalDelta,),
            SourceCheckpointDecision::Preserve {
                reason: CheckpointHoldReason::StateNotDurable,
                recovery: RecoveryAction::ReplayFromPriorToken,
            }
        );
    }

    #[test]
    fn incomplete_token_proof_preserves_concrete_retry_passes() {
        let pass = download::PassKey {
            index: 2,
            kind: PassKind::Unfiled,
            label: "unfiled".to_string(),
        };
        let result = download::SyncResult {
            outcome: download::DownloadOutcome::Success,
            sync_token: None,
            stats: download::SyncStats {
                sync_token_blocked: true,
                checkpoint_retry_passes: vec![pass.clone()],
                ..download::SyncStats::default()
            },
            full_enumeration_ran: true,
        };

        assert_eq!(
            source_checkpoint_decision(&result, false, false, CheckpointBasis::CompleteInventory),
            SourceCheckpointDecision::Preserve {
                reason: CheckpointHoldReason::TokenProofIncomplete,
                recovery: RecoveryAction::RetryPasses(vec![pass]),
            }
        );
    }

    #[test]
    fn contract_source_checkpoint_requires_durable_recovery_exhaustive() {
        let tokens = [Some("zone-token"), None, Some("   ")];
        for mask in 0u16..512 {
            let dry_run = mask & 1 != 0;
            let stale_pass_plan = mask & 2 != 0;
            let session_expired = mask & 4 != 0;
            let interrupted = mask & 8 != 0;
            let enumeration_error = mask & 16 != 0;
            let enumeration_incomplete = mask & 32 != 0;
            let state_not_durable = mask & 64 != 0;
            let token_proof_blocked = mask & 128 != 0;
            let durable_transfer_failure = mask & 256 != 0;
            let has_safety_blocker = mask & 255 != 0;
            for token in tokens {
                let result = download::SyncResult {
                    outcome: if session_expired {
                        download::DownloadOutcome::SessionExpired {
                            auth_error_count: 1,
                        }
                    } else if durable_transfer_failure {
                        download::DownloadOutcome::PartialFailure { failed_count: 1 }
                    } else {
                        download::DownloadOutcome::Success
                    },
                    sync_token: token.map(str::to_string),
                    stats: download::SyncStats {
                        interrupted,
                        enumeration_errors: usize::from(enumeration_error),
                        enumeration_incomplete,
                        state_write_failures: usize::from(state_not_durable),
                        sync_token_blocked: token_proof_blocked,
                        ..download::SyncStats::default()
                    },
                    full_enumeration_ran: false,
                };
                let should_advance = !has_safety_blocker && token == Some("zone-token");
                let decision = source_checkpoint_decision(
                    &result,
                    dry_run,
                    stale_pass_plan,
                    CheckpointBasis::IncrementalDelta,
                );
                assert_eq!(
                    matches!(decision, SourceCheckpointDecision::Advance { .. }),
                    should_advance,
                    "unexpected checkpoint decision for mask {mask:#010b} and token {token:?}"
                );
            }
        }
    }

    #[test]
    fn inventory_drop_classifier_uses_five_percent_threshold() {
        assert_eq!(classify_inventory_drop(100, 96), None);
        assert_eq!(
            classify_inventory_drop(100, 95),
            Some(InventoryDrop {
                previous_total: 100,
                current_total: 95,
                drop_assets: 5,
                drop_percent: 5.0,
            })
        );
        assert_eq!(classify_inventory_drop(0, 0), None);
        assert_eq!(classify_inventory_drop(100, 101), None);
    }

    #[tokio::test]
    async fn inventory_anchor_warns_and_updates_on_comparable_drop() {
        let db = state::SqliteStateDb::open_in_memory().expect("state db");
        let key = inventory_anchor_key("hash-a", "PrimarySync");
        let prior = InventoryAnchor {
            api_total_at_start: 100,
            assets_seen: 100,
            completed_at: 1_700_000_000,
        };
        db.set_metadata(&key, &serde_json::to_string(&prior).unwrap())
            .await
            .expect("seed anchor");

        let mut stats = download::SyncStats {
            api_total_at_start: Some(80),
            assets_seen: 80,
            ..download::SyncStats::default()
        };
        update_inventory_anchor_for_cycle(&db, "hash-a", "PrimarySync", &mut stats).await;

        assert_eq!(stats.inventory_drop_warnings, 1);
        assert_eq!(stats.inventory_drop_assets, 20);
        assert_eq!(stats.inventory_drop_previous_total, Some(100));
        assert_eq!(stats.inventory_drop_current_total, Some(80));
        assert_eq!(
            stats.inventory_drop_library,
            Some("PrimarySync".to_string())
        );
        let raw = db
            .get_metadata(&key)
            .await
            .expect("read anchor")
            .expect("anchor exists");
        let updated: InventoryAnchor = serde_json::from_str(&raw).expect("anchor json");
        assert_eq!(updated.api_total_at_start, 80);
    }

    #[tokio::test]
    async fn inventory_anchor_is_scoped_by_enum_config_hash() {
        let db = state::SqliteStateDb::open_in_memory().expect("state db");
        let old_key = inventory_anchor_key("old-hash", "PrimarySync");
        let prior = InventoryAnchor {
            api_total_at_start: 100,
            assets_seen: 100,
            completed_at: 1_700_000_000,
        };
        db.set_metadata(&old_key, &serde_json::to_string(&prior).unwrap())
            .await
            .expect("seed anchor");

        let mut stats = download::SyncStats {
            api_total_at_start: Some(80),
            assets_seen: 80,
            ..download::SyncStats::default()
        };
        update_inventory_anchor_for_cycle(&db, "new-hash", "PrimarySync", &mut stats).await;

        assert_eq!(stats.inventory_drop_warnings, 0);
        assert!(
            db.get_metadata(&inventory_anchor_key("new-hash", "PrimarySync"))
                .await
                .expect("read new anchor")
                .is_some(),
            "new hash should get an independent anchor"
        );
    }

    #[tokio::test]
    async fn determine_sync_mode_two_normal_syncs_reuse_stored_token() {
        let db = state::SqliteStateDb::open_in_memory().expect("state db");
        let sync_token_key = sync_token_key("PrimarySync");
        db.set_metadata(&sync_token_key, "stored-token-abc")
            .await
            .expect("set token");

        for cycle in 1..=2 {
            let mode =
                determine_sync_mode(false, 1, Some(&db), &sync_token_key, "PrimarySync").await;
            assert!(
                matches!(mode, download::SyncMode::Incremental { ref zone_sync_token } if zone_sync_token == "stored-token-abc"),
                "normal sync cycle {cycle} should use the stored token, got {mode:?}"
            );
        }

        assert_eq!(
            db.get_metadata(&sync_token_key).await.expect("read token"),
            Some("stored-token-abc".to_string()),
            "mode selection must not consume or clear the stored token"
        );
    }

    #[test]
    fn metadata_refresh_forces_full_enumeration() {
        assert_eq!(
            metadata_refresh_mode_decision(true),
            Some(SyncModeDecision {
                mode: download::SyncMode::Full,
                full_enumeration_reason: Some(download::FullEnumerationReason::MetadataBackfill),
            })
        );
        assert_eq!(metadata_refresh_mode_decision(false), None);
    }

    #[tokio::test]
    async fn determine_sync_mode_decision_records_explicit_full_reasons() {
        let db = state::SqliteStateDb::open_in_memory().expect("state db");
        let sync_token_key = sync_token_key("PrimarySync");
        db.set_metadata(&sync_token_key, "stored-token-abc")
            .await
            .expect("set token");

        let retry =
            determine_sync_mode_decision(true, 1, Some(&db), &sync_token_key, "PrimarySync").await;
        assert!(matches!(
            retry.mode,
            download::SyncMode::Incremental { ref zone_sync_token }
                if zone_sync_token == "stored-token-abc"
        ));
        assert_eq!(retry.full_enumeration_reason, None);

        db.delete_metadata_by_prefix(SYNC_TOKEN_PREFIX)
            .await
            .expect("clear token");
        let no_token =
            determine_sync_mode_decision(false, 1, Some(&db), &sync_token_key, "PrimarySync").await;
        assert!(matches!(no_token.mode, download::SyncMode::Full));
        assert_eq!(
            no_token.full_enumeration_reason,
            Some(download::FullEnumerationReason::NoStoredToken)
        );
    }
}
