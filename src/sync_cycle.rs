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
pub(crate) const PENDING_ZONE_TOKEN_PREFIX: &str = "pending_sync_token:v27:";
const LAST_CHECKPOINT_STATUS_KEY: &str = "last_checkpoint_status";
const LAST_RECOVERY_ACTION_KEY: &str = "last_recovery_action";
const LAST_FULL_ENUMERATION_REASON_KEY: &str = "last_full_enumeration_reason";

/// Current sync epoch. Older opaque cursors remain stored for rollback but
/// cannot authorize skipping the v27 filesystem/provider comparison.
pub(crate) const SYNC_TOKEN_PREFIX: &str = "sync_token:v27:";

/// Current observed-work checkpoints do not certify historical attribution.
async fn sparse_identity_holds_current(
    db: &dyn download::DownloadStore,
    library: &str,
    current_epoch: bool,
) -> Result<bool, state::error::StateError> {
    let marker = db
        .get_metadata(&state::unresolved_identity_key(library))
        .await?;
    let marked = marker.is_some();
    if !current_epoch || marker.as_deref() == Some(state::UNACCOUNTED_CURRENT_IDENTITY) {
        return Ok(marked);
    }
    let rows = db.sparse_identities(library).await?;
    Ok(marked || !rows.is_empty())
}

pub(crate) fn is_current_zone_checkpoint(key: &str) -> bool {
    key.starts_with(SYNC_TOKEN_PREFIX) || key.starts_with(PENDING_ZONE_TOKEN_PREFIX)
}

// Legacy inventory_anchor: rows lack coverage proof and are never read or migrated.
const INVENTORY_ANCHOR_PREFIX: &str = "inventory_anchor_v1:";
const INVENTORY_ANCHOR_VERSION: u8 = 1;
/// Presence is a durable obligation, including unknown future payloads. Only a
/// proven FINISHED full inventory may remove it with checkpoint promotion.
pub(crate) const INDEXING_RECONCILIATION_PREFIX: &str = "indexing_reconciliation_required:";

pub(crate) fn indexing_reconciliation_key(zone: &str) -> String {
    format!("{INDEXING_RECONCILIATION_PREFIX}{zone}")
}

/// A qualified inventory retains the old cursor and schedules its replay in a
/// later trusted cycle. No deletion-capable bridge runs inside the held phase.
pub(crate) const INDEXING_REPLAY_PREFIX: &str = "indexing_replay_required:v27:";

pub(crate) fn indexing_replay_key(zone: &str) -> String {
    format!("{INDEXING_REPLAY_PREFIX}{zone}")
}

/// Managed qualification did not queue files. This obligation forces a full
/// managed publication pass before retained-cursor replay may discharge it.
pub(crate) fn indexing_materialization_key(zone: &str) -> String {
    format!("indexing_materialization_required:{zone}")
}

/// Observe readiness and durably retain every RUNNING observation, even if a
/// later readiness request fails before enumeration starts.
pub(crate) async fn prepare_indexing_reconciliation(
    library: &crate::icloud::photos::PhotoLibrary,
    db: Option<&dyn download::DownloadStore>,
    dry_run: bool,
) -> anyhow::Result<(bool, Option<String>)> {
    let key = indexing_reconciliation_key(library.zone_name());
    let mut hold = match db {
        Some(db) => db.get_metadata(&key).await?,
        None => None,
    };
    if !library.last_indexing_finished() && !dry_run {
        let db = db.ok_or_else(|| {
            anyhow::anyhow!("Provisional indexing downloads require durable state")
        })?;
        if hold.is_none() {
            db.set_metadata(&key, "1").await?;
            hold = Some("1".to_owned());
        }
    }
    let finished = library.check_indexing().await?;
    if !finished && !dry_run {
        let db = db.ok_or_else(|| {
            anyhow::anyhow!("Provisional indexing downloads require durable state")
        })?;
        if hold.is_none() {
            db.set_metadata(&key, "1").await?;
            hold = Some("1".to_owned());
        }
    }
    Ok((finished, hold))
}

/// Cross-zone album hydration contributes to the selected listing too. Any
/// unfinished source makes the selected zone provisional until all are ready.
pub(crate) async fn prepare_library_indexing(
    library: &LibraryState,
    db: Option<&dyn download::DownloadStore>,
    dry_run: bool,
) -> anyhow::Result<(bool, Option<String>)> {
    let (mut finished, mut hold) =
        prepare_indexing_reconciliation(&library.library, db, dry_run).await?;
    for source in &library.cross_zone_libraries {
        if source.zone_name() == library.zone_name {
            continue;
        }
        let source_was_running = !source.last_indexing_finished();
        let (source_finished, _) = prepare_indexing_reconciliation(source, db, dry_run).await?;
        finished &= source_finished;
        if (source_was_running || !source_finished) && hold.is_none() && !dry_run {
            let db = db.ok_or_else(|| {
                anyhow::anyhow!("Provisional indexing downloads require durable state")
            })?;
            db.set_metadata(&indexing_reconciliation_key(&library.zone_name), "1")
                .await?;
            hold = Some("1".to_owned());
        }
    }
    Ok((finished, hold))
}

pub(crate) const RETAINED_CHECKPOINT_HOLD_PREFIX: &str = "retained_checkpoint_hold:v27:";
const RETAINED_CHECKPOINT_RETRY_SECONDS: i64 = 60 * 60;
const RETAINED_CHECKPOINT_MAX_ATTEMPTS: u8 = 3;

const fn first_retained_checkpoint_attempt() -> u8 {
    1
}

/// A delay is bound to the retained boundary and the requested policy. It never
/// certifies inventory coverage or historical identity/deletion resolution.
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RetainedCheckpointHold {
    version: u8,
    prior_cursor_sha256: String,
    enum_config_hash: String,
    preservation_config_hash: Option<String>,
    next_attempt_at: i64,
    #[serde(default = "first_retained_checkpoint_attempt")]
    attempt_count: u8,
}

impl RetainedCheckpointHold {
    fn new(prior_cursor: &str, enum_hash: &str, preservation_hash: Option<&str>) -> Self {
        use sha2::{Digest, Sha256};
        Self {
            version: 1,
            attempt_count: 1,
            prior_cursor_sha256: data_encoding::HEXLOWER.encode(&Sha256::digest(prior_cursor)),
            enum_config_hash: enum_hash.to_owned(),
            preservation_config_hash: preservation_hash.map(str::to_owned),
            next_attempt_at: chrono::Utc::now()
                .timestamp()
                .saturating_add(RETAINED_CHECKPOINT_RETRY_SECONDS),
        }
    }

    fn matches(&self, current: &Self) -> bool {
        self.version == 1
            && self.attempt_count > 0
            && self.prior_cursor_sha256 == current.prior_cursor_sha256
            && self.enum_config_hash == current.enum_config_hash
            && self.preservation_config_hash == current.preservation_config_hash
    }

    fn deferred(&self, now: i64) -> bool {
        self.next_attempt_at > now
            && self.next_attempt_at <= now.saturating_add(RETAINED_CHECKPOINT_RETRY_SECONDS)
    }
}

pub(crate) fn retained_checkpoint_hold_action(encoded: &str) -> &'static str {
    match serde_json::from_str::<RetainedCheckpointHold>(encoded) {
        Ok(hold) if hold.version == 1 && hold.attempt_count > 0 => {
            if hold.attempt_count >= RETAINED_CHECKPOINT_MAX_ATTEMPTS {
                RecoveryAction::RetainedCheckpointRetryExhausted.as_str()
            } else {
                RecoveryAction::AwaitRetainedCheckpointEvidence.as_str()
            }
        }
        _ => RecoveryAction::RepairRetainedCheckpointEvidence.as_str(),
    }
}

fn retained_checkpoint_hold_key(zone: &str) -> String {
    format!("{RETAINED_CHECKPOINT_HOLD_PREFIX}{zone}")
}

fn block_for_retained_checkpoint(stats: &mut download::SyncStats) {
    stats.sync_token_blocked = true;
    stats.sync_token_blocked_reason = Some("retained_checkpoint_expired");
    stats.sync_token_blocked_source = Some("kei");
    stats.sync_token_blocked_explanation = Some(download::sync_token_blocked_explanation(
        "retained_checkpoint_expired",
    ));
}

async fn persist_retained_checkpoint_hold(
    db: &dyn download::DownloadStore,
    key: String,
    hold: &RetainedCheckpointHold,
    expected_metadata: Vec<(String, Option<String>)>,
) -> anyhow::Result<()> {
    db.commit_checkpoint_transition(state::CheckpointTransition {
        expected_metadata,
        legacy_preservation_proofs: Vec::new(),
        legacy_config_hash: None,
        sparse_identity_proofs: Vec::new(),
        metadata_updates: vec![
            (key, serde_json::to_string(hold)?),
            (
                LAST_CHECKPOINT_STATUS_KEY.to_owned(),
                "preserved".to_owned(),
            ),
            (
                LAST_RECOVERY_ACTION_KEY.to_owned(),
                RecoveryAction::AwaitRetainedCheckpointEvidence
                    .as_str()
                    .to_owned(),
            ),
        ],
        metadata_deletes: Vec::new(),
    })
    .await?;
    Ok(())
}
const INVENTORY_DROP_THRESHOLD_PERCENT: f64 = 5.0;

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
struct InventoryAnchor {
    version: u8,
    scope: download::InventoryScope,
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

fn inventory_anchor_key(
    enum_config_hash: &str,
    zone_name: &str,
    scope: &download::InventoryScope,
) -> String {
    use sha2::{Digest, Sha256};
    let scope_hash = data_encoding::HEXLOWER.encode(&Sha256::digest(scope.0.as_bytes()));
    format!("{INVENTORY_ANCHOR_PREFIX}{enum_config_hash}:{zone_name}:{scope_hash}")
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

/// Bounded reason class for a selected library that did not finish its own
/// current source work this cycle. These labels are safe for human, health and
/// notification output: they never contain provider record identifiers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LibraryHoldKind {
    BaselinePending,
    IndexingReconciliation,
    IdentityUnresolved,
    RetainedCheckpoint,
    PreservationPending,
}

impl LibraryHoldKind {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::BaselinePending => "baseline_pending",
            Self::IndexingReconciliation => "indexing_reconciliation",
            Self::IdentityUnresolved => "identity_unresolved",
            Self::RetainedCheckpoint => "retained_checkpoint",
            Self::PreservationPending => "preservation_pending",
        }
    }
}

/// One selected library's real hold this cycle. This is a private reporting
/// fact, not a new report/JSON/metric contract.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct LibraryHoldFact {
    /// Safe display label derived from the configured `{library}` token.
    pub(crate) label: String,
    /// Exact non-public selected source identity. Used only for deduplication
    /// and distinct-source counting; never rendered into reports or errors.
    pub(crate) source: String,
    /// Stable selected-source ordinal in the cycle's library list.
    pub(crate) ordinal: usize,
    pub(crate) kind: LibraryHoldKind,
    pub(crate) reason: &'static str,
}

impl std::fmt::Debug for LibraryHoldFact {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LibraryHoldFact")
            .field("label", &self.label)
            .field("source", &"<redacted>")
            .field("ordinal", &self.ordinal)
            .field("kind", &self.kind)
            .field("reason", &self.reason)
            .finish()
    }
}

/// Human-safe joined hold summary. It exposes only the safe label, bounded kind
/// and configured reason, never a provider zone or record identifier.
pub(crate) fn selected_hold_summary(holds: &[LibraryHoldFact]) -> Option<String> {
    if holds.is_empty() {
        return None;
    }
    Some(
        holds
            .iter()
            .map(|hold| format!("{}: {} ({})", hold.label, hold.kind.as_str(), hold.reason))
            .collect::<Vec<_>>()
            .join(", "),
    )
}

/// Record a hold for one selected library and retain the existing singular
/// cycle-level machine block fields only as a compatibility projection. The
/// per-library facts keep every affected label visible without inventing
/// failed assets or counts.
pub(crate) fn record_selected_hold(
    holds: &mut Vec<LibraryHoldFact>,
    stats: &mut download::SyncStats,
    source: &str,
    ordinal: usize,
    kind: LibraryHoldKind,
    reason: &'static str,
    explanation: &'static str,
    project_token_hold: bool,
) {
    let label = download::paths::truncate_library_zone(source).to_owned();
    if holds
        .iter()
        .any(|hold| hold.source == source && hold.kind == kind && hold.reason == reason)
    {
        return;
    }
    holds.push(LibraryHoldFact {
        label,
        source: source.to_owned(),
        ordinal,
        kind,
        reason,
    });
    if project_token_hold && stats.sync_token_blocked_zone.is_none() {
        stats.sync_token_blocked = true;
        stats.sync_token_blocked_reason = Some(reason);
        stats.sync_token_blocked_source = Some("kei");
        stats.sync_token_blocked_explanation = Some(explanation);
        stats.sync_token_blocked_zone = Some(source.to_owned());
    }
}

/// How this wake treated one expected source before the cycle body.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SkipAuthority {
    Reached,
    NoChanges,
}

/// Complete expected active source universe for this wake. Never rendered.
#[derive(Clone)]
pub(crate) struct ExpectedSource {
    pub(crate) zone_name: String,
    pub(crate) baseline_scope: String,
    pub(crate) active_passes: bool,
    pub(crate) skip_authority: SkipAuthority,
}

/// A filtered watch cycle cannot certify the complete selection plan universe.
fn current_retry_plan_universe<'a>(
    all: &'a [&'a LibraryState],
    reached: &[&LibraryState],
    expected: &[ExpectedSource],
) -> Option<Vec<&'a [crate::commands::AlbumPass]>> {
    if all.is_empty()
        || all.len() != expected.len()
        || all
            .iter()
            .any(|state| state.plan_is_stale || state.plan_needs_refresh)
        || reached
            .iter()
            .any(|state| !all.iter().any(|full| std::ptr::eq(*full, *state)))
        || all.iter().any(|state| {
            expected
                .iter()
                .filter(|source| source.zone_name == state.zone_name)
                .count()
                != 1
        })
        || expected.iter().any(|source| {
            let mut matches = all
                .iter()
                .filter(|state| state.zone_name == source.zone_name);
            match (matches.next(), matches.next()) {
                (Some(state), None) => {
                    state.library.baseline_scope() != source.baseline_scope
                        || has_active_passes(state) != source.active_passes
                }
                _ => true,
            }
        })
    {
        return None;
    }
    Some(
        all.iter()
            .map(|state| state.plan.passes.as_slice())
            .collect(),
    )
}

#[derive(Clone)]
pub(crate) struct SourceOutcome {
    pub(crate) zone_name: String,
    pub(crate) baseline_scope: String,
    pub(crate) reached: bool,
    /// This source's own ordinary checkpoint transition committed.
    pub(crate) ordinary_committed: bool,
    /// Actual outcome/checkpoint/provider failure, never a positive count.
    pub(crate) genuine_error: bool,
    pub(crate) sparse: Option<SparseHeldWitness>,
    pub(crate) operational_prework: Option<download::OperationalPrework>,
    pub(crate) media_failures: Vec<download::pipeline::AccountedMediaFailure>,
    pub(crate) media_failure_unaccounted: bool,
    operational_active: bool,
    operational_eligible: bool,
    operational_other_error: bool,
    operational_pre_tail_paths: Option<Vec<(std::path::PathBuf, std::path::PathBuf)>>,
}

impl std::fmt::Debug for SourceOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SourceOutcome")
            .field("reached", &self.reached)
            .field("ordinary_committed", &self.ordinary_committed)
            .field("genuine_error", &self.genuine_error)
            .field("has_sparse_witness", &self.sparse.is_some())
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Debug)]
pub(crate) struct SparseHeldWitness {
    pub(crate) witness: crate::state::db::SparseCurrentWitness,
    pub(crate) completed_delta_replay: bool,
}

/// Outcome of a single sync cycle across all libraries.
#[derive(Debug)]
pub(crate) struct CycleResult {
    pub(crate) failed_count: usize,
    pub(crate) session_expired: bool,
    pub(crate) stats: download::SyncStats,
    pub(crate) db_sync_token_advance_safe: bool,
    /// Selected libraries that retained real incomplete work this cycle.
    pub(crate) holds: Vec<LibraryHoldFact>,
    pub(crate) sources: Vec<SourceOutcome>,
    pub(crate) operational_safe_holds: bool,
}

impl CycleResult {
    #[cfg(test)]
    pub(crate) fn operational_tail_prepared(&self) -> bool {
        !self.sources.is_empty()
            && self
                .sources
                .iter()
                .all(|source| source.operational_pre_tail_paths.is_some())
    }

    async fn operational_paths(
        &self,
        session: &auth::SharedSession,
    ) -> anyhow::Result<Option<Vec<Vec<(std::path::PathBuf, std::path::PathBuf)>>>> {
        if self.holds.is_empty()
            || self.session_expired
            || self.stats.interrupted
            || self.stats.skipped.retry_exhausted > 0
            || self.holds.iter().any(|hold| {
                self.sources
                    .iter()
                    .filter(|source| source.operational_active && source.zone_name == hold.source)
                    .count()
                    != 1
            })
        {
            return Ok(None);
        }
        let receipts = self
            .sources
            .iter()
            .map(|source| source.media_failures.len())
            .sum::<usize>();
        // Counts are only negative checks. Positive authority comes from the
        // typed, per-task successors matched to the complete owner snapshot.
        if self.stats.failed != receipts || self.failed_count != receipts {
            return Ok(None);
        }
        let mut task_keys = std::collections::HashSet::new();
        if self
            .sources
            .iter()
            .flat_map(|source| &source.media_failures)
            .any(|failure| !task_keys.insert(failure.task_identity()))
        {
            return Ok(None);
        }
        let session = session.read().await;
        let mut all_paths = Vec::with_capacity(self.sources.len());
        for source in &self.sources {
            if !source.operational_active {
                // No independent owner census was captured for this source.
                return Ok(None);
            }
            if !source.operational_eligible
                || !source.reached
                || source.operational_other_error
                || source.media_failure_unaccounted
            {
                return Ok(None);
            }
            let Some(prework) = &source.operational_prework else {
                return Ok(None);
            };
            if !session.operational_owner_matches(&prework.owner).await {
                return Ok(None);
            }
            if source
                .media_failures
                .iter()
                .any(|failure| !failure.matches_source(&source.zone_name))
            {
                return Ok(None);
            }
            let mut sparse = false;
            let mut preservation = false;
            for hold in self
                .holds
                .iter()
                .filter(|hold| hold.source == source.zone_name)
            {
                match hold.kind {
                    LibraryHoldKind::IdentityUnresolved => sparse = true,
                    LibraryHoldKind::PreservationPending => preservation = true,
                    _ => return Ok(None),
                }
            }
            // Preservation alone is not proof that the source made ordinary
            // progress. Sparse holds instead require a completed delta below.
            if !sparse && !source.ordinary_committed {
                return Ok(None);
            }
            let witness = if sparse {
                let Some(held) = &source.sparse else {
                    return Ok(None);
                };
                if !held.completed_delta_replay
                    || held.witness.baseline_scope() != Some(source.baseline_scope.as_str())
                {
                    return Ok(None);
                }
                Some(held.witness.clone())
            } else {
                None
            };
            let successors = source
                .media_failures
                .iter()
                .map(|failure| match failure.cause {
                    download::pipeline::MediaFailureCause::Transfer
                    | download::pipeline::MediaFailureCause::ExpiredUrl => {
                        failure.successor.clone()
                    }
                })
                .collect();
            let Some(accounting) = prework
                .db
                .operational_accounting(prework.prework.clone(), successors, witness, preservation)
                .await?
            else {
                return Ok(None);
            };
            all_paths.push(accounting.paths);
        }
        Ok(Some(all_paths))
    }

    /// Record the exact journals before the runner schedules its joined tails.
    pub(crate) async fn prepare_operational_tail(
        &mut self,
        session: &auth::SharedSession,
        shutdown: &CancellationToken,
    ) -> anyhow::Result<()> {
        // A pre-tail sparse attestation cannot survive newly discovered work
        // or a failed journal check without the post-tail observation.
        if !self.holds.is_empty() {
            self.operational_safe_holds = false;
        }
        let Some(paths) = self.operational_paths(session).await? else {
            return Ok(());
        };
        let flat: Vec<_> = paths.iter().flatten().cloned().collect();
        let _guards = match download::guard_operational_journals(&flat, shutdown).await {
            Ok(Some(guards)) => guards,
            Ok(None) => return Ok(()),
            Err(_) => {
                tracing::warn!("Could not inspect operational journals; retaining partial health");
                return Ok(());
            }
        };
        for (source, paths) in self.sources.iter_mut().zip(paths) {
            source.operational_pre_tail_paths = Some(paths);
        }
        Ok(())
    }

