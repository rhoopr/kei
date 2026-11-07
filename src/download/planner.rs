//! Asset-to-task planning shared by full, incremental, dry-run, and cleanup
//! paths.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use rustc_hash::{FxHashMap, FxHashSet};

use crate::icloud::photos::PhotoAsset;
use crate::state::db::identity::{LegacyOwnersByLibrary, same_logical_asset};
use crate::state::{
    AssetRecord, DownloadStateStore, MembershipStore, ReconciliationCatalogPath,
    ReconciliationContent, ReconciliationPathKey, ReconciliationReservation, VersionSizeKey,
};

use super::filter::{
    DownloadTask, FilterReason, MalformedTaskResource, NormalizedPath, PathPlanningMode,
    determine_media_type, filter_asset_to_tasks, is_asset_filtered, pre_ensure_asset_dir,
};
use super::paths;
use super::{DownloadConfig, DownloadStore};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ReconciliationOwner {
    library: Arc<str>,
    asset_id: Box<str>,
    version_size: VersionSizeKey,
}

#[derive(Clone, Copy)]
enum ReservedReplay {
    RequestedDirectory,
    PendingRetry,
}

/// Ephemeral producer provenance, never reconstructed from missing evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReservationOrigin {
    OrdinaryCollision,
    PriorPublication,
    ExactReplayCandidate,
}

#[derive(Debug)]
struct PlannedReservation {
    reservation: ReconciliationReservation,
    origin: ReservationOrigin,
}

/// Keep durable path ownership off the stack of the shared async planner.
#[derive(Debug, Default)]
struct ReconciliationPlanning {
    claims: FxHashMap<ReconciliationOwner, FxHashMap<NormalizedPath, u64>>,
    path_owners: FxHashMap<NormalizedPath, FxHashSet<ReconciliationOwner>>,
    destinations:
        FxHashMap<(ReconciliationOwner, ReconciliationContent, NormalizedPath), std::path::PathBuf>,
    path_contents: FxHashMap<NormalizedPath, FxHashSet<Option<ReconciliationContent>>>,
    reservations: Vec<PlannedReservation>,
    durable_destinations: FxHashSet<NormalizedPath>,
    downloaded_paths: FxHashMap<ReconciliationOwner, Vec<crate::state::DownloadedFileRecord>>,
    #[cfg(test)]
    receipt_file_reads: std::sync::atomic::AtomicUsize,
}

/// A checked, transient restriction. It never supplies positive file ownership.
/// Private construction prevents an unchecked context from granting mutation.
#[derive(Debug, Clone, Copy)]
pub(crate) struct OwnershipCheck {
    complete: bool,
}

impl OwnershipCheck {
    pub(crate) async fn inspect<D: crate::state::ReconciliationStateStore + ?Sized>(
        db: &D,
    ) -> Result<Self> {
        let records = db.get_reconciliation_catalog_paths().await?;
        Ok(Self {
            complete: records
                .iter()
                .all(|record| PathPlanningMode::Reconciliation.key(&record.path).is_ok()),
        })
    }

    pub(crate) fn allows_existing_mutation(self) -> bool {
        self.complete
    }
}

/// Mutable path-planning state carried across assets in one pass.
#[derive(Debug)]
pub(super) struct TaskPlanner {
    claimed_paths: FxHashMap<NormalizedPath, u64>,
    path_mode: PathPlanningMode,
    reconciliation: Box<ReconciliationPlanning>,
    dir_cache: paths::DirCache,
    baseline_matcher: super::baseline_matching::BaselineMatcher,
    legacy_owners: LegacyOwnersByLibrary,
    ownership_incomplete: bool,
}

#[cfg(test)]
pub(crate) mod distinct_timing_test_hook {
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex, OnceLock};
    use std::time::Duration;
    use tokio::sync::Notify;

    struct Probe {
        positive: bool,
        published: Notify,
        first_release: Notify,
        receipt: Notify,
        matched: Notify,
        match_release: Notify,
        decision: Mutex<Option<String>>,
        explanation: Mutex<Option<crate::state::db::match_diagnostics::MatchExplanation>>,
        events: Arc<Mutex<Vec<String>>>,
    }
    type Map = Mutex<HashMap<PathBuf, Arc<Probe>>>;
    fn firsts() -> &'static Map {
        static MAP: OnceLock<Map> = OnceLock::new();
        MAP.get_or_init(|| Mutex::new(HashMap::new()))
    }
    fn seconds() -> &'static Map {
        static MAP: OnceLock<Map> = OnceLock::new();
        MAP.get_or_init(|| Mutex::new(HashMap::new()))
    }
    fn retries() -> &'static Map {
        static MAP: OnceLock<Map> = OnceLock::new();
        MAP.get_or_init(|| Mutex::new(HashMap::new()))
    }
    async fn wait(signal: &Notify) {
        tokio::time::timeout(Duration::from_secs(30), signal.notified())
            .await
            .expect("synthetic timing barrier timed out");
    }
    pub(crate) struct Control {
        first: PathBuf,
        second: PathBuf,
        probe: Arc<Probe>,
    }
    impl Control {
        pub(crate) fn install(
            first: PathBuf,
            second: PathBuf,
            events: Arc<Mutex<Vec<String>>>,
        ) -> Self {
            Self::create(first, second, false, events)
        }
        pub(crate) fn install_positive(first: PathBuf, second: PathBuf) -> Self {
            Self::create(first, second, true, Arc::new(Mutex::new(Vec::new())))
        }
        fn create(
            first: PathBuf,
            second: PathBuf,
            positive: bool,
            events: Arc<Mutex<Vec<String>>>,
        ) -> Self {
            let probe = Arc::new(Probe {
                positive,
                published: Notify::new(),
                first_release: Notify::new(),
                receipt: Notify::new(),
                matched: Notify::new(),
                match_release: Notify::new(),
                decision: Mutex::new(None),
                explanation: Mutex::new(None),
                events,
            });
            assert!(
                firsts()
                    .lock()
                    .unwrap()
                    .insert(first.clone(), probe.clone())
                    .is_none()
            );
            assert!(
                seconds()
                    .lock()
                    .unwrap()
                    .insert(second.clone(), probe.clone())
                    .is_none()
            );
            if !positive {
                assert!(
                    retries()
                        .lock()
                        .unwrap()
                        .insert(second.clone(), probe.clone())
                        .is_none()
                );
            }
            Self {
                first,
                second,
                probe,
            }
        }
        pub(crate) async fn matched(
            &self,
        ) -> (
            String,
            crate::state::db::match_diagnostics::MatchExplanation,
        ) {
            wait(&self.probe.matched).await;
            (
                self.probe.decision.lock().unwrap().clone().unwrap(),
                self.probe
                    .explanation
                    .lock()
                    .unwrap()
                    .clone()
                    .expect("uncertain decision explanation"),
            )
        }
        pub(crate) fn release(&self) {
            self.probe.first_release.notify_one();
            self.probe.match_release.notify_one();
            self.probe.receipt.notify_one();
            self.probe.published.notify_one();
        }
    }
    impl Drop for Control {
        fn drop(&mut self) {
            self.release();
            firsts().lock().unwrap().remove(&self.first);
            seconds().lock().unwrap().remove(&self.second);
            retries().lock().unwrap().remove(&self.second);
        }
    }
    pub(crate) async fn first_before_receipt(path: &Path) {
        let probe = firsts().lock().unwrap().get(path).cloned();
        if let Some(probe) = probe
            && !probe.positive
        {
            firsts().lock().unwrap().remove(path);
            probe.published.notify_one();
            wait(&probe.first_release).await;
        }
    }
    pub(crate) fn first_after_receipt(path: &Path) {
        let probe = firsts().lock().unwrap().remove(path);
        if let Some(probe) = probe
            && probe.positive
        {
            probe.receipt.notify_one();
        }
    }
    pub(crate) async fn before_match(path: &Path) {
        let probe = seconds().lock().unwrap().get(path).cloned();
        if let Some(probe) = probe {
            if probe.positive {
                wait(&probe.receipt).await;
            } else {
                wait(&probe.published).await;
            }
        }
    }
    pub(in crate::download) async fn after_match(
        path: &Path,
        decision: &crate::download::baseline_matching::BaselineMatch,
    ) {
        let probe = seconds().lock().unwrap().remove(path);
        if let Some(probe) = probe {
            if !probe.positive {
                *probe.decision.lock().unwrap() = Some(format!("{decision:?}"));
                if let crate::download::baseline_matching::BaselineMatch::Uncertain(explanation) =
                    decision
                {
                    *probe.explanation.lock().unwrap() = Some(explanation.clone());
                }
                probe.matched.notify_one();
                wait(&probe.match_release).await;
            }
        } else {
            observe_retry(path, decision);
        }
    }
    pub(in crate::download) fn observe_retry(
        path: &Path,
        decision: &crate::download::baseline_matching::BaselineMatch,
    ) {
        let probe = retries().lock().unwrap().get(path).cloned();
        if let Some(probe) = probe {
            probe
                .events
                .lock()
                .unwrap()
                .push(format!("decision:{decision:?}"));
        }
    }
}

#[cfg(test)]
pub(crate) mod distinct_occupant_test_hook {
    use std::path::{Path, PathBuf};
    use std::sync::{Mutex, OnceLock};

    type Before = Box<dyn FnOnce() + Send>;
    type After = Box<dyn FnOnce(bool, bool) + Send>;
    struct Probe {
        before: Option<Before>,
        after: After,
    }
    fn callbacks() -> &'static Mutex<std::collections::HashMap<PathBuf, Probe>> {
        static CALLBACKS: OnceLock<Mutex<std::collections::HashMap<PathBuf, Probe>>> =
            OnceLock::new();
        CALLBACKS.get_or_init(|| Mutex::new(std::collections::HashMap::new()))
    }
    pub(crate) fn install(path: PathBuf, before: Before, after: After) {
        assert!(
            callbacks()
                .lock()
                .unwrap()
                .insert(
                    path,
                    Probe {
                        before: Some(before),
                        after
                    }
                )
                .is_none()
        );
    }
    pub(super) fn run(path: &Path) {
        let callback = callbacks()
            .lock()
            .unwrap()
            .get_mut(path)
            .and_then(|probe| probe.before.take());
        if let Some(callback) = callback {
            callback();
        }
    }
    pub(super) fn finish(path: &Path, failed: bool, exact: bool) {
        if let Some(probe) = callbacks().lock().unwrap().remove(path) {
            (probe.after)(failed, exact);
        }
    }
    pub(crate) fn discard(path: &Path) {
        callbacks().lock().unwrap().remove(path);
    }
}

async fn distinct_occupant_bytes_match(
    root: &std::path::Path,
    path: &std::path::Path,
    expected_size: u64,
    expected_sha256: &str,
) -> bool {
    let Ok(Some(file)) = super::file::retain_pending_file(root, path).await else {
        return false;
    };
    file.fingerprint.size == expected_size
        && data_encoding::HEXLOWER.encode(&file.fingerprint.sha256) == expected_sha256
        && file.validate().await.is_ok()
}

impl TaskPlanner {
    /// Advance both snapshots only with evidence of a successful durable claim.
    #[must_use = "conflicting owner views must stop the asset before planning"]
    pub(super) fn remember_confirmed_legacy_owner(
        &mut self,
        claim: Option<super::orchestration::context::ConfirmedLegacyOwner>,
    ) -> std::result::Result<(), crate::state::error::StateError> {
        if let Some(claim) = claim {
            let (library, master, child) = claim.parts();
            self.remember_legacy_owner(library, master, child)?;
        }
        Ok(())
    }

    fn remember_legacy_owner(
        &mut self,
        library: &str,
        master: &str,
        child: &str,
    ) -> std::result::Result<(), crate::state::error::StateError> {
        if !self.legacy_owners.accepts_claim(library, master, child)
            || !self
                .baseline_matcher
                .accepts_legacy_claim(library, master, child)
        {
            return Err(crate::state::error::StateError::Invariant {
                operation: "remember_confirmed_legacy_owner",
                detail: "durable legacy claim conflicts with an ownership snapshot".into(),
            });
        }
        self.legacy_owners.remember_claim(library, master, child);
        self.baseline_matcher
            .remember_legacy_claim(library, master, child);
        Ok(())
    }

    pub(super) fn new() -> Self {
        Self {
            claimed_paths: FxHashMap::default(),
            path_mode: PathPlanningMode::Download,
            reconciliation: Box::default(),
            dir_cache: paths::DirCache::new(),
            baseline_matcher: super::baseline_matching::BaselineMatcher::default(),
            legacy_owners: LegacyOwnersByLibrary::default(),
            ownership_incomplete: false,
        }
    }

    /// Reserve durable paths before planning so a later or unresolved asset's
    /// current destination cannot be claimed by an earlier collision peer.
    pub(super) fn for_reconciliation(
        records: Vec<ReconciliationCatalogPath>,
        reservations: Vec<ReconciliationReservation>,
    ) -> Result<Self> {
        let mut planner = Self::new();
        planner.path_mode = PathPlanningMode::Reconciliation;
        for record in records {
            planner.add_reconciliation_claim(
                ReconciliationOwner {
                    library: record.library,
                    asset_id: record.asset_id,
                    version_size: record.version_size,
                },
                PathPlanningMode::Reconciliation.key(&record.path)?,
                0,
            );
        }
        for reservation in reservations {
            let owner = ReconciliationOwner {
                library: Arc::clone(&reservation.library),
                asset_id: reservation.asset_id.clone(),
                version_size: reservation.version_size,
            };
            let destination =
                PathPlanningMode::Reconciliation.key(&reservation.destination_path)?;
            anyhow::ensure!(
                destination.as_ref() == reservation.destination_path_key.0,
                "reconciliation reservation has an inconsistent destination key"
            );
            planner
                .reconciliation
                .durable_destinations
                .insert(destination.clone());
            // Reconciliation uses occupancy, not sizes, for collision decisions.
            planner.add_reconciliation_claim(owner.clone(), destination.clone(), 0);
            planner
                .reconciliation
                .path_contents
                .entry(destination)
                .or_default()
                .insert(reservation.content.clone());
            let Some(content) = reservation.content else {
                continue;
            };
            planner.reconciliation.destinations.insert(
                (
                    owner,
                    content,
                    NormalizedPath::from_key(reservation.requested_path_key),
                ),
                reservation.destination_path,
            );
        }
        Ok(planner)
    }

    /// Load publication receipts without treating historical paths as new destinations.
    pub(super) async fn for_download(db: Option<&dyn DownloadStore>) -> Result<Self> {
        let Some(db) = db else {
            return Ok(Self::new());
        };
        let reservations = db.get_reconciliation_reservations().await?;
        let catalog = db.get_reconciliation_catalog_paths().await?;
        let mut ownership_incomplete = false;
        let mut planner = if reservations.is_empty() {
            let mut planner = Self::new();
            for record in catalog {
                let Ok(key) = PathPlanningMode::Reconciliation.key(&record.path) else {
                    planner.ownership_incomplete = true;
                    continue;
                };
                planner
                    .reconciliation
                    .path_owners
                    .entry(key)
                    .or_default()
                    .insert(ReconciliationOwner {
                        library: record.library,
                        asset_id: record.asset_id,
                        version_size: record.version_size,
                    });
            }
            planner
        } else {
            let catalog = catalog
                .into_iter()
                .filter(|record| {
                    let valid = PathPlanningMode::Reconciliation.key(&record.path).is_ok();
                    ownership_incomplete |= !valid;
                    valid
                })
                .collect::<Vec<_>>();
            let mut planner = Self::for_reconciliation(catalog, reservations)?;
            planner.path_mode = PathPlanningMode::ReservedDownload;
            planner
        };
        planner.ownership_incomplete |= ownership_incomplete;
        for (library, asset_id, version_size, path) in db.get_compact_native_owners().await? {
            let Ok(key) = PathPlanningMode::Reconciliation.key(&path) else {
                planner.ownership_incomplete = true;
                continue;
            };
            planner
                .reconciliation
                .path_owners
                .entry(key)
                .or_default()
                .insert(ReconciliationOwner {
                    library: library.into(),
                    asset_id: asset_id.into(),
                    version_size,
                });
        }
        let records = db.get_downloaded_path_records().await?;
        planner.add_downloaded_paths(records);
        let legacy_owners =
            LegacyOwnersByLibrary::from_rows(db.get_legacy_master_state_owners().await?);
        planner.baseline_matcher = super::baseline_matching::BaselineMatcher::with_legacy_owners(
            db.get_baseline_receipts().await?,
            legacy_owners.clone(),
        );
        planner.legacy_owners = legacy_owners;
        Ok(planner)
    }

    /// Explicit repair must persist its first sibling choice, not only replay
    /// reservations left by an earlier run. Reuse normal reserved-download planning.
    pub(super) fn reserve_explicit_retry_destinations(&mut self) {
        if matches!(self.path_mode, PathPlanningMode::Download) {
            self.path_mode = PathPlanningMode::ReservedDownload;
        }
    }

    /// Pending recovery needs positive ownership even in ordinary download mode.
    #[must_use]
    pub(super) fn pending_publication_path_owned(
        &self,
        proof: &crate::state::PendingPublicationRecord,
    ) -> bool {
        if self.ownership_incomplete {
            return false;
        }
        let Ok(key) = PathPlanningMode::Reconciliation.key(&proof.local_path) else {
            return false;
        };
        self.reconciliation
            .path_owners
            .get(&key)
            .is_some_and(|owners| {
                !owners.is_empty()
                    && owners.iter().all(|owner| {
                        owner.library.as_ref() == proof.library
                            && owner.asset_id.as_ref() == proof.id
                            && owner.version_size == proof.version_size
                    })
            })
            && self.content_path_allowed(&key, &proof.checksum, proof.size)
    }