    /// Revalidate after joined tails and retain destination guards through the
    /// caller's health observation. This grants no checkpoint authority.
    pub(crate) async fn finish_operational_tail(
        &mut self,
        session: &auth::SharedSession,
        shutdown: &CancellationToken,
    ) -> anyhow::Result<Option<download::OperationalJournalGuards>> {
        let Some(paths) = self.operational_paths(session).await? else {
            return Ok(None);
        };
        if self
            .sources
            .iter()
            .zip(&paths)
            .any(|(source, paths)| source.operational_pre_tail_paths.as_ref() != Some(paths))
        {
            return Ok(None);
        }
        let flat: Vec<_> = paths.iter().flatten().cloned().collect();
        let guards = match download::guard_operational_journals(&flat, shutdown).await {
            Ok(Some(guards)) => guards,
            Ok(None) => return Ok(None),
            Err(_) => {
                tracing::warn!("Could not inspect operational journals; retaining partial health");
                return Ok(None);
            }
        };
        if self.operational_paths(session).await?.as_ref() != Some(&paths)
            || shutdown.is_cancelled()
        {
            return Ok(None);
        }
        match download::operational_media_journals_clear(&flat).await {
            Ok(true) => {}
            Ok(false) => return Ok(None),
            Err(_) => {
                tracing::warn!("Could not recheck operational journals; retaining partial health");
                return Ok(None);
            }
        }
        self.operational_safe_holds = true;
        Ok(Some(guards))
    }

    /// The database pre-check needs a clean aggregate cycle as well as safe
    /// selected-zone checkpoints. A durable transfer failure can satisfy the
    /// zone gate without satisfying this broader gate.
    #[must_use]
    pub(crate) fn can_advance_database_checkpoint(&self) -> bool {
        !self.session_expired
            && self.failed_count == 0
            && !self.stats.interrupted
            && self.db_sync_token_advance_safe
    }

    /// True when at least one selected library kept real incomplete work.
    /// Holds are not failed assets and must not be converted into counts.
    #[must_use]
    pub(crate) fn has_selected_holds(&self) -> bool {
        !self.holds.is_empty()
    }

    /// Distinct exact selected sources with at least one hold. One source may
    /// carry several reasons and must never count as several libraries.
    #[must_use]
    pub(crate) fn held_source_count(&self) -> usize {
        self.holds
            .iter()
            .map(|hold| hold.source.as_str())
            .collect::<std::collections::BTreeSet<_>>()
            .len()
    }