    pub(super) fn remember_recovered_publication(
        &mut self,
        proof: &crate::state::PendingPublicationRecord,
    ) {
        self.add_downloaded_paths(vec![crate::state::DownloadedFileRecord {
            added_at: None,
            is_current_path: true,
            is_preserved_history: false,
            library: proof.library.clone(),
            id: proof.id.clone(),
            version_size: proof.version_size,
            checksum: proof.checksum.clone(),
            local_path: Some(proof.local_path.clone()),
            local_checksum: Some(proof.local_checksum.clone()),
            download_checksum: Some(proof.download_checksum.clone()),
        }]);
    }

    /// Retire only an ordinary in-flight claim; durable choices remain occupied.
    pub(super) fn release_unpublished_task_claim(&mut self, task: &DownloadTask) {
        if matches!(self.path_mode, PathPlanningMode::Download)
            && let Ok(key) = self.path_mode.key(&task.download_path)
            && self.claimed_paths.get(key.as_ref()) == Some(&task.size)
        {
            self.claimed_paths.remove(key.as_ref());
        }
    }

    /// Index every finalized publication, including another selected album's copy.
    pub(super) fn add_downloaded_paths(
        &mut self,
        records: Vec<crate::state::DownloadedFileRecord>,
    ) {
        for record in records {
            self.reconciliation
                .downloaded_paths
                .entry(ReconciliationOwner {
                    library: Arc::from(record.library.as_str()),
                    asset_id: record.id.as_str().into(),
                    version_size: record.version_size,
                })
                .or_default()
                .push(record);
        }
    }

    /// A planner can only restrict the operation's earlier ownership check.
    pub(super) fn allows_existing_mutation(&self, config: &DownloadConfig) -> bool {
        !self.ownership_incomplete && config.allows_existing_mutation()
    }

    /// A completed current publication for this exact asset wins over a newly
    /// planned suffix. The later matcher and adoption census still revalidate
    /// the receipt, bytes and ownership; this only restores the recorded path.
    async fn completed_exact_current_path(
        &self,
        asset: &PhotoAsset,
        config: &DownloadConfig,
        task: &DownloadTask,
    ) -> Result<Option<std::path::PathBuf>> {
        let version_size = task.version_size;
        let owner = ReconciliationOwner {
            library: Arc::from(asset.source_zone().unwrap_or(&config.library)),
            asset_id: asset.state_id().into(),
            version_size,
        };
        let Some(records) = self.reconciliation.downloaded_paths.get(&owner) else {
            return Ok(None);
        };
        let derived_paths = super::filter::derive_expected_paths(asset, config);
        let Some(derived) = derived_paths
            .iter()
            .find(|path| path.version_size == version_size)
        else {
            return Ok(None);
        };
        for record in records {
            if !record.is_current_path {
                continue;
            }
            let (Some(path), Some(checksum)) = (&record.local_path, &record.local_checksum) else {
                continue;
            };
            if record.checksum != derived.checksum.as_ref() {
                continue;
            }
            let matches_family = super::filter::stored_path_matches_download_family(
                asset.state_id(),
                derived,
                &derived_paths,
                config,
                path,
            );
            if !matches_family && let Some(owner_run) = &config.selection_run {
                let run = owner_run.for_asset(asset)?;
                let key = config
                    .selection_pass
                    .as_deref()
                    .context("Missing selection pass")?;
                if !run
                    .capture
                    .db
                    .selection_path_reusable_for_pass(
                        run.capture.owner.clone(),
                        run.root.id.clone(),
                        run.root.spec.scope.clone(),
                        key.to_owned(),
                        asset.asset_record_name().to_owned(),
                        crate::state::db::provider_selection::SelectionPath::from_path(path),
                    )
                    .await?
                {
                    // A current other-pass publication cannot choose this pass's
                    // first destination; preserve its requested collision plan.
                    continue;
                }
            }
            if !matches_family
                && matches!(
                    config.primary_layout_pass_kind,
                    Some(crate::commands::PassKind::Album | crate::commands::PassKind::SmartFolder)
                )
            {
                let (Some(binding), Some(album)) =
                    (&task.publication_binding, &config.primary_layout_source)
                else {
                    continue;
                };
                let Some((capture, scope, _)) = album.publication_scope(&task.library)? else {
                    continue;
                };
                anyhow::ensure!(scope == binding.scope, "Publication scope changed");
                let receipts = capture
                    .db
                    .bound_download_receipts(
                        binding.as_ref().clone(),
                        version_size.as_str().into(),
                        task.checksum.to_string(),
                        task.size,
                    )
                    .await?;
                if !receipts.iter().any(|receipt| {
                    receipt.native_path.to_path() == *path && receipt.local_checksum == *checksum
                }) {
                    continue;
                }
            }
            let Ok(fingerprint) =
                super::file::fingerprint_downloaded_path(&config.directory, path).await
            else {
                continue;
            };
            if data_encoding::HEXLOWER.encode(&fingerprint.sha256) != *checksum {
                continue;
            }
            let metadata_changed_size = record
                .download_checksum
                .as_ref()
                .is_some_and(|downloaded| downloaded != checksum);
            if fingerprint.size == derived.size || metadata_changed_size {
                return Ok(Some(path.clone()));
            }
        }
        Ok(None)
    }

    /// Provider declarations and reconciliation reservations both precede reuse.
    async fn frozen_task_destination(
        &self,
        asset: &PhotoAsset,
        config: &DownloadConfig,
        task: &DownloadTask,
    ) -> Result<Option<std::path::PathBuf>> {
        let reserved = self
            .has_durable_destination(task)
            .then(|| task.download_path.clone());
        if let Some(owner_run) = &config.selection_run {
            let key = config.selection_pass.as_ref().context("Missing pass")?;
            let run = owner_run.for_asset(asset)?;
            if let Some(previous) = run
                .capture
                .db
                .selection_decision(
                    run.capture.owner.clone(),
                    run.root.id.clone(),
                    key.clone(),
                    task.asset_record_name.to_string(),
                )
                .await?
                && !previous.is_deferred_observation()
            {
                anyhow::ensure!(
                    previous.state_id == task.asset_id.as_ref(),
                    "Frozen selection identity changed"
                );
                let destination = previous
                    .decision
                    .destinations
                    .iter()
                    .find(|destination| destination.version_size == task.version_size.as_str())
                    .context("Frozen selection lost a rendition")?
                    .path
                    .to_path();
                anyhow::ensure!(
                    reserved.as_ref().is_none_or(|path| *path == destination),
                    "Frozen destination conflicts with reservation"
                );
                return Ok(Some(destination));
            }
        }
        Ok(reserved)
    }

    async fn restore_completed_current_paths(
        &self,
        plan: &mut AssetTaskPlan,
        asset: &PhotoAsset,
        config: &DownloadConfig,
    ) -> Result<()> {
        for task in &mut plan.tasks {
            if self
                .frozen_task_destination(asset, config, task)
                .await?
                .is_some()
            {
                continue;
            }
            if let Some(path) = self
                .completed_exact_current_path(asset, config, task)
                .await?
            {
                task.download_path = path;
            }
        }
        Ok(())
    }

    /// Accept only a current-content receipt in this pass's filename family.
    /// Additional copies and legacy numbered companions require exact local hashes.
    pub(super) async fn verified_downloaded_path(
        &self,
        asset: &PhotoAsset,
        config: &DownloadConfig,
        version_size: VersionSizeKey,
    ) -> Option<std::path::PathBuf> {
        let owner = ReconciliationOwner {
            library: Arc::from(asset.source_zone().unwrap_or(&config.library)),
            asset_id: asset.state_id().into(),
            version_size,
        };
        let records = self.reconciliation.downloaded_paths.get(&owner)?;
        let derived_paths = super::filter::derive_expected_paths(asset, config);
        let derived = derived_paths
            .iter()
            .find(|path| path.version_size == version_size)?;
        for record in records {
            let (Some(path), Some(checksum)) = (&record.local_path, &record.local_checksum) else {
                continue;
            };
            if let Some(db) = config.state_db.as_deref()
                && !super::legacy_preservation::current_path_is_independent(db, path)
                    .await
                    .ok()?
            {
                continue;
            }
            if let Some(db) = config.state_db.as_deref()
                && db
                    .guard_primary_slot(
                        &owner.library,
                        &owner.asset_id,
                        version_size.as_str(),
                        &derived.checksum,
                        path,
                    )
                    .await
                    .is_err()
            {
                continue;
            }
            if record.checksum != derived.checksum.as_ref()
                || self.conflicts_with_durable_choice(
                    &owner.library,
                    &owner.asset_id,
                    version_size,
                    &derived.checksum,
                    derived.size,
                    path,
                )
                || !super::filter::stored_path_matches_download_family(
                    asset.state_id(),
                    derived,
                    &derived_paths,
                    config,
                    path,
                )
                || !self.retry_path_allowed(
                    &owner.library,
                    &owner.asset_id,
                    asset.asset_record_name(),
                    version_size,
                    &derived.checksum,
                    derived.size,
                    path,
                )
            {
                continue;
            }
            let Ok(key) = PathPlanningMode::Reconciliation.key(path) else {
                continue;
            };
            if self
                .reconciliation
                .path_owners
                .get(&key)
                .is_none_or(|owners| owners.iter().any(|other| other != &owner))
            {
                continue;
            }
            #[cfg(test)]
            self.reconciliation
                .receipt_file_reads
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let Ok(fingerprint) =
                super::file::fingerprint_downloaded_path(&config.directory, path).await
            else {
                continue;
            };
            if data_encoding::HEXLOWER.encode(&fingerprint.sha256) != *checksum {
                continue;
            }
            let metadata_changed_size = record
                .download_checksum
                .as_ref()
                .is_some_and(|downloaded| downloaded != checksum);
            if fingerprint.size == derived.size || metadata_changed_size {
                return Some(path.clone());
            }
        }
        None
    }

    /// Only suppress still-name lookup; the pipeline still owns the media skip.
    /// Additional copies and numbered legacy companions retain hash verification.
    async fn current_motion_paths_exist(
        &self,
        asset: &PhotoAsset,
        config: &DownloadConfig,
        derived_paths: &[super::filter::DerivedPath],
    ) -> bool {
        for derived in derived_paths
            .iter()
            .filter(|path| path.version_size.is_live_photo_motion())
        {
            let owner = ReconciliationOwner {
                library: Arc::from(asset.source_zone().unwrap_or(&config.library)),
                asset_id: asset.state_id().into(),
                version_size: derived.version_size,
            };
            let Some(record) = self
                .reconciliation
                .downloaded_paths
                .get(&owner)
                .and_then(|records| records.iter().find(|record| record.is_current_path))
            else {
                return false;
            };
            let Some(path) = &record.local_path else {
                return false;
            };
            if let Some(db) = config.state_db.as_deref()
                && !super::legacy_preservation::current_path_is_independent(db, path)
                    .await
                    .unwrap_or(false)
            {
                return false;
            }
            if record.checksum != derived.checksum.as_ref()
                || !super::filter::stored_path_matches_current_collision_family(
                    asset.state_id(),
                    derived,
                    derived_paths,
                    config,
                    path,
                )
                || !self.retry_path_allowed(
                    &owner.library,
                    &owner.asset_id,
                    asset.asset_record_name(),
                    owner.version_size,
                    &derived.checksum,
                    derived.size,
                    path,
                )
                || !tokio::fs::symlink_metadata(path)
                    .await
                    .is_ok_and(|metadata| metadata.is_file() && metadata.len() == derived.size)
            {
                return false;
            }
        }
        true
    }

    pub(super) async fn plan_download_asset(
        &mut self,
        asset: &PhotoAsset,
        config: &DownloadConfig,
    ) -> Result<AssetTaskPlan> {
        let mut plan = self
            .plan_download_asset_with_replay(asset, config, ReservedReplay::RequestedDirectory)
            .await?;
        self.include_existing_comparison_tasks(asset, config, &mut plan);
        self.bind_publication_tasks(asset, config, &mut plan)
            .await?;
        if matches!(self.path_mode, PathPlanningMode::Download) {
            self.restore_completed_current_paths(&mut plan, asset, config)
                .await?;
        }
        if matches!(self.path_mode, PathPlanningMode::Download) {
            self.relocate_protected_tasks(&mut plan, asset, config)
                .await?;
        }
        if matches!(
            self.path_mode,
            PathPlanningMode::Download | PathPlanningMode::ReservedDownload
        ) {
            self.reserve_changed_publication_choices(asset, config, &plan)
                .await?;
        }
        Ok(plan)
    }

    // Only nominate choices backed by scoped prior publications. The caller
    // persists the final choice before selection admission or ordinary dispatch.
    async fn reserve_changed_publication_choices(
        &mut self,
        asset: &PhotoAsset,
        config: &DownloadConfig,
        plan: &AssetTaskPlan,
    ) -> Result<()> {
        let Some(album) = &config.primary_layout_source else {
            return Ok(());
        };
        let derived = super::filter::derive_expected_paths(asset, config);
        for task in &plan.tasks {
            let Some(binding) = task.publication_binding.as_deref() else {
                continue;
            };
            let Some(requested) = derived
                .iter()
                .find(|path| path.version_size == task.version_size)
            else {
                continue;
            };
            let requested_key = PathPlanningMode::Reconciliation.key(&requested.path)?;
            let destination_key = PathPlanningMode::Reconciliation.key(&task.download_path)?;
            if requested_key == destination_key {
                continue;
            }
            let Some((capture, scope, _)) = album.publication_scope(&task.library)? else {
                continue;
            };
            anyhow::ensure!(scope == binding.scope, "Publication scope changed");
            let historical = capture
                .db
                .bound_historical_downloads(
                    binding.clone(),
                    task.version_size.as_str().into(),
                    task.checksum.to_string(),
                    task.size,
                )
                .await?;
            if task
                .superseded_requested(&config.directory, &requested.path)
                .is_none()
                && !historical.iter().any(|receipt| {
                    PathPlanningMode::Reconciliation
                        .key(&receipt.native_path.to_path())
                        .is_ok_and(|key| key == requested_key)
                })
                && self
                    .cross_rendition_original(asset, config, task, &requested.path)
                    .await?
                    .is_none()
            {
                continue;
            }
            let reservation = ReconciliationReservation {
                library: Arc::clone(&task.library),
                asset_id: task.asset_id.as_ref().into(),
                version_size: task.version_size,
                requested_path_key: ReconciliationPathKey(requested_key.as_ref().to_owned()),
                destination_path_key: ReconciliationPathKey(destination_key.as_ref().to_owned()),
                destination_path: crate::fs_util::absolute_confined_path(&task.download_path)?,
                content: Some(ReconciliationContent {
                    checksum: task.checksum.clone(),
                    size: task.size,
                }),
            };
            let mut matching = None;
            for (index, planned) in self.reconciliation.reservations.iter().enumerate() {
                let saved = &planned.reservation;
                if saved.library == reservation.library
                    && saved.asset_id == reservation.asset_id
                    && saved.version_size == reservation.version_size
                    && saved.content == reservation.content
                    && saved.requested_path_key == reservation.requested_path_key
                {
                    anyhow::ensure!(
                        saved == &reservation,
                        "Conflicting prior publication destination"
                    );
                    anyhow::ensure!(
                        matching.replace(index).is_none(),
                        "Duplicate prior publication nomination"
                    );
                }
            }
            if let Some(index) = matching {
                let planned = self
                    .reconciliation
                    .reservations
                    .get_mut(index)
                    .context("Prior publication nomination disappeared")?;
                if planned.origin == ReservationOrigin::OrdinaryCollision {
                    planned.origin = ReservationOrigin::PriorPublication;
                }
            } else {
                self.reconciliation.reservations.push(PlannedReservation {
                    origin: ReservationOrigin::PriorPublication,
                    reservation,
                });
            }
        }
        Ok(())
    }

    /// Commit only final execution choices, never listing/dry-run nominations.
    /// True confirms durable intent, whether newly inserted or exactly replayed.
    /// It never confirms admission, bytes, metadata or checkpoint completion.
    pub(super) async fn persist_execution_publication_choices(
        &mut self,
        asset: &PhotoAsset,
        config: &DownloadConfig,
        tasks: &[DownloadTask],
    ) -> Result<bool> {
        let library = asset.source_zone().unwrap_or(&config.library);
        if !self.reconciliation.reservations.iter().any(|planned| {
            let r = &planned.reservation;
            planned.origin != ReservationOrigin::OrdinaryCollision
                && r.library.as_ref() == library
                && r.asset_id.as_ref() == asset.state_id()
                && r.requested_path_key != r.destination_path_key
                && tasks.iter().any(|task| task.version_size == r.version_size)
        }) {
            return Ok(false);
        }
        let album = config
            .primary_layout_source
            .as_ref()
            .context("Missing choice source")?;
        album
            .publication_scope(library)?
            .context("Missing choice scope")?;
        config
            .state_db
            .as_ref()
            .context("Missing choice state store")?;
        let mut checked = AssetTaskPlan {
            tasks: tasks.to_vec(),
            filter_reason: None,
            malformed_resource: None,
        };
        self.bind_publication_tasks(asset, config, &mut checked)
            .await?;
        let derived = super::filter::derive_expected_paths(asset, config);
        let mut choices = Vec::new();
        let mut replayed = Vec::new();
        let mut consumed = Vec::new();
        let mut binding = None;
        let mut store = None;
        for (index, planned) in self.reconciliation.reservations.iter().enumerate() {
            let reservation = &planned.reservation;
            if reservation.library.as_ref() != library
                || reservation.asset_id.as_ref() != asset.state_id()
            {
                continue;
            }
            let Some((task, rebound)) = tasks
                .iter()
                .zip(&checked.tasks)
                .find(|(task, _)| task.version_size == reservation.version_size)
            else {
                continue;
            };
            if PathPlanningMode::Reconciliation
                .key(&task.download_path)?
                .as_ref()
                != reservation.destination_path_key.0
            {
                consumed.push(index);
                continue;
            }
            if reservation.requested_path_key == reservation.destination_path_key {
                continue;
            }
            if planned.origin == ReservationOrigin::OrdinaryCollision {
                continue;
            }
            anyhow::ensure!(
                reservation
                    .content
                    .as_ref()
                    .is_some_and(|c| c.checksum == task.checksum && c.size == task.size),
                "Choice no longer matches nominated resource"
            );
            let original = task
                .publication_binding
                .as_deref()
                .context("Choice lost publication binding")?;
            anyhow::ensure!(
                rebound.publication_binding.as_deref() == Some(original),
                "Publication identity changed before reservation"
            );
            let requested = derived
                .iter()
                .find(|p| p.version_size == task.version_size)
                .context("Choice lost its rendition")?;
            anyhow::ensure!(
                requested.checksum.as_ref() == task.checksum.as_ref()
                    && requested.size == task.size
                    && crate::fs_util::absolute_confined_path(&reservation.destination_path)?
                        == crate::fs_util::absolute_confined_path(&task.download_path)?
                    && PathPlanningMode::Reconciliation
                        .key(&requested.path)?
                        .as_ref()
                        == reservation.requested_path_key.0,
                "Choice no longer matches current requested resource"
            );
            anyhow::ensure!(
                self.frozen_task_destination(asset, config, task)
                    .await?
                    .is_none_or(|p| p == task.download_path),
                "Choice conflicts with frozen destination"
            );
            let album = config
                .primary_layout_source
                .as_ref()
                .context("Missing choice source")?;
            let (capture, scope, _) = album
                .publication_scope(library)?
                .context("Missing choice scope")?;
            anyhow::ensure!(
                scope == original.scope
                    && original.origin_pass
                        == config
                            .primary_layout_pass
                            .as_deref()
                            .context("Missing choice pass")?
                    && original.master == asset.id(),
                "Choice source association changed"
            );
            if let Some(run) = &config.selection_run {
                let run = run.for_asset(asset)?;
                anyhow::ensure!(
                    Arc::ptr_eq(&run.capture.db, &capture.db)
                        && run.root.spec.scope == scope
                        && config.selection_pass.as_deref() == Some(original.origin_pass.as_str()),
                    "Choice selection source changed"
                );
            }
            let root = crate::fs_util::absolute_confined_path(&config.directory)?;
            anyhow::ensure!(
                crate::fs_util::absolute_confined_path(&requested.path)?.starts_with(&root)
                    && crate::fs_util::absolute_confined_path(&task.download_path)?
                        .starts_with(&root),
                "Choice escaped requested root"
            );
            // Always authenticate, including zero-history nominations and exact replay.
            let historical = capture
                .db
                .bound_historical_downloads(
                    original.clone(),
                    task.version_size.as_str().into(),
                    task.checksum.to_string(),
                    task.size,
                )
                .await?;
            let persisted = capture
                .db
                .publication_choice_is_persisted(
                    original.clone(),
                    reservation.clone(),
                    crate::state::db::provider_selection::SelectionPath::from_path(&requested.path),
                )
                .await?;
            if planned.origin == ReservationOrigin::ExactReplayCandidate {
                anyhow::ensure!(persisted, "Exact choice replay lost persisted intent");
                replayed.push(reservation.clone());
                consumed.push(index);
                continue;
            }
            anyhow::ensure!(
                task.superseded_requested(&config.directory, &requested.path)
                    .is_some()
                    || historical
                        .iter()
                        .any(|r| r.native_path.to_path() == requested.path)
                    || self
                        .cross_rendition_original(asset, config, task, &requested.path)
                        .await?
                        .is_some(),
                "Choice lost authenticated historical source"
            );
            anyhow::ensure!(
                binding.as_ref().is_none_or(|b| b == original),
                "Choice batch changed binding"
            );
            binding = Some(original.clone());
            store = Some(capture.db);
            choices.push((
                reservation.clone(),
                crate::state::db::provider_selection::SelectionPath::from_path(&requested.path),
            ));
            consumed.push(index);
        }
        let confirmed = !choices.is_empty() || !replayed.is_empty();
        if let (Some(db), Some(binding)) = (store, binding) {
            db.reserve_bound_publication_choices(binding, choices.clone())
                .await?;
            self.remember_download_reservations(choices.into_iter().map(|(r, _)| r).collect());
        }
        self.remember_download_reservations(replayed);
        let mut index = 0;
        self.reconciliation.reservations.retain(|_| {
            let keep = !consumed.contains(&index);
            index += 1;
            keep
        });
        Ok(confirmed)
    }

    async fn bind_publication_tasks(
        &self,
        asset: &PhotoAsset,
        config: &DownloadConfig,
        plan: &mut AssetTaskPlan,
    ) -> Result<()> {
        if plan.tasks.is_empty() {
            return Ok(());
        }
        let (Some(album), Some(pass), Some(db)) = (
            &config.primary_layout_source,
            &config.primary_layout_pass,
            &config.state_db,
        ) else {
            return Ok(());
        };
        let library = asset.source_zone().unwrap_or(config.library.as_ref());
        let Some((capture, scope, _zone)) = album.publication_scope(library)? else {
            return Ok(());
        };
        let concrete = Arc::clone(&capture.db) as Arc<dyn DownloadStore>;
        anyhow::ensure!(
            Arc::ptr_eq(db, &concrete),
            "Publication source database context mismatch"
        );
        let binding = capture
            .db
            .bind_publication(
                capture.owner,
                crate::state::db::PublicationBinding {
                    account_key: String::new(),
                    provider_key: String::new(),
                    scope,
                    library: library.to_owned(),
                    child: asset.asset_record_name().to_owned(),
                    master: asset.id().to_owned(),
                    state_id: asset.state_id().to_owned(),
                    origin_pass: pass.clone(),
                },
            )
            .await?;
        let binding = Arc::new(binding);
        for task in &mut plan.tasks {
            anyhow::ensure!(
                task.library.as_ref() == library
                    && task.asset_id.as_ref() == asset.state_id()
                    && task.asset_record_name.as_ref() == asset.asset_record_name(),
                "Publication task identity differs from selected source"
            );
            task.publication_binding = Some(Arc::clone(&binding));
        }
        Ok(())
    }

    /// Include occupied selected paths in the comparison. Path occupancy cannot
    /// decide completion before receipts and local bytes have been checked.
    fn include_existing_comparison_tasks(
        &self,
        asset: &PhotoAsset,
        config: &DownloadConfig,
        plan: &mut AssetTaskPlan,
    ) {
        if plan.filter_reason.is_some() || !asset.has_valid_id() {
            return;
        }
        // The original task builder applies exclusions to synthesized names too.
        // Reintroducing occupied paths must preserve that same selection decision.
        if super::filter::fallback_filename_excluded(asset, config) {
            return;
        }
        for derived in super::filter::derive_expected_paths(asset, config) {
            if plan
                .tasks
                .iter()
                .any(|task| task.version_size == derived.version_size)
            {
                continue;
            }
            plan.tasks.push(DownloadTask {
                require_created_entry: self.ownership_incomplete
                    || !config.allows_existing_mutation(),
                url: derived.url,
                download_path: derived.path,
                replacement_fingerprint: None,
                publication_binding: None,
                pending_cross_parent_root: None,
                superseded_requested_path: None,
                checksum: derived.checksum,
                asset_id: asset.state_id_arc(),
                asset_record_name: asset.asset_record_name_arc(),
                library: Arc::from(asset.source_zone().unwrap_or(&config.library)),
                metadata: super::filter::build_selected_payload(
                    asset,
                    config,
                    derived.version_size,
                ),
                size: derived.size,
                created_local: asset.created_local(),
                version_size: derived.version_size,
                media_type: determine_media_type(derived.version_size, asset),
            });
        }
    }