    /// Recheck every held sparse witness after a tail that may have changed
    /// owner-read state. Any refusal clears the operational exception.
    pub(crate) async fn revalidate_sparse_holds(
        &mut self,
        db: Option<&dyn download::DownloadStore>,
    ) {
        let Some(db) = db else {
            self.operational_safe_holds = false;
            return;
        };
        for source in &self.sources {
            if !self.holds.iter().any(|hold| {
                hold.source == source.zone_name && hold.kind == LibraryHoldKind::IdentityUnresolved
            }) {
                continue;
            }
            if let Some(sparse) = &source.sparse
                && db.attest_current_sparse(&sparse.witness).await.is_err()
            {
                self.operational_safe_holds = false;
                return;
            }
        }
    }
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

/// Capture the owning decision without serializing its provider token.
pub(crate) fn checkpoint_support_fields(decision: &SourceCheckpointDecision) -> serde_json::Value {
    match decision {
        SourceCheckpointDecision::Advance { basis, .. } => serde_json::json!({
            "decision":"advanced", "basis":basis.as_str(), "recovery":"none"
        }),
        SourceCheckpointDecision::Preserve { reason, recovery } => serde_json::json!({
            "decision":"preserved", "reason":reason.as_str(), "recovery":recovery.as_str()
        }),
    }
}

fn observe_sync_component(component: &'static str, result: &download::SyncResult, passes: usize) {
    crate::support::observe(
        "support_pass_completion_v1",
        serde_json::json!({
            "component":component, "count_scope":"selected_invocation", "passes":passes,
            "phase":if result.full_enumeration_ran {"full"} else {"incremental"},
            // These totals describe this invocation, before any bridge composition.
            // Query EOF evidence is recorded by the provider fetcher, not inferred
            // from absence of reported errors or widened into inventory coverage.
            "inventory_complete":serde_json::Value::Null,
            "selection_comparable":serde_json::Value::Null,
            "enumeration_incomplete":result.checkpoint.enumeration_incomplete,
            "enumeration_errors":result.checkpoint.enumeration_errors,
            "identity_incomplete":result.checkpoint.identity_incomplete,
            "interrupted":result.checkpoint.interrupted,
            "api_total":result.stats.api_total_at_start, "assets_seen":result.stats.assets_seen,
            "outcome":match result.outcome {
                download::DownloadOutcome::Success => "success",
                download::DownloadOutcome::SessionExpired { .. } => "session_expired",
                download::DownloadOutcome::PartialFailure { .. } => "partial_failure",
            },
            "token_present":result.sync_token.is_some(),
            "token_usable":result.sync_token.as_ref().is_some_and(|token| !token.trim().is_empty()),
            "sync_token_blocked":result.checkpoint.sync_token_blocked,
            "sync_token_blocked_reason":result.stats.sync_token_blocked_reason,
            "sync_token_blocked_source":result.stats.sync_token_blocked_source,
            "sync_token_expected_receivers":result.stats.sync_token_expected_receivers,
            "sync_token_receivers_with_token":result.stats.sync_token_receivers_with_token,
            "sync_token_receivers_missing":result.stats.sync_token_receivers_missing,
            "sync_token_receivers_blank":result.stats.sync_token_receivers_blank,
            "sync_token_receivers_dropped":result.stats.sync_token_receivers_dropped,
            "sync_token_unique_values":result.stats.sync_token_unique_values,
            "same_cycle_recovery_attempts":result.stats.same_cycle_recovery_attempts,
            "same_cycle_recovery_successes":result.stats.same_cycle_recovery_successes,
            "completed_delta_replay":result.checkpoint.completed_delta_replay,
            "retained_token_rejected":result.checkpoint.retained_token_rejected,
        }),
    );
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
    if result.checkpoint.interrupted {
        return preserve(
            CheckpointHoldReason::Interrupted,
            RecoveryAction::ReplayFromPriorToken,
        );
    }
    if result.checkpoint.enumeration_errors > 0 || result.checkpoint.enumeration_incomplete {
        return preserve(
            CheckpointHoldReason::EnumerationIncomplete,
            RecoveryAction::ContinueTail,
        );
    }
    if result.checkpoint.state_write_failures > 0 {
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
        let recovery = if !result.checkpoint.retry_passes.is_empty() {
            RecoveryAction::RetryPasses(result.checkpoint.retry_passes.clone())
        } else if !result.checkpoint.revalidate_records.is_empty() {
            RecoveryAction::RevalidateRecords(result.checkpoint.revalidate_records.clone())
        } else {
            RecoveryAction::ReplayFromPriorToken
        };
        return preserve(CheckpointHoldReason::TokenProofIncomplete, recovery);
    };
    if result.checkpoint.sync_token_blocked || result.checkpoint.identity_incomplete {
        let recovery = if result.checkpoint.retry_passes.is_empty() {
            RecoveryAction::ReplayFromPriorToken
        } else {
            RecoveryAction::RetryPasses(result.checkpoint.retry_passes.clone())
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
    inventory: &download::InventoryMeasurement,
    stats: &mut download::SyncStats,
) where
    D: state::SyncTokenStore + ?Sized,
{
    let current_total = inventory.total;
    let key = inventory_anchor_key(enum_config_hash, zone_name, &inventory.scope);
    match db.get_metadata(&key).await {
        Ok(Some(raw)) => match serde_json::from_str::<InventoryAnchor>(&raw) {
            Ok(anchor)
                if anchor.version == INVENTORY_ANCHOR_VERSION
                    && anchor.scope == inventory.scope =>
            {
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
            Ok(_) => {}
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
            return;
        }
    }

    let anchor = InventoryAnchor {
        version: INVENTORY_ANCHOR_VERSION,
        scope: inventory.scope.clone(),
        api_total_at_start: current_total,
        assets_seen: inventory.assets_seen,
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
    /// No prior hash and the initial hash write failed; the effective hash is
    /// not durable, so this cycle is not a clean operational success.
    InitialWriteFailed,
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
            Self::Changed
                | Self::ChangedTokenPurgeFailed
                | Self::InitialWriteFailed
                | Self::ReadFailed
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
        matches!(
            self,
            Self::Changed | Self::ReadFailed | Self::TokenPurgeFailed
        )
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
            if let Err(e) = db.purge_superseded_pending_tokens().await {
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
                if let Err(e) = db.purge_superseded_pending_tokens().await {
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
        if matches!(outcome, EnumConfigHashOutcome::Initial) {
            return EnumConfigHashOutcome::InitialWriteFailed;
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
                    expected_metadata: Vec::new(),
                    legacy_preservation_proofs: Vec::new(),
                    legacy_config_hash: None,
                    sparse_identity_proofs: Vec::new(),
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
struct EpochLocalRecovery {
    failures: usize,
    reconciliation_stats: Option<download::SyncStats>,
    path_complete: bool,
    path_safe: bool,
    config: Arc<download::DownloadConfig>,
}

/// Replay durable pending work with the retry-only contract. This never
/// enumerates source inventory and never drains the historical metadata queue.
async fn run_epoch_pending_retry(
    download_client: &reqwest::Client,
    lib_state: &LibraryState,
    config: &download::DownloadConfig,
    download_controls: download::DownloadControls,
    shutdown_token: &CancellationToken,
) -> anyhow::Result<Option<download::SyncResult>> {
    let mut retry_config = config.clone();
    retry_config.retry_only = true;
    let retry_config = Arc::new(retry_config);
    // The legacy pending asset owner is the only retry authority reachable under
    // retry-only. Admitted generation destinations stay pending and untouched
    // while an epoch is held. Build no selection context here: that is why the
    // previous has_due_selection_work/has_due_retained_work prechecks were always
    // None/false under retry_only. replay_pending_work returns a no-work result
    // without source enumeration when no legacy asset is due.
    download::replay_pending_work(
        download_client,
        &lib_state.plan.passes,
        &retry_config,
        download_controls,
        shutdown_token.clone(),
    )
    .await
    .map(Some)
}

#[allow(
    clippy::too_many_arguments,
    reason = "narrow held-cycle retry adapter mirrors the existing cycle mutation facts"
)]
async fn retry_due_admitted_while_held(
    download_client: &reqwest::Client,
    lib_state: &LibraryState,
    selection: &crate::selection::Selection,
    all_plans: Option<&[&[crate::commands::AlbumPass]]>,
    retry_config: Arc<download::DownloadConfig>,
    download_controls: download::DownloadControls,
    shutdown_token: &CancellationToken,
    cycle_stats: &mut download::SyncStats,
    cycle_failed_count: &mut usize,
    cycle_session_expired: &mut bool,
    db_sync_token_advance_safe: &mut bool,
    context: &'static str,
) -> bool {
    match Box::pin(download::replay_due_admitted_selection(
        download_client,
        &lib_state.plan.passes,
        (!lib_state.plan_is_stale && !lib_state.plan_needs_refresh).then_some(selection),
        all_plans,
        retry_config,
        download_controls,
        shutdown_token.clone(),
    ))
    .await
    {
        Ok(Some(retried)) => {
            cycle_stats.accumulate(&retried.stats);
            match retried.outcome {
                download::DownloadOutcome::Success => {}
                download::DownloadOutcome::SessionExpired { .. } => {
                    *cycle_session_expired = true;
                    *db_sync_token_advance_safe = false;
                }
                download::DownloadOutcome::PartialFailure { failed_count } => {
                    *cycle_failed_count = cycle_failed_count.saturating_add(failed_count);
                }
            }
            if retried.stats.state_write_failures > 0 || retried.stats.interrupted {
                *db_sync_token_advance_safe = false;
            }
        }
        Ok(None) => {}
        Err(error) => {
            *cycle_failed_count = cycle_failed_count.saturating_add(1);
            *db_sync_token_advance_safe = false;
            tracing::warn!(
                error = %error,
                context,
                "Due admitted selection retry failed while the epoch was held"
            );
            return false;
        }
    }
    true
}

/// Capture the actual source plan and durable fences before a provider boundary
/// or delta stream. The returned request carries no cursor authority yet.
#[expect(
    clippy::too_many_arguments,
    reason = "the catalog owner requires the actual plan, epoch and checkpoint fences"
)]
async fn capture_catalog_request(
    library: &LibraryState,
    config: &download::DownloadConfig,
    controls: download::DownloadControls,
    db: &dyn download::DownloadStore,
    epoch: &str,
    attempt: &str,
    baseline_key: &str,
    enum_hash: &str,
    preservation_hash: Option<&str>,
    zone_fence: &[(String, Option<String>)],
    path_fence: &[(String, Option<String>)],
) -> anyhow::Result<
    Option<(
        crate::icloud::photos::inbox::ShadowCapture,
        state::db::source_revalidation::FreshCatalogOriginRequest,
    )>,
> {
    let Some(plan) =
        download::catalog_plan_context_for_source(&library.plan.passes, config, controls, None)
            .await?
    else {
        return Ok(None);
    };
    let mut capture = None;
    for pass in &library.plan.passes {
        if let Some((source, scope, zone)) = pass.album.download_scope()?
            && scope == plan.scope
            && zone == plan.zone
        {
            capture = Some(source);
            break;
        }
    }
    let Some(capture) = capture else {
        return Ok(None);
    };
    anyhow::ensure!(
        std::ptr::addr_eq(db, capture.db.as_ref() as &dyn download::DownloadStore),
        "Fresh catalog database context mismatch"
    );
    let mut expected_metadata = zone_fence.to_vec();
    expected_metadata.extend(path_fence.iter().cloned());
    for key in [
        state::SYNC_EPOCH_KEY.to_owned(),
        state::db::sync_epoch::library_key(&library.zone_name),
        ENUM_CONFIG_HASH_KEY.to_owned(),
        PENDING_ENUM_CONFIG_HASH_KEY.to_owned(),
        download::DOWNLOAD_CONFIG_HASH_KEY.to_owned(),
        PENDING_DOWNLOAD_CONFIG_HASH_KEY.to_owned(),
        library.sync_token_key.clone(),
        format!("observed_selection:v27:{}", library.zone_name),
        format!("download_ledger_scope:{}", library.zone_name),
    ] {
        if !expected_metadata.iter().any(|(saved, _)| saved == &key) {
            expected_metadata.push((key.clone(), db.get_metadata(&key).await?));
        }
    }
    let expected_baseline = db.get_metadata(baseline_key).await?;
    let expected_catalog_snapshot = capture
        .db
        .fresh_catalog_snapshot(capture.owner.clone(), library.zone_name.clone())
        .await?;
    let request = state::db::source_revalidation::FreshCatalogOriginRequest {
        library: library.zone_name.clone(),
        plan: state::db::source_revalidation::FreshCatalogPlan {
            account: plan.account,
            provider: plan.provider,
            database: plan.database,
            zone: plan.zone,
            scope: plan.scope,
            config_hash: plan.config_hash,
            metadata_enabled: plan.metadata_enabled,
            metadata_flags: plan.metadata_flags,
            passes: plan
                .passes
                .into_iter()
                .map(|pass| state::db::source_revalidation::FreshCatalogPass {
                    key: pass.key,
                    kind: pass.kind,
                    scope: pass.scope,
                    excludes: pass.excludes,
                })
                .collect(),
            profile: plan.profile,
        },
        baseline_key: baseline_key.to_owned(),
        preservation_config_hash: preservation_hash.unwrap_or_default().to_owned(),
        expected_baseline,
        fresh_cursor: String::new(),
        enum_hash: enum_hash.to_owned(),
        expected_epoch: epoch.to_owned(),
        expected_attempt: attempt.to_owned(),
        expected_metadata,
        expected_catalog_snapshot,
    };
    Ok(Some((capture, request)))
}

async fn run_epoch_local_recovery(
    download_client: &reqwest::Client,
    db: &dyn download::DownloadStore,
    lib_state: &LibraryState,
    config: &config::Config,
    build_download_config: &BuildDownloadConfigFn<'_>,
    shutdown_token: &CancellationToken,
    run_path_reconciliation: bool,
) -> anyhow::Result<EpochLocalRecovery> {
    let local_config = build_download_config(
        download::SyncMode::Full,
        Arc::new(rustc_hash::FxHashSet::default()),
        Arc::new(download::AssetGroupings::default()),
        Arc::from(lib_state.zone_name.as_str()),
    );
    let local_config = Arc::new(local_config.check_ownership().await?);
    let mut failures = 0usize;
    let mut reconciliation_stats = None;
    let mut path_complete = true;
    let mut path_safe = true;
    if run_path_reconciliation {
        let reconciliation = Box::pin(download::recover_recorded_sources(
            download_client,
            &lib_state.plan.passes,
            Arc::clone(&local_config),
            shutdown_token.clone(),
        ))
        .await?;
        let reconciliation_failures = reconciliation
            .stats
            .failed
            .saturating_add(reconciliation.stats.exif_failures)
            .saturating_add(reconciliation.stats.state_write_failures);
        failures = failures.saturating_add(reconciliation_failures);
        reconciliation_stats = Some(reconciliation.stats);
        path_complete = reconciliation.complete || reconciliation.complete_after_smart_query;
        if reconciliation_failures > 0 {
            path_safe = false;
        }
    }
    let metadata_failures = Box::pin(download::drain_pending_metadata_rewrites(
        db,
        &local_config.directory,
        &config.metadata,
        local_config.capture_timestamp_repair,
        &[lib_state.zone_name.as_str()],
        Arc::clone(&local_config.temp_suffix),
        shutdown_token,
    ))
    .await;
    if metadata_failures > 0 {
        failures = failures.saturating_add(metadata_failures);
        path_safe = false;
    }
    Ok(EpochLocalRecovery {
        failures,
        reconciliation_stats,
        path_complete,
        path_safe,
        config: local_config,
    })
}

#[cfg_attr(
    not(test),
    allow(dead_code, reason = "tests exercise the unchanged run_cycle entry")
)]
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
    let expected_sources: Vec<ExpectedSource> = library_states
        .iter()
        .map(|state| ExpectedSource {
            zone_name: state.zone_name.clone(),
            baseline_scope: state.library.baseline_scope(),
            active_passes: has_active_passes(state),
            skip_authority: SkipAuthority::Reached,
        })
        .collect();
    let metadata_witness = Arc::new(download::MetadataWriterWitness::default());
    Box::pin(run_cycle_with_witness(
        library_states,
        config,
        state_db,
        is_retry_failed,
        build_download_config,
        download_controls,
        shared_session,
        shutdown_token,
        metadata_witness,
        &expected_sources,
        None,
    ))
    .await
}

#[allow(
    clippy::too_many_arguments,
    reason = "keeps the unchanged run_cycle signature and adds only the transient witness"
)]
pub(crate) async fn run_cycle_with_witness(
    library_states: &[&LibraryState],
    config: &config::Config,
    state_db: Option<&dyn download::DownloadStore>,
    is_retry_failed: bool,
    build_download_config: &BuildDownloadConfigFn<'_>,
    download_controls: download::DownloadControls,
    shared_session: &auth::SharedSession,
    shutdown_token: &CancellationToken,
    metadata_witness: Arc<download::MetadataWriterWitness>,
    expected_sources: &[ExpectedSource],
    all_library_states: Option<&[&LibraryState]>,
) -> anyhow::Result<CycleResult> {
    let all_retry_plans = all_library_states
        .and_then(|all| current_retry_plan_universe(all, library_states, expected_sources));
    let sync_epoch = match state_db {
        Some(db) => db.sync_epoch_id().await?,
        None => None,
    };
    let mut cycle_failed_count = 0usize;
    let mut cycle_session_expired = false;
    let mut cycle_stats = download::SyncStats::default();
    let mut library_holds: Vec<LibraryHoldFact> = Vec::new();
    let mut sources: Vec<SourceOutcome> = expected_sources
        .iter()
        .map(|expected| SourceOutcome {
            zone_name: expected.zone_name.clone(),
            baseline_scope: expected.baseline_scope.clone(),
            reached: false,
            ordinary_committed: false,
            genuine_error: false,
            sparse: None,
            operational_prework: None,
            media_failures: Vec::new(),
            media_failure_unaccounted: false,
            operational_active: expected.active_passes,
            operational_eligible: false,
            operational_other_error: false,
            operational_pre_tail_paths: None,
        })
        .collect();
    let mut sparse_identity_proofs = Vec::new();

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
    let mut path_reconciliation_cycle_safe = true;
    let path_reconciliation_has_smart_folders = library_states.iter().any(|state| {
        state
            .plan
            .passes
            .iter()
            .any(|pass| pass.kind == crate::commands::PassKind::SmartFolder)
    });
    let mut checkpoint_hold_action: Option<&'static str> = None;
    let mut operational_state_fault = false;
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
            EnumConfigHashOutcome::ChangedTokenPurgeFailed
                | EnumConfigHashOutcome::InitialWriteFailed
                | EnumConfigHashOutcome::ReadFailed
        ) {
            checkpoint_transition_state_safe = false;
            db_sync_token_advance_safe = false;
            operational_state_fault = true;
        }
    }

    let mut preservation_config_hash = None;

    // A path-affecting config drift does not make the CloudKit token itself
    // unsafe, but it does make an incremental cycle insufficient: with no
    // changed assets, `/changes/zone` would never re-plan already-known media
    // into the new directory/template. Detect that before choosing
    // incremental mode. Each selected library establishes its new-policy
    // observation once; completed libraries can then continue incrementally.
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
        {
            let mut zones = library_states
                .iter()
                .map(|library| library.zone_name.clone())
                .collect::<Vec<_>>();
            zones.sort();
            zones.dedup();
            let coverage = download::sync_coverage_fingerprint_json(
                config,
                "icloud",
                1,
                &zones,
                &enum_config_hash,
                &download_config_hash,
            )?;
            let policy = serde_json::json!([
                1,
                state::METADATA_CAPTURE_REVISION,
                coverage,
                format!("{:?}", probe_config.metadata),
                config.runtime.refresh_metadata,
                config.runtime.repair_capture_timestamps,
                config.runtime.repair_truncated
            ]);
            preservation_config_hash = Some(hash_legacy_preservation_policy(
                &policy,
                probe_config.legacy_preservation_allow_hardlinks,
            ));
        }

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
        if outcome.token_purge_failed() || matches!(outcome, DownloadConfigHashOutcome::ReadFailed)
        {
            checkpoint_transition_state_safe = false;
            db_sync_token_advance_safe = false;
            operational_state_fault = true;
        }
    }

    let path_metadata_at_start = if let Some(db) = state_db {
        vec![
            (
                download::DOWNLOAD_CONFIG_HASH_KEY.to_owned(),
                db.get_metadata(download::DOWNLOAD_CONFIG_HASH_KEY).await?,
            ),
            (
                PENDING_DOWNLOAD_CONFIG_HASH_KEY.to_owned(),
                pending_download_config_hash.clone(),
            ),
        ]
    } else {
        Vec::new()
    };

    let enum_metadata_at_start = if let Some(db) = state_db {
        vec![
            (
                ENUM_CONFIG_HASH_KEY.to_owned(),
                db.get_metadata(ENUM_CONFIG_HASH_KEY).await?,
            ),
            (
                PENDING_ENUM_CONFIG_HASH_KEY.to_owned(),
                db.get_metadata(PENDING_ENUM_CONFIG_HASH_KEY).await?,
            ),
        ]
    } else {
        Vec::new()
    };
    let mut active_cursor_fences = Vec::new();

    for (library_ordinal, lib_state) in library_states.iter().copied().enumerate() {
        if shutdown_token.is_cancelled() {
            break;
        }

        if let Some(db) = state_db
            && !config.runtime.dry_run
            && !config.runtime.only_print_filenames
            && !is_retry_failed
        {
            db.sync_epoch_enroll(vec![(
                lib_state.zone_name.clone(),
                lib_state.library.baseline_scope(),
            )])
            .await?;
        }

        let indexing_key = indexing_reconciliation_key(&lib_state.zone_name);
        let (indexing_finished, indexing_hold_at_start) =
            prepare_library_indexing(lib_state, state_db, config.runtime.dry_run).await?;
        let indexing_reconciliation = indexing_hold_at_start.is_some() || !indexing_finished;
        if !has_active_passes(lib_state) {
            if let Some(db) = state_db
                && !config.runtime.dry_run
                && !config.runtime.only_print_filenames
                && !is_retry_failed
                && !indexing_reconciliation
            {
                db.defer_sync_epoch(
                    lib_state.zone_name.clone(),
                    config.watch.interval,
                    "no_active_passes",
                )
                .await?;
            }
            db_sync_token_advance_safe &= !indexing_reconciliation;
            continue;
        }
        if !indexing_finished {
            db_sync_token_advance_safe = false;
            path_reconciliation_complete = false;
            tracing::warn!(
                "Photo library indexing is RUNNING; downloads are provisional, deletions and checkpoint promotion are disabled"
            );
        }
        let epoch_ready = indexing_finished
            && !indexing_reconciliation
            && !is_retry_failed
            && download_controls.run_mode.downloads_files();
        let qualification_candidate = indexing_finished
            && indexing_reconciliation
            && !is_retry_failed
            && !config.runtime.only_print_filenames
            && download_controls.run_mode.downloads_files()
            && !lib_state.plan_is_stale;
        let retained_cursor_at_start = if let Some(db) = state_db {
            db.get_metadata(&lib_state.sync_token_key).await?
        } else {
            None
        };
        let mut retained_checkpoint_attempt = 1;
        let hold_key = retained_checkpoint_hold_key(&lib_state.zone_name);
        let retained_hold_at_start = if let Some(db) = state_db {
            db.get_metadata(&hold_key).await?
        } else {
            None
        };
        let replay_key = indexing_replay_key(&lib_state.zone_name);
        let replay_at_start = match state_db {
            Some(db) => db.get_metadata(&replay_key).await?,
            None => None,
        };
        let indexing_replay_pending = replay_at_start.is_some();
        let materialization_key = indexing_materialization_key(&lib_state.zone_name);
        let materialization_at_start = match state_db {
            Some(db) => db.get_metadata(&materialization_key).await?,
            None => None,
        };
        let indexing_materialization_pending = materialization_at_start.is_some();
        let mut zone_metadata_fence = enum_metadata_at_start.clone();
        zone_metadata_fence.push((
            materialization_key.clone(),
            materialization_at_start.clone(),
        ));
        active_cursor_fences.push((
            materialization_key.clone(),
            materialization_at_start.clone(),
        ));
        zone_metadata_fence.push((indexing_key.clone(), indexing_hold_at_start.clone()));
        zone_metadata_fence.push((replay_key.clone(), replay_at_start.clone()));
        active_cursor_fences.push((replay_key.clone(), replay_at_start.clone()));
        active_cursor_fences.push((indexing_key.clone(), indexing_hold_at_start.clone()));
        zone_metadata_fence.push((
            lib_state.sync_token_key.clone(),
            retained_cursor_at_start.clone(),
        ));
        active_cursor_fences.push((
            lib_state.sync_token_key.clone(),
            retained_cursor_at_start.clone(),
        ));
        zone_metadata_fence.push((hold_key.clone(), retained_hold_at_start.clone()));
        // Successful zone staging removes its hold before aggregate activation.
        active_cursor_fences.push((hold_key.clone(), None));

        let mut force_epoch_baseline = false;
        let mut promote_only = false;
        let mut epoch_attempt: Option<String> = None;
        let mut qualification_reservation: Option<state::QualificationReservation> = None;
        if qualification_candidate {
            let db = state_db
                .ok_or_else(|| anyhow::anyhow!("Indexing qualification requires durable state"))?;
            let mut expected_metadata = enum_metadata_at_start.clone();
            expected_metadata.extend(path_metadata_at_start.iter().cloned());
            expected_metadata.push((
                materialization_key.clone(),
                materialization_at_start.clone(),
            ));
            expected_metadata.push((indexing_key.clone(), indexing_hold_at_start.clone()));
            expected_metadata.push((replay_key.clone(), replay_at_start.clone()));
            expected_metadata.push((
                lib_state.sync_token_key.clone(),
                retained_cursor_at_start.clone(),
            ));
            expected_metadata.push((hold_key.clone(), retained_hold_at_start.clone()));
            expected_metadata.push((
                state::SYNC_EPOCH_KEY.to_owned(),
                db.get_metadata(state::SYNC_EPOCH_KEY).await?,
            ));
            match db
                .reserve_indexing_qualification(
                    lib_state.zone_name.clone(),
                    indexing_key.clone(),
                    replay_key.clone(),
                    expected_metadata,
                    config.watch.interval,
                )
                .await?
            {
                state::QualificationReserve::Reserved(reservation) => {
                    qualification_reservation = Some(*reservation);
                }
                state::QualificationReserve::NotDue { .. } => {
                    record_selected_hold(
                        &mut library_holds,
                        &mut cycle_stats,
                        &lib_state.zone_name,
                        library_ordinal,
                        LibraryHoldKind::IndexingReconciliation,
                        "indexing_qualification_not_due",
                        "a selected library retains its existing indexing qualification deadline; no baseline attempt or cursor was claimed",
                        true,
                    );
                    db_sync_token_advance_safe = false;
                    if !is_retry_failed
                        && !config.runtime.only_print_filenames
                        && download_controls.run_mode.downloads_files()
                        && !shutdown_token.is_cancelled()
                    {
                        let download_client = shared_session.read().await.download_client().clone();
                        let retry_config = build_download_config(
                            download::SyncMode::Full,
                            Arc::new(rustc_hash::FxHashSet::default()),
                            Arc::new(download::AssetGroupings::default()),
                            Arc::from(lib_state.zone_name.as_str()),
                        );
                        let retry_config = Arc::new(retry_config.check_ownership().await?);
                        let _ = retry_due_admitted_while_held(
                            &download_client,
                            lib_state,
                            &config.filters.selection,
                            all_retry_plans.as_deref(),
                            retry_config,
                            download_controls,
                            shutdown_token,
                            &mut cycle_stats,
                            &mut cycle_failed_count,
                            &mut cycle_session_expired,
                            &mut db_sync_token_advance_safe,
                            "indexing qualification",
                        )
                        .await;
                    }
                    continue;
                }
                state::QualificationReserve::NotPending => {
                    // A validated complete/ready/blocked record keeps its
                    // ordinary gate path below.
                }
                state::QualificationReserve::StaleFence => {
                    record_selected_hold(
                        &mut library_holds,
                        &mut cycle_stats,
                        &lib_state.zone_name,
                        library_ordinal,
                        LibraryHoldKind::IndexingReconciliation,
                        "indexing_qualification_stale_fence",
                        "selected library epoch qualification evidence changed; this cycle refuses qualification and ordinary fallback",
                        true,
                    );
                    db_sync_token_advance_safe = false;
                    continue;
                }
            }
        }
        let qualification_owned = qualification_reservation.is_some();
        if qualification_reservation.is_none()
            && let Some(db) = state_db
            && !config.runtime.dry_run
        {
            let ineligible_without_defer = config.runtime.only_print_filenames || is_retry_failed;
            let epoch_gate = if !epoch_ready {
                if db
                    .sync_epoch_has_pending(lib_state.zone_name.clone())
                    .await?
                {
                    if !ineligible_without_defer && !indexing_reconciliation {
                        db.defer_sync_epoch(
                            lib_state.zone_name.clone(),
                            config.watch.interval,
                            "epoch_waiting_for_eligible_run",
                        )
                        .await?;
                    }
                    state::SyncEpochGate::Defer {
                        reason: "epoch_waiting_for_eligible_run",
                    }
                } else {
                    state::SyncEpochGate::Open
                }
            } else {
                db.sync_epoch_gate(
                    lib_state.zone_name.clone(),
                    lib_state.library.baseline_scope(),
                    config.watch.interval,
                )
                .await?
            };
            let retry_during_hold = matches!(
                &epoch_gate,
                state::SyncEpochGate::Defer { .. } | state::SyncEpochGate::Blocked { .. }
            );
            match epoch_gate {
                state::SyncEpochGate::Open => {}
                state::SyncEpochGate::Baseline { attempt } => {
                    force_epoch_baseline = true;
                    epoch_attempt = Some(attempt);
                }
                state::SyncEpochGate::Promote { .. } => {
                    promote_only = true;
                }
                state::SyncEpochGate::Defer { reason }
                | state::SyncEpochGate::Blocked { reason } => {
                    if !config.runtime.only_print_filenames && !is_retry_failed {
                        let explanation = if reason == "epoch_blocked" {
                            "a selected library's baseline is blocked; local work is retained"
                        } else {
                            "a selected library is waiting for its scheduled baseline attempt; local work is retained"
                        };
                        record_selected_hold(
                            &mut library_holds,
                            &mut cycle_stats,
                            &lib_state.zone_name,
                            library_ordinal,
                            LibraryHoldKind::BaselinePending,
                            reason,
                            explanation,
                            true,
                        );
                        // A pending selected baseline is an aggregate database
                        // checkpoint hold even when a healthy peer progressed.
                        db_sync_token_advance_safe = false;
                    }
                    if config.runtime.only_print_filenames || is_retry_failed {
                        // Ineligible runs must not drain queued metadata or run
                        // historical path reconciliation. Retry-only still
                        // replays durable pending transfers without enumeration.
                        if is_retry_failed && download_controls.run_mode.downloads_files() {
                            let download_client =
                                shared_session.read().await.download_client().clone();
                            let retry_config = build_download_config(
                                download::SyncMode::Full,
                                Arc::new(rustc_hash::FxHashSet::default()),
                                Arc::new(download::AssetGroupings::default()),
                                Arc::from(lib_state.zone_name.as_str()),
                            );
                            let retry_config = Arc::new(retry_config.check_ownership().await?);
                            match Box::pin(run_epoch_pending_retry(
                                &download_client,
                                lib_state,
                                &retry_config,
                                download_controls,
                                shutdown_token,
                            ))
                            .await
                            {
                                Ok(Some(result)) => {
                                    cycle_stats.accumulate(&result.stats);
                                    match result.outcome {
                                        download::DownloadOutcome::Success => {}
                                        download::DownloadOutcome::SessionExpired { .. } => {
                                            cycle_session_expired = true;
                                            db_sync_token_advance_safe = false;
                                        }
                                        download::DownloadOutcome::PartialFailure {
                                            failed_count,
                                        } => {
                                            cycle_failed_count =
                                                cycle_failed_count.saturating_add(failed_count);
                                            db_sync_token_advance_safe = false;
                                        }
                                    }
                                    if result.stats.state_write_failures > 0
                                        || result.stats.interrupted
                                    {
                                        db_sync_token_advance_safe = false;
                                    }
                                }
                                Ok(None) => {}
                                Err(error) => {
                                    cycle_failed_count = cycle_failed_count.saturating_add(1);
                                    db_sync_token_advance_safe = false;
                                    tracing::warn!(
                                        error = %error,
                                        "Retry-only replay failed while the epoch baseline waits"
                                    );
                                }
                            }
                        }
                        tracing::info!(
                            reason,
                            only_print_filenames = config.runtime.only_print_filenames,
                            retry_failed = is_retry_failed,
                            "Sync epoch deferred; ineligible run preserved local metadata and recovery state"
                        );
                        continue;
                    }
                    // Indexing already ran. Keep local path, layout and metadata
                    // recovery moving even while the provider baseline waits.
                    let download_client = shared_session.read().await.download_client().clone();
                    let local = Box::pin(run_epoch_local_recovery(
                        &download_client,
                        db,
                        lib_state,
                        config,
                        build_download_config,
                        shutdown_token,
                        indexing_finished
                            && !indexing_reconciliation
                            && pending_download_config_hash.is_some()
                            && download_controls.run_mode.downloads_files(),
                    ))
                    .await?;
                    cycle_failed_count = cycle_failed_count.saturating_add(local.failures);
                    if let Some(stats) = local.reconciliation_stats {
                        cycle_stats.accumulate(&stats);
                    }
                    path_reconciliation_complete &= local.path_complete;
                    path_reconciliation_cycle_safe &= local.path_safe;
                    if local.failures > 0 {
                        db_sync_token_advance_safe = false;
                    }
                    if retry_during_hold
                        && !is_retry_failed
                        && !config.runtime.only_print_filenames
                        && download_controls.run_mode.downloads_files()
                        && !shutdown_token.is_cancelled()
                    {
                        let _ = retry_due_admitted_while_held(
                            &download_client,
                            lib_state,
                            &config.filters.selection,
                            all_retry_plans.as_deref(),
                            Arc::clone(&local.config),
                            download_controls,
                            shutdown_token,
                            &mut cycle_stats,
                            &mut cycle_failed_count,
                            &mut cycle_session_expired,
                            &mut db_sync_token_advance_safe,
                            "deferred epoch",
                        )
                        .await;
                    }
                    db.set_metadata(LAST_RECOVERY_ACTION_KEY, "source_epoch_deferred")
                        .await?;
                    tracing::info!(
                        reason,
                        "Sync epoch library deferred; indexing and local recovery retained"
                    );
                    continue;
                }
            }
        }

        if !config.runtime.dry_run
            && retained_hold_at_start.as_deref().is_some_and(|encoded| {
                retained_checkpoint_hold_action(encoded)
                    == RecoveryAction::RepairRetainedCheckpointEvidence.as_str()
            })
        {
            record_selected_hold(
                &mut library_holds,
                &mut cycle_stats,
                &lib_state.zone_name,
                library_ordinal,
                LibraryHoldKind::RetainedCheckpoint,
                "retained_checkpoint_hold_invalid",
                download::sync_token_blocked_explanation("retained_checkpoint_hold_invalid"),
                true,
            );
            cycle_failed_count = cycle_failed_count.saturating_add(1);
            db_sync_token_advance_safe = false;
            checkpoint_hold_action =
                Some(RecoveryAction::RepairRetainedCheckpointEvidence.as_str());
            tracing::warn!(
                diagnostic = "retained_checkpoint_hold_invalid",
                "Retained checkpoint evidence requires validation; preserving the durable hold"
            );
            continue;
        }

        // A rejected retained boundary cannot be repaired by repeating the same
        // rank inventory. Reserve each due attempt durably before provider or
        // legacy work, including attempts later cancelled or found incomplete.
        if !config.runtime.dry_run
            && let Some(db) = state_db
            && let Some(encoded) = retained_hold_at_start
            && let Ok(hold) = serde_json::from_str::<RetainedCheckpointHold>(&encoded)
            && let Some(prior) = retained_cursor_at_start.as_deref()
        {
            let mut current = RetainedCheckpointHold::new(
                prior,
                &enum_config_hash,
                preservation_config_hash.as_deref(),
            );
            if hold.matches(&current) {
                let exhausted = hold.attempt_count >= RETAINED_CHECKPOINT_MAX_ATTEMPTS;
                current.attempt_count = hold.attempt_count.saturating_add(1);
                retained_checkpoint_attempt = current.attempt_count;
                let deferred = exhausted || hold.deferred(chrono::Utc::now().timestamp());
                let reservation_failed = if deferred {
                    false
                } else {
                    persist_retained_checkpoint_hold(
                        db,
                        hold_key.clone(),
                        &current,
                        zone_metadata_fence.clone(),
                    )
                    .await
                    .is_err()
                };
                if !deferred
                    && !reservation_failed
                    && let Some((_, expected)) = zone_metadata_fence.last_mut()
                {
                    *expected = Some(serde_json::to_string(&current)?);
                }
                if deferred || reservation_failed {
                    if reservation_failed {
                        cycle_stats.state_write_failures =
                            cycle_stats.state_write_failures.saturating_add(1);
                    }
                    let reason = if exhausted {
                        "retained_checkpoint_retry_exhausted"
                    } else {
                        "retained_checkpoint_expired"
                    };
                    record_selected_hold(
                        &mut library_holds,
                        &mut cycle_stats,
                        &lib_state.zone_name,
                        library_ordinal,
                        LibraryHoldKind::RetainedCheckpoint,
                        reason,
                        download::sync_token_blocked_explanation(reason),
                        true,
                    );
                    db_sync_token_advance_safe = false;
                    checkpoint_hold_action = Some(if exhausted {
                        RecoveryAction::RetainedCheckpointRetryExhausted.as_str()
                    } else {
                        RecoveryAction::AwaitRetainedCheckpointEvidence.as_str()
                    });
                    tracing::warn!(
                        diagnostic = "retained_checkpoint_expired",
                        retry_deferred = deferred,
                        state_write_failed = reservation_failed,
                        retry_exhausted = exhausted,
                        "Retained checkpoint evidence is unavailable; preserving history and deferring recovery"
                    );
                    if deferred
                        && !is_retry_failed
                        && !config.runtime.only_print_filenames
                        && download_controls.run_mode.downloads_files()
                        && !shutdown_token.is_cancelled()
                    {
                        let download_client = shared_session.read().await.download_client().clone();
                        let retry_config = build_download_config(
                            download::SyncMode::Full,
                            Arc::new(rustc_hash::FxHashSet::default()),
                            Arc::new(download::AssetGroupings::default()),
                            Arc::from(lib_state.zone_name.as_str()),
                        );
                        let retry_config = Arc::new(retry_config.check_ownership().await?);
                        let _ = retry_due_admitted_while_held(
                            &download_client,
                            lib_state,
                            &config.filters.selection,
                            all_retry_plans.as_deref(),
                            retry_config,
                            download_controls,
                            shutdown_token,
                            &mut cycle_stats,
                            &mut cycle_failed_count,
                            &mut cycle_session_expired,
                            &mut db_sync_token_advance_safe,
                            "retained checkpoint",
                        )
                        .await;
                        if cycle_session_expired || shutdown_token.is_cancelled() {
                            break;
                        }
                    }
                    continue;
                }
            }
        }

        // Historical attribution remains retained evidence. The v27 cursor
        // accounts for current observations and does not activate old proofs.

        if promote_only {
            let Some(db) = state_db else {
                continue;
            };
            let download_client = shared_session.read().await.download_client().clone();
            let local = Box::pin(run_epoch_local_recovery(
                &download_client,
                db,
                lib_state,
                config,
                build_download_config,
                shutdown_token,
                indexing_finished
                    && !indexing_reconciliation
                    && pending_download_config_hash.is_some()
                    && download_controls.run_mode.downloads_files(),
            ))
            .await?;
            cycle_failed_count = cycle_failed_count.saturating_add(local.failures);
            if let Some(stats) = local.reconciliation_stats {
                cycle_stats.accumulate(&stats);
            }
            path_reconciliation_complete &= local.path_complete;
            path_reconciliation_cycle_safe &= local.path_safe;
            if local.failures > 0 {
                db_sync_token_advance_safe = false;
            }
            let mut replay_ok = true;
            if !is_retry_failed
                && !config.runtime.only_print_filenames
                && download_controls.run_mode.downloads_files()
                && !shutdown_token.is_cancelled()
            {
                replay_ok = retry_due_admitted_while_held(
                    &download_client,
                    lib_state,
                    &config.filters.selection,
                    all_retry_plans.as_deref(),
                    Arc::clone(&local.config),
                    download_controls,
                    shutdown_token,
                    &mut cycle_stats,
                    &mut cycle_failed_count,
                    &mut cycle_session_expired,
                    &mut db_sync_token_advance_safe,
                    "epoch promotion",
                )
                .await;
            }
            if replay_ok
                && local.failures == 0
                && !cycle_session_expired
                && !shutdown_token.is_cancelled()
                && download_controls.run_mode.downloads_files()
            {
                match Box::pin(run_epoch_pending_retry(
                    &download_client,
                    lib_state,
                    &local.config,
                    download_controls,
                    shutdown_token,
                ))
                .await
                {
                    Ok(Some(result)) => {
                        cycle_stats.accumulate(&result.stats);
                        match result.outcome {
                            download::DownloadOutcome::Success => {}
                            download::DownloadOutcome::SessionExpired { .. } => {
                                cycle_session_expired = true;
                                db_sync_token_advance_safe = false;
                            }
                            download::DownloadOutcome::PartialFailure { failed_count } => {
                                cycle_failed_count =
                                    cycle_failed_count.saturating_add(failed_count);
                                db_sync_token_advance_safe = false;
                            }
                        }
                        if result.stats.state_write_failures > 0 || result.stats.interrupted {
                            db_sync_token_advance_safe = false;
                        }
                    }
                    Ok(None) => {}
                    Err(error) => {
                        cycle_failed_count = cycle_failed_count.saturating_add(1);
                        db_sync_token_advance_safe = false;
                        tracing::warn!(
                            error = %error,
                            "Pending generation replay failed while an epoch proof waits"
                        );
                    }
                }
            }
            let unresolved = sparse_identity_holds_current(
                db,
                &lib_state.zone_name,
                is_current_zone_checkpoint(&lib_state.sync_token_key),
            )
            .await?;
            if unresolved {
                download::block_sync_token_for_unresolved_identity(&mut cycle_stats);
                record_selected_hold(
                    &mut library_holds,
                    &mut cycle_stats,
                    &lib_state.zone_name,
                    library_ordinal,
                    LibraryHoldKind::IdentityUnresolved,
                    "asset_delta_hydration_incomplete",
                    download::sync_token_blocked_explanation("asset_delta_hydration_incomplete"),
                    true,
                );
                db_sync_token_advance_safe = false;
            }
            // Keep the exact staged token, descriptor and proof in place. The
            // all-zone aggregate promotion at the end of the cycle is the only
            // writer of the active token.
            continue;
        }

        // Determine source-enumeration mode per library. Failed transfers are
        // rehydrated from durable pending state by the download engine and do
        // not require replaying the provider inventory.
        let policy_probe = build_download_config(
            download::SyncMode::Full,
            Arc::new(rustc_hash::FxHashSet::default()),
            Arc::new(download::AssetGroupings::default()),
            Arc::from(lib_state.zone_name.as_str()),
        );
        let pending_zone_token =
            if matches!(enum_config_hash_outcome, EnumConfigHashOutcome::Changed) {
                let key = pending_zone_token_key(&enum_config_hash, &lib_state.zone_name);
                if let Some(db) = state_db
                    && download::has_current_observation(
                        &lib_state.plan.passes,
                        &policy_probe,
                        download_controls,
                        &key,
                    )
                    .await?
                {
                    db.get_metadata(&key)
                        .await?
                        .filter(|token| !token.trim().is_empty())
                } else {
                    None
                }
            } else {
                None
            };
        let download_policy_observed = pending_download_config_hash.is_some()
            && download::has_current_observation(
                &lib_state.plan.passes,
                &policy_probe,
                download_controls,
                &lib_state.sync_token_key,
            )
            .await?;
        let mut sync_mode_decision = if indexing_reconciliation || indexing_materialization_pending
        {
            SyncModeDecision {
                mode: download::SyncMode::Full,
                full_enumeration_reason: Some(download::FullEnumerationReason::OtherStaticReason),
            }
        } else if let Some(refresh_decision) =
            metadata_refresh_mode_decision(config.runtime.refresh_metadata)
        {
            refresh_decision
        } else if force_epoch_baseline {
            let reason = download::FullEnumerationReason::SyncEpochEnrollment;
            tracing::info!(
                full_enumeration_reason = reason.as_str(),
                "Starting the one-time sync epoch baseline"
            );
            SyncModeDecision {
                mode: download::SyncMode::Full,
                full_enumeration_reason: Some(reason),
            }
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
        } else if force_full_for_download_config_hash && !download_policy_observed {
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
        let mut download_config = build_download_config(
            sync_mode,
            Arc::new(rustc_hash::FxHashSet::default()),
            Arc::clone(&asset_groupings),
            Arc::from(lib_state.zone_name.as_str()),
        );
        download_config = Arc::new(download_config.check_ownership().await?);
        Arc::make_mut(&mut download_config).metadata_write_witness =
            Some(Arc::clone(&metadata_witness));
        if let Err(error) = download::prepare_retained_sources(
            &lib_state.plan.passes,
            &download_config,
            download_controls,
            shutdown_token,
        )
        .await
        {
            // Only this selected library's due historical hydration failed.
            // Its source cursor and retry owners remain unchanged; other
            // libraries retain their own independent checkpoint authority.
            cycle_failed_count = cycle_failed_count.saturating_add(1);
            cycle_stats.failed = cycle_stats.failed.saturating_add(1);
            cycle_stats.sync_token_blocked = true;
            cycle_stats
                .sync_token_blocked_zone
                .get_or_insert_with(|| lib_state.zone_name.clone());
            db_sync_token_advance_safe = false;
            path_reconciliation_complete = false;
            path_reconciliation_cycle_safe = false;
            tracing::warn!(error = %error, diagnostic = "retained_source_replay_failed",
                "Selected library retains its cursor because due historical source evidence is invalid");
            continue;
        }
        let baseline_key = match sync_epoch.as_deref() {
            Some(epoch) => format!(
                "baseline_cursor:v27:{}:{}:{}:{}",
                epoch,
                lib_state.library.baseline_scope(),
                enum_config_hash,
                preservation_config_hash.as_deref().unwrap_or(""),
            ),
            None => format!(
                "baseline_cursor:v27:{}:{}:{}",
                lib_state.library.baseline_scope(),
                enum_config_hash,
                preservation_config_hash.as_deref().unwrap_or(""),
            ),
        };
        let mut fresh_origin = None;
        let pending_catalog = if force_epoch_baseline
            && retained_cursor_at_start.is_none()
            && matches!(download_config.sync_mode, download::SyncMode::Full)
            && !lib_state.plan_is_stale
            && !lib_state.plan_needs_refresh
            && indexing_finished
            && !indexing_reconciliation
            && checkpoint_transition_state_safe
            && !download_config.retry_only
            && !config.runtime.dry_run
            && download_controls.run_mode.downloads_files()
            && let (Some(db), Some(epoch), Some(attempt)) =
                (state_db, sync_epoch.as_deref(), epoch_attempt.as_deref())
        {
            let result: anyhow::Result<_> = Box::pin(async {
                let Some((capture, mut request)) = capture_catalog_request(
                    lib_state,
                    &download_config,
                    download_controls,
                    db,
                    epoch,
                    attempt,
                    &baseline_key,
                    &enum_config_hash,
                    preservation_config_hash.as_deref(),
                    &zone_metadata_fence,
                    &path_metadata_at_start,
                )
                .await?
                else {
                    return Ok(None);
                };
                let resume = capture
                    .db
                    .catalog_resume(capture.owner.clone(), request.clone())
                    .await?;
                let (boundary, origin) = if let Some(resume) = resume {
                    anyhow::ensure!(
                        resume.relation
                            == state::db::source_revalidation::CatalogResumeRelation::PendingAttempt,
                        "Catalog resume is not a pending attempt"
                    );
                    (resume.boundary, resume.origin)
                } else {
                    let boundary = lib_state.library.fresh_baseline_cursor().await?;
                    request.fresh_cursor = boundary.clone();
                    let origin = capture
                        .db
                        .install_fresh_catalog_origin(capture.owner.clone(), request)
                        .await?;
                    (boundary, origin)
                };
                capture
                    .db
                    .seed_source_revalidation(
                        capture.owner.clone(),
                        lib_state.zone_name.clone(),
                        origin.scope(),
                        enum_config_hash.clone(),
                        state::db::source_revalidation::CatalogOrigin::boundary(&boundary),
                        shutdown_token.clone(),
                    )
                    .await?;
                anyhow::ensure!(
                    !shutdown_token.is_cancelled(),
                    "Pending catalog census interrupted"
                );
                Ok(Some((boundary, origin)))
            })
            .await;
            match result {
                Ok(prepared) => prepared,
                Err(error) => {
                    db_sync_token_advance_safe = false;
                    path_reconciliation_complete = false;
                    path_reconciliation_cycle_safe = false;
                    if shutdown_token.is_cancelled() {
                        cycle_stats.interrupted = true;
                        break;
                    }
                    if crate::icloud::photos::session::is_session_error(&error) {
                        cycle_session_expired = true;
                        break;
                    }
                    if matches!(
                        error.downcast_ref::<state::error::StateError>(),
                        Some(
                            state::error::StateError::Query { .. }
                                | state::error::StateError::Migration(_)
                                | state::error::StateError::LockPoisoned(_)
                                | state::error::StateError::Spawn(_)
                        )
                    ) {
                        cycle_stats.state_write_failures =
                            cycle_stats.state_write_failures.saturating_add(1);
                        operational_state_fault = true;
                    }
                    record_selected_hold(
                        &mut library_holds,
                        &mut cycle_stats,
                        &lib_state.zone_name,
                        library_ordinal,
                        LibraryHoldKind::BaselinePending,
                        "pending_catalog_unavailable",
                        "the source baseline and its local work are retained for retry",
                        true,
                    );
                    continue;
                }
            }
        } else {
            None
        };
        let mut baseline_cursor_at_start = if retained_cursor_at_start.is_none()
            && matches!(download_config.sync_mode, download::SyncMode::Full)
            && download_controls.run_mode.downloads_files()
            && !download_config.retry_only
            && indexing_finished
            && !indexing_reconciliation
        {
            let db = state_db
                .ok_or_else(|| anyhow::anyhow!("Baseline comparison requires durable state"))?;
            let cursor = match pending_catalog {
                Some((boundary, origin)) => {
                    fresh_origin = Some(origin);
                    boundary
                }
                None => match db.get_metadata(&baseline_key).await? {
                    Some(cursor) if !cursor.trim().is_empty() => cursor,
                    Some(_) => anyhow::bail!("Saved baseline cursor is invalid"),
                    None => {
                        let cursor = lib_state.library.fresh_baseline_cursor().await?;
                        db.set_metadata(&baseline_key, &cursor).await?;
                        cursor
                    }
                },
            };
            zone_metadata_fence.push((baseline_key.clone(), Some(cursor.clone())));
            Some(cursor)
        } else {
            None
        };
        if fresh_origin.is_none()
            && let Some(boundary) = &baseline_cursor_at_start
            && !lib_state.plan_is_stale
            && let [pass] = lib_state.plan.passes.as_slice()
            && pass.kind == crate::commands::PassKind::Unfiled
            && pass.exclude_ids.is_empty()
            && pass.album.has_collection_independent_rank()
            && download_config.media.is_all()
            && download_config.recent.is_none()
            && download_config.skip_created_before.is_none()
            && download_config.skip_created_after.is_none()
            && download_config.filename_exclude.is_empty()
            && !config.runtime.dry_run
            && let Some((capture, scope, _)) = pass.album.download_scope()?
        {
            use sha2::{Digest, Sha256};
            let expected_db = state_db
                .ok_or_else(|| anyhow::anyhow!("Source revalidation requires durable state"))?;
            anyhow::ensure!(
                std::ptr::addr_eq(
                    expected_db,
                    capture.db.as_ref() as &dyn download::DownloadStore
                ),
                "Source revalidation database context mismatch"
            );
            capture
                .db
                .seed_source_revalidation(
                    capture.owner,
                    lib_state.zone_name.clone(),
                    scope,
                    enum_config_hash.clone(),
                    format!("{:x}", Sha256::digest(boundary.as_bytes())),
                    shutdown_token.clone(),
                )
                .await?;
        }
        // FINISHED is a prerequisite, not proof that this inventory remained
        // complete. Deletion-capable maintenance stays deferred until the
        // full scope and its final readiness check have qualified.
        Arc::make_mut(&mut download_config).provisional =
            indexing_reconciliation || !indexing_finished;
        // Indexing recovery uses ordinary append-only matching and downloads.
        // Historical layout bindings do not require an inventory-only pass.
        Arc::make_mut(&mut download_config).indexing_inventory_only = false;
        if pending_download_config_hash.is_some() && path_reconciliation_has_smart_folders {
            Arc::make_mut(&mut download_config).complete_path_selection = true;
        }
        let download_client = shared_session.read().await.download_client().clone();
        // Only an unrestricted current full-library selection can admit an exact
        // sparse target without inventing album/rank membership.
        let mut sparse_targets = Vec::new();
        if download_controls.run_mode.downloads_files() && !is_retry_failed {
            for target in library_states
                .iter()
                .copied()
                .filter(|target| target.zone_name != lib_state.zone_name && !target.plan_is_stale)
            {
                for pass in &target.plan.passes {
                    if (!pass.album.has_collection_independent_rank()
                        && !pass.album.is_hidden_media_selection())
                        || !pass.exclude_ids.is_empty()
                    {
                        continue;
                    }
                    #[cfg(feature = "xmp")]
                    let target_groupings =
                        if config.metadata.embed_xmp || config.metadata.xmp_sidecar {
                            preload_asset_groupings(state_db, &target.zone_name).await
                        } else {
                            Arc::new(download::AssetGroupings::default())
                        };
                    #[cfg(not(feature = "xmp"))]
                    let target_groupings = Arc::new(download::AssetGroupings::default());
                    let target_config = build_download_config(
                        download::SyncMode::Full,
                        Arc::clone(&pass.exclude_ids),
                        target_groupings,
                        Arc::from(target.zone_name.as_str()),
                    );
                    if target_config.recent.is_some()
                        || target_config.skip_created_before.is_some()
                        || target_config.skip_created_after.is_some()
                        || target_config.retry_only
                        || target_config.edited_naming == crate::types::EditedNaming::Primary
                    {
                        continue;
                    }
                    let mut target_config = target_config.with_pass(pass);
                    // A full-library path label is not evidence of album membership.
                    if pass.kind == crate::commands::PassKind::Unfiled {
                        target_config.album_name = None;
                    }
                    sparse_targets.push(download::sparse_target::SelectedSparseTarget {
                        library: target.library.clone(),
                        album: pass.album.clone(),
                        config: Arc::new(target_config),
                        client: download_client.clone(),
                        controls: download_controls,
                    });
                }
            }
        }
        Arc::make_mut(&mut download_config).sparse_targets = sparse_targets.into();

        let mut path_reconciliation_requires_smart_query = false;
        if indexing_finished
            && !indexing_reconciliation
            && pending_download_config_hash.is_some()
            && download_controls.run_mode.downloads_files()
        {
            let reconciliation = download::recover_recorded_sources(
                &download_client,
                &lib_state.plan.passes,
                Arc::clone(&download_config),
                shutdown_token.clone(),
            )
            .await?;
            path_reconciliation_requires_smart_query = lib_state
                .plan
                .passes
                .iter()
                .any(|pass| pass.kind == crate::commands::PassKind::SmartFolder);
            path_reconciliation_complete &=
                reconciliation.complete || reconciliation.complete_after_smart_query;
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
        // Capture the pre-dispatch epoch/attempt identity for the one current
        // sparse witness. Never derive the expected identity from a final read.
        let expected_epoch = sync_epoch.clone();
        let mut expected_attempt = epoch_attempt.clone();
        if expected_attempt.is_none()
            && let Some(db) = state_db
            && let Some(raw) = db
                .get_metadata(&state::db::sync_epoch::library_key(&lib_state.zone_name))
                .await?
        {
            expected_attempt =
                serde_json::from_str::<state::db::sync_epoch::SyncEpochLibrary>(&raw)
                    .ok()
                    .map(|record| record.attempt);
        }
        if pending_download_config_hash.is_none()
            && let (Some(epoch), Some(attempt)) = (&expected_epoch, &expected_attempt)
            && let Some(source) = sources
                .iter_mut()
                .find(|source| source.zone_name == lib_state.zone_name)
        {
            source.operational_prework = download::operational_prework(
                &lib_state.plan.passes,
                &download_config,
                download_controls,
                &source.baseline_scope,
                epoch,
                attempt,
                &enum_config_hash,
            )
            .await?;
            if let Some(prework) = &source.operational_prework {
                let same_store = state_db.is_some_and(|db| {
                    std::ptr::eq(
                        std::ptr::from_ref(db).cast::<()>(),
                        Arc::as_ptr(&prework.db).cast::<()>(),
                    )
                });
                if !same_store {
                    source.operational_prework = None;
                }
            }
        }
        if let Some(source) = sources
            .iter_mut()
            .find(|source| source.zone_name == lib_state.zone_name)
        {
            source.reached = true;
        }
        // Only a current, authenticated local source can hand a rejected
        // incremental token back for an atomic fresh-origin installation.
        let fresh_catalog = if expected_epoch.is_some()
            && expected_attempt.is_some()
            && !lib_state.plan_is_stale
            && indexing_finished
            && !indexing_reconciliation
            && checkpoint_transition_state_safe
            && !download_config.retry_only
            && !config.runtime.dry_run
            && download_controls.run_mode.downloads_files()
            && matches!(
                &download_config.sync_mode,
                download::SyncMode::Incremental { zone_sync_token }
                    if Some(zone_sync_token) == retained_cursor_at_start.as_ref()
            )
            && download::has_current_observation(
                &lib_state.plan.passes,
                &download_config,
                download_controls,
                &lib_state.sync_token_key,
            )
            .await?
        {
            let plan = download::catalog_plan_context_for_source(
                &lib_state.plan.passes,
                &download_config,
                download_controls,
                None,
            )
            .await?;
            let mut capture = None;
            if let Some(plan) = plan {
                for pass in &lib_state.plan.passes {
                    if let Some((source, scope, zone)) = pass.album.download_scope()?
                        && scope == plan.scope
                        && zone == plan.zone
                    {
                        capture = Some((plan, source));
                        break;
                    }
                }
            }
            capture
        } else {
            None
        };
        enum CatalogPrestream {
            Current,
            Bootstrap(
                Box<(
                    crate::icloud::photos::inbox::ShadowCapture,
                    state::db::source_revalidation::FreshCatalogOriginRequest,
                )>,
            ),
            Resume {
                origin: state::db::source_revalidation::CatalogOrigin,
                boundary: String,
            },
        }
        let mut bootstrap_candidate = None;
        if matches!(
            &download_config.sync_mode,
            download::SyncMode::Incremental { zone_sync_token }
                if Some(zone_sync_token) == retained_cursor_at_start.as_ref()
        ) && !force_full_for_config_hash
            && pending_download_config_hash.is_none()
            && !lib_state.plan_is_stale
            && indexing_finished
            && !indexing_reconciliation
            && checkpoint_transition_state_safe
            && !download_config.retry_only
            && !config.runtime.dry_run
            && download_controls.run_mode.downloads_files()
            && let (Some(db), Some(epoch), Some(attempt), Some(token)) = (
                state_db,
                expected_epoch.as_deref(),
                expected_attempt.as_deref(),
                retained_cursor_at_start.as_deref(),
            )
        {
            let prepared: anyhow::Result<_> = Box::pin(async {
                anyhow::ensure!(
                    !lib_state.plan_needs_refresh,
                    "Current catalog source plan needs refresh"
                );
                let (capture, mut request) = capture_catalog_request(
                    lib_state,
                    &download_config,
                    download_controls,
                    db,
                    epoch,
                    attempt,
                    &baseline_key,
                    &enum_config_hash,
                    preservation_config_hash.as_deref(),
                    &zone_metadata_fence,
                    &path_metadata_at_start,
                )
                .await?
                .ok_or_else(|| anyhow::anyhow!("Current catalog source is unavailable"))?;
                request.fresh_cursor = token.to_owned();
                let resume = capture
                    .db
                    .catalog_resume(capture.owner.clone(), request.clone())
                    .await?;
                if let Some(resume) = resume {
                    anyhow::ensure!(
                        resume.relation
                            != state::db::source_revalidation::CatalogResumeRelation::PendingAttempt,
                        "Current catalog resume unexpectedly belongs to a pending attempt"
                    );
                    // Equal token text is not EOF proof. Both relations still
                    // require a full inventory and a bridge from this boundary.
                    let origin = resume.origin;
                    let boundary = resume.boundary;
                    capture
                        .db
                        .seed_source_revalidation(
                            capture.owner.clone(),
                            lib_state.zone_name.clone(),
                            origin.scope(),
                            enum_config_hash.clone(),
                            state::db::source_revalidation::CatalogOrigin::boundary(&boundary),
                            shutdown_token.clone(),
                        )
                        .await?;
                    anyhow::ensure!(!shutdown_token.is_cancelled(), "Catalog census interrupted");
                    return Ok(CatalogPrestream::Resume { origin, boundary });
                }
                match capture
                    .db
                    .catalog_bootstrap_eligibility(capture.owner.clone(), request.clone())
                    .await?
                {
                    state::db::source_revalidation::CatalogBootstrapEligibility::CurrentLink
                    | state::db::source_revalidation::CatalogBootstrapEligibility::CurrentLegacyLink => {
                        Ok(CatalogPrestream::Current)
                    }
                    state::db::source_revalidation::CatalogBootstrapEligibility::AttemptBootstrap => {
                        Ok(CatalogPrestream::Bootstrap(Box::new((capture, request))))
                    }
                }
            })
            .await;
            match prepared {
                Ok(CatalogPrestream::Current) => {}
                Ok(CatalogPrestream::Bootstrap(candidate)) => {
                    bootstrap_candidate = Some(candidate);
                }
                Ok(CatalogPrestream::Resume { origin, boundary }) => {
                    zone_metadata_fence.push((baseline_key.clone(), Some(boundary.clone())));
                    baseline_cursor_at_start = Some(boundary);
                    fresh_origin = Some(origin);
                    Arc::make_mut(&mut download_config).sync_mode = download::SyncMode::Full;
                    let reason = download::FullEnumerationReason::OtherStaticReason;
                    sync_mode_decision.full_enumeration_reason = Some(reason);
                    if let Err(error) = db
                        .set_metadata(LAST_FULL_ENUMERATION_REASON_KEY, reason.as_str())
                        .await
                    {
                        tracing::debug!(error = %error, "Failed to persist full-enumeration reason");
                    }
                    tracing::info!(
                        zone = %lib_state.zone_name,
                        full_enumeration_reason = reason.as_str(),
                        "Resuming the catalog inventory from its observed boundary"
                    );
                }
                Err(error) => {
                    db_sync_token_advance_safe = false;
                    path_reconciliation_complete = false;
                    path_reconciliation_cycle_safe = false;
                    if shutdown_token.is_cancelled() {
                        cycle_stats.interrupted = true;
                        break;
                    }
                    if matches!(
                        error.downcast_ref::<state::error::StateError>(),
                        Some(
                            state::error::StateError::Query { .. }
                                | state::error::StateError::Migration(_)
                                | state::error::StateError::LockPoisoned(_)
                                | state::error::StateError::Spawn(_)
                        )
                    ) {
                        cycle_stats.state_write_failures =
                            cycle_stats.state_write_failures.saturating_add(1);
                        operational_state_fault = true;
                    }
                    record_selected_hold(
                        &mut library_holds,
                        &mut cycle_stats,
                        &lib_state.zone_name,
                        library_ordinal,
                        LibraryHoldKind::BaselinePending,
                        "catalog_bootstrap_unavailable",
                        "the source checkpoint and local work are retained until catalog evidence is current",
                        true,
                    );
                    continue;
                }
            }
        }
        let mut dispatch_controls = download_controls;
        dispatch_controls.defer_fallback_to_cycle =
            fresh_catalog.is_some() || fresh_origin.is_some();
        let mut retained_main_rejected = false;
        let initial_result = Box::pin(download::download_photos_with_sync(
            &download_client,
            &lib_state.plan.passes,
            Arc::clone(&download_config),
            dispatch_controls,
            shutdown_token.clone(),
        ))
        .await;
        let mut sync_result = match initial_result {
            Ok(result) => result,
            Err(error)
                if error
                    .downcast_ref::<download::FreshBaselineRequired>()
                    .is_some() =>
            {
                // The rejected interval is owned by the fresh-boundary path,
                // not by the pre-stream hint for a completed delta.
                bootstrap_candidate = None;
                let Some((plan, capture)) = fresh_catalog else {
                    return Err(error);
                };
                if matches!(
                    error.downcast_ref::<download::FreshBaselineRequired>(),
                    Some(download::FreshBaselineRequired::StaticFallback)
                ) {
                    record_selected_hold(
                        &mut library_holds,
                        &mut cycle_stats,
                        &lib_state.zone_name,
                        library_ordinal,
                        LibraryHoldKind::RetainedCheckpoint,
                        "incremental_fallback_unproven",
                        "the retained provider interval could not be accounted; its checkpoint and local work are retained",
                        true,
                    );
                    db_sync_token_advance_safe = false;
                    path_reconciliation_complete = false;
                    path_reconciliation_cycle_safe = false;
                    continue;
                }
                retained_main_rejected = true;
                let mut prepared_baseline_fence = None;
                let prepared: anyhow::Result<_> = Box::pin(async {
                    let db = state_db
                        .ok_or_else(|| anyhow::anyhow!("Fresh catalog requires durable state"))?;
                    anyhow::ensure!(
                        std::ptr::addr_eq(db, capture.db.as_ref() as &dyn download::DownloadStore),
                        "Fresh catalog database context mismatch"
                    );
                    let mut expected_metadata = zone_metadata_fence.clone();
                    expected_metadata.extend(path_metadata_at_start.iter().cloned());
                    for key in [
                        state::SYNC_EPOCH_KEY.to_owned(),
                        state::db::sync_epoch::library_key(&lib_state.zone_name),
                        ENUM_CONFIG_HASH_KEY.to_owned(),
                        PENDING_ENUM_CONFIG_HASH_KEY.to_owned(),
                        download::DOWNLOAD_CONFIG_HASH_KEY.to_owned(),
                        PENDING_DOWNLOAD_CONFIG_HASH_KEY.to_owned(),
                        lib_state.sync_token_key.clone(),
                        format!("observed_selection:v27:{}", lib_state.zone_name),
                        format!("download_ledger_scope:{}", lib_state.zone_name),
                    ] {
                        if !expected_metadata.iter().any(|(saved, _)| saved == &key) {
                            let value = db.get_metadata(&key).await?;
                            expected_metadata.push((key, value));
                        }
                    }
                    let expected_baseline = db.get_metadata(&baseline_key).await?;
                    prepared_baseline_fence = Some(expected_baseline.clone());
                    let snapshot = capture
                        .db
                        .fresh_catalog_snapshot(capture.owner.clone(), lib_state.zone_name.clone())
                        .await?;
                    let boundary = lib_state.library.fresh_baseline_cursor().await?;
                    let request = state::db::source_revalidation::FreshCatalogOriginRequest {
                        library: lib_state.zone_name.clone(),
                        plan: state::db::source_revalidation::FreshCatalogPlan {
                            account: plan.account,
                            provider: plan.provider,
                            database: plan.database,
                            zone: plan.zone,
                            scope: plan.scope,
                            config_hash: plan.config_hash,
                            metadata_enabled: plan.metadata_enabled,
                            metadata_flags: plan.metadata_flags,
                            passes: plan
                                .passes
                                .into_iter()
                                .map(|pass| state::db::source_revalidation::FreshCatalogPass {
                                    key: pass.key,
                                    kind: pass.kind,
                                    scope: pass.scope,
                                    excludes: pass.excludes,
                                })
                                .collect(),
                            profile: plan.profile,
                        },
                        baseline_key: baseline_key.clone(),
                        preservation_config_hash: preservation_config_hash
                            .clone()
                            .unwrap_or_default(),
                        expected_baseline,
                        fresh_cursor: boundary.clone(),
                        enum_hash: enum_config_hash.clone(),
                        expected_epoch: expected_epoch
                            .clone()
                            .ok_or_else(|| anyhow::anyhow!("Fresh catalog requires an epoch"))?,
                        expected_attempt: expected_attempt
                            .clone()
                            .ok_or_else(|| anyhow::anyhow!("Fresh catalog requires an attempt"))?,
                        expected_metadata,
                        expected_catalog_snapshot: snapshot,
                    };
                    let origin = capture
                        .db
                        .install_fresh_catalog_origin(capture.owner.clone(), request)
                        .await?;
                    prepared_baseline_fence = Some(Some(boundary.clone()));
                    capture
                        .db
                        .seed_source_revalidation(
                            capture.owner.clone(),
                            lib_state.zone_name.clone(),
                            origin.scope(),
                            enum_config_hash.clone(),
                            state::db::source_revalidation::CatalogOrigin::boundary(&boundary),
                            shutdown_token.clone(),
                        )
                        .await?;
                    anyhow::ensure!(
                        !shutdown_token.is_cancelled(),
                        "Fresh catalog census interrupted"
                    );
                    Ok((boundary, origin))
                })
                .await;
                let (boundary, origin) = match prepared {
                    Ok(prepared) => prepared,
                    Err(error) => {
                        db_sync_token_advance_safe = false;
                        path_reconciliation_complete = false;
                        path_reconciliation_cycle_safe = false;
                        if shutdown_token.is_cancelled() {
                            cycle_stats.interrupted = true;
                            break;
                        }
                        if crate::icloud::photos::session::is_session_error(&error) {
                            cycle_session_expired = true;
                            break;
                        }
                        if matches!(
                            error.downcast_ref::<state::error::StateError>(),
                            Some(
                                state::error::StateError::Query { .. }
                                    | state::error::StateError::Migration(_)
                                    | state::error::StateError::LockPoisoned(_)
                                    | state::error::StateError::Spawn(_)
                            )
                        ) {
                            cycle_stats.state_write_failures =
                                cycle_stats.state_write_failures.saturating_add(1);
                            operational_state_fault = true;
                        }
                        if let (Some(db), Some(prior), Some(expected_baseline)) = (
                            state_db,
                            retained_cursor_at_start.as_deref(),
                            prepared_baseline_fence,
                        ) {
                            let mut hold = RetainedCheckpointHold::new(
                                prior,
                                &enum_config_hash,
                                preservation_config_hash.as_deref(),
                            );
                            hold.attempt_count = retained_checkpoint_attempt;
                            let expected = zone_metadata_fence
                                .iter()
                                .cloned()
                                .chain(path_metadata_at_start.iter().cloned())
                                .chain([(baseline_key.clone(), expected_baseline)])
                                .collect();
                            if let Err(hold_error) = persist_retained_checkpoint_hold(
                                db,
                                hold_key.clone(),
                                &hold,
                                expected,
                            )
                            .await
                            {
                                tracing::warn!(error = %hold_error, "Failed to retain rejected main checkpoint hold");
                                cycle_stats.state_write_failures =
                                    cycle_stats.state_write_failures.saturating_add(1);
                                operational_state_fault = true;
                            }
                        }
                        record_selected_hold(
                            &mut library_holds,
                            &mut cycle_stats,
                            &lib_state.zone_name,
                            library_ordinal,
                            LibraryHoldKind::RetainedCheckpoint,
                            "fresh_catalog_unavailable",
                            "a fresh qualified source boundary could not be installed; the checkpoint and local work are retained",
                            true,
                        );
                        continue;
                    }
                };
                zone_metadata_fence.push((baseline_key.clone(), Some(boundary.clone())));
                baseline_cursor_at_start = Some(boundary);
                fresh_origin = Some(origin);
                Arc::make_mut(&mut download_config).sync_mode = download::SyncMode::Full;
                Box::pin(download::download_photos_with_sync(
                    &download_client,
                    &lib_state.plan.passes,
                    Arc::clone(&download_config),
                    download_controls,
                    shutdown_token.clone(),
                ))
                .await?
            }
            Err(error) => return Err(error),
        };

        // Capture only the initial complete selected scope, before a bridge
        // adds repeated count observations to the public activity statistics.
        let inventory = sync_result.inventory.take().filter(|measurement| {
            measurement.scope == download::inventory_scope(&lib_state.plan.passes, &download_config)
        });

        observe_sync_component("initial", &sync_result, lib_state.plan.passes.len());

        let mut retained_checkpoint_expired = false;
        let mut checkpoint_basis =
            if sync_result.full_enumeration_ran && !sync_result.checkpoint.completed_delta_replay {
                CheckpointBasis::CompleteInventory
            } else {
                CheckpointBasis::IncrementalDelta
            };

        // Eligibility reconciliation begins from a trusted prior cursor when
        // one exists. The inventory covers unchanged newly eligible assets;
        // replaying the prior cursor then bridges changes that occurred while
        // that inventory was running. Only the bridged token may replace the
        // active checkpoint.
        let unresolved_identity = if let Some(db) = state_db {
            sparse_identity_holds_current(
                db,
                &lib_state.zone_name,
                is_current_zone_checkpoint(&lib_state.sync_token_key),
            )
            .await?
        } else {
            false
        };
        if (baseline_cursor_at_start.is_some()
            || (config.runtime.refresh_metadata && retained_cursor_at_start.is_some())
            || matches!(enum_config_hash_outcome, EnumConfigHashOutcome::Changed)
            || pending_download_config_hash.is_some()
            || unresolved_identity
            || ((indexing_replay_pending || indexing_materialization_pending)
                && !is_retry_failed
                && !download_config.retry_only))
            && indexing_finished
            && !indexing_reconciliation
            && sync_result.full_enumeration_ran
            && !sync_result.checkpoint.completed_delta_replay
            && checkpoint_transition_state_safe
            && (!indexing_materialization_pending
                || (matches!(sync_result.outcome, download::DownloadOutcome::Success)
                    && sync_result.stats.failed == 0
                    && sync_result.stats.exif_failures == 0
                    && download_config.recent.is_none()
                    && download_config.skip_created_before.is_none()
                    && download_config.skip_created_after.is_none()))
        {
            let prior_token = baseline_cursor_at_start
                .clone()
                .or_else(|| retained_cursor_at_start.clone());
            if let Some(mut prior_token) = prior_token.filter(|token| !token.trim().is_empty()) {
                for baseline_attempt in 0..2 {
                    let inventory_decision = source_checkpoint_decision(
                        &sync_result,
                        config.runtime.dry_run,
                        lib_state.plan_is_stale,
                        CheckpointBasis::CompleteInventory,
                    );
                    let complete_selected_window = sync_result.checkpoint.selection_window_complete
                        && !config.runtime.dry_run
                        && !sync_result.checkpoint.sync_token_blocked
                        && !sync_result.checkpoint.identity_incomplete
                        && !sync_result.checkpoint.enumeration_incomplete
                        && sync_result.checkpoint.enumeration_errors == 0
                        && sync_result.checkpoint.state_write_failures == 0
                        && !sync_result.checkpoint.interrupted
                        && !lib_state.plan_is_stale;
                    if matches!(inventory_decision, SourceCheckpointDecision::Advance { .. })
                        || complete_selected_window
                    {
                        let mut bridge_config = build_download_config(
                            download::SyncMode::Incremental {
                                zone_sync_token: prior_token.clone(),
                            },
                            Arc::new(rustc_hash::FxHashSet::default()),
                            Arc::clone(&asset_groupings),
                            Arc::from(lib_state.zone_name.as_str()),
                        );
                        tracing::info!(
                            zone = %lib_state.zone_name,
                            "Inventory reconciliation complete; bridging changes from the preserved provider checkpoint"
                        );
                        Arc::make_mut(&mut bridge_config).metadata_write_witness =
                            Some(Arc::clone(&metadata_witness));
                        Arc::make_mut(&mut bridge_config).sparse_targets =
                            Arc::clone(&download_config.sparse_targets);
                        Arc::make_mut(&mut bridge_config).prior_inventory_proof =
                            sync_result.checkpoint.observation.clone().map(Box::new);
                        Arc::make_mut(&mut bridge_config).prior_sparse_bridge_proofs =
                            Arc::from(sync_result.checkpoint.sparse_identity_proofs.clone());
                        let mut bridge_controls = if fresh_origin.is_some() {
                            dispatch_controls
                        } else {
                            download_controls
                        };
                        bridge_controls.defer_changed_fresh_bridge_inventory =
                            fresh_origin.is_some();
                        let bridge_result = Box::pin(download::download_photos_with_sync(
                            &download_client,
                            &lib_state.plan.passes,
                            bridge_config,
                            bridge_controls,
                            shutdown_token.clone(),
                        ))
                        .await;
                        let bridge_result = match bridge_result {
                            Ok(result) => result,
                            Err(error)
                                if error
                                    .downcast_ref::<download::FreshBaselineRequired>()
                                    .is_some() =>
                            {
                                sync_result.sync_token = None;
                                sync_result.checkpoint.sync_token_blocked = true;
                                sync_result.checkpoint.project(&mut sync_result.stats);
                                block_for_retained_checkpoint(&mut sync_result.stats);
                                record_selected_hold(
                                    &mut library_holds,
                                    &mut cycle_stats,
                                    &lib_state.zone_name,
                                    library_ordinal,
                                    LibraryHoldKind::RetainedCheckpoint,
                                    "retained_checkpoint_expired",
                                    download::sync_token_blocked_explanation(
                                        "retained_checkpoint_expired",
                                    ),
                                    true,
                                );
                                break;
                            }
                            Err(error) => return Err(error),
                        };
                        observe_sync_component(
                            "delta_bridge",
                            &bridge_result,
                            lib_state.plan.passes.len(),
                        );
                        let bridge_decision = source_checkpoint_decision(
                            &bridge_result,
                            config.runtime.dry_run,
                            lib_state.plan_is_stale,
                            CheckpointBasis::IncrementalDelta,
                        );
                        sync_result.outcome =
                            merge_download_outcomes(&sync_result.outcome, &bridge_result.outcome);
                        sync_result.accumulate(&bridge_result);
                        if fresh_origin.is_some() && bridge_result.checkpoint.inventory_rank_changed
                        {
                            // A catalog origin is bound to the original boundary.
                            // Do not rewrite that boundary independently of its owner.
                            sync_result.sync_token = None;
                            sync_result.checkpoint.sync_token_blocked = true;
                            sync_result.checkpoint.project(&mut sync_result.stats);
                            sync_result.stats.sync_token_blocked_reason =
                                Some("baseline_changed_during_inventory");
                            sync_result.stats.sync_token_blocked_source = Some("icloud");
                            sync_result.stats.sync_token_blocked_explanation = Some(
                                "provider changes shifted the inventory; the original boundary and completed work are retained",
                            );
                            break;
                        }
                        if baseline_cursor_at_start.is_some()
                            && bridge_result.checkpoint.retained_token_rejected
                            && let Some(db) = state_db
                        {
                            // This boundary never became an active token. Retain
                            // all ledger work but obtain a fresh boundary next run.
                            db.delete_metadata_by_prefix(&baseline_key).await?;
                        }
                        if baseline_cursor_at_start.is_some()
                            && bridge_result.checkpoint.inventory_rank_changed
                            && let SourceCheckpointDecision::Advance { token, .. } =
                                &bridge_decision
                        {
                            let db = state_db.ok_or_else(|| {
                                anyhow::anyhow!("Baseline comparison requires durable state")
                            })?;
                            db.set_metadata(&baseline_key, token).await?;
                            for (key, expected) in &mut zone_metadata_fence {
                                if key == &baseline_key {
                                    *expected = Some(token.clone());
                                }
                            }
                            if baseline_attempt == 0 {
                                prior_token = token.clone();
                                tracing::info!(
                                    "Provider changed during baseline; repeating the rank inventory with completed work retained"
                                );
                                // Selected admission may already have enumerated
                                // after processing this delta. Bridge that inventory
                                // instead of immediately enumerating it a third time.
                                if !bridge_result.full_enumeration_ran {
                                    let previous = sync_result;
                                    sync_result = Box::pin(download::download_photos_with_sync(
                                        &download_client,
                                        &lib_state.plan.passes,
                                        Arc::clone(&download_config),
                                        download_controls,
                                        shutdown_token.clone(),
                                    ))
                                    .await?;
                                    sync_result.outcome = merge_download_outcomes(
                                        &previous.outcome,
                                        &sync_result.outcome,
                                    );
                                    sync_result.accumulate(&previous);
                                }
                                // Only the upcoming bridge may complete this new
                                // rank inventory; older replay evidence is not its proof.
                                if bridge_result.full_enumeration_ran {
                                    sync_result.checkpoint.observation = bridge_result
                                        .checkpoint
                                        .observation
                                        .clone()
                                        .and_then(|p| p.restarted_inventory());
                                }
                                sync_result.checkpoint.completed_delta_replay = false;
                                sync_result.checkpoint.inventory_rank_changed = false;
                                continue;
                            }
                            sync_result.sync_token = None;
                            sync_result.checkpoint.sync_token_blocked = true;
                            sync_result.checkpoint.project(&mut sync_result.stats);
                            sync_result.stats.sync_token_blocked_reason =
                                Some("baseline_changed_during_inventory");
                            sync_result.stats.sync_token_blocked_source = Some("icloud");
                            sync_result.stats.sync_token_blocked_explanation = Some(
                                "provider changes can shift rank pages; completed files and pending work are retained for the next baseline pass",
                            );
                            break;
                        }
                        if baseline_cursor_at_start.is_none()
                            && bridge_result.checkpoint.retained_token_rejected
                            && !bridge_result.checkpoint.completed_delta_replay
                            && let Some(db) = state_db
                        {
                            retained_checkpoint_expired = true;
                            let mut hold = RetainedCheckpointHold::new(
                                &prior_token,
                                &enum_config_hash,
                                preservation_config_hash.as_deref(),
                            );
                            hold.attempt_count = retained_checkpoint_attempt;
                            if persist_retained_checkpoint_hold(
                                db,
                                retained_checkpoint_hold_key(&lib_state.zone_name),
                                &hold,
                                zone_metadata_fence.clone(),
                            )
                            .await
                            .is_err()
                            {
                                sync_result.checkpoint.state_write_failures = sync_result
                                    .checkpoint
                                    .state_write_failures
                                    .saturating_add(1);
                                sync_result.stats.state_write_failures =
                                    sync_result.stats.state_write_failures.saturating_add(1);
                            }
                            block_for_retained_checkpoint(&mut sync_result.stats);
                        }
                        if (fresh_origin.is_none() && !bridge_result.full_enumeration_ran)
                            || bridge_result.checkpoint.completed_delta_replay
                        {
                            if let SourceCheckpointDecision::Advance { token, .. } = bridge_decision
                            {
                                sync_result.checkpoint.observation = sync_result
                                    .checkpoint
                                    .observation
                                    .take()
                                    .zip(bridge_result.checkpoint.observation.clone())
                                    .and_then(|(inventory, delta)| {
                                        inventory.bridge(delta, &prior_token)
                                    })
                                    .map(|proof| match &fresh_origin {
                                        Some(origin) => proof.with_catalog_origin(origin.clone()),
                                        None => proof,
                                    });
                                if fresh_origin.is_some()
                                    && sync_result.checkpoint.observation.is_none()
                                {
                                    sync_result.sync_token = None;
                                    sync_result.checkpoint.sync_token_blocked = true;
                                    sync_result.checkpoint.project(&mut sync_result.stats);
                                    block_for_retained_checkpoint(&mut sync_result.stats);
                                } else {
                                    sync_result.sync_token = Some(token);
                                    checkpoint_basis = CheckpointBasis::InventoryWithDeltaBridge;
                                }
                            } else {
                                sync_result.sync_token = None;
                                sync_result.checkpoint.sync_token_blocked = true;
                                sync_result.checkpoint.project(&mut sync_result.stats);
                                sync_result.stats.sync_token_blocked_reason =
                                    Some("inventory_delta_bridge_failed");
                                sync_result.stats.sync_token_blocked_source = Some("kei");
                                sync_result.stats.sync_token_blocked_explanation = Some(
                                    "the inventory completed, but replay from the preserved provider checkpoint did not finish safely",
                                );
                            }
                        } else {
                            sync_result.sync_token = None;
                            sync_result.checkpoint.sync_token_blocked = true;
                            sync_result.checkpoint.project(&mut sync_result.stats);
                            if bridge_result.checkpoint.retained_token_rejected {
                                block_for_retained_checkpoint(&mut sync_result.stats);
                            } else {
                                sync_result.stats.sync_token_blocked_reason =
                                    Some("inventory_delta_bridge_failed");
                                sync_result.stats.sync_token_blocked_source = Some("kei");
                                sync_result.stats.sync_token_blocked_explanation = Some(
                                    "the prior provider checkpoint could not be replayed incrementally after inventory reconciliation",
                                );
                            }
                        }
                    }
                    break;
                }
            }
        }

        if unresolved_identity
            && !indexing_reconciliation
            && matches!(checkpoint_basis, CheckpointBasis::CompleteInventory)
        {
            // An inventory cannot prove that the previously ambiguous deltas
            // were resolved. Require replay from the retained cursor.
            sync_result.block_for_unresolved_identity();
        }

        if retained_checkpoint_expired {
            block_for_retained_checkpoint(&mut sync_result.stats);
        }

        if sync_result.full_enumeration_ran && sync_result.stats.full_enumeration_reason.is_none() {
            sync_result.stats.full_enumeration_reason = sync_mode_decision.full_enumeration_reason;
        }

        // --refresh-metadata bypasses automatic capture repair. Inspect the
        // retained queue before inventory anchors or provider cursors commit,
        // not just when reporting aggregate cycle health afterward.
        if let Some(db) = state_db
            && let Some(capture) = db
                .get_summary()
                .await?
                .metadata_capture
                .into_iter()
                .find(|capture| capture.library == lib_state.zone_name)
        {
            sync_result.stats.unattributed_legacy_assets = capture.unattributed_legacy_assets;
            sync_result.stats.unattributed_legacy_pending = capture.unattributed_legacy_pending;
            sync_result.stats.metadata_capture_remaining = capture.remaining_assets;
            sync_result.stats.metadata_capture_unresolved = capture.unresolved_assets;
            sync_result.stats.metadata_capture_deferred = capture.deferred_assets;
        }

        let library_completed_without_errors =
            matches!(&sync_result.outcome, download::DownloadOutcome::Success)
                && !sync_result.checkpoint.identity_incomplete
                && !sync_result.checkpoint.interrupted
                && sync_result.checkpoint.enumeration_errors == 0
                && !shutdown_token.is_cancelled();
        if path_reconciliation_requires_smart_query {
            // Historical smart membership cannot certify the current selection.
            // Full query proof is separate from a bounded source cursor hold.
            path_reconciliation_complete &= library_completed_without_errors
                && download_controls.run_mode.downloads_files()
                && sync_result.full_enumeration_ran;
        }
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
        if indexing_finished
            && !indexing_reconciliation
            && library_completed_without_errors
            && !sync_result.checkpoint.enumeration_incomplete
            && !sync_result.checkpoint.sync_token_blocked
            && sync_result.checkpoint.state_write_failures == 0
            && !cycle_has_stale_plan
            && !is_retry_failed
            && download_controls.run_mode.downloads_files()
            && let Some(inventory) = inventory.as_ref()
            && let Some(db) = state_db
        {
            update_inventory_anchor_for_cycle(
                db,
                &enum_config_hash,
                &lib_state.zone_name,
                inventory,
                &mut sync_result.stats,
            )
            .await;
        }

        // Readiness can regress during a long inventory. Recheck at the
        // checkpoint boundary; even a failed probe leaves durable retry debt.
        let readiness_after = tokio::select! {
            biased;
            () = shutdown_token.cancelled() => None,
            result = async {
                let mut finished = lib_state.library.check_indexing().await?;
                for source in &lib_state.cross_zone_libraries {
                    if source.zone_name() != lib_state.zone_name {
                        finished &= source.check_indexing().await?;
                    }
                }
                Ok::<_, crate::icloud::error::ICloudError>(finished)
            } => Some(result),
        };
        // Cancellation is not an indexing observation. Preserve existing debt,
        // but do not turn ordinary interrupted sync into a new indexing hold.
        let indexing_hold_required =
            !indexing_finished || matches!(readiness_after, Some(Ok(false)) | Some(Err(_)));
        if indexing_hold_required {
            if !config.runtime.dry_run
                && let Some(db) = state_db
            {
                db.set_metadata(&indexing_key, "1").await?;
                // This cycle must not stage or activate a token, even if all
                // selected records happened to carry one unanimous candidate.
            }
            sync_result.checkpoint.sync_token_blocked = true;
            sync_result.checkpoint.project(&mut sync_result.stats);
            sync_result.stats.sync_token_blocked_reason = Some("indexing_reconciliation_required");
            sync_result.stats.sync_token_blocked_source = Some("icloud");
            sync_result.stats.sync_token_blocked_explanation = Some(
                "indexing results are provisional; a FINISHED full reconciliation is required",
            );
            db_sync_token_advance_safe = false;
            path_reconciliation_complete = false;
        }
        if indexing_hold_required && !indexing_reconciliation {
            record_selected_hold(
                &mut library_holds,
                &mut cycle_stats,
                &lib_state.zone_name,
                library_ordinal,
                LibraryHoldKind::IndexingReconciliation,
                "indexing_reconciliation_required",
                "a selected library's indexing readiness regressed during enumeration; deletions and checkpoint promotion are disabled",
                true,
            );
            db_sync_token_advance_safe = false;
        }
        // Retry-only work cannot discharge either phase: new assets may be
        // skipped before they acquire durable retry rows. Keep their cursor.
        let recovery_retry_only = (indexing_reconciliation
            || indexing_replay_pending
            || indexing_materialization_pending)
            && (is_retry_failed || download_config.retry_only);
        if recovery_retry_only
            || (indexing_reconciliation
                && (!sync_result.full_enumeration_ran
                    || !library_completed_without_errors
                    || sync_result.stats.failed > 0
                    || sync_result.stats.exif_failures > 0))
        {
            sync_result.checkpoint.sync_token_blocked = true;
            sync_result.checkpoint.project(&mut sync_result.stats);
            if recovery_retry_only && !indexing_reconciliation {
                sync_result.stats.sync_token_blocked_reason = Some("indexing_replay_required");
                sync_result.stats.sync_token_blocked_source = Some("kei");
                sync_result.stats.sync_token_blocked_explanation =
                    Some("retry-only processing cannot discharge retained-cursor replay");
            }
        }

        // An inventory-only pass queued nothing. Unlike ordinary durable
        // transfer retries, managed recovery cannot release its full-work debt
        // until every selected publication and its retained bridge succeeds.
        if indexing_materialization_pending
            && !indexing_reconciliation
            && (!library_completed_without_errors
                || !sync_result.full_enumeration_ran
                || sync_result.stats.failed > 0
                || sync_result.stats.exif_failures > 0
                || download_config.recent.is_some()
                || download_config.skip_created_before.is_some()
                || download_config.skip_created_after.is_some()
                || (retained_cursor_at_start
                    .as_ref()
                    .is_some_and(|s| !s.trim().is_empty())
                    && !matches!(checkpoint_basis, CheckpointBasis::InventoryWithDeltaBridge)))
        {
            sync_result.checkpoint.sync_token_blocked = true;
            sync_result.checkpoint.project(&mut sync_result.stats);
        }

        // Provider cursor safety is independent from transfer completion.
        // The download pipeline persists every planned work item before this
        // boundary, so failed transfers may advance while state, identity,
        // enumeration, auth, and token-proof failures preserve the old cursor.
        let mut checkpoint_decision =
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

        // Only the typed rejection of this cycle's active main cursor can
        // establish this hold. A failed historical sparse validation cannot.
        // The replacement origin is not completion until its own bridge and
        // checkpoint proof succeed. Bind the hold to the original cursor, not
        // the fresh boundary used for the replacement inventory.
        if retained_main_rejected
            && let SourceCheckpointDecision::Preserve { reason, recovery } =
                &mut checkpoint_decision
            && let (Some(db), Some(prior)) = (state_db, retained_cursor_at_start.as_deref())
        {
            let mut hold = RetainedCheckpointHold::new(
                prior,
                &enum_config_hash,
                preservation_config_hash.as_deref(),
            );
            hold.attempt_count = retained_checkpoint_attempt;
            let expected = zone_metadata_fence
                .iter()
                .cloned()
                .chain(path_metadata_at_start.iter().cloned())
                .collect();
            if let Err(error) =
                persist_retained_checkpoint_hold(db, hold_key.clone(), &hold, expected).await
            {
                tracing::warn!(error = %error, "Failed to retain rejected main checkpoint hold");
                sync_result.checkpoint.state_write_failures = sync_result
                    .checkpoint
                    .state_write_failures
                    .saturating_add(1);
                sync_result.stats.state_write_failures =
                    sync_result.stats.state_write_failures.saturating_add(1);
                operational_state_fault = true;
                if !matches!(
                    reason,
                    CheckpointHoldReason::SessionExpired | CheckpointHoldReason::Interrupted
                ) {
                    *reason = CheckpointHoldReason::StateNotDurable;
                    *recovery = RecoveryAction::ReplayFromPriorToken;
                }
            } else {
                if *reason == CheckpointHoldReason::TokenProofIncomplete {
                    *recovery = RecoveryAction::AwaitRetainedCheckpointEvidence;
                }
                record_selected_hold(
                    &mut library_holds,
                    &mut cycle_stats,
                    &lib_state.zone_name,
                    library_ordinal,
                    LibraryHoldKind::RetainedCheckpoint,
                    "retained_checkpoint_expired",
                    download::sync_token_blocked_explanation("retained_checkpoint_expired"),
                    true,
                );
            }
        }

        if sync_result.stats.sync_token_blocked_reason == Some("retained_checkpoint_expired")
            && let SourceCheckpointDecision::Preserve { reason, .. } = checkpoint_decision
        {
            checkpoint_decision = SourceCheckpointDecision::Preserve {
                reason,
                recovery: RecoveryAction::AwaitRetainedCheckpointEvidence,
            };
        }

        if indexing_reconciliation {
            let mut qualified = false;
            if let Some(reservation) = qualification_reservation.take()
                && let Some(db) = state_db
            {
                let auth_or_cancel = matches!(
                    sync_result.outcome,
                    download::DownloadOutcome::SessionExpired { .. }
                ) || sync_result.stats.interrupted
                    || shutdown_token.is_cancelled();
                let qualification_reason = if auth_or_cancel {
                    None
                } else if lib_state.plan_is_stale {
                    Some(state::QualificationFailure::StalePlan)
                } else if !indexing_finished || indexing_hold_required {
                    Some(state::QualificationFailure::ReadinessRegressed)
                } else if !sync_result.full_enumeration_ran {
                    Some(state::QualificationFailure::NotFullEnumeration)
                } else if sync_result.checkpoint.enumeration_incomplete
                    || sync_result.checkpoint.enumeration_errors > 0
                {
                    Some(state::QualificationFailure::EnumerationIncomplete)
                } else if sync_result.stats.failed > 0 {
                    Some(state::QualificationFailure::FailedTransfers)
                } else if sync_result.stats.exif_failures > 0
                    || sync_result.stats.metadata_capture_failures > 0
                {
                    Some(state::QualificationFailure::MetadataFailures)
                } else if !library_completed_without_errors
                    || sync_result.checkpoint.state_write_failures > 0
                    || !checkpoint_transition_state_safe
                    || !matches!(
                        checkpoint_decision,
                        SourceCheckpointDecision::Advance { .. }
                    )
                {
                    Some(state::QualificationFailure::ProofIncomplete)
                } else {
                    None
                };
                let finish = if auth_or_cancel {
                    db.finish_indexing_qualification_auth_or_cancel(
                        reservation,
                        config.watch.interval,
                    )
                    .await
                    .map(|_| ())
                } else if let Some(reason) = qualification_reason {
                    db.finish_indexing_qualification_nonqualified(reservation, reason)
                        .await
                        .map(|_| ())
                } else {
                    match db.commit_indexing_qualification(reservation).await {
                        Ok(()) => {
                            qualified = true;
                            Ok(())
                        }
                        Err(error) => {
                            tracing::warn!(
                                error = %error,
                                "Failed to commit indexing qualification; retaining the durable reservation guard"
                            );
                            Err(error)
                        }
                    }
                };
                if let Err(error) = finish {
                    tracing::warn!(
                        error = %error,
                        "Indexing qualification finish failed; the committed reservation guard remains"
                    );
                    sync_result.checkpoint.state_write_failures = sync_result
                        .checkpoint
                        .state_write_failures
                        .saturating_add(1);
                    sync_result.stats.state_write_failures =
                        sync_result.stats.state_write_failures.saturating_add(1);
                    cycle_failed_count = cycle_failed_count.saturating_add(1);
                }
            } else if matches!(
                checkpoint_decision,
                SourceCheckpointDecision::Advance { .. }
            ) && !is_retry_failed
                && !download_config.retry_only
                && !indexing_hold_required
                && indexing_finished
                && sync_result.full_enumeration_ran
                && library_completed_without_errors
                && let Some(db) = state_db
            {
                // Existing complete-epoch indexing qualification keeps its
                // ordinary path. Pending ExistingUpgrade records only use the
                // reservation owner transaction above.
                let obligations = vec![(replay_key.clone(), "1".to_owned())];
                match db
                    .commit_checkpoint_transition(state::CheckpointTransition {
                        expected_metadata: zone_metadata_fence.clone(),
                        legacy_preservation_proofs: Vec::new(),
                        legacy_config_hash: None,
                        sparse_identity_proofs: Vec::new(),
                        metadata_updates: obligations,
                        metadata_deletes: vec![indexing_key.clone()],
                    })
                    .await
                {
                    Ok(()) => qualified = true,
                    Err(error) => {
                        tracing::warn!(error = %error, "Failed to qualify indexing inventory; retaining recovery hold");
                        sync_result.checkpoint.state_write_failures += 1;
                        sync_result.stats.state_write_failures += 1;
                    }
                }
            }
            if matches!(
                checkpoint_decision,
                SourceCheckpointDecision::Advance { .. }
            ) {
                checkpoint_decision = SourceCheckpointDecision::Preserve {
                    reason: CheckpointHoldReason::TokenProofIncomplete,
                    recovery: RecoveryAction::ReplayFromPriorToken,
                };
            }
            sync_result.checkpoint.sync_token_blocked = true;
            sync_result.checkpoint.project(&mut sync_result.stats);
            sync_result.stats.sync_token_blocked_reason = Some(if qualified {
                "indexing_inventory_qualified"
            } else {
                "indexing_reconciliation_required"
            });
            sync_result.stats.sync_token_blocked_source = Some("kei");
            sync_result.stats.sync_token_blocked_explanation = Some(if qualified {
                "full inventory qualified; the old trusted cursor is retained for the next incremental cycle"
            } else {
                "a complete non-retry inventory from a FINISHED plan is still required"
            });
            db_sync_token_advance_safe = false;
            path_reconciliation_complete = false;
            path_reconciliation_cycle_safe = false;
            let reason = sync_result
                .stats
                .sync_token_blocked_reason
                .unwrap_or("indexing_reconciliation_required");
            let explanation = sync_result
                .stats
                .sync_token_blocked_explanation
                .unwrap_or("a selected library is waiting for complete indexing and a valid baseline; deletions and checkpoint promotion are disabled");
            record_selected_hold(
                &mut library_holds,
                &mut cycle_stats,
                &lib_state.zone_name,
                library_ordinal,
                LibraryHoldKind::IndexingReconciliation,
                reason,
                explanation,
                true,
            );
        }

        let legacy_proofs = Vec::new();

        path_reconciliation_cycle_safe &= library_completed_without_errors
            && checkpoint_transition_state_safe
            && !retained_checkpoint_expired
            && (matches!(
                checkpoint_decision,
                SourceCheckpointDecision::Advance { .. }
            ) || (matches!(
                checkpoint_decision,
                SourceCheckpointDecision::Preserve {
                    reason: CheckpointHoldReason::TokenProofIncomplete,
                    ..
                }
            ) && sync_result.checkpoint.bounded_path_selection_complete
                && !sync_result.checkpoint.enumeration_incomplete
                && sync_result.checkpoint.state_write_failures == 0));

        let mut checkpoint_evidence = checkpoint_support_fields(&checkpoint_decision);
        let mut checkpoint_persistence = "no_state_db";
        let mut advance_write_failed = false;
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
                        sparse_identity_proofs.extend(
                            sync_result
                                .checkpoint
                                .sparse_identity_proofs
                                .iter()
                                .cloned(),
                        );
                        let candidate_key =
                            pending_zone_token_key(&enum_config_hash, &lib_state.zone_name);
                        if let Err(e) = db
                            .commit_observation_transition(
                                state::CheckpointTransition {
                                    expected_metadata: zone_metadata_fence
                                        .iter()
                                        .cloned()
                                        .chain(path_metadata_at_start.iter().cloned())
                                        .collect(),
                                    legacy_preservation_proofs: legacy_proofs,
                                    legacy_config_hash: preservation_config_hash.clone(),
                                    sparse_identity_proofs: sync_result
                                        .checkpoint
                                        .sparse_identity_proofs
                                        .clone(),
                                    metadata_updates: vec![(candidate_key, token)],
                                    metadata_deletes: vec![
                                        retained_checkpoint_hold_key(&lib_state.zone_name),
                                        baseline_key.clone(),
                                    ],
                                },
                                sync_result
                                    .checkpoint
                                    .observation
                                    .clone()
                                    .map(|proof| proof.with_epoch_attempt(epoch_attempt.clone())),
                                false,
                            )
                            .await
                        {
                            advance_write_failed = true;
                            checkpoint_hold_action =
                                Some(RecoveryAction::ReplayFromPriorToken.as_str());
                            db_sync_token_advance_safe = false;
                            path_reconciliation_cycle_safe = false;
                            tracing::warn!(
                                zone = %lib_state.zone_name,
                                error = %e,
                                "Failed to retain completed zone reconciliation checkpoint"
                            );
                            checkpoint_persistence = "failed";
                        } else {
                            checkpoint_persistence = "staged_reconciliation";
                        }
                    } else {
                        let mut metadata_updates =
                            vec![(lib_state.sync_token_key.clone(), token.clone())];
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
                        let bootstrapping = bootstrap_candidate.is_some();
                        let transition = state::CheckpointTransition {
                            expected_metadata: zone_metadata_fence
                                .iter()
                                .cloned()
                                .chain(path_metadata_at_start.iter().cloned())
                                .collect(),
                            legacy_preservation_proofs: legacy_proofs,
                            legacy_config_hash: preservation_config_hash.clone(),
                            sparse_identity_proofs: sync_result
                                .checkpoint
                                .sparse_identity_proofs
                                .clone(),
                            metadata_updates,
                            metadata_deletes: [(!is_current_zone_checkpoint(
                                &lib_state.sync_token_key,
                            ))
                            .then(|| state::unresolved_identity_key(&lib_state.zone_name))]
                            .into_iter()
                            .flatten()
                            // The atomic catalog owner replaces this key after
                            // validating its pre-stream value.
                            .chain((!bootstrapping).then(|| baseline_key.clone()))
                            .chain(vec![
                                retained_checkpoint_hold_key(&lib_state.zone_name),
                                indexing_key.clone(),
                                replay_key.clone(),
                                materialization_key.clone(),
                            ])
                            .collect(),
                        };
                        let committed = if let Some(candidate) = bootstrap_candidate {
                            let (capture, mut request) = *candidate;
                            request.fresh_cursor = token;
                            match sync_result.checkpoint.observation.clone() {
                                Some(proof) if sync_result.checkpoint.completed_delta_replay => {
                                    let proof = proof
                                        .with_epoch_attempt(Some(request.expected_attempt.clone()));
                                    capture
                                        .db
                                        .install_catalog_origin_after_delta(
                                            capture.owner,
                                            request,
                                            transition,
                                            proof,
                                        )
                                        .await
                                        .map(|_| ())
                                }
                                _ => Err(state::error::StateError::Invariant {
                                    operation: "catalog_bootstrap",
                                    detail: "completed delta observation is unavailable".into(),
                                }),
                            }
                        } else {
                            db.commit_observation_transition(
                                transition,
                                sync_result
                                    .checkpoint
                                    .observation
                                    .clone()
                                    .map(|proof| proof.with_epoch_attempt(epoch_attempt.clone())),
                                false,
                            )
                            .await
                        };
                        if let Err(e) = committed {
                            advance_write_failed = true;
                            checkpoint_hold_action =
                                Some(RecoveryAction::ReplayFromPriorToken.as_str());
                            db_sync_token_advance_safe = false;
                            path_reconciliation_cycle_safe = false;
                            tracing::warn!(error = %e, "Failed to store provider checkpoint");
                            checkpoint_persistence = "failed";
                        } else if bootstrapping {
                            checkpoint_persistence = "catalog_bootstrap_pending";
                            db_sync_token_advance_safe = false;
                            path_reconciliation_complete = false;
                            path_reconciliation_cycle_safe = false;
                            record_selected_hold(
                                &mut library_holds,
                                &mut cycle_stats,
                                &lib_state.zone_name,
                                library_ordinal,
                                LibraryHoldKind::BaselinePending,
                                "catalog_bootstrap_pending",
                                "the observed checkpoint is retained while its catalog inventory and bridge complete",
                                true,
                            );
                        } else {
                            checkpoint_persistence = "stored";
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
                            expected_metadata: Vec::new(),
                            legacy_preservation_proofs: Vec::new(),
                            legacy_config_hash: None,
                            sparse_identity_proofs: Vec::new(),
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
                    path_reconciliation_cycle_safe = false;
                    tracing::debug!(error = %e, "Failed to persist checkpoint hold status");
                    checkpoint_persistence = "failed";
                } else if state_db.is_some() {
                    checkpoint_persistence = "stored_hold";
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

        // A rejected main cursor is not repaired when the fresh proof cannot
        // commit. The failed checkpoint transaction has rolled back, so the
        // same captured cursor, baseline and policy fences still apply.
        if retained_main_rejected
            && advance_write_failed
            && let (Some(db), Some(prior)) = (state_db, retained_cursor_at_start.as_deref())
        {
            let mut hold = RetainedCheckpointHold::new(
                prior,
                &enum_config_hash,
                preservation_config_hash.as_deref(),
            );
            hold.attempt_count = retained_checkpoint_attempt;
            let expected = zone_metadata_fence
                .iter()
                .cloned()
                .chain(path_metadata_at_start.iter().cloned())
                .collect();
            if let Err(error) =
                persist_retained_checkpoint_hold(db, hold_key.clone(), &hold, expected).await
            {
                tracing::warn!(error = %error, "Failed to retain rejected main checkpoint hold after checkpoint failure");
                operational_state_fault = true;
            }
        }

        if checkpoint_persistence == "failed" {
            // A failed current checkpoint transaction is an operational error,
            // even when every observed media item was already settled.
            cycle_failed_count = cycle_failed_count.saturating_add(1);
            sync_result.checkpoint.state_write_failures = sync_result
                .checkpoint
                .state_write_failures
                .saturating_add(1);
            sync_result.stats.state_write_failures =
                sync_result.stats.state_write_failures.saturating_add(1);
        }
        let blocked_non_sparse = sync_result
            .stats
            .sync_token_blocked_reason
            .is_some_and(|reason| reason != "asset_delta_hydration_incomplete");
        let genuine_error = matches!(
            sync_result.outcome,
            download::DownloadOutcome::SessionExpired { .. }
                | download::DownloadOutcome::PartialFailure { .. }
        ) || sync_result.checkpoint.enumeration_errors > 0
            || sync_result.checkpoint.state_write_failures > 0
            || sync_result.checkpoint.retained_token_rejected
            || blocked_non_sparse
            || checkpoint_persistence == "failed";
        if let Some(source) = sources
            .iter_mut()
            .find(|source| source.zone_name == lib_state.zone_name)
        {
            source.reached = true;
            source.ordinary_committed = checkpoint_persistence == "stored";
            source.genuine_error =
                genuine_error || source.baseline_scope != lib_state.library.baseline_scope();
            source.media_failures = sync_result.checkpoint.media_failures.clone();
            source.media_failure_unaccounted = sync_result.checkpoint.media_failure_unaccounted;
            source.operational_other_error = matches!(
                sync_result.outcome,
                download::DownloadOutcome::SessionExpired { .. }
            ) || sync_result.checkpoint.enumeration_errors > 0
                || sync_result.checkpoint.state_write_failures > 0
                || sync_result.checkpoint.retained_token_rejected
                || blocked_non_sparse
                || checkpoint_persistence == "failed";
            source.sparse = match (&expected_epoch, &expected_attempt) {
                (Some(epoch), Some(attempt)) => {
                    let mut witness = sync_result.checkpoint.sparse_current.clone();
                    witness.bind_epoch(epoch, attempt);
                    witness.bind_baseline_scope(&lib_state.library.baseline_scope());
                    if let Some(projection) = lib_state.library.owned_provider_scope_projection() {
                        witness.bind_provider_scope_projection(projection);
                    }
                    Some(SparseHeldWitness {
                        witness,
                        completed_delta_replay: sync_result.checkpoint.completed_delta_replay,
                    })
                }
                _ => None,
            };
        }
        if let Some(fields) = checkpoint_evidence.as_object_mut() {
            fields.insert(
                "persistence".into(),
                serde_json::json!(checkpoint_persistence),
            );
            fields.insert(
                "token_present".into(),
                serde_json::json!(sync_result.sync_token.is_some()),
            );
            fields.insert(
                "full_enumeration".into(),
                serde_json::json!(sync_result.full_enumeration_ran),
            );
            fields.insert(
                "identity_incomplete".into(),
                serde_json::json!(sync_result.checkpoint.identity_incomplete),
            );
            fields.insert(
                "state_write_failures".into(),
                serde_json::json!(sync_result.checkpoint.state_write_failures),
            );
            fields.insert(
                "enumeration_errors".into(),
                serde_json::json!(sync_result.checkpoint.enumeration_errors),
            );
            if checkpoint_persistence == "failed" {
                fields.insert(
                    "recovery".into(),
                    serde_json::json!("replay_from_prior_token"),
                );
            }
        }
        crate::support::observe("support_checkpoint_v1", checkpoint_evidence);
        observe_sync_component("combined", &sync_result, lib_state.plan.passes.len());
        if sync_result.stats.sync_token_blocked
            && sync_result.stats.sync_token_blocked_zone.is_none()
        {
            sync_result.stats.sync_token_blocked_zone = Some(lib_state.zone_name.clone());
        }

        if sync_result.stats.sync_token_blocked_reason == Some("retained_checkpoint_expired") {
            record_selected_hold(
                &mut library_holds,
                &mut cycle_stats,
                &lib_state.zone_name,
                library_ordinal,
                LibraryHoldKind::RetainedCheckpoint,
                "retained_checkpoint_expired",
                download::sync_token_blocked_explanation("retained_checkpoint_expired"),
                true,
            );
        }

        if sync_result.checkpoint.identity_incomplete {
            // The checkpoint owner retains unresolved current identity; report
            // it as a hold label instead of fabricating a failed-asset count.
            record_selected_hold(
                &mut library_holds,
                &mut cycle_stats,
                &lib_state.zone_name,
                library_ordinal,
                LibraryHoldKind::IdentityUnresolved,
                "asset_delta_hydration_incomplete",
                download::sync_token_blocked_explanation("asset_delta_hydration_incomplete"),
                true,
            );
        }

        if sync_result.stats.unattributed_legacy_assets > 0
            && let Some(db) = state_db
            && let Some(capture) = db
                .get_summary()
                .await?
                .metadata_capture
                .into_iter()
                .find(|capture| capture.library == lib_state.zone_name)
        {
            sync_result.stats.unattributed_legacy_assets = capture.unattributed_legacy_assets;
            sync_result.stats.unattributed_legacy_pending = capture.unattributed_legacy_pending;
        }

        if sync_result.stats.unattributed_legacy_pending > 0 {
            record_selected_hold(
                &mut library_holds,
                &mut cycle_stats,
                &lib_state.zone_name,
                library_ordinal,
                LibraryHoldKind::PreservationPending,
                "legacy_preservation_pending",
                "legacy current-work certification remains incomplete",
                false,
            );
        }

        // Accumulate stats across libraries.
        cycle_stats.accumulate(&sync_result.stats);

        if shutdown_token.is_cancelled()
            && !qualification_owned
            && let Some(db) = state_db
            && !config.runtime.dry_run
        {
            db.recover_sync_epoch_attempt(
                lib_state.zone_name.clone(),
                false,
                config.watch.interval,
            )
            .await?;
        }
        match sync_result.outcome {
            download::DownloadOutcome::Success => {}
            download::DownloadOutcome::SessionExpired { auth_error_count } => {
                tracing::warn!(
                    auth_error_count,
                    zone = %lib_state.zone_name,
                    "Session expired during library sync"
                );
                if let Some(db) = state_db
                    && !config.runtime.dry_run
                    && !qualification_owned
                {
                    db.recover_sync_epoch_attempt(
                        lib_state.zone_name.clone(),
                        false,
                        config.watch.interval,
                    )
                    .await?;
                }
                cycle_session_expired = true;
                break; // Stop iterating libraries -- need re-auth
            }
            download::DownloadOutcome::PartialFailure { failed_count } => {
                if !qualification_owned
                    && let Some(db) = state_db
                    && !config.runtime.dry_run
                {
                    if sync_result.stats.state_write_failures > 0 {
                        db.defer_sync_epoch(
                            lib_state.zone_name.clone(),
                            config.watch.interval,
                            "state_write_failure",
                        )
                        .await?;
                    } else {
                        db.recover_sync_epoch_attempt(
                            lib_state.zone_name.clone(),
                            !shutdown_token.is_cancelled(),
                            config.watch.interval,
                        )
                        .await?;
                    }
                }
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
                    path_reconciliation_cycle_safe = false;
                    tracing::warn!(
                        zone = %lib_state.zone_name,
                        "Config reconciliation has no completed checkpoint for selected zone"
                    );
                    break;
                };
                active_cursor_fences.push((candidate_key.clone(), Some(token.clone())));
                metadata_updates.push((lib_state.sync_token_key.clone(), token));
                metadata_deletes.push(candidate_key);
                if !is_current_zone_checkpoint(&lib_state.sync_token_key) {
                    metadata_deletes.push(state::unresolved_identity_key(&lib_state.zone_name));
                }
                metadata_deletes.push(indexing_reconciliation_key(&lib_state.zone_name));
                metadata_deletes.push(indexing_replay_key(&lib_state.zone_name));
                metadata_deletes.push(indexing_materialization_key(&lib_state.zone_name));
            }
            if db_sync_token_advance_safe {
                metadata_updates.push((ENUM_CONFIG_HASH_KEY.to_owned(), enum_config_hash));
                metadata_deletes.push(PENDING_ENUM_CONFIG_HASH_KEY.to_owned());
            }
        }
        if db_sync_token_advance_safe {
            metadata_updates.push((LAST_CHECKPOINT_STATUS_KEY.to_owned(), "current".to_owned()));
            metadata_updates.push((LAST_RECOVERY_ACTION_KEY.to_owned(), "none".to_owned()));
            let mut expected_metadata = enum_metadata_at_start
                .iter()
                .cloned()
                .chain(active_cursor_fences)
                .chain(path_metadata_at_start.iter().cloned())
                .collect::<Vec<_>>();
            for lib_state in library_states
                .iter()
                .copied()
                .filter(|state| has_active_passes(state))
            {
                if let Some(evidence) = db
                    .sync_epoch_baseline_evidence(lib_state.zone_name.clone())
                    .await?
                {
                    expected_metadata.push(evidence);
                }
            }
            if let Err(e) = db
                .commit_observation_transition(
                    state::CheckpointTransition {
                        expected_metadata,
                        legacy_preservation_proofs: Vec::new(),
                        legacy_config_hash: preservation_config_hash.clone(),
                        sparse_identity_proofs,
                        metadata_updates,
                        metadata_deletes,
                    },
                    None,
                    true,
                )
                .await
            {
                db_sync_token_advance_safe = false;
                path_reconciliation_cycle_safe = false;
                operational_state_fault = true;
                tracing::warn!(error = %e, "Failed to atomically promote checkpoint reconciliation");
            }
        }
    }

    if path_reconciliation_complete
        && (!path_reconciliation_has_smart_folders
            || (cycle_failed_count == 0 && path_reconciliation_cycle_safe && !cycle_has_stale_plan))
        && !cycle_session_expired
        && !shutdown_token.is_cancelled()
        && let (Some(db), Some(download_config_hash)) = (state_db, pending_download_config_hash)
        && let Err(e) = db
            .commit_checkpoint_transition(state::CheckpointTransition {
                expected_metadata: path_metadata_at_start,
                legacy_preservation_proofs: Vec::new(),
                legacy_config_hash: None,
                sparse_identity_proofs: Vec::new(),
                metadata_updates: vec![(
                    download::DOWNLOAD_CONFIG_HASH_KEY.to_owned(),
                    download_config_hash,
                )],
                metadata_deletes: vec![PENDING_DOWNLOAD_CONFIG_HASH_KEY.to_owned()],
            })
            .await
    {
        operational_state_fault = true;
        tracing::warn!(error = %e, "Failed to promote completed local path reconciliation");
    }

    let mut retained_recovery_summary = false;
    if let Some(db) = state_db {
        let summary = db.get_summary().await?;
        let mut current_identity_hold = false;
        for (library_ordinal, library) in library_states
            .iter()
            .copied()
            .enumerate()
            .filter(|(_, library)| has_active_passes(library))
        {
            let held = sparse_identity_holds_current(
                db,
                &library.zone_name,
                is_current_zone_checkpoint(&library.sync_token_key),
            )
            .await?;
            if held {
                record_selected_hold(
                    &mut library_holds,
                    &mut cycle_stats,
                    &library.zone_name,
                    library_ordinal,
                    LibraryHoldKind::IdentityUnresolved,
                    "asset_delta_hydration_incomplete",
                    download::sync_token_blocked_explanation("asset_delta_hydration_incomplete"),
                    true,
                );
            }
            current_identity_hold |= held;
        }
        if current_identity_hold {
            download::block_sync_token_for_unresolved_identity(&mut cycle_stats);
            db_sync_token_advance_safe = false;
        }
        if summary
            .last_recovery_action
            .as_deref()
            .is_some_and(|action| {
                matches!(
                    action,
                    "await_retained_checkpoint_evidence"
                        | "retained_checkpoint_retry_exhausted"
                        | "repair_retained_checkpoint_evidence"
                )
            })
        {
            // This database-wide recovery action has no source identity. It
            // may hold the aggregate checkpoint, but cannot replace a
            // source's diagnostic or manufacture a per-library hold.
            retained_recovery_summary = true;
            cycle_stats.sync_token_blocked = true;
            db_sync_token_advance_safe = false;
        }
    }

    let operational_candidate_eligible = !config.runtime.dry_run
        && !config.runtime.only_print_filenames
        && !is_retry_failed
        && state_db.is_some()
        && !cycle_session_expired
        && !cycle_stats.interrupted
        && !cycle_has_stale_plan
        && !operational_state_fault
        && !retained_recovery_summary
        && cycle_stats.exif_failures == 0
        && cycle_stats.state_write_failures == 0
        && cycle_stats.enumeration_errors == 0
        && !cycle_stats.enumeration_incomplete
        && cycle_stats.count_probe_failures == 0;
    for source in &mut sources {
        source.operational_eligible = operational_candidate_eligible;
    }

    let operational_safe_holds = {
        let eligible = !config.runtime.dry_run
            && !config.runtime.only_print_filenames
            && !is_retry_failed
            && state_db.is_some()
            && !cycle_session_expired
            && !cycle_stats.interrupted
            && !cycle_has_stale_plan
            && !operational_state_fault
            && !retained_recovery_summary;
        let clean_aggregate = cycle_stats.failed == 0
            && cycle_stats.exif_failures == 0
            && cycle_stats.state_write_failures == 0
            && cycle_stats.enumeration_errors == 0
            && !cycle_stats.enumeration_incomplete
            && cycle_stats.count_probe_failures == 0;
        if !(eligible && clean_aggregate) {
            false
        } else {
            let mut safe = true;
            for expected in expected_sources
                .iter()
                .filter(|source| source.active_passes)
            {
                let Some(source) = sources
                    .iter()
                    .find(|source| source.zone_name == expected.zone_name)
                else {
                    safe = false;
                    break;
                };
                if source.baseline_scope != expected.baseline_scope {
                    safe = false;
                    break;
                }
                if expected.skip_authority == SkipAuthority::NoChanges {
                    continue;
                }
                if !source.reached || source.genuine_error {
                    safe = false;
                    break;
                }
                let mut has_identity_hold = false;
                let mut has_other_hold = false;
                for hold in library_holds
                    .iter()
                    .filter(|hold| hold.source == source.zone_name)
                {
                    if hold.kind == LibraryHoldKind::IdentityUnresolved {
                        has_identity_hold = true;
                    } else {
                        has_other_hold = true;
                    }
                }
                if has_other_hold {
                    safe = false;
                    break;
                }
                if !has_identity_hold {
                    if !source.ordinary_committed {
                        safe = false;
                        break;
                    }
                    continue;
                }
                let Some(held) = &source.sparse else {
                    safe = false;
                    break;
                };
                if held.witness.baseline_scope() != Some(expected.baseline_scope.as_str()) {
                    safe = false;
                    break;
                }
                if !held.completed_delta_replay {
                    safe = false;
                    break;
                }
                let Some(db) = state_db else {
                    safe = false;
                    break;
                };
                let attest = db.attest_current_sparse(&held.witness).await;
                if attest.is_err() {
                    safe = false;
                    break;
                }
            }
            safe
        }
    };

    #[cfg(all(test, target_os = "linux"))]
    crate::test_helpers::process_death_point("checkpoint-persisted");

    Ok(CycleResult {
        failed_count: cycle_failed_count,
        session_expired: cycle_session_expired,
        stats: cycle_stats,
        db_sync_token_advance_safe,
        holds: library_holds,
        sources,
        operational_safe_holds,
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

/// Keep strict-policy hashes byte-compatible with existing preservation proofs.
/// The opt-in affects only legacy evidence qualification, not download provenance.
pub(crate) fn hash_legacy_preservation_policy(
    policy: &serde_json::Value,
    allow_hardlinks: bool,
) -> String {
    use sha2::{Digest, Sha256};
    let mut hash = Sha256::new();
    hash.update(policy.to_string().as_bytes());
    if allow_hardlinks {
        hash.update(b"\0legacy-preservation-allow-hardlinks:v1");
    }
    data_encoding::HEXLOWER.encode(&hash.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::PassKind;

    #[test]
    fn support_component_totals_keep_inventory_and_bridge_separate() {
        use tracing_subscriber::layer::SubscriberExt;
        let (layer, drain) = crate::support::test_capture::test_layer();
        tracing::subscriber::with_default(tracing_subscriber::registry().with(layer), || {
            let mut inventory = download::SyncResult::from_execution(
                download::DownloadOutcome::Success,
                Some("PRIVATE_TOKEN".into()),
                download::SyncStats {
                    assets_seen: 3899,
                    api_total_at_start: Some(3899),
                    ..Default::default()
                },
            );
            inventory.full_enumeration_ran = true;
            let bridge = download::SyncResult::from_execution(
                download::DownloadOutcome::Success,
                Some("PRIVATE_SUCCESSOR".into()),
                download::SyncStats {
                    assets_seen: 238,
                    api_total_at_start: Some(238),
                    ..Default::default()
                },
            );
            observe_sync_component("initial", &inventory, 1);
            observe_sync_component("delta_bridge", &bridge, 1);
            inventory.accumulate(&bridge);
            observe_sync_component("combined", &inventory, 1);
        });
        let evidence = drain();
        assert_eq!(evidence.len(), 3);
        assert_eq!(evidence[0]["fields"]["assets_seen"], 3899);
        assert_eq!(evidence[1]["fields"]["assets_seen"], 238);
        assert_eq!(evidence[2]["fields"]["assets_seen"], 4137);
        assert_eq!(evidence[0]["fields"]["component"], "initial");
        assert!(evidence[2]["fields"]["inventory_complete"].is_null());
        assert!(
            !serde_json::to_string(&evidence)
                .unwrap()
                .contains("PRIVATE")
        );
    }

    #[test]
    fn retained_checkpoint_hold_round_trip_and_version_one_migration() {
        let hold = RetainedCheckpointHold::new(
            "private-cursor",
            "enum-policy",
            Some("preservation-policy"),
        );
        let encoded = serde_json::to_string(&hold).expect("encode receipt");
        assert!(!encoded.contains("private-cursor"));
        let decoded: RetainedCheckpointHold =
            serde_json::from_str(&encoded).expect("decode receipt");
        assert!(decoded.matches(&hold));
        assert_eq!(decoded.attempt_count, 1);
        assert_eq!(decoded.next_attempt_at, hold.next_attempt_at);
        assert_eq!(serde_json::to_string(&decoded).unwrap(), encoded);

        let mut version_one: serde_json::Value = serde_json::from_str(&encoded).unwrap();
        version_one.as_object_mut().unwrap().remove("attempt_count");
        let migrated: RetainedCheckpointHold = serde_json::from_value(version_one).unwrap();
        assert!(migrated.matches(&hold));
        assert_eq!(
            migrated.attempt_count, 1,
            "an existing receipt already consumed its first attempt"
        );
        let migrated_encoded = serde_json::to_string(&migrated).unwrap();
        assert_eq!(
            retained_checkpoint_hold_action(&migrated_encoded),
            "await_retained_checkpoint_evidence"
        );
        let mut exhausted = migrated;
        exhausted.attempt_count = RETAINED_CHECKPOINT_MAX_ATTEMPTS;
        assert_eq!(
            retained_checkpoint_hold_action(&serde_json::to_string(&exhausted).unwrap()),
            "retained_checkpoint_retry_exhausted"
        );
    }

    #[test]
    fn legacy_preservation_trust_hash_preserves_strict_compatibility() {
        let policy = serde_json::json!([1, 1, "coverage", "None", false, false, false]);
        // Historical SHA-256 of the exact serialized strict policy, independently computed.
        let strict = "6f307ea388dd8281b2db2d31c9a8ba37f3c910027bbdb47e95c014477ddf358f";
        assert_eq!(hash_legacy_preservation_policy(&policy, false), strict);
        assert_ne!(hash_legacy_preservation_policy(&policy, true), strict);
        assert_eq!(hash_legacy_preservation_policy(&policy, false), strict);
    }

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
    fn retry_plan_universe_requires_complete_current_sources_and_exact_local_plan() {
        let primary = make_library_state(true);
        let mut peer = make_library_state(true);
        peer.zone_name = "SharedSync-peer".into();
        peer.library = crate::icloud::photos::PhotoLibrary::new_stub_with_zone(
            Box::new(crate::test_helpers::MockPhotosSession::new()),
            "SharedSync-peer",
        );
        peer.plan.passes[0].kind = PassKind::Album;
        let expected = |states: &[&LibraryState]| -> Vec<ExpectedSource> {
            states
                .iter()
                .map(|state| ExpectedSource {
                    zone_name: state.zone_name.clone(),
                    baseline_scope: state.library.baseline_scope(),
                    active_passes: has_active_passes(state),
                    skip_authority: SkipAuthority::NoChanges,
                })
                .collect()
        };
        let sources = expected(&[&primary, &peer]);
        let all = [&primary, &peer];
        let reached = [&primary];
        let plans = current_retry_plan_universe(&all, &reached, &sources).unwrap();
        assert_eq!(
            plans.len(),
            2,
            "watch-omitted peer must remain in the universe"
        );
        assert!(std::ptr::eq(plans[0], primary.plan.passes.as_slice()));
        assert_eq!(plans[1][0].kind, PassKind::Album);
        assert!(current_retry_plan_universe(&reached, &reached, &sources).is_none());
        assert!(
            current_retry_plan_universe(&all, &[&make_library_state(true)], &sources).is_none()
        );
        assert!(
            current_retry_plan_universe(&all, &reached, &[sources[0].clone(), sources[0].clone()])
                .is_none()
        );
        peer.plan_needs_refresh = true;
        assert!(current_retry_plan_universe(&[&primary, &peer], &reached, &sources).is_none());
        peer.plan_needs_refresh = false;
        peer.plan_is_stale = true;
        assert!(current_retry_plan_universe(&[&primary, &peer], &reached, &sources).is_none());
    }

    #[test]
    fn should_warn_zero_assets_requires_active_passes() {
        let sync_result = download::SyncResult {
            full_enumeration_ran: true,
            ..download::SyncResult::from_execution(
                download::DownloadOutcome::Success,
                Some("zone-token".to_string()),
                download::SyncStats {
                    assets_seen: 0,
                    ..download::SyncStats::default()
                },
            )
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
            full_enumeration_ran: true,
            ..download::SyncResult::from_execution(
                download::DownloadOutcome::Success,
                Some("zone-token".to_string()),
                download::SyncStats {
                    assets_seen: 0,
                    ..download::SyncStats::default()
                },
            )
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
        let result = download::SyncResult::from_execution(
            download::DownloadOutcome::PartialFailure { failed_count: 1 },
            Some("zone-token-next".to_string()),
            download::SyncStats {
                failed: 1,
                ..download::SyncStats::default()
            },
        );

        assert_eq!(
            source_checkpoint_decision(&result, false, false, CheckpointBasis::IncrementalDelta,),
            SourceCheckpointDecision::Advance {
                token: "zone-token-next".to_string(),
                basis: CheckpointBasis::IncrementalDelta,
            }
        );
    }

    #[test]
    fn unresolved_identity_hold_survives_report_changes_and_result_composition() {
        let mut result = download::SyncResult::from_execution(
            download::DownloadOutcome::Success,
            Some("zone-token-next".to_string()),
            download::SyncStats::default(),
        );
        result.block_for_unresolved_identity();
        result.stats = download::SyncStats::default();
        result.accumulate(&download::SyncResult::from_execution(
            download::DownloadOutcome::Success,
            None,
            download::SyncStats::default(),
        ));
        assert!(result.checkpoint.identity_incomplete);
        assert!(result.stats.identity_incomplete);
        assert_eq!(
            source_checkpoint_decision(&result, false, false, CheckpointBasis::IncrementalDelta),
            SourceCheckpointDecision::Preserve {
                reason: CheckpointHoldReason::TokenProofIncomplete,
                recovery: RecoveryAction::ReplayFromPriorToken,
            }
        );
    }

    #[test]
    fn failed_retry_state_write_preserves_source_checkpoint() {
        let result = download::SyncResult::from_execution(
            download::DownloadOutcome::PartialFailure { failed_count: 1 },
            Some("zone-token-next".to_string()),
            download::SyncStats {
                failed: 1,
                state_write_failures: 1,
                ..download::SyncStats::default()
            },
        );

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
        let mut result = download::SyncResult {
            full_enumeration_ran: true,
            ..download::SyncResult::from_execution(
                download::DownloadOutcome::Success,
                None,
                download::SyncStats {
                    sync_token_blocked: true,
                    ..download::SyncStats::default()
                },
            )
        };

        result.checkpoint.retry_passes = vec![pass.clone()];
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
        for mask in 0u16..1024 {
            let dry_run = mask & 1 != 0;
            let stale_pass_plan = mask & 2 != 0;
            let session_expired = mask & 4 != 0;
            let interrupted = mask & 8 != 0;
            let enumeration_error = mask & 16 != 0;
            let enumeration_incomplete = mask & 32 != 0;
            let state_not_durable = mask & 64 != 0;
            let token_proof_blocked = mask & 128 != 0;
            let durable_transfer_failure = mask & 256 != 0;
            let identity_incomplete = mask & 512 != 0;
            let has_safety_blocker = mask & 255 != 0 || identity_incomplete;
            for token in tokens {
                let mut result = download::SyncResult::from_execution(
                    if session_expired {
                        download::DownloadOutcome::SessionExpired {
                            auth_error_count: 1,
                        }
                    } else if durable_transfer_failure {
                        download::DownloadOutcome::PartialFailure { failed_count: 1 }
                    } else {
                        download::DownloadOutcome::Success
                    },
                    token.map(str::to_string),
                    download::SyncStats {
                        interrupted,
                        enumeration_errors: usize::from(enumeration_error),
                        enumeration_incomplete,
                        state_write_failures: usize::from(state_not_durable),
                        sync_token_blocked: token_proof_blocked,
                        identity_incomplete,
                        ..download::SyncStats::default()
                    },
                );
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
                // Reporting snapshots cannot remove or introduce safety evidence.
                result.stats = download::SyncStats {
                    state_write_failures: usize::from(!state_not_durable),
                    enumeration_errors: usize::from(!enumeration_error),
                    enumeration_incomplete: !enumeration_incomplete,
                    interrupted: !interrupted,
                    sync_token_blocked: !token_proof_blocked,
                    identity_incomplete: !identity_incomplete,
                    ..download::SyncStats::default()
                };
                let reported_decision = source_checkpoint_decision(
                    &result,
                    dry_run,
                    stale_pass_plan,
                    CheckpointBasis::IncrementalDelta,
                );
                assert_eq!(
                    reported_decision, decision,
                    "report mutation changed mask {mask:#010b}"
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
        let inventory = download::InventoryMeasurement {
            scope: download::InventoryScope("test complete scope".to_owned()),
            total: 80,
            assets_seen: 80,
        };
        let key = inventory_anchor_key("hash-a", "PrimarySync", &inventory.scope);
        let prior = InventoryAnchor {
            version: INVENTORY_ANCHOR_VERSION,
            scope: inventory.scope.clone(),
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
        update_inventory_anchor_for_cycle(&db, "hash-a", "PrimarySync", &inventory, &mut stats)
            .await;

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
        let inventory = download::InventoryMeasurement {
            scope: download::InventoryScope("test complete scope".to_owned()),
            total: 80,
            assets_seen: 80,
        };
        let old_key = inventory_anchor_key("old-hash", "PrimarySync", &inventory.scope);
        let prior = InventoryAnchor {
            version: INVENTORY_ANCHOR_VERSION,
            scope: inventory.scope.clone(),
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
        update_inventory_anchor_for_cycle(&db, "new-hash", "PrimarySync", &inventory, &mut stats)
            .await;

        assert_eq!(stats.inventory_drop_warnings, 0);
        assert!(
            db.get_metadata(&inventory_anchor_key(
                "new-hash",
                "PrimarySync",
                &inventory.scope
            ))
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

    #[tokio::test]
    async fn v27_ignores_old_cursors_and_preserves_them_across_reopen() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("state.sqlite");
        let db = state::SqliteStateDb::open(&path).await.unwrap();
        for (key, value) in [
            ("sync_token:PrimarySync", "old-opaque-zone"),
            ("pending_sync_token:config:PrimarySync", "old-staged"),
            ("retained_checkpoint_hold:PrimarySync", "old-hold"),
            ("indexing_replay_required:PrimarySync", "old-replay"),
        ] {
            db.set_metadata(key, value).await.unwrap();
        }
        let current_key = sync_token_key("PrimarySync");
        assert!(matches!(
            determine_sync_mode(false, 1, Some(&db), &current_key, "PrimarySync").await,
            download::SyncMode::Full
        ));
        assert!(
            db.get_metadata(&pending_zone_token_key("config", "PrimarySync"))
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            db.get_metadata(&retained_checkpoint_hold_key("PrimarySync"))
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            db.get_metadata(&indexing_replay_key("PrimarySync"))
                .await
                .unwrap()
                .is_none()
        );
        db.set_metadata(&current_key, "fresh-opaque-zone")
            .await
            .unwrap();
        drop(db);
        for _ in 0..2 {
            let db = state::SqliteStateDb::open(&path).await.unwrap();
            assert!(matches!(
                determine_sync_mode(false, 1, Some(&db), &current_key, "PrimarySync").await,
                download::SyncMode::Incremental { zone_sync_token } if zone_sync_token == "fresh-opaque-zone"
            ));
            assert_eq!(
                db.get_metadata("sync_token:PrimarySync")
                    .await
                    .unwrap()
                    .as_deref(),
                Some("old-opaque-zone")
            );
        }
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

    #[test]
    fn selected_hold_deduplicates_exact_source_not_display_label() {
        let mut holds = Vec::new();
        let mut stats = download::SyncStats::default();
        record_selected_hold(
            &mut holds,
            &mut stats,
            "SharedSync-A1B2C3D4-EEEE",
            0,
            LibraryHoldKind::BaselinePending,
            "epoch_backoff",
            "deferred baseline",
            true,
        );
        record_selected_hold(
            &mut holds,
            &mut stats,
            "SharedSync-A1B2C3D4-FFFF",
            1,
            LibraryHoldKind::BaselinePending,
            "epoch_backoff",
            "deferred baseline",
            true,
        );
        record_selected_hold(
            &mut holds,
            &mut stats,
            "SharedSync-A1B2C3D4-EEEE",
            0,
            LibraryHoldKind::BaselinePending,
            "epoch_backoff",
            "deferred baseline",
            true,
        );
        assert_eq!(holds.len(), 2, "prefix-colliding sources stay distinct");
        assert_eq!(holds[0].label, holds[1].label);
        assert_ne!(holds[0].source, holds[1].source);
        assert_eq!(holds[0].ordinal, 0);
        assert_eq!(holds[1].ordinal, 1);
        assert!(stats.sync_token_blocked);
        assert_eq!(
            stats.sync_token_blocked_zone.as_deref(),
            Some(holds[0].source.as_str())
        );

        let mut preservation_stats = download::SyncStats::default();
        record_selected_hold(
            &mut holds,
            &mut preservation_stats,
            "SharedSync-A1B2C3D4-EEEE",
            0,
            LibraryHoldKind::PreservationPending,
            "legacy_preservation_pending",
            "pending preservation",
            false,
        );
        assert!(
            !preservation_stats.sync_token_blocked,
            "preservation alone must not project a token block"
        );
        assert!(preservation_stats.sync_token_blocked_zone.is_none());
    }

    #[test]
    fn library_hold_debug_redacts_full_source() {
        let raw_source = "SharedSync-A1B2C3D4-E5F6-7890-secret-provider-zone";
        let hold = LibraryHoldFact {
            label: "SharedSync-A1B2C3D4".to_owned(),
            source: raw_source.to_owned(),
            ordinal: 3,
            kind: LibraryHoldKind::BaselinePending,
            reason: "epoch_backoff",
        };
        let rendered = format!("{hold:?}");
        assert!(!rendered.contains(raw_source), "{rendered}");
        assert!(!rendered.contains("secret-provider-zone"), "{rendered}");
        assert!(rendered.contains("source: \"<redacted>\""), "{rendered}");
        assert!(rendered.contains("SharedSync-A1B2C3D4"), "{rendered}");
        assert!(rendered.contains("ordinal: 3"), "{rendered}");
        assert!(rendered.contains("BaselinePending"), "{rendered}");
        assert!(rendered.contains("epoch_backoff"), "{rendered}");
        assert_eq!(hold.clone(), hold);
    }
}