    /// Discover only historical filename families in the requested directory.
    /// Arbitrary unindexed moves or unrelated renames are outside this census.
    /// Names locate candidates; the matcher still requires full provider proof.
    async fn discover_baseline_family(
        &mut self,
        asset: &PhotoAsset,
        requested: &Path,
    ) -> super::baseline_matching::BaselineCandidates {
        let Some(parent) = requested.parent() else {
            return super::baseline_matching::BaselineCandidates {
                paths: Vec::new(),
                complete: false,
            };
        };
        let Some(filename) = requested.file_name().and_then(|name| name.to_str()) else {
            return super::baseline_matching::BaselineCandidates {
                paths: Vec::new(),
                complete: false,
            };
        };
        let mut bases = vec![paths::normalize_ampm(filename)];
        if let Some(original) = asset.filename() {
            // The current rendition supplies the extension. The old provider
            // basename remains a clue when an identity suffix changed labels.
            let mut original = std::path::PathBuf::from(paths::clean_filename(original).as_ref());
            original.set_extension(requested.extension().unwrap_or_default());
            if let Some(name) = original.file_name().and_then(|name| name.to_str()) {
                bases.push(paths::normalize_ampm(name));
            }
        }
        bases.sort();
        bases.dedup();
        let prefixes: Vec<String> = bases
            .iter()
            .filter_map(|name| {
                Path::new(name)
                    .file_stem()
                    .and_then(|stem| stem.to_str())
                    .filter(|stem| !stem.is_empty())
                    .map(str::to_owned)
            })
            .collect();
        if prefixes.is_empty() {
            return super::baseline_matching::BaselineCandidates {
                paths: Vec::new(),
                complete: false,
            };
        }
        let (mut candidates, complete) =
            self.dir_cache.baseline_candidates(parent, &prefixes).await;
        candidates.retain(|path| {
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                return false;
            };
            let normalized = paths::normalize_ampm(name);
            let candidate = Path::new(&normalized);
            bases.iter().any(|base| {
                let base = Path::new(base);
                let extension = |path: &Path| {
                    path.extension()
                        .and_then(|ext| ext.to_str())
                        .unwrap_or("")
                        .to_owned()
                };
                if !extension(candidate).eq_ignore_ascii_case(&extension(base)) {
                    return false;
                }
                let Some(stem) = base.file_stem().and_then(|stem| stem.to_str()) else {
                    return false;
                };
                candidate
                    .file_stem()
                    .and_then(|name| name.to_str())
                    .and_then(|name| name.strip_prefix(stem))
                    .is_some_and(|tail| {
                        tail.is_empty() || tail.starts_with('-') || tail.starts_with('_')
                    })
            })
        });
        super::baseline_matching::BaselineCandidates {
            paths: candidates,
            complete,
        }
    }

    pub(super) async fn match_existing(
        &mut self,
        asset: &PhotoAsset,
        config: &DownloadConfig,
        task: &DownloadTask,
    ) -> super::baseline_matching::BaselineMatch {
        let frozen = match self.frozen_task_destination(asset, config, task).await {
            Ok(path) => path,
            Err(_) => {
                return super::baseline_matching::uncertain(
                    "scoped_receipt_or_reservation_validation_failed",
                    Some(&task.download_path),
                );
            }
        };
        // Fresh fallback publications carry an authenticated compact receipt even
        // when no active selection generation was created (for example cross-zone albums).
        if let (Some(binding), Some(album)) =
            (&task.publication_binding, &config.primary_layout_source)
        {
            let capture = match album.publication_scope(&task.library) {
                Ok(Some((capture, scope, _))) if scope == binding.scope => capture,
                _ => {
                    return super::baseline_matching::uncertain(
                        "scoped_receipt_or_reservation_validation_failed",
                        Some(&task.download_path),
                    );
                }
            };
            let db = &capture.db;
            match db
                .bound_download_receipts(
                    binding.as_ref().clone(),
                    task.version_size.as_str().into(),
                    task.checksum.to_string(),
                    task.size,
                )
                .await
            {
                Ok(receipts) => {
                    for receipt in receipts {
                        if config
                            .selection_run
                            .as_ref()
                            .is_some_and(|run| run.is_changed_legacy_retry(config))
                            && receipt.native_path.to_path() != task.download_path
                        {
                            continue;
                        }
                        if frozen
                            .as_ref()
                            .is_some_and(|path| *path != receipt.native_path.to_path())
                        {
                            return super::baseline_matching::uncertain(
                                "receipt_destination_differs_from_reserved_destination",
                                Some(&receipt.native_path.to_path()),
                            );
                        }
                        match super::file::retain_pending_file(
                            &config.directory,
                            &receipt.native_path.to_path(),
                        )
                        .await
                        {
                            Ok(None) => continue,
                            Ok(Some(file))
                                if data_encoding::HEXLOWER.encode(&file.fingerprint.sha256)
                                    == receipt.local_checksum
                                    && (receipt.source_checksum != receipt.local_checksum
                                        || file.fingerprint.size == task.size) =>
                            {
                                return super::baseline_matching::BaselineMatch::RecordedPresent {
                                    receipt,
                                    file,
                                };
                            }
                            // A saved scoped native destination remains a candidate after
                            // compatibility rows retire or the configured template changes.
                            // Changed bytes and unreadable paths must not become new downloads.
                            Ok(Some(_)) => {
                                return super::baseline_matching::uncertain(
                                    "recorded_local_hash_or_size_changed",
                                    Some(&receipt.native_path.to_path()),
                                );
                            }
                            Err(_) => {
                                return super::baseline_matching::uncertain(
                                    "candidate_unreadable_unsafe_or_changed",
                                    Some(&receipt.native_path.to_path()),
                                );
                            }
                        }
                    }
                }
                Err(_) => {
                    return super::baseline_matching::uncertain(
                        "scoped_receipt_or_reservation_validation_failed",
                        Some(&task.download_path),
                    );
                }
            }
        }
        let mut historical = if let (Some(binding), Some(album)) =
            (&task.publication_binding, &config.primary_layout_source)
        {
            let capture = match album.publication_scope(&task.library) {
                Ok(Some((capture, scope, _))) if scope == binding.scope => capture,
                _ => {
                    return super::baseline_matching::uncertain(
                        "scoped_receipt_or_reservation_validation_failed",
                        Some(&task.download_path),
                    );
                }
            };
            match capture
                .db
                .bound_historical_downloads(
                    binding.as_ref().clone(),
                    task.version_size.as_str().into(),
                    task.checksum.to_string(),
                    task.size,
                )
                .await
            {
                Ok(receipts) => receipts,
                Err(_) => {
                    return super::baseline_matching::uncertain(
                        "scoped_receipt_or_reservation_validation_failed",
                        Some(&task.download_path),
                    );
                }
            }
        } else {
            Vec::new()
        };
        let derived = super::filter::derive_expected_paths(asset, config);
        let requested = derived
            .iter()
            .find(|path| path.version_size == task.version_size)
            .map_or(task.download_path.as_path(), |path| path.path.as_path());
        match self
            .cross_rendition_original(asset, config, task, requested)
            .await
        {
            Ok(Some(receipt)) => historical.push(receipt),
            Ok(None) => match self
                .missing_cross_rendition_original(asset, config, task, requested)
                .await
            {
                Ok(Some(receipt)) => historical.push(receipt),
                Ok(None) => {}
                Err(_) => {
                    return super::baseline_matching::uncertain(
                        "scoped_receipt_or_reservation_validation_failed",
                        Some(requested),
                    );
                }
            },
            Err(_) => {
                return super::baseline_matching::uncertain(
                    "scoped_receipt_or_reservation_validation_failed",
                    Some(requested),
                );
            }
        }
        let selected_copy_scope =
            if let (Some(run), Some(key)) = (&config.selection_run, &config.primary_layout_pass) {
                match run
                    .capture
                    .db
                    .selection_destination_scope(
                        run.capture.owner.clone(),
                        run.root.id.clone(),
                        key.to_string(),
                        task.asset_record_name.to_string(),
                        (
                            task.asset_id.to_string(),
                            task.version_size.as_str().to_owned(),
                            crate::state::db::provider_selection::SelectionPath::from_path(
                                &task.download_path,
                            ),
                            task.checksum.to_string(),
                            task.size,
                        ),
                        crate::state::db::provider_selection::SelectionPath::from_path(requested),
                    )
                    .await
                {
                    Ok(admitted) => admitted,
                    Err(_) => {
                        return super::baseline_matching::uncertain(
                            "scoped_receipt_or_reservation_validation_failed",
                            Some(&task.download_path),
                        );
                    }
                }
            } else {
                Default::default()
            };
        // Changed legacy selection may only settle its admitted frozen copy.
        // A same-checksum file for another child is not authority to move it.
        if config
            .selection_run
            .as_ref()
            .is_some_and(|run| run.is_changed_legacy_retry(config))
        {
            if !selected_copy_scope.admitted {
                return super::baseline_matching::uncertain(
                    "recorded_destination_not_admitted",
                    Some(&task.download_path),
                );
            }
            return match super::file::retain_pending_file(&config.directory, &task.download_path)
                .await
            {
                Ok(None) => {
                    let Some(db) = config.state_db.as_deref() else {
                        return super::baseline_matching::uncertain(
                            "recorded_source_database_missing",
                            Some(&task.download_path),
                        );
                    };
                    match db.guard_primary_writer(&task.download_path).await {
                        Ok(()) => super::baseline_matching::BaselineMatch::Missing,
                        Err(_) => super::baseline_matching::uncertain(
                            "scoped_receipt_or_reservation_validation_failed",
                            Some(&task.download_path),
                        ),
                    }
                }
                Ok(Some(_)) => super::baseline_matching::uncertain(
                    "recorded_destination_occupied_without_receipt",
                    Some(&task.download_path),
                ),
                Err(_) => super::baseline_matching::uncertain(
                    "candidate_unreadable_unsafe_or_changed",
                    Some(&task.download_path),
                ),
            };
        }
        let discovered = self.discover_baseline_family(asset, requested).await;
        if task.version_size == VersionSizeKey::Adjusted {
            let mut seen = FxHashSet::default();
            for candidate in &discovered.paths {
                if !seen.insert(candidate) {
                    continue;
                }
                match self
                    .cross_rendition_original(asset, config, task, candidate)
                    .await
                {
                    Ok(Some(receipt)) => {
                        if !historical.contains(&receipt) {
                            historical.push(receipt);
                        }
                    }
                    Ok(None) => {}
                    Err(_) => {
                        return super::baseline_matching::uncertain(
                            "scoped_receipt_or_reservation_validation_failed",
                            Some(candidate),
                        );
                    }
                }
            }
        }
        // A separately admitted absent destination may ignore an independently
        // authenticated occupant, but only for the particular path proved here.
        let mut distinct = std::collections::HashSet::new();
        if selected_copy_scope.admitted
            && task.replacement_fingerprint.is_none()
            && task.pending_cross_parent_root.is_none()
            && matches!(
                super::file::inspect_pending_candidate(&config.directory, &task.download_path)
                    .await,
                Ok(false)
            )
            && let (Some(binding), Some(run), Some(pass), Some(album)) = (
                &task.publication_binding,
                &config.selection_run,
                &config.primary_layout_pass,
                &config.primary_layout_source,
            )
            && binding.scope == run.root.spec.scope
            && binding.origin_pass == *pass
            && binding.child == task.asset_record_name.as_ref()
            && binding.master == asset.id()
            && binding.state_id == task.asset_id.as_ref()
            && binding.library == task.library.as_ref()
            && let Ok(Some((capture, scope, _))) = album.publication_scope(&task.library)
            && scope == binding.scope
            && Arc::ptr_eq(&capture.db, &run.capture.db)
        {
            let paths = [requested.to_path_buf(), task.download_path.clone()]
                .into_iter()
                .chain(discovered.paths.iter().cloned())
                .chain(self.baseline_matcher.candidate_paths(task));
            for path in paths {
                let keys = crate::fs_util::confined_path_key(&path)
                    .ok()
                    .zip(crate::fs_util::confined_path_key(&task.download_path).ok());
                if !matches!(keys, Some((a, b)) if a != b) {
                    continue;
                }
                let native = crate::state::db::provider_selection::SelectionPath::from_path(&path);
                let read = || {
                    capture.db.bound_distinct_occupant(
                        binding.as_ref().clone(),
                        native.clone(),
                        task.version_size.as_str().into(),
                        task.checksum.to_string(),
                        task.size,
                    )
                };
                let Ok(Some(receipt)) = read().await else {
                    continue;
                };
                if distinct_occupant_bytes_match(
                    &config.directory,
                    &path,
                    receipt.provider_size,
                    &receipt.local_checksum,
                )
                .await
                {
                    #[cfg(test)]
                    distinct_occupant_test_hook::run(&path);
                    let reread = read().await;
                    #[cfg(test)]
                    distinct_occupant_test_hook::finish(
                        &path,
                        reread.is_err(),
                        matches!(&reread, Ok(Some(current)) if current == &receipt),
                    );
                    if matches!(reread, Ok(Some(current)) if current == receipt) {
                        distinct.insert(path);
                    }
                }
            }
        }
        let path_owners = &self.reconciliation.path_owners;
        let downloaded_paths = &self.reconciliation.downloaded_paths;
        let legacy_owners = &self.legacy_owners;
        let baseline_matcher = &self.baseline_matcher;
        let owner_excluded = |path: &Path| {
            if baseline_matcher.has_exact_current_receipt(task, path) {
                return false;
            }
            let Ok(key) = PathPlanningMode::Reconciliation.key(path) else {
                return true;
            };
            let Some(owners) = path_owners.get(&key) else {
                return false;
            };
            let incompatible = owners.iter().any(|owner| {
                !same_logical_asset(
                    task.library.as_ref(),
                    task.asset_id.as_ref(),
                    task.asset_record_name.as_ref(),
                    owner.library.as_ref(),
                    owner.asset_id.as_ref(),
                    legacy_owners,
                )
            });
            if !incompatible {
                return false;
            }
            let target = ReconciliationOwner {
                library: Arc::clone(&task.library),
                asset_id: task.asset_id.as_ref().into(),
                version_size: task.version_size,
            };
            !downloaded_paths.get(&target).is_some_and(|records| {
                records.iter().any(|record| {
                    record.is_current_path
                        && record.checksum.as_str() == task.checksum.as_ref()
                        && record.local_checksum.is_some()
                        && record.local_path.as_ref().is_some_and(|path| {
                            crate::fs_util::confined_path_key(path)
                                .ok()
                                .is_some_and(|path_key| path_key.as_str() == key.as_ref())
                        })
                })
            })
        };
        let matched = self
            .baseline_matcher
            .match_task_with_distinct_and_exclusions(
                &config.directory,
                task,
                requested,
                config.album_name.is_some()
                    && config.primary_layout_pass_kind != Some(crate::commands::PassKind::Unfiled),
                &selected_copy_scope,
                &discovered,
                (&historical, &distinct),
                &owner_excluded,
            )
            .await;
        if matches!(matched, super::baseline_matching::BaselineMatch::Missing)
            && let Some(db) = config.state_db.as_deref()
            && db.guard_primary_writer(&task.download_path).await.is_err()
        {
            // An absent leaf can still belong to an interrupted publication.
            // Preserve its claim rather than transferring into the owned slot.
            return super::baseline_matching::uncertain(
                "scoped_receipt_or_reservation_validation_failed",
                Some(&task.download_path),
            );
        }
        matched
    }

    /// A source-backed original can occupy the requested adjusted name without
    /// supplying evidence for the adjusted bytes. Recheck the confined file on
    /// every use; a missing or changed occupant is never positive proof.
    async fn cross_rendition_original(
        &self,
        asset: &PhotoAsset,
        config: &DownloadConfig,
        task: &DownloadTask,
        requested: &Path,
    ) -> Result<Option<crate::state::db::download_receipts::DownloadReceipt>> {
        if task.version_size != VersionSizeKey::Adjusted || self.ownership_incomplete {
            return Ok(None);
        }
        let key = PathPlanningMode::Reconciliation.key(requested)?;
        if self
            .reconciliation
            .path_owners
            .get(&key)
            .is_some_and(|owners| {
                owners.iter().any(|owner| {
                    !same_logical_asset(
                        task.library.as_ref(),
                        task.asset_id.as_ref(),
                        task.asset_record_name.as_ref(),
                        owner.library.as_ref(),
                        owner.asset_id.as_ref(),
                        &self.legacy_owners,
                    ) || !matches!(
                        owner.version_size,
                        VersionSizeKey::Original | VersionSizeKey::Adjusted
                    )
                })
            })
        {
            return Ok(None);
        }
        let (Some(binding), Some(album)) =
            (&task.publication_binding, &config.primary_layout_source)
        else {
            return Ok(None);
        };
        let Some((capture, scope, _)) = album.publication_scope(&task.library)? else {
            return Ok(None);
        };
        anyhow::ensure!(scope == binding.scope, "Publication scope changed");
        let original_key =
            super::filter::provider_version_for_selected(asset, config, VersionSizeKey::Original);
        let Some((_, original)) = asset
            .versions()
            .iter()
            .find(|(key, _)| VersionSizeKey::from(*key) == original_key)
        else {
            return Ok(None);
        };
        let Ok(fingerprint) =
            super::file::fingerprint_downloaded_path(&config.directory, requested).await
        else {
            return Ok(None);
        };
        let proof = capture
            .db
            .bound_cross_rendition_original(
                binding.as_ref().clone(),
                task.version_size,
                crate::state::db::CrossRenditionOriginal {
                    path: crate::state::db::provider_selection::SelectionPath::from_path(
                        &std::path::absolute(requested)?,
                    ),
                    provider_checksum: original.checksum.to_string(),
                    provider_size: original.size,
                    local_checksum: Some(data_encoding::HEXLOWER.encode(&fingerprint.sha256)),
                },
            )
            .await?;
        Ok(proof.map(|proof| proof.receipt))
    }

    /// A missing source-backed original is negative evidence only: it holds
    /// the selected path until the retained file can be revalidated.
    async fn missing_cross_rendition_original(
        &self,
        asset: &PhotoAsset,
        config: &DownloadConfig,
        task: &DownloadTask,
        requested: &Path,
    ) -> Result<Option<crate::state::db::download_receipts::DownloadReceipt>> {
        if task.version_size != VersionSizeKey::Adjusted
            || !matches!(
                super::file::inspect_pending_candidate(&config.directory, requested).await,
                Ok(false)
            )
        {
            return Ok(None);
        }
        let (Some(binding), Some(album)) =
            (&task.publication_binding, &config.primary_layout_source)
        else {
            return Ok(None);
        };
        let Some((capture, scope, _)) = album.publication_scope(&task.library)? else {
            return Ok(None);
        };
        anyhow::ensure!(scope == binding.scope, "Publication scope changed");
        let key =
            super::filter::provider_version_for_selected(asset, config, VersionSizeKey::Original);
        let current = asset
            .versions()
            .iter()
            .find(|(version, _)| VersionSizeKey::from(*version) == key)
            .map(|(_, original)| (original.checksum.to_string(), original.size));
        let (checksum, size) = current.clone().unwrap_or_default();
        let mut receipts = capture
            .db
            .bound_historical_downloads(
                binding.as_ref().clone(),
                "original".into(),
                checksum.clone(),
                size,
            )
            .await?;
        if current.is_some() {
            receipts.extend(
                capture
                    .db
                    .bound_download_receipts(
                        binding.as_ref().clone(),
                        "original".into(),
                        checksum,
                        size,
                    )
                    .await?,
            );
        }
        let requested_key = crate::fs_util::confined_path_key(requested)?;
        for receipt in receipts {
            if crate::fs_util::confined_path_key(&receipt.native_path.to_path())? == requested_key {
                return Ok(Some(receipt));
            }
        }
        Ok(None)
    }

    /// Choose and reserve a sibling only for a newly selected obligation.
    pub(super) async fn choose_cross_rendition_sibling(
        &mut self,
        asset: &PhotoAsset,
        config: &DownloadConfig,
        task: &mut DownloadTask,
    ) -> Result<bool> {
        let derived = super::filter::derive_expected_paths(asset, config);
        let Some(requested) = derived
            .iter()
            .find(|path| path.version_size == task.version_size)
        else {
            return Ok(false);
        };
        if self.path_mode.key(&requested.path)? != self.path_mode.key(&task.download_path)?
            || self
                .cross_rendition_original(asset, config, task, &requested.path)
                .await?
                .is_none()
        {
            return Ok(false);
        }
        let Some(db) = config.state_db.as_deref() else {
            return Ok(false);
        };
        self.choose_independent_sibling(db, task).await?;
        self.reconciliation.reservations.push(PlannedReservation {
            origin: ReservationOrigin::PriorPublication,
            reservation: ReconciliationReservation {
                library: Arc::clone(&task.library),
                asset_id: task.asset_id.as_ref().into(),
                version_size: task.version_size,
                requested_path_key: ReconciliationPathKey(
                    PathPlanningMode::Reconciliation
                        .key(&requested.path)?
                        .as_ref()
                        .to_owned(),
                ),
                destination_path_key: ReconciliationPathKey(
                    PathPlanningMode::Reconciliation
                        .key(&task.download_path)?
                        .as_ref()
                        .to_owned(),
                ),
                destination_path: crate::fs_util::absolute_confined_path(&task.download_path)?,
                content: Some(ReconciliationContent {
                    checksum: task.checksum.clone(),
                    size: task.size,
                }),
            },
        });
        Ok(true)
    }

    /// Ordinary downloads must never publish to a protected legacy identity,
    /// even when the directory cache cannot see the protected file under the
    /// target spelling. Choose the existing identity suffix instead of asking
    /// the state writer to reject the protected destination later.
    async fn relocate_protected_tasks(
        &mut self,
        plan: &mut AssetTaskPlan,
        asset: &PhotoAsset,
        config: &DownloadConfig,
    ) -> Result<()> {
        let Some(db) = config.state_db.as_deref() else {
            return Ok(());
        };
        for task in &mut plan.tasks {
            let frozen = self.frozen_task_destination(asset, config, task).await?;
            if frozen.is_some() {
                continue;
            }
            // Nominate fresh storage, never adopt prior-generation bytes.
            if !self.path_has_completed_exact_target(task, &task.download_path)
                && self.path_has_prior_publication(task)
            {
                let requested = task.download_path.clone();
                self.choose_independent_sibling(db, task).await?;
                task.remember_superseded_requested(&config.directory, requested);
                continue;
            }
            self.relocate_protected_task(db, task).await?;
        }
        Ok(())
    }

    #[must_use]
    fn path_has_prior_publication(&self, task: &DownloadTask) -> bool {
        if task.checksum.trim().is_empty()
            || !self.path_has_compatible_owner(task, &task.download_path)
        {
            return false;
        }
        let Ok(key) = PathPlanningMode::Reconciliation.key(&task.download_path) else {
            return false;
        };
        let Some(owners) = self.reconciliation.path_owners.get(&key) else {
            return false;
        };
        owners
            .iter()
            .filter_map(|owner| self.reconciliation.downloaded_paths.get(owner))
            .flatten()
            .any(|record| {
                record.checksum != task.checksum.as_ref()
                    && !record.checksum.trim().is_empty()
                    && record.local_checksum.as_ref().is_some_and(|hash| {
                        hash.len() == 64 && hash.bytes().all(|byte| byte.is_ascii_hexdigit())
                    })
                    && record.local_path.as_ref().is_some_and(|path| {
                        PathPlanningMode::Reconciliation
                            .key(path)
                            .is_ok_and(|other| other == key)
                    })
            })
    }

    async fn relocate_protected_task(
        &mut self,
        db: &dyn DownloadStore,
        task: &mut DownloadTask,
    ) -> Result<()> {
        let independent =
            super::legacy_preservation::current_path_is_independent(db, &task.download_path)
                .await?;
        let exact_target = self.path_owned_by_exact_target(task, &task.download_path);
        let compatible_owner = self.path_has_compatible_owner(task, &task.download_path);
        let exact_completed = self.path_has_completed_exact_target(task, &task.download_path);
        if exact_completed
            || compatible_owner
            || (independent
                && (exact_target || !self.path_has_incompatible_owner(task, &task.download_path)))
        {
            return Ok(());
        }
        self.choose_independent_sibling(db, task).await
    }

    fn path_owned_by_exact_target(&self, task: &DownloadTask, path: &Path) -> bool {
        PathPlanningMode::Reconciliation
            .key(path)
            .ok()
            .and_then(|key| self.reconciliation.path_owners.get(&key))
            .is_some_and(|owners| {
                owners.iter().any(|owner| {
                    owner.library.as_ref() == task.library.as_ref()
                        && owner.asset_id.as_ref() == task.asset_id.as_ref()
                })
            })
    }

    fn path_has_compatible_owner(&self, task: &DownloadTask, path: &Path) -> bool {
        let Ok(key) = PathPlanningMode::Reconciliation.key(path) else {
            return false;
        };
        self.reconciliation
            .path_owners
            .get(&key)
            .is_some_and(|owners| {
                !owners.is_empty()
                    && owners.iter().all(|owner| {
                        same_logical_asset(
                            task.library.as_ref(),
                            task.asset_id.as_ref(),
                            task.asset_record_name.as_ref(),
                            owner.library.as_ref(),
                            owner.asset_id.as_ref(),
                            &self.legacy_owners,
                        )
                    })
            })
    }

    fn path_has_completed_exact_target(&self, task: &DownloadTask, path: &Path) -> bool {
        let Ok(key) = PathPlanningMode::Reconciliation.key(path) else {
            return false;
        };
        let owner = ReconciliationOwner {
            library: Arc::clone(&task.library),
            asset_id: task.asset_id.as_ref().into(),
            version_size: task.version_size,
        };
        self.reconciliation
            .downloaded_paths
            .get(&owner)
            .is_some_and(|records| {
                records.iter().any(|record| {
                    record.is_current_path
                        && record.checksum.as_str() == task.checksum.as_ref()
                        && record.local_checksum.is_some()
                        && record.local_path.as_ref().is_some_and(|record_path| {
                            crate::fs_util::confined_path_key(record_path)
                                .ok()
                                .is_some_and(|record_key| record_key.as_str() == key.as_ref())
                        })
                })
            })
    }

    fn path_has_incompatible_owner(&self, task: &DownloadTask, path: &Path) -> bool {
        let Ok(key) = PathPlanningMode::Reconciliation.key(path) else {
            return true;
        };
        self.reconciliation
            .path_owners
            .get(&key)
            .is_some_and(|owners| {
                owners.iter().any(|owner| {
                    !same_logical_asset(
                        task.library.as_ref(),
                        task.asset_id.as_ref(),
                        task.asset_record_name.as_ref(),
                        owner.library.as_ref(),
                        owner.asset_id.as_ref(),
                        &self.legacy_owners,
                    )
                })
            })
    }

    pub(super) async fn choose_independent_sibling(
        &mut self,
        db: &dyn DownloadStore,
        task: &mut DownloadTask,
    ) -> Result<()> {
        let protected_key = self.path_mode.key(&task.download_path)?;
        if self.claimed_paths.get(protected_key.as_ref()) == Some(&task.size) {
            self.claimed_paths.remove(protected_key.as_ref());
        }
        let parent = task
            .download_path
            .parent()
            .context("protected download target has no parent")?;
        self.dir_cache.ensure_dir_async(parent).await;
        let filename = task
            .download_path
            .file_name()
            .and_then(|name| name.to_str())
            .context("protected download target has no filename")?;
        let mut ordinal: Option<u64> = None;
        loop {
            let candidate_filename = match ordinal {
                None => paths::insert_asset_identity_suffix(filename, &task.asset_id),
                Some(ordinal) => {
                    paths::insert_asset_identity_ordinal_suffix(filename, &task.asset_id, ordinal)
                }
            };
            let candidate = parent.join(candidate_filename);
            let key = self.path_mode.key(&candidate)?;
            let owner_key = PathPlanningMode::Reconciliation.key(&candidate)?;
            let foreign_owner =
                self.reconciliation
                    .path_owners
                    .get(&owner_key)
                    .is_some_and(|owners| {
                        owners.iter().any(|owner| {
                            !same_logical_asset(
                                task.library.as_ref(),
                                task.asset_id.as_ref(),
                                task.asset_record_name.as_ref(),
                                owner.library.as_ref(),
                                owner.asset_id.as_ref(),
                                &self.legacy_owners,
                            ) || owner.version_size != task.version_size
                        })
                    });
            let available = !foreign_owner
                && !self.claimed_paths.contains_key(key.as_ref())
                && !self.dir_cache.exists(&candidate)
                && super::legacy_preservation::current_path_is_independent(db, &candidate).await?;
            if available {
                self.claimed_paths.insert(key, task.size);
                task.download_path = candidate;
                return Ok(());
            }
            ordinal = Some(match ordinal {
                None => 2,
                Some(ordinal) => ordinal
                    .checked_add(1)
                    .context("protected identity suffix exhausted")?,
            });
        }
    }

    /// A durable retry may have recorded a destination in another directory.
    /// Replay that exact choice only beneath the current download root.
    pub(super) async fn plan_pending_retry_asset(
        &mut self,
        asset: &PhotoAsset,
        config: &DownloadConfig,
    ) -> Result<AssetTaskPlan> {
        let mut plan = self
            .plan_download_asset_with_replay(asset, config, ReservedReplay::PendingRetry)
            .await?;
        self.include_existing_comparison_tasks(asset, config, &mut plan);
        self.bind_publication_tasks(asset, config, &mut plan)
            .await?;
        Ok(plan)
    }

    async fn plan_download_asset_with_replay(
        &mut self,
        asset: &PhotoAsset,
        config: &DownloadConfig,
        replay: ReservedReplay,
    ) -> Result<AssetTaskPlan> {
        self.reconciliation.reservations.clear();
        if let Some(filter_reason) = is_asset_filtered(asset, config) {
            return Ok(AssetTaskPlan {
                tasks: Vec::new(),
                filter_reason: Some(filter_reason),
                malformed_resource: None,
            });
        }
        let derived = super::filter::derive_expected_paths(asset, config);
        let primary_path = if derived
            .iter()
            .any(|path| path.version_size.is_live_photo_motion())
            && !self
                .current_motion_paths_exist(asset, config, &derived)
                .await
        {
            if let Some(primary) = derived
                .iter()
                .find(|path| path.version_size.is_primary_media())
            {
                self.verified_downloaded_path(asset, config, primary.version_size)
                    .await
            } else {
                None
            }
        } else {
            None
        };
        let primary_filename = primary_path
            .as_deref()
            .and_then(Path::file_name)
            .and_then(|name| name.to_str());
        match self.path_mode {
            PathPlanningMode::Download if primary_filename.is_none() => {
                Ok(self.plan_asset(asset, config).await)
            }
            PathPlanningMode::Download => {
                self.plan_asset_with_mode(
                    asset,
                    config,
                    PathPlanningMode::Download,
                    primary_filename,
                )
                .await
            }
            // Adoption verifies existing files separately. Reuse the
            // ownership-aware plan, including exact saved destination choices.
            mode => {
                self.plan_owned_asset(asset, config, mode, primary_filename, replay)
                    .await
            }
        }
    }

    /// Reject foreign ownership before adoption or recorded-path overrides.
    /// An invalid confined path is never permission to claim a reservation.
    #[must_use]
    pub(super) fn retry_path_allowed(
        &self,
        library: &str,
        asset_id: &str,
        actual_child: &str,
        version_size: VersionSizeKey,
        checksum: &str,
        size: u64,
        path: &Path,
    ) -> bool {
        if matches!(self.path_mode, PathPlanningMode::Download) {
            return true;
        }
        self.path_mode.key(path).is_ok_and(|key| {
            self.content_path_allowed(&key, checksum, size)
                && self
                    .reconciliation
                    .path_owners
                    .get(&key)
                    .is_none_or(|owners| {
                        owners.iter().all(|owner| {
                            same_logical_asset(
                                library,
                                asset_id,
                                actual_child,
                                owner.library.as_ref(),
                                owner.asset_id.as_ref(),
                                &self.legacy_owners,
                            ) && owner.version_size == version_size
                        })
                    })
        })
    }

    fn content_path_allowed(&self, key: &NormalizedPath, checksum: &str, size: u64) -> bool {
        self.reconciliation
            .path_contents
            .get(key)
            .is_none_or(|contents| {
                contents.iter().all(|content| {
                    content.as_ref().is_some_and(|content| {
                        content.checksum.as_ref() == checksum && content.size == size
                    })
                })
            })
    }

    /// A saved choice for this generation cannot be displaced by an older receipt.
    pub(super) fn conflicts_with_durable_destination(
        &self,
        task: &DownloadTask,
        candidate: &Path,
    ) -> bool {
        self.conflicts_with_durable_choice(
            &task.library,
            &task.asset_id,
            task.version_size,
            &task.checksum,
            task.size,
            candidate,
        )
    }

    pub(super) fn conflicts_with_durable_choice(
        &self,
        library: &str,
        asset_id: &str,
        version: VersionSizeKey,
        checksum: &str,
        size: u64,
        candidate: &Path,
    ) -> bool {
        let Ok(candidate_key) = PathPlanningMode::Reconciliation.key(candidate) else {
            return true;
        };
        self.reconciliation.durable_destinations.iter().any(|key| {
            *key != candidate_key
                && self
                    .reconciliation
                    .path_owners
                    .get(key)
                    .is_some_and(|owners| {
                        owners.iter().any(|owner| {
                            owner.library.as_ref() == library
                                && owner.asset_id.as_ref() == asset_id
                                && owner.version_size == version
                        })
                    })
                && self
                    .reconciliation
                    .path_contents
                    .get(key)
                    .is_none_or(|contents| {
                        contents.iter().any(|content| {
                            content.as_ref().is_none_or(|content| {
                                content.checksum.as_ref() == checksum && content.size == size
                            })
                        })
                    })
        })
    }

    /// A committed choice wins over an older recorded source path on restart.
    #[must_use]
    pub(super) fn has_durable_destination(&self, task: &DownloadTask) -> bool {
        self.path_mode
            .key(&task.download_path)
            .is_ok_and(|key| self.reconciliation.durable_destinations.contains(&key))
            && self.retry_path_allowed(
                &task.library,
                &task.asset_id,
                task.asset_record_name.as_ref(),
                task.version_size,
                &task.checksum,
                task.size,
                &task.download_path,
            )
    }

    /// Inspect every saved alias before a pass can fall back to size-only adoption.
    pub(super) fn cross_parent_retry_destinations(
        &self,
        library: &str,
        asset_id: &str,
        version_size: VersionSizeKey,
        checksum: &str,
        size: u64,
    ) -> Result<Vec<std::path::PathBuf>> {
        let mut destinations = Vec::new();
        for ((owner, content, requested), destination) in &self.reconciliation.destinations {
            if owner.library.as_ref() != library
                || owner.asset_id.as_ref() != asset_id
                || owner.version_size != version_size
                || content.checksum.as_ref() != checksum
                || content.size != size
            {
                continue;
            }
            let destination_key = PathPlanningMode::Reconciliation.key(destination)?;
            if Path::new(requested.as_ref()).parent()
                != Path::new(destination_key.as_ref()).parent()
                && !destinations.contains(destination)
            {
                destinations.push(destination.clone());
            }
        }
        Ok(destinations)
    }

    fn add_reconciliation_claim(
        &mut self,
        owner: ReconciliationOwner,
        path: NormalizedPath,
        size: u64,
    ) {
        self.claimed_paths.insert(path.clone(), size);
        self.reconciliation
            .path_owners
            .entry(path.clone())
            .or_default()
            .insert(owner.clone());
        self.reconciliation
            .claims
            .entry(owner)
            .or_default()
            .insert(path, size);
    }

    pub(super) fn reconciliation_reservations(
        &self,
    ) -> impl Iterator<Item = &ReconciliationReservation> {
        self.reconciliation
            .reservations
            .iter()
            .map(|planned| &planned.reservation)
    }

    /// Commit final choices, including recorded-path overrides, before dispatch.
    /// Drain each plan once so a streaming pass does not retain a growing write batch.
    pub(super) async fn persist_download_reservations(
        &mut self,
        db: &dyn DownloadStore,
        tasks: &[DownloadTask],
    ) -> Result<()> {
        anyhow::ensure!(
            !self.reconciliation.reservations.iter().any(|planned| {
                planned.origin == ReservationOrigin::PriorPublication
                    && tasks.iter().any(|task| {
                        task.library == planned.reservation.library
                            && task.asset_id.as_ref() == planned.reservation.asset_id.as_ref()
                            && task.version_size == planned.reservation.version_size
                    })
            }),
            "Unconfirmed prior publication cannot use ordinary reservation writer"
        );
        let mut reservations: Vec<_> = std::mem::take(&mut self.reconciliation.reservations)
            .into_iter()
            .map(|planned| planned.reservation)
            .collect();
        reservations.retain_mut(|reservation| {
            let Some(task) = tasks.iter().find(|task| {
                task.library == reservation.library
                    && task.asset_id.as_ref() == reservation.asset_id.as_ref()
                    && task.version_size == reservation.version_size
            }) else {
                return false;
            };
            reservation.destination_path = task.download_path.clone();
            true
        });
        for reservation in &mut reservations {
            reservation.destination_path =
                crate::fs_util::absolute_confined_path(&reservation.destination_path)?;
            reservation.destination_path_key = ReconciliationPathKey(
                PathPlanningMode::Reconciliation
                    .key(&reservation.destination_path)?
                    .as_ref()
                    .to_owned(),
            );
        }
        if !reservations.is_empty() {
            let mut children = reservations.iter().filter_map(|reservation| {
                tasks
                    .iter()
                    .find(|task| {
                        task.library == reservation.library
                            && task.asset_id.as_ref() == reservation.asset_id.as_ref()
                            && task.version_size == reservation.version_size
                    })
                    .map(|task| task.asset_record_name.as_ref())
            });
            let first_child = children.next().map(str::to_owned);
            let actual_child =
                first_child.filter(|child| children.all(|candidate| candidate == child));
            db.reserve_reconciliation_paths(&reservations, actual_child.as_deref())
                .await?;
            self.remember_download_reservations(reservations);
        }
        Ok(())
    }

    fn remember_download_reservations(&mut self, reservations: Vec<ReconciliationReservation>) {
        for reservation in reservations {
            let Some(content) = reservation.content else {
                continue;
            };
            self.reconciliation
                .path_contents
                .entry(NormalizedPath::from_key(
                    reservation.destination_path_key.clone(),
                ))
                .or_default()
                .insert(Some(content.clone()));
            self.reconciliation
                .durable_destinations
                .insert(NormalizedPath::from_key(reservation.destination_path_key));
            self.reconciliation.destinations.insert(
                (
                    ReconciliationOwner {
                        library: reservation.library,
                        asset_id: reservation.asset_id,
                        version_size: reservation.version_size,
                    },
                    content,
                    NormalizedPath::from_key(reservation.requested_path_key),
                ),
                reservation.destination_path,
            );
        }
    }

    /// Convert one asset into download tasks after applying the shared
    /// asset-level filters and path-aware on-disk checks.
    pub(super) async fn plan_asset(
        &mut self,
        asset: &PhotoAsset,
        config: &DownloadConfig,
    ) -> AssetTaskPlan {
        #[expect(
            clippy::expect_used,
            reason = "ordinary download keys never perform fallible path resolution"
        )]
        self.plan_asset_with_mode(asset, config, PathPlanningMode::Download, None)
            .await
            .expect("ordinary download planning uses infallible spelling-only path keys")
    }

    async fn plan_owned_asset(
        &mut self,
        asset: &PhotoAsset,
        config: &DownloadConfig,
        mode: PathPlanningMode,
        primary_filename: Option<&str>,
        replay: ReservedReplay,
    ) -> Result<AssetTaskPlan> {
        let expected = super::filter::expected_paths_for(asset, config);
        let library = asset
            .source_zone()
            .map(Arc::from)
            .unwrap_or_else(|| Arc::clone(&config.library));
        let mut owned = FxHashMap::default();
        for requested in &expected {
            let owner = ReconciliationOwner {
                library: Arc::clone(&library),
                asset_id: asset.state_id().into(),
                version_size: requested.version_size,
            };
            let claims = self
                .reconciliation
                .claims
                .remove(&owner)
                .unwrap_or_default();
            // Keep unselected renditions and shared legacy paths occupied.
            for path in claims.keys() {
                if self
                    .reconciliation
                    .path_owners
                    .get(path)
                    .is_some_and(|owners| owners.len() == 1)
                    && self.content_path_allowed(path, &requested.checksum, requested.size)
                {
                    self.claimed_paths.remove(path);
                }
            }
            owned.insert(owner, claims);
        }
        let plan = self
            .plan_asset_with_mode(asset, config, mode, primary_filename)
            .await
            .and_then(|mut plan| {
                for task in &mut plan.tasks {
                    let Some(requested) = expected
                        .iter()
                        .find(|path| path.version_size == task.version_size)
                    else {
                        anyhow::bail!("reconciliation task has no requested path");
                    };
                    let requested_key = PathPlanningMode::Reconciliation.key(&requested.path)?;
                    let owner = ReconciliationOwner {
                        library: Arc::clone(&task.library),
                        asset_id: task.asset_id.as_ref().into(),
                        version_size: task.version_size,
                    };
                    let content = ReconciliationContent {
                        checksum: task.checksum.clone(),
                        size: task.size,
                    };
                    let slot = (owner.clone(), content.clone(), requested_key.clone());
                    let mut destination = self.reconciliation.destinations.get(&slot).cloned();
                    let origin = if destination.is_some() {
                        ReservationOrigin::ExactReplayCandidate
                    } else {
                        ReservationOrigin::OrdinaryCollision
                    };
                    let mut reused_logical_reservation = false;
                    if destination.is_none() {
                        // A previously recorded owner may be the same logical
                        // asset under an explicit legacy claim. Reuse its exact
                        // saved destination without rewriting the durable row.
                        let mut choices = self
                            .reconciliation
                            .destinations
                            .iter()
                            .filter(|((saved_owner, saved_content, _), _)| {
                                saved_content == &content
                                    && saved_owner.library.as_ref() == task.library.as_ref()
                                    && saved_owner.version_size == task.version_size
                                    && (matches!(replay, ReservedReplay::PendingRetry)
                                        || saved_owner.asset_id.as_ref()
                                            != task.asset_id.as_ref())
                                    && same_logical_asset(
                                        task.library.as_ref(),
                                        task.asset_id.as_ref(),
                                        task.asset_record_name.as_ref(),
                                        saved_owner.library.as_ref(),
                                        saved_owner.asset_id.as_ref(),
                                        &self.legacy_owners,
                                    )
                            })
                            .map(|((saved_owner, _, _), path)| {
                                (
                                    path.clone(),
                                    saved_owner.asset_id.as_ref() != task.asset_id.as_ref(),
                                )
                            });
                        if let Some((first, logical)) = choices.next() {
                            anyhow::ensure!(
                                choices.all(|(path, _)| path == first),
                                "logical owner has multiple recorded destinations"
                            );
                            destination = Some(first);
                            reused_logical_reservation = logical;
                        }
                    }
                    if let Some(destination) = destination.as_ref() {
                        let planned_key =
                            PathPlanningMode::Reconciliation.key(&task.download_path)?;
                        self.claimed_paths.remove(&planned_key);
                        let destination_key = PathPlanningMode::Reconciliation.key(destination)?;
                        if self
                            .reconciliation
                            .path_owners
                            .get(&destination_key)
                            .is_some_and(|owners| {
                                owners.iter().all(|owner| {
                                    same_logical_asset(
                                        task.library.as_ref(),
                                        task.asset_id.as_ref(),
                                        task.asset_record_name.as_ref(),
                                        owner.library.as_ref(),
                                        owner.asset_id.as_ref(),
                                        &self.legacy_owners,
                                    )
                                })
                            })
                        {
                            self.claimed_paths.remove(&destination_key);
                        }
                        anyhow::ensure!(
                            !self.claimed_paths.contains_key(&destination_key),
                            "reserved reconciliation destination has another owner"
                        );
                        match replay {
                            ReservedReplay::RequestedDirectory => {
                                let filename = destination.file_name().ok_or_else(|| {
                                    anyhow::anyhow!("reserved destination has no filename")
                                })?;
                                // Preserve the current root spelling while replaying the
                                // exact reserved leaf, including identity/ordinal suffixes.
                                task.download_path = requested.path.with_file_name(filename);
                                anyhow::ensure!(
                                    PathPlanningMode::Reconciliation.key(&task.download_path)?
                                        == destination_key,
                                    "reserved reconciliation destination left its requested directory"
                                );
                            }
                            ReservedReplay::PendingRetry => {
                                anyhow::ensure!(
                                    self.retry_path_allowed(
                                        &task.library,
                                        &task.asset_id,
                                        task.asset_record_name.as_ref(),
                                        task.version_size,
                                        &task.checksum,
                                        task.size,
                                        destination,
                                    ),
                                    "reserved pending retry destination has conflicting ownership or content"
                                );
                                task.download_path = crate::fs_util::reserved_path_under_root(
                                    &config.directory, destination,
                                )?;
                                anyhow::ensure!(
                                    PathPlanningMode::Reconciliation.key(&task.download_path)?
                                        == destination_key,
                                    "pending retry destination does not match its reservation"
                                );
                            }
                        }
                    }
                    let destination_key =
                        PathPlanningMode::Reconciliation.key(&task.download_path)?;
                    self.reconciliation
                        .path_contents
                        .entry(destination_key.clone())
                        .or_default()
                        .insert(Some(content.clone()));
                    self.claimed_paths
                        .insert(destination_key.clone(), task.size);
                    owned
                        .entry(owner)
                        .or_default()
                        .insert(destination_key.clone(), task.size);
                    self.reconciliation
                        .destinations
                        .insert(slot, task.download_path.clone());
                    if !reused_logical_reservation {
                        self.reconciliation
                            .reservations
                            .push(PlannedReservation {
                                origin,
                                reservation: ReconciliationReservation {
                                content: Some(content),
                                library: Arc::clone(&task.library),
                                asset_id: task.asset_id.as_ref().into(),
                                version_size: task.version_size,
                                requested_path_key: ReconciliationPathKey(
                                    requested_key.as_ref().to_owned(),
                                ),
                                destination_path_key: ReconciliationPathKey(
                                    destination_key.as_ref().to_owned(),
                                ),
                                destination_path: crate::fs_util::absolute_confined_path(
                                    &task.download_path,
                                )?,
                                },
                            });
                    }
                }
                Ok(plan)
            });
        for (owner, claims) in owned {
            for (path, size) in claims {
                self.add_reconciliation_claim(owner.clone(), path, size);
            }
        }
        plan
    }

    async fn plan_asset_with_mode(
        &mut self,
        asset: &PhotoAsset,
        config: &DownloadConfig,
        planning_mode: PathPlanningMode,
        primary_filename: Option<&str>,
    ) -> Result<AssetTaskPlan> {
        if let Some(filter_reason) = is_asset_filtered(asset, config) {
            return Ok(AssetTaskPlan {
                tasks: Vec::new(),
                filter_reason: Some(filter_reason),
                malformed_resource: None,
            });
        }

        pre_ensure_asset_dir(&mut self.dir_cache, asset, config).await;
        let mut tasks = if primary_filename.is_some() {
            super::filter::filter_asset_to_tasks_with_primary(
                asset,
                config,
                &mut self.claimed_paths,
                &mut self.dir_cache,
                planning_mode,
                primary_filename,
            )?
        } else {
            filter_asset_to_tasks(
                asset,
                config,
                &mut self.claimed_paths,
                &mut self.dir_cache,
                planning_mode,
            )?
        };
        if self.ownership_incomplete {
            for task in &mut tasks {
                task.require_created_entry = true;
            }
        }
        let malformed_resource = if tasks.is_empty() {
            super::filter::malformed_no_task_resource(asset, config)
        } else {
            None
        };
        Ok(AssetTaskPlan {
            tasks,
            filter_reason: None,
            malformed_resource,
        })
    }

    pub(super) async fn prepare_path_parent(&mut self, path: &Path) {
        if let Some(parent) = path.parent() {
            self.dir_cache.ensure_dir_async(parent).await;
        }
    }

    fn retry_claim_available(&self, task: &DownloadTask, path: &Path) -> bool {
        self.retry_path_allowed(
            &task.library,
            &task.asset_id,
            task.asset_record_name.as_ref(),
            task.version_size,
            &task.checksum,
            task.size,
            path,
        ) && self.path_mode.key(path).is_ok_and(|key| {
            !self.claimed_paths.contains_key(&key)
                // Ordinary catalog receipts must not release another in-flight claim.
                || (!matches!(self.path_mode, PathPlanningMode::Download)
                    && self.reconciliation.path_owners.contains_key(&key))
        })
    }

    pub(super) fn retain_retry_claim(&mut self, task: &DownloadTask) -> Result<()> {
        if !matches!(self.path_mode, PathPlanningMode::Download) {
            self.add_reconciliation_claim(
                ReconciliationOwner {
                    library: Arc::clone(&task.library),
                    asset_id: task.asset_id.as_ref().into(),
                    version_size: task.version_size,
                },
                self.path_mode.key(&task.download_path)?,
                task.size,
            );
        }
        Ok(())
    }

    pub(super) async fn claim_recorded_repair_path(
        &mut self,
        recorded_path: &Path,
        task: &DownloadTask,
    ) -> bool {
        let Ok(planned) = self.path_mode.key(&task.download_path) else {
            return false;
        };
        if matches!(self.path_mode, PathPlanningMode::Download)
            && self.claimed_paths.get(planned.as_ref()) == Some(&task.size)
        {
            self.claimed_paths.remove(planned.as_ref());
        }

        let Ok(recorded) = self.path_mode.key(recorded_path) else {
            return false;
        };
        if !self.retry_claim_available(task, recorded_path) {
            return false;
        }
        self.prepare_path_parent(recorded_path).await;
        self.claimed_paths.insert(recorded, task.size);
        true
    }

    pub(super) async fn resolve_recorded_retry_path(
        &mut self,
        recorded_path: &Path,
        task: &DownloadTask,
    ) -> Option<std::path::PathBuf> {
        // Release ordinary in-flight claims when choosing a recorded path.
        // Durable reservations stay occupied even if this task chooses a sibling.
        let planned = self.path_mode.key(&task.download_path).ok()?;
        if matches!(self.path_mode, PathPlanningMode::Download)
            && self.claimed_paths.get(planned.as_ref()) == Some(&task.size)
        {
            self.claimed_paths.remove(planned.as_ref());
        }

        let parent = recorded_path.parent()?;
        let filename = recorded_path.file_name()?.to_str()?;
        self.dir_cache.ensure_dir_async(parent).await;

        let existing_size = self.dir_cache.file_size(recorded_path);
        let normalized = self.path_mode.key(recorded_path).ok()?;
        if existing_size.is_none() && self.retry_claim_available(task, recorded_path) {
            self.claimed_paths.insert(normalized, task.size);
            return Some(recorded_path.to_path_buf());
        }

        let mut tried = Vec::<Box<str>>::with_capacity(4);
        let preferred = if existing_size == Some(task.size) {
            paths::insert_asset_identity_suffix(filename, &task.asset_id)
        } else {
            paths::add_dedup_suffix(filename, task.size)
        };
        for candidate in [
            preferred,
            paths::insert_asset_identity_suffix(filename, &task.asset_id),
        ] {
            if let Some(path) = self.available_recorded_retry_sibling(parent, candidate, &mut tried)
            {
                self.claimed_paths
                    .insert(self.path_mode.key(&path).ok()?, task.size);
                return Some(path);
            }
        }

        let mut ordinal = 2u64;
        loop {
            let candidate =
                paths::insert_asset_identity_ordinal_suffix(filename, &task.asset_id, ordinal);
            if let Some(path) = self.available_recorded_retry_sibling(parent, candidate, &mut tried)
            {
                self.claimed_paths
                    .insert(self.path_mode.key(&path).ok()?, task.size);
                return Some(path);
            }
            ordinal = ordinal.checked_add(1)?;
        }
    }

    fn available_recorded_retry_sibling(
        &mut self,
        parent: &Path,
        filename: String,
        tried: &mut Vec<Box<str>>,
    ) -> Option<std::path::PathBuf> {
        let path = parent.join(filename);
        let normalized = self.path_mode.key(&path).ok()?;
        if tried
            .iter()
            .any(|seen| seen.as_ref() == normalized.as_ref())
        {
            return None;
        }
        tried.push(normalized.as_ref().into());

        (!self.dir_cache.exists(&path) && !self.claimed_paths.contains_key(normalized.as_ref()))
            .then_some(path)
    }
}

/// Result of planning a single asset.
#[derive(Debug)]
pub(super) struct AssetTaskPlan {
    pub(super) tasks: Vec<DownloadTask>,
    pub(super) filter_reason: Option<FilterReason>,
    pub(super) malformed_resource: Option<MalformedTaskResource>,
}

/// Persist the pending state row that a later `mark_downloaded` /
/// `mark_failed` call will finalize.
pub(super) async fn upsert_seen_for_task<D>(
    db: &D,
    config: &DownloadConfig,
    asset: &PhotoAsset,
    task: &DownloadTask,
) -> Result<(), crate::state::error::StateError>
where
    D: DownloadStateStore + ?Sized,
{
    upsert_asset_master_mapping(db, &task.library, asset).await?;

    db.upsert_seen(&pending_record_for_task(config, asset, task))
        .await
}

pub(super) fn pending_record_for_task(
    config: &DownloadConfig,
    asset: &PhotoAsset,
    task: &DownloadTask,
) -> AssetRecord {
    let media_type = determine_media_type(task.version_size, asset);
    AssetRecord::new_pending(
        Arc::clone(&task.library),
        task.asset_id.to_string(),
        task.version_size,
        task.checksum.to_string(),
        task.download_path
            .file_name()
            .and_then(|f| f.to_str())
            .unwrap_or("")
            .to_string(),
        asset.created(),
        Some(asset.added_date()),
        task.size,
        media_type,
    )
    .with_metadata_arc(super::filter::metadata_for_selected_version(
        asset,
        config,
        task.version_size,
    ))
}

/// Persist the durable CloudKit identifier bridge used to resolve future
/// `CPLAsset` hard-delete tombstones back to the matching state row family.
pub(super) async fn upsert_asset_master_mapping<D>(
    db: &D,
    library: &str,
    asset: &PhotoAsset,
) -> Result<(), crate::state::error::StateError>
where
    D: DownloadStateStore + ?Sized,
{
    db.upsert_asset_master_mapping(library, asset.asset_record_name(), asset.id())
        .await
}

/// Record an asset's membership in the current concrete album/smart-folder
/// pass. Returns `Ok(())` without touching the DB when the pass is not
/// album-scoped.
pub(super) async fn record_album_membership_if_named<D>(
    db: &D,
    config: &DownloadConfig,
    asset: &PhotoAsset,
) -> Result<(), crate::state::error::StateError>
where
    D: MembershipStore + ?Sized,
{
    // An unfiled path label is not evidence of actual album membership.
    // Recording it would exclude the asset from later unfiled reconciliation.
    if config.primary_layout_pass_kind == Some(crate::commands::PassKind::Unfiled) {
        return Ok(());
    }
    let Some(album_name) = config.album_name.as_deref().filter(|name| !name.is_empty()) else {
        return Ok(());
    };
    let library = asset.source_zone().unwrap_or(&config.library);
    add_asset_album_with_retry(db, library, asset.state_id(), album_name, "icloud").await
}

/// Bounded retry attempts for `add_asset_album`. SQLite-busy under WAL
/// contention is the dominant transient failure; three attempts at
/// 200ms / 400ms / 800ms cover the common case while staying short enough
/// that a wedged DB doesn't stall the producer indefinitely. After retries
/// are exhausted the caller logs the persistent failure.
pub(super) const ADD_ASSET_ALBUM_MAX_RETRIES: u32 = 3;

/// Insert an asset/album row with a bounded inline retry loop. The
/// underlying call is `INSERT OR IGNORE` so retries are idempotent. Returns
/// the final result so the caller can log on persistent failure.
pub(super) async fn add_asset_album_with_retry<D>(
    db: &D,
    library: &str,
    asset_id: &str,
    album_name: &str,
    source: &str,
) -> Result<(), crate::state::error::StateError>
where
    D: MembershipStore + ?Sized,
{
    use rand::RngExt;
    let mut last_err: Option<crate::state::error::StateError> = None;
    for attempt in 1..=ADD_ASSET_ALBUM_MAX_RETRIES {
        match db
            .add_asset_album(library, asset_id, album_name, source)
            .await
        {
            Ok(()) => return Ok(()),
            Err(e) => {
                if attempt < ADD_ASSET_ALBUM_MAX_RETRIES {
                    tracing::debug!(
                        asset_id,
                        album = album_name,
                        library,
                        attempt,
                        error = %e,
                        "add_asset_album retry"
                    );
                    let base_ms = 200u64 * u64::from(1u32 << (attempt - 1));
                    let jitter_ms = rand::rng().random_range(0..base_ms.max(1) / 4);
                    tokio::time::sleep(Duration::from_millis(base_ms + jitter_ms)).await;
                }
                last_err = Some(e);
            }
        }
    }
    // ADD_ASSET_ALBUM_MAX_RETRIES is `>= 1` (compile-time-checked below) so
    // `last_err` is always populated when the loop exits. The fallback to
    // `LockPoisoned` is a defensive landing the type system cannot otherwise
    // statically rule out.
    Err(last_err.unwrap_or_else(|| {
        crate::state::error::StateError::LockPoisoned(
            "add_asset_album_with_retry: no attempts ran".into(),
        )
    }))
}

const _: () = assert!(
    ADD_ASSET_ALBUM_MAX_RETRIES >= 1,
    "ADD_ASSET_ALBUM_MAX_RETRIES must be at least 1; otherwise the retry helper never calls the DB"
);

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use rustc_hash::FxHashSet;
    use tempfile::TempDir;

    use crate::commands::{AlbumPass, PassKind};
    use crate::icloud::photos::{PhotoAlbum, PhotoAsset};
    use crate::state::SqliteStateDb;
    use crate::test_helpers::TestPhotoAsset;
    use serde_json::json;

    use super::*;

    #[test]
    fn confirmed_claim_preflights_both_views_and_is_scoped_and_idempotent() {
        let accepts = |p: &TaskPlanner, child| {
            (
                p.legacy_owners.accepts_claim("z", "m", child),
                p.baseline_matcher.accepts_legacy_claim("z", "m", child),
            )
        };
        for planner_conflict in [true, false] {
            let mut planner = TaskPlanner::new();
            if planner_conflict {
                planner.legacy_owners.remember_claim("z", "m", "b");
            } else {
                planner
                    .baseline_matcher
                    .remember_legacy_claim("z", "m", "b");
            }
            assert!(planner.remember_legacy_owner("z", "m", "a").is_err());
            assert_eq!(
                accepts(&planner, "a"),
                (!planner_conflict, planner_conflict)
            );
            // Refusal left the previously empty peer empty, not partially updated.
            assert_eq!(
                accepts(&planner, "c"),
                (!planner_conflict, planner_conflict)
            );
        }
        let mut planner = TaskPlanner::new();
        for _ in 0..2 {
            planner.remember_legacy_owner("z", "m", "a").unwrap();
        }
        assert_eq!(accepts(&planner, "a"), (true, true));
        assert_eq!(accepts(&planner, "b"), (false, false));
        assert!(planner.legacy_owners.accepts_claim("other", "m", "b"));
        assert!(
            planner
                .baseline_matcher
                .accepts_legacy_claim("other", "m", "b")
        );
    }

    fn test_config(root: &std::path::Path) -> DownloadConfig {
        let mut config = DownloadConfig::test_default();
        config.directory = Arc::from(root);
        config.folder_structure = "%Y/%m/%d".to_string();
        config.folder_structure_albums = Arc::from("{album}/%Y/%m/%d");
        config
    }

    #[tokio::test]
    async fn prior_publication_relocation_respects_current_and_frozen_paths() {
        use crate::state::{DownloadContextStateStore, ReconciliationStateStore};
        use crate::test_helpers::TestAssetRecord;
        use sha2::{Digest, Sha256};
        for case in [
            "new", "cross", "current", "frozen", "missing", "unproven", "empty", "blank",
        ] {
            let bytes = b"prior publication";
            let root = TempDir::new().unwrap();
            let db_path = root.path().join("state.db");
            let db = Arc::new(SqliteStateDb::open(&db_path).await.unwrap());
            let mut config = test_config(root.path());
            config.state_db = Some(db.clone());
            let asset = TestPhotoAsset::new("GENERATION")
                .filename("photo.JPG")
                .orig_checksum("selected")
                .orig_size(bytes.len() as u64)
                .build();
            let mut seed = TaskPlanner::for_reconciliation(Vec::new(), Vec::new()).unwrap();
            let mut plan = seed.plan_download_asset(&asset, &config).await.unwrap();
            let task = &plan.tasks[0];
            let path = task.download_path.clone();
            let version = if case == "cross" {
                VersionSizeKey::Adjusted
            } else {
                task.version_size
            };
            let hash = data_encoding::HEXLOWER.encode(&Sha256::digest(bytes));
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, bytes).unwrap();
            let mut downloaded = Vec::new();
            for checksum in ["prior", "selected"] {
                if checksum == "selected" && case != "current" {
                    continue;
                }
                db.upsert_seen(
                    &TestAssetRecord::new(asset.id())
                        .library(&task.library)
                        .version_size(version)
                        .checksum(checksum)
                        .size(bytes.len() as u64)
                        .build(),
                )
                .await
                .unwrap();
                db.mark_verified_download(
                    &task.library,
                    asset.id(),
                    version.as_str(),
                    &path,
                    &hash,
                    Some(&hash),
                    false,
                )
                .await
                .unwrap();
                downloaded.extend(db.get_downloaded_path_records().await.unwrap());
            }
            if case == "unproven" {
                downloaded[0].local_checksum = None;
            }
            let mut catalog = db.get_reconciliation_catalog_paths().await.unwrap();
            if case == "missing" {
                catalog.clear();
            }
            let reservations = if matches!(case, "frozen" | "missing") {
                seed.reconciliation_reservations().cloned().collect()
            } else {
                Vec::new()
            };
            db.reserve_reconciliation_paths(&reservations, None)
                .await
                .unwrap();
            let saved = db.get_reconciliation_reservations().await.unwrap();
            assert_eq!(saved, reservations);
            let mut planner = TaskPlanner::for_reconciliation(catalog, saved).unwrap();
            planner.add_downloaded_paths(downloaded);
            if matches!(case, "empty" | "blank") {
                plan.tasks[0].checksum = if case == "empty" { "" } else { " \t" }.into();
            }
            assert_eq!(
                planner.path_has_prior_publication(&plan.tasks[0]),
                !matches!(case, "unproven" | "empty" | "blank")
            );
            planner
                .relocate_protected_tasks(&mut plan, &asset, &config)
                .await
                .unwrap();
            let relocate = matches!(case, "new" | "cross");
            assert_eq!(plan.tasks[0].download_path != path, relocate, "{case}");
            assert_eq!(
                plan.tasks[0].superseded_requested_path.is_some(),
                relocate,
                "{case}"
            );
            assert_eq!(
                plan.tasks[0].superseded_requested(&config.directory, &path),
                relocate.then_some(path.as_path()),
                "{case}"
            );
            if relocate {
                let sibling = paths::insert_asset_identity_suffix("photo.JPG", asset.id());
                assert_eq!(
                    plan.tasks[0].download_path,
                    path.with_file_name(sibling),
                    "{case}"
                );
            }
            assert_eq!(std::fs::read(&path).unwrap(), bytes, "{case}");
            let reopened = SqliteStateDb::open(&db_path).await.unwrap();
            assert_eq!(
                reopened.get_reconciliation_reservations().await.unwrap(),
                reservations
            );
            if matches!(case, "frozen" | "missing") {
                let prior = reopened.get_downloaded_path_records().await.unwrap();
                assert_eq!(prior.len(), 1);
                assert_eq!(prior[0].checksum, "prior");
                assert_eq!(prior[0].local_path.as_ref(), Some(&path));
                assert_eq!(prior[0].local_checksum.as_deref(), Some(hash.as_str()));
                assert_eq!(reservations.len(), 1);
                let current = &reservations[0];
                assert_eq!(current.destination_path, path);
                let content = current.content.as_ref().unwrap();
                assert_eq!(content.checksum.as_ref(), "selected");
                assert_eq!(content.size, bytes.len() as u64);
            }
        }
    }

    #[tokio::test]
    async fn current_live_photo_and_filtered_assets_do_not_read_receipts() {
        use sha2::{Digest, Sha256};
        use std::sync::atomic::Ordering;
        let dir = TempDir::new().unwrap();
        let db = SqliteStateDb::open_in_memory().unwrap();
        let bytes = vec![1u8; 4096];
        let hash = data_encoding::HEXLOWER.encode(&Sha256::digest(&bytes));
        let asset = TestPhotoAsset::new("STEADY_LIVE")
            .filename("PHOTO.HEIC")
            .item_type("public.heic")
            .orig_file_type("public.heic")
            .orig_size(bytes.len() as u64)
            .orig_checksum("still")
            .live_photo(
                "https://p01.icloud-content.com/motion",
                "motion",
                bytes.len() as u64,
            )
            .build();
        let mut config = test_config(dir.path());
        let derived = super::super::filter::derive_expected_paths(&asset, &config);
        for path in &derived {
            std::fs::create_dir_all(path.path.parent().unwrap()).unwrap();
            std::fs::write(&path.path, &bytes).unwrap();
            let record = crate::test_helpers::TestAssetRecord::new(asset.id())
                .version_size(path.version_size)
                .checksum(&path.checksum)
                .filename(&path.filename)
                .size(path.size)
                .build();
            db.upsert_seen(&record).await.unwrap();
            db.mark_downloaded(
                "PrimarySync",
                asset.id(),
                path.version_size.as_str(),
                &path.path,
                &hash,
                Some(&hash),
            )
            .await
            .unwrap();
        }
        for _ in 0..2 {
            let mut planner = TaskPlanner::for_download(Some(&db)).await.unwrap();
            planner.plan_download_asset(&asset, &config).await.unwrap();
            assert_eq!(
                planner
                    .reconciliation
                    .receipt_file_reads
                    .load(Ordering::Relaxed),
                0
            );
        }
        let motion = derived
            .iter()
            .find(|path| path.version_size.is_live_photo_motion())
            .unwrap();
        std::fs::remove_file(&motion.path).unwrap();
        config.filename_exclude = vec![glob::Pattern::new("*").unwrap()].into();
        let mut planner = TaskPlanner::for_download(Some(&db)).await.unwrap();
        let filtered = planner.plan_download_asset(&asset, &config).await.unwrap();
        assert!(matches!(
            filtered.filter_reason,
            Some(FilterReason::Filename)
        ));
        assert_eq!(
            planner
                .reconciliation
                .receipt_file_reads
                .load(Ordering::Relaxed),
            0
        );
        config.filename_exclude = Arc::from([]);
        let plan = planner.plan_download_asset(&asset, &config).await.unwrap();
        assert_eq!(
            planner
                .reconciliation
                .receipt_file_reads
                .load(Ordering::Relaxed),
            1
        );
        assert!(
            plan.tasks
                .iter()
                .any(|task| task.version_size == motion.version_size
                    && task.download_path == motion.path)
        );
    }

    #[tokio::test]
    async fn ordinary_catalog_receipt_does_not_release_in_flight_claim() {
        let dir = TempDir::new().unwrap();
        let config = test_config(dir.path());
        let asset = TestPhotoAsset::new("CLAIMED").build();
        let mut planner = TaskPlanner::new();
        let plan = planner.plan_asset(&asset, &config).await;
        let task = plan.tasks.first().expect("fixture must plan a download");
        assert!(!planner.retry_claim_available(task, &task.download_path));
        planner.reconciliation.path_owners.insert(
            PathPlanningMode::Reconciliation
                .key(&task.download_path)
                .unwrap(),
            FxHashSet::from_iter([ReconciliationOwner {
                library: Arc::clone(&task.library),
                asset_id: task.asset_id.as_ref().into(),
                version_size: task.version_size,
            }]),
        );
        assert!(!planner.retry_claim_available(task, &task.download_path));
    }

    fn make_pass(kind: PassKind, name: &str) -> AlbumPass {
        AlbumPass {
            kind,
            album: PhotoAlbum::stub_for_test(Arc::from(name)),
            exclude_ids: Arc::new(FxHashSet::default()),
        }
    }

    #[tokio::test]
    async fn planner_uses_per_pass_album_path() {
        let tmp = TempDir::new().unwrap();
        let base = test_config(tmp.path());
        let pass_config = base.with_pass(&make_pass(PassKind::Album, "Vacation"));
        let asset = TestPhotoAsset::new("ALBUM_PATH")
            .filename("IMG_0001.JPG")
            .build();

        let mut planner = TaskPlanner::new();
        let plan = planner.plan_asset(&asset, &pass_config).await;

        assert_eq!(plan.filter_reason, None);
        assert_eq!(plan.tasks.len(), 1);
        assert!(
            plan.tasks[0]
                .download_path
                .strip_prefix(tmp.path())
                .unwrap()
                .starts_with("Vacation"),
            "album pass must route through the expanded album folder"
        );
    }

    #[tokio::test]
    async fn planner_applies_filename_date_media_and_unfiled_exclusions() {
        let tmp = TempDir::new().unwrap();
        let mut config = test_config(tmp.path());
        config.filename_exclude = Arc::from(vec![glob::Pattern::new("*.AAE").unwrap()]);
        let asset = TestPhotoAsset::new("FILTERED_NAME")
            .filename("IMG_0001.AAE")
            .build();
        let mut planner = TaskPlanner::new();
        let plan = planner.plan_asset(&asset, &config).await;
        assert_eq!(plan.filter_reason, Some(FilterReason::Filename));

        let mut config = test_config(tmp.path());
        config.media.photos = false;
        let asset = TestPhotoAsset::new("FILTERED_MEDIA").build();
        let mut planner = TaskPlanner::new();
        let plan = planner.plan_asset(&asset, &config).await;
        assert_eq!(plan.filter_reason, Some(FilterReason::MediaType));

        let mut config = test_config(tmp.path());
        config.exclude_asset_ids = Arc::new(FxHashSet::from_iter(["EXCLUDED".to_string()]));
        let asset = TestPhotoAsset::new("EXCLUDED").build();
        let mut planner = TaskPlanner::new();
        let plan = planner.plan_asset(&asset, &config).await;
        assert_eq!(plan.filter_reason, Some(FilterReason::ExcludedAlbum));
    }

    #[tokio::test]
    async fn planner_routes_existing_same_size_file_to_identity_path() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(tmp.path());
        let asset = TestPhotoAsset::new("ON_DISK")
            .filename("IMG_0002.JPG")
            .orig_size(1000)
            .build();

        let mut first = TaskPlanner::new();
        let plan = first.plan_asset(&asset, &config).await;
        assert_eq!(plan.tasks.len(), 1);
        let path = plan.tasks[0].download_path.clone();
        tokio::fs::create_dir_all(path.parent().unwrap())
            .await
            .unwrap();
        tokio::fs::write(&path, vec![0u8; 1000]).await.unwrap();

        let mut second = TaskPlanner::new();
        let plan = second.plan_asset(&asset, &config).await;
        assert_eq!(plan.filter_reason, None);
        assert_eq!(plan.tasks.len(), 1);
        assert!(
            plan.tasks[0]
                .download_path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.contains("ON_DISK")),
            "same-size on-disk collision should use an identity path: {:?}",
            plan.tasks[0].download_path
        );
    }

    #[tokio::test]
    async fn planner_avoids_state_protected_legacy_target_without_disk_visibility() {
        let tmp = TempDir::new().unwrap();
        let db = Arc::new(SqliteStateDb::open_in_memory().unwrap());
        let child = TestPhotoAsset::new("CHILD-1")
            .filename("photo.jpg")
            .orig_size(1000)
            .build();
        let mut config = test_config(tmp.path());
        config.state_db = Some(db.clone() as Arc<dyn DownloadStore>);
        let protected = super::super::filter::derive_expected_paths(&child, &config)
            .into_iter()
            .find(|path| path.version_size.is_primary_media())
            .expect("child primary path")
            .path;
        assert_eq!(
            protected.file_name().and_then(|name| name.to_str()),
            Some("photo.JPG"),
            "fixture uses the rendered extension"
        );
        db.upsert_seen(
            &crate::test_helpers::TestAssetRecord::new("MASTER")
                .filename("photo.jpg")
                .checksum("provider")
                .size(1000)
                .build(),
        )
        .await
        .unwrap();
        db.mark_downloaded(
            "PrimarySync",
            "MASTER",
            "original",
            &protected,
            "legacy-local",
            None,
        )
        .await
        .unwrap();
        db.set_metadata_capture_revision_for_test("PrimarySync", "MASTER", 0);
        for id in ["CHILD-1", "CHILD-2"] {
            db.upsert_asset_master_mapping("PrimarySync", id, "MASTER")
                .await
                .unwrap();
        }
        let candidate = db
            .get_metadata_capture_candidates("PrimarySync", 1, 1)
            .await
            .unwrap()
            .remove(0);
        assert!(
            db.defer_metadata_capture_ambiguity(&candidate, 1)
                .await
                .unwrap()
        );
        assert!(
            !crate::download::legacy_preservation::current_path_is_independent(
                db.as_ref(),
                &protected,
            )
            .await
            .unwrap(),
            "the historical original must be protected in state"
        );

        let mut planner = TaskPlanner::for_download(Some(db.as_ref())).await.unwrap();
        // Prime the directory cache before the exact protected file exists.
        // This mirrors a case-insensitive host whose cached spelling differs
        // from the protected key; the file is nonetheless present at planning.
        super::super::filter::pre_ensure_asset_dir(&mut planner.dir_cache, &child, &config).await;
        std::fs::create_dir_all(protected.parent().unwrap()).unwrap();
        std::fs::write(&protected, b"protected legacy bytes").unwrap();
        let foreign_path = protected.with_file_name(paths::insert_asset_identity_suffix(
            protected.file_name().unwrap().to_str().unwrap(),
            "CHILD-1",
        ));
        planner.reconciliation.path_owners.insert(
            planner.path_mode.key(&foreign_path).unwrap(),
            [ReconciliationOwner {
                library: Arc::from("SharedSync-foreign"),
                asset_id: Box::from("CHILD-1"),
                version_size: VersionSizeKey::Original,
            }]
            .into_iter()
            .collect(),
        );
        let plan = planner.plan_download_asset(&child, &config).await.unwrap();
        assert_eq!(plan.tasks.len(), 1);
        let planned = &plan.tasks[0].download_path;
        assert_ne!(
            planned, &foreign_path,
            "same child name in another library does not release its reserved destination"
        );
        assert_ne!(
            planned, &protected,
            "normal download must not claim the protected legacy identity"
        );
        assert_eq!(
            std::fs::read(&protected).unwrap(),
            b"protected legacy bytes",
            "the exact protected original stays unchanged"
        );
        assert!(
            planned
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.contains("CHILD-1")),
            "normal download uses an independent identity suffix: {planned:?}"
        );
        assert!(
            crate::download::legacy_preservation::current_path_is_independent(
                db.as_ref(),
                planned,
            )
            .await
            .unwrap()
        );
    }

    #[tokio::test]
    async fn planner_reports_null_selected_primary_resource_as_malformed() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(tmp.path());
        let asset = PhotoAsset::new(
            json!({
                "recordName": "MALFORMED_PRIMARY",
                "fields": {
                    "filenameEnc": {"value": "bad.jpg", "type": "STRING"},
                    "itemType": {"value": "public.jpeg"},
                    "resOriginalRes": {"value": null},
                    "resOriginalFileType": {"value": "public.jpeg"}
                }
            }),
            json!({"fields": {"assetDate": {"value": 1736899200000.0}}}),
        );

        let mut planner = TaskPlanner::new();
        let plan = planner.plan_asset(&asset, &config).await;

        assert!(plan.tasks.is_empty());
        let malformed = plan.malformed_resource.unwrap();
        assert_eq!(malformed.field.as_ref(), "resOriginalRes");
        assert_eq!(malformed.reason.as_ref(), "resource value is null");
    }

    #[tokio::test]
    async fn planner_ignores_malformed_optional_alternative_when_primary_is_valid() {
        let tmp = TempDir::new().unwrap();
        let mut config = test_config(tmp.path());
        config.alternative = true;
        let asset = PhotoAsset::new(
            json!({
                "recordName": "VALID_PRIMARY_BAD_ALT",
                "fields": {
                    "filenameEnc": {"value": "good.jpg", "type": "STRING"},
                    "itemType": {"value": "public.jpeg"},
                    "resOriginalRes": {"value": {
                        "size": 1000,
                        "downloadURL": "https://p01.icloud-content.com/orig",
                        "fileChecksum": "ck_orig"
                    }},
                    "resOriginalFileType": {"value": "public.jpeg"},
                    "resOriginalAltRes": {"value": null},
                    "resOriginalAltFileType": {"value": "public.camera-raw-image"}
                }
            }),
            json!({"fields": {"assetDate": {"value": 1736899200000.0}}}),
        );

        let mut planner = TaskPlanner::new();
        let plan = planner.plan_asset(&asset, &config).await;

        assert_eq!(plan.tasks.len(), 1);
        assert!(plan.malformed_resource.is_none());
    }

    #[tokio::test]
    async fn planner_upserts_seen_and_records_album_membership() {
        let tmp = TempDir::new().unwrap();
        let base = test_config(tmp.path());
        let pass_config = base.with_pass(&make_pass(PassKind::Album, "Family"));
        let asset = TestPhotoAsset::new("STATEFUL")
            .filename("IMG_0003.JPG")
            .build();
        let db = SqliteStateDb::open_in_memory().unwrap();

        let mut planner = TaskPlanner::new();
        let plan = planner.plan_asset(&asset, &pass_config).await;
        assert_eq!(plan.tasks.len(), 1);
        upsert_seen_for_task(&db, &pass_config, &asset, &plan.tasks[0])
            .await
            .unwrap();
        record_album_membership_if_named(&db, &pass_config, &asset)
            .await
            .unwrap();

        let pending = db.get_pending().await.unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].id.as_ref(), "STATEFUL");
        assert_eq!(
            db.get_master_record_name_for_asset(&pass_config.library, asset.asset_record_name())
                .await
                .unwrap()
                .as_deref(),
            Some("STATEFUL")
        );
        let albums = db.get_all_asset_albums(&pass_config.library).await.unwrap();
        assert_eq!(albums, vec![("STATEFUL".to_string(), "Family".to_string())]);
    }

    #[tokio::test]
    async fn planner_redispatch_reactivates_policy_excluded_asset() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(tmp.path());
        let asset = TestPhotoAsset::new("POLICY_RESELECTED")
            .filename("reselected.mov")
            .build();
        let db = SqliteStateDb::open_in_memory().unwrap();
        let mut task_planner = TaskPlanner::new();
        let plan = task_planner.plan_asset(&asset, &config).await;
        let task = plan
            .tasks
            .first()
            .expect("selected asset should plan a task");
        upsert_seen_for_task(&db, &config, &asset, task)
            .await
            .unwrap();
        assert!(
            db.mark_policy_excluded(
                task.library.as_ref(),
                task.asset_id.as_ref(),
                task.version_size.as_str(),
            )
            .await
            .unwrap()
        );
        assert_eq!(db.get_summary().await.unwrap().policy_excluded, 1);

        upsert_seen_for_task(&db, &config, &asset, task)
            .await
            .unwrap();

        let summary = db.get_summary().await.unwrap();
        assert_eq!(summary.policy_excluded, 0);
        assert_eq!(summary.pending, 1);
    }

    #[tokio::test]
    async fn download_reservations_keep_unknown_legacy_content_occupied() {
        use crate::state::ReconciliationStateStore;

        let root = TempDir::new().unwrap();
        let db_path = root.path().join("state.db");
        let db = SqliteStateDb::open(&db_path).await.unwrap();
        let config = test_config(root.path());
        let asset = TestPhotoAsset::new("LEGACY").filename("legacy.JPG").build();
        let mut planner = TaskPlanner::for_reconciliation(Vec::new(), Vec::new()).unwrap();
        let initial = planner.plan_download_asset(&asset, &config).await.unwrap();
        let task = &initial.tasks[0];
        let legacy_path = task.download_path.clone();
        let mut legacy = planner
            .reconciliation_reservations()
            .next()
            .unwrap()
            .clone();
        legacy.content = None;
        db.reserve_reconciliation_paths(std::slice::from_ref(&legacy), None)
            .await
            .unwrap();
        std::fs::create_dir_all(legacy_path.parent().unwrap()).unwrap();
        std::fs::write(&legacy_path, b"older content without provenance").unwrap();
        let mut planner = TaskPlanner::for_download(Some(&db)).await.unwrap();
        assert!(!planner.retry_path_allowed(
            &task.library,
            &task.asset_id,
            task.asset_record_name.as_ref(),
            task.version_size,
            &task.checksum,
            task.size,
            &legacy_path
        ));
        let plan = planner.plan_download_asset(&asset, &config).await.unwrap();
        let new_path = plan.tasks[0].download_path.clone();
        assert_ne!(new_path, legacy_path);
        planner
            .persist_download_reservations(&db, &plan.tasks)
            .await
            .unwrap();
        let choices = db.get_reconciliation_reservations().await.unwrap();
        assert_eq!(choices.len(), 2);
        assert!(choices.contains(&legacy));
        drop(db);
        for _ in 0..2 {
            let db = SqliteStateDb::open(&db_path).await.unwrap();
            let mut planner = TaskPlanner::for_download(Some(&db)).await.unwrap();
            let plan = planner.plan_download_asset(&asset, &config).await.unwrap();
            assert_eq!(plan.tasks[0].download_path, new_path);
            assert!(planner.has_durable_destination(&plan.tasks[0]));
            planner
                .persist_download_reservations(&db, &plan.tasks)
                .await
                .unwrap();
            assert_eq!(db.get_reconciliation_reservations().await.unwrap(), choices);
        }
        assert_eq!(
            std::fs::read(legacy_path).unwrap(),
            b"older content without provenance"
        );
    }

    #[tokio::test]
    async fn download_reservations_keep_cross_zone_owners_across_restart() {
        use crate::state::ReconciliationStateStore;

        let root = TempDir::new().unwrap();
        let db_path = root.path().join("state.db");
        let db = SqliteStateDb::open(&db_path).await.unwrap();
        let config = test_config(root.path());
        let primary = TestPhotoAsset::new("SAME_ID")
            .filename("shared.JPG")
            .build();
        let shared = primary
            .clone()
            .with_source_zone(Arc::from("SharedSync-abc"));
        let mut planner = TaskPlanner::for_reconciliation(Vec::new(), Vec::new()).unwrap();
        let initial = planner.plan_download_asset(&shared, &config).await.unwrap();
        let shared_path = initial.tasks[0].download_path.clone();
        db.reserve_reconciliation_paths(
            &planner
                .reconciliation_reservations()
                .cloned()
                .collect::<Vec<_>>(),
            None,
        )
        .await
        .unwrap();
        let mut planner = TaskPlanner::for_download(Some(&db)).await.unwrap();
        let primary_plan = planner
            .plan_download_asset(&primary, &config)
            .await
            .unwrap();
        let primary_path = primary_plan.tasks[0].download_path.clone();
        assert_ne!(primary_path, shared_path);
        planner
            .persist_download_reservations(&db, &primary_plan.tasks)
            .await
            .unwrap();
        let reservations = db.get_reconciliation_reservations().await.unwrap();
        drop(db);

        for _ in 0..2 {
            let db = SqliteStateDb::open(&db_path).await.unwrap();
            let mut planner = TaskPlanner::for_download(Some(&db)).await.unwrap();
            for (asset, library, path) in [
                (&shared, "SharedSync-abc", &shared_path),
                (&primary, "PrimarySync", &primary_path),
            ] {
                let plan = planner.plan_download_asset(asset, &config).await.unwrap();
                assert_eq!(plan.tasks.len(), 1);
                assert_eq!(plan.tasks[0].library.as_ref(), library);
                assert_eq!(&plan.tasks[0].download_path, path);
                planner
                    .persist_download_reservations(&db, &plan.tasks)
                    .await
                    .unwrap();
            }
            assert_eq!(
                db.get_reconciliation_reservations().await.unwrap(),
                reservations
            );
        }
    }

    #[tokio::test]
    async fn planner_uses_cross_zone_asset_library_for_state_and_membership() {
        let tmp = TempDir::new().unwrap();
        let base = test_config(tmp.path());
        let pass_config = base.with_pass(&make_pass(PassKind::Album, "Family"));
        let asset = TestPhotoAsset::new("CROSS_ZONE")
            .filename("IMG_0004.JPG")
            .build()
            .with_source_zone(Arc::from("SharedSync-abc"));
        let db = SqliteStateDb::open_in_memory().unwrap();

        let mut planner = TaskPlanner::new();
        let plan = planner.plan_asset(&asset, &pass_config).await;
        assert_eq!(plan.tasks.len(), 1);
        assert_eq!(plan.tasks[0].library.as_ref(), "SharedSync-abc");
        upsert_seen_for_task(&db, &pass_config, &asset, &plan.tasks[0])
            .await
            .unwrap();
        record_album_membership_if_named(&db, &pass_config, &asset)
            .await
            .unwrap();

        let pending = db.get_pending().await.unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].library.as_ref(), "SharedSync-abc");
        assert_eq!(pending[0].id.as_ref(), "CROSS_ZONE");
        assert_eq!(
            db.get_master_record_name_for_asset("SharedSync-abc", asset.asset_record_name())
                .await
                .unwrap()
                .as_deref(),
            Some("CROSS_ZONE")
        );
        assert!(
            db.get_all_asset_albums(&pass_config.library)
                .await
                .unwrap()
                .is_empty(),
            "album membership must not be recorded under the owner pass zone"
        );
        let albums = db.get_all_asset_albums("SharedSync-abc").await.unwrap();
        assert_eq!(
            albums,
            vec![("CROSS_ZONE".to_string(), "Family".to_string())]
        );
    }

    #[tokio::test]
    async fn mixed_owner_new_adoption_must_progress_to_safe_sibling() {
        use crate::download::baseline_matching::BaselineMatch;
        use crate::state::DownloadStateStore;
        use crate::state::ReconciliationStateStore;
        use crate::state::{
            ReconciliationContent, ReconciliationPathKey, ReconciliationReservation, VersionSizeKey,
        };
        use crate::test_helpers::TestAssetRecord;
        use sha2::Digest;

        let root = TempDir::new().unwrap();
        let db_path = root.path().join("state.db");
        let db = Arc::new(SqliteStateDb::open(&db_path).await.unwrap());
        let mut config = test_config(root.path());
        config.folder_structure = "none".into();
        config.state_db = Some(db.clone());
        let asset = TestPhotoAsset::new("current-child")
            .filename("mixed.JPG")
            .orig_size(4)
            .orig_checksum("generation")
            .build();
        let old_path = root.path().join("mixed.JPG");
        std::fs::write(&old_path, b"good").unwrap();
        let old_mtime = std::fs::metadata(&old_path).unwrap().modified().unwrap();
        let digest = data_encoding::HEXLOWER.encode(&sha2::Sha256::digest(b"good"));
        db.upsert_seen(
            &TestAssetRecord::new("old-master")
                .library("PrimarySync")
                .filename("mixed.JPG")
                .checksum("generation")
                .size(4)
                .build(),
        )
        .await
        .unwrap();
        db.mark_verified_download(
            "PrimarySync",
            "old-master",
            "original",
            &old_path,
            &digest,
            Some(&digest),
            false,
        )
        .await
        .unwrap();
        assert!(
            db.claim_legacy_master_state_owner("PrimarySync", "old-master", "current-child")
                .await
                .unwrap()
        );
        let key = crate::fs_util::confined_path_key(&old_path).unwrap();
        db.reserve_reconciliation_paths(
            &[ReconciliationReservation {
                content: Some(ReconciliationContent {
                    checksum: "generation".into(),
                    size: 4,
                }),
                library: Arc::from("PrimarySync"),
                asset_id: "foreign".into(),
                version_size: VersionSizeKey::Original,
                requested_path_key: ReconciliationPathKey(key.clone()),
                destination_path_key: ReconciliationPathKey(key),
                destination_path: old_path.clone(),
            }],
            None,
        )
        .await
        .unwrap();

        let mut planner = TaskPlanner::for_download(Some(db.as_ref())).await.unwrap();
        let plan = planner.plan_download_asset(&asset, &config).await.unwrap();
        let task = &plan.tasks[0];
        assert_ne!(task.download_path, old_path);
        let decision = planner.match_existing(&asset, &config, task).await;
        assert!(
            matches!(decision, BaselineMatch::Missing),
            "mixed foreign owner must not return the old completed P: {decision:?}"
        );
        assert!(
            db.get_downloaded_page(0, 100)
                .await
                .unwrap()
                .iter()
                .all(|row| row.id.as_ref() != "current-child")
        );
        assert_eq!(std::fs::read(&old_path).unwrap(), b"good");
        assert_eq!(
            std::fs::metadata(&old_path).unwrap().modified().unwrap(),
            old_mtime
        );

        drop(planner);
        drop(db);
        let db = Arc::new(SqliteStateDb::open(&db_path).await.unwrap());
        config.state_db = Some(db.clone());
        let mut planner = TaskPlanner::for_download(Some(db.as_ref())).await.unwrap();
        let plan = planner.plan_download_asset(&asset, &config).await.unwrap();
        assert_ne!(plan.tasks[0].download_path, old_path);
        let decision = planner
            .match_existing(&asset, &config, &plan.tasks[0])
            .await;
        assert!(matches!(decision, BaselineMatch::Missing));
    }

    #[tokio::test]
    async fn exclusive_legacy_receipt_alone_still_reuses_without_download() {
        use crate::download::baseline_matching::{BaselineMatch, commit_match};
        use crate::state::DownloadStateStore;
        use crate::test_helpers::TestAssetRecord;
        use sha2::Digest;
        use tokio_util::sync::CancellationToken;

        let root = TempDir::new().unwrap();
        let db = Arc::new(
            SqliteStateDb::open(&root.path().join("state.db"))
                .await
                .unwrap(),
        );
        let mut config = test_config(root.path());
        config.folder_structure = "none".into();
        config.state_db = Some(db.clone());
        let asset = TestPhotoAsset::new("current-child")
            .filename("mixed.JPG")
            .orig_size(4)
            .orig_checksum("generation")
            .build();
        let old_path = root.path().join("mixed.JPG");
        std::fs::write(&old_path, b"good").unwrap();
        let digest = data_encoding::HEXLOWER.encode(&sha2::Sha256::digest(b"good"));
        db.upsert_seen(
            &TestAssetRecord::new("old-master")
                .library("PrimarySync")
                .filename("mixed.JPG")
                .checksum("generation")
                .size(4)
                .build(),
        )
        .await
        .unwrap();
        db.mark_verified_download(
            "PrimarySync",
            "old-master",
            "original",
            &old_path,
            &digest,
            Some(&digest),
            false,
        )
        .await
        .unwrap();
        assert!(
            db.claim_legacy_master_state_owner("PrimarySync", "old-master", "current-child")
                .await
                .unwrap()
        );
        let mut planner = TaskPlanner::for_download(Some(db.as_ref())).await.unwrap();
        let plan = planner.plan_download_asset(&asset, &config).await.unwrap();
        let task = &plan.tasks[0];
        let decision = planner.match_existing(&asset, &config, task).await;
        assert!(matches!(decision, BaselineMatch::Present { .. }));
        let settled = commit_match(decision, &config, &asset, task, &CancellationToken::new())
            .await
            .unwrap();
        assert!(settled);
        let current = db
            .get_downloaded_page(0, 100)
            .await
            .unwrap()
            .into_iter()
            .find(|row| row.id.as_ref() == "current-child")
            .unwrap();
        assert_eq!(current.local_path.as_ref(), Some(&old_path));
        assert!(!task.download_path.exists());
    }

    #[tokio::test]
    async fn proven_legacy_reservation_reuses_canonical_child_destination() {
        use crate::state::ReconciliationStateStore;
        use crate::state::{
            ReconciliationContent, ReconciliationPathKey, ReconciliationReservation, VersionSizeKey,
        };

        for file_present in [false, true] {
            let root = TempDir::new().unwrap();
            let db = Arc::new(
                SqliteStateDb::open(&root.path().join("state.db"))
                    .await
                    .unwrap(),
            );
            let mut config = test_config(root.path());
            config.folder_structure = "none".into();
            config.state_db = Some(db.clone());
            let asset = TestPhotoAsset::new("current-child")
                .filename("reserved.JPG")
                .orig_size(4)
                .orig_checksum("generation")
                .build();
            let destination = root.path().join("reserved.JPG");
            if file_present {
                std::fs::write(&destination, b"good").unwrap();
            }
            assert!(
                db.claim_legacy_master_state_owner("PrimarySync", "old-master", "current-child")
                    .await
                    .unwrap()
            );
            let key = crate::fs_util::confined_path_key(&destination).unwrap();
            db.reserve_reconciliation_paths(
                &[ReconciliationReservation {
                    content: Some(ReconciliationContent {
                        checksum: "generation".into(),
                        size: 4,
                    }),
                    library: Arc::from("PrimarySync"),
                    asset_id: "old-master".into(),
                    version_size: VersionSizeKey::Original,
                    requested_path_key: ReconciliationPathKey(key.clone()),
                    destination_path_key: ReconciliationPathKey(key),
                    destination_path: destination.clone(),
                }],
                None,
            )
            .await
            .unwrap();
            let mut planner = TaskPlanner::for_download(Some(db.as_ref())).await.unwrap();
            let plan = planner.plan_download_asset(&asset, &config).await.unwrap();
            assert_eq!(
                plan.tasks[0].download_path, destination,
                "canonical child must reuse its proof-bearing M reservation"
            );
            planner
                .persist_download_reservations(db.as_ref(), &plan.tasks)
                .await
                .unwrap();
            let reservations = db.get_reconciliation_reservations().await.unwrap();
            assert_eq!(reservations.len(), 1);
            assert_eq!(reservations[0].asset_id.as_ref(), "old-master");
            assert_eq!(reservations[0].destination_path, destination);
        }
    }
}

#[cfg(test)]
#[path = "planner/baseline_discovery_tests.rs"]
mod baseline_discovery_tests;

#[cfg(test)]
#[path = "planner/compact_receipt_tests.rs"]
mod compact_receipt_tests;
