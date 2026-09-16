//! Asset-to-task planning shared by full, incremental, dry-run, and cleanup
//! paths.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use rustc_hash::{FxHashMap, FxHashSet};

use crate::icloud::photos::PhotoAsset;
use crate::state::{
    AssetRecord, DownloadStateStore, MembershipStore, ReconciliationCatalogPath,
    ReconciliationPathKey, ReconciliationReservation, VersionSizeKey,
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

/// Keep durable path ownership off the stack of the shared async planner.
#[derive(Debug, Default)]
struct ReconciliationPlanning {
    claims: FxHashMap<ReconciliationOwner, FxHashMap<NormalizedPath, u64>>,
    path_owners: FxHashMap<NormalizedPath, FxHashSet<ReconciliationOwner>>,
    destinations: FxHashMap<(ReconciliationOwner, NormalizedPath), std::path::PathBuf>,
    reservations: Vec<ReconciliationReservation>,
    durable_destinations: FxHashSet<NormalizedPath>,
}

/// Mutable path-planning state carried across assets in one pass.
#[derive(Debug)]
pub(super) struct TaskPlanner {
    claimed_paths: FxHashMap<NormalizedPath, u64>,
    path_mode: PathPlanningMode,
    reconciliation: Box<ReconciliationPlanning>,
    dir_cache: paths::DirCache,
}

impl TaskPlanner {
    pub(super) fn new() -> Self {
        Self {
            claimed_paths: FxHashMap::default(),
            path_mode: PathPlanningMode::Download,
            reconciliation: Box::default(),
            dir_cache: paths::DirCache::new(),
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
            planner.add_reconciliation_claim(owner.clone(), destination, 0);
            planner.reconciliation.destinations.insert(
                (
                    owner,
                    NormalizedPath::from_key(reservation.requested_path_key),
                ),
                reservation.destination_path,
            );
        }
        Ok(planner)
    }

    /// Preserve ordinary download behavior until reconciliation has reserved paths.
    pub(super) async fn for_download(db: Option<&dyn DownloadStore>) -> Result<Self> {
        let Some(db) = db else {
            return Ok(Self::new());
        };
        let reservations = db.get_reconciliation_reservations().await?;
        if reservations.is_empty() {
            return Ok(Self::new());
        }
        let mut planner =
            Self::for_reconciliation(db.get_reconciliation_catalog_paths().await?, reservations)?;
        planner.path_mode = PathPlanningMode::ReservedDownload;
        Ok(planner)
    }

    pub(super) async fn plan_download_asset(
        &mut self,
        asset: &PhotoAsset,
        config: &DownloadConfig,
    ) -> Result<AssetTaskPlan> {
        self.reconciliation.reservations.clear();
        match self.path_mode {
            PathPlanningMode::Download => Ok(self.plan_asset(asset, config).await),
            // Adoption verifies existing files separately. Reuse the
            // ownership-aware plan, including exact saved destination choices.
            mode => self.plan_owned_asset(asset, config, mode).await,
        }
    }

    /// Reject foreign ownership before adoption or recorded-path overrides.
    /// An invalid confined path is never permission to claim a reservation.
    #[must_use]
    pub(super) fn retry_path_allowed(
        &self,
        library: &str,
        asset_id: &str,
        version_size: VersionSizeKey,
        path: &Path,
    ) -> bool {
        if matches!(self.path_mode, PathPlanningMode::Download) {
            return true;
        }
        self.path_mode.key(path).is_ok_and(|key| {
            self.reconciliation
                .path_owners
                .get(&key)
                .is_none_or(|owners| {
                    owners.iter().all(|owner| {
                        owner.library.as_ref() == library
                            && owner.asset_id.as_ref() == asset_id
                            && owner.version_size == version_size
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
                task.version_size,
                &task.download_path,
            )
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

    pub(super) fn reconciliation_reservations(&self) -> &[ReconciliationReservation] {
        &self.reconciliation.reservations
    }

    /// Commit final choices, including recorded-path overrides, before dispatch.
    /// Drain each plan once so a streaming pass does not retain a growing write batch.
    pub(super) async fn persist_download_reservations(
        &mut self,
        db: &dyn DownloadStore,
        tasks: &[DownloadTask],
    ) -> Result<()> {
        let mut reservations = std::mem::take(&mut self.reconciliation.reservations);
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
                self.path_mode
                    .key(&reservation.destination_path)?
                    .as_ref()
                    .to_owned(),
            );
        }
        if !reservations.is_empty() {
            db.reserve_reconciliation_paths(&reservations).await?;
            for reservation in reservations {
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
                        NormalizedPath::from_key(reservation.requested_path_key),
                    ),
                    reservation.destination_path,
                );
            }
        }
        Ok(())
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
        self.plan_asset_with_mode(asset, config, PathPlanningMode::Download)
            .await
            .expect("ordinary download planning uses infallible spelling-only path keys")
    }

    pub(super) async fn plan_reconciliation_asset(
        &mut self,
        asset: &PhotoAsset,
        config: &DownloadConfig,
    ) -> Result<AssetTaskPlan> {
        self.plan_owned_asset(asset, config, PathPlanningMode::Reconciliation)
            .await
    }

    async fn plan_owned_asset(
        &mut self,
        asset: &PhotoAsset,
        config: &DownloadConfig,
        mode: PathPlanningMode,
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
                {
                    self.claimed_paths.remove(path);
                }
            }
            owned.insert(owner, claims);
        }
        let plan = self
            .plan_asset_with_mode(asset, config, mode)
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
                    let slot = (owner.clone(), requested_key.clone());
                    if let Some(destination) = self.reconciliation.destinations.get(&slot) {
                        let planned_key =
                            PathPlanningMode::Reconciliation.key(&task.download_path)?;
                        self.claimed_paths.remove(&planned_key);
                        let destination_key = PathPlanningMode::Reconciliation.key(destination)?;
                        anyhow::ensure!(
                            !self.claimed_paths.contains_key(&destination_key),
                            "reserved reconciliation destination has another owner"
                        );
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
                    let destination_key =
                        PathPlanningMode::Reconciliation.key(&task.download_path)?;
                    self.claimed_paths
                        .insert(destination_key.clone(), task.size);
                    owned
                        .entry(owner)
                        .or_default()
                        .insert(destination_key.clone(), task.size);
                    self.reconciliation
                        .destinations
                        .insert(slot, task.download_path.clone());
                    self.reconciliation
                        .reservations
                        .push(ReconciliationReservation {
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
                        });
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
    ) -> Result<AssetTaskPlan> {
        if let Some(filter_reason) = is_asset_filtered(asset, config) {
            return Ok(AssetTaskPlan {
                tasks: Vec::new(),
                filter_reason: Some(filter_reason),
                malformed_resource: None,
            });
        }

        pre_ensure_asset_dir(&mut self.dir_cache, asset, config).await;
        let tasks = filter_asset_to_tasks(
            asset,
            config,
            &mut self.claimed_paths,
            &mut self.dir_cache,
            planning_mode,
        )?;
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

    pub(super) fn existing_path_match(&mut self, path: &Path) -> ExistingPathMatch {
        match self.existing_path(path) {
            Some(found) if found == path => ExistingPathMatch::Exact,
            Some(_) => ExistingPathMatch::AmpmVariant,
            None => ExistingPathMatch::Missing,
        }
    }

    pub(super) fn existing_path(&mut self, path: &Path) -> Option<std::path::PathBuf> {
        self.existing_path_with_size(path).map(|(path, _)| path)
    }

    pub(super) fn existing_path_with_size(
        &mut self,
        path: &Path,
    ) -> Option<(std::path::PathBuf, u64)> {
        if let Some(size) = self.dir_cache.file_size(path) {
            Some((path.to_path_buf(), size))
        } else {
            let variant = self.dir_cache.find_ampm_variant(path)?;
            let size = self.dir_cache.file_size(&variant)?;
            Some((variant, size))
        }
    }

    pub(super) async fn prepare_path_parent(&mut self, path: &Path) {
        if let Some(parent) = path.parent() {
            self.dir_cache.ensure_dir_async(parent).await;
        }
    }

    fn retry_claim_available(&self, task: &DownloadTask, path: &Path) -> bool {
        self.retry_path_allowed(&task.library, &task.asset_id, task.version_size, path)
            && self.path_mode.key(path).is_ok_and(|key| {
                !self.claimed_paths.contains_key(&key)
                    || self.reconciliation.path_owners.contains_key(&key)
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ExistingPathMatch {
    Exact,
    AmpmVariant,
    Missing,
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

    let media_type = determine_media_type(task.version_size, asset);
    let record = AssetRecord::new_pending(
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
    ));
    db.upsert_seen(&record).await
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

    fn test_config(root: &std::path::Path) -> DownloadConfig {
        let mut config = DownloadConfig::test_default();
        config.directory = Arc::from(root);
        config.folder_structure = "%Y/%m/%d".to_string();
        config.folder_structure_albums = Arc::from("{album}/%Y/%m/%d");
        config
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
    async fn reconciliation_planner_reuses_each_live_photo_family_across_passes() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(tmp.path());
        let first_asset = TestPhotoAsset::new("FIRST")
            .filename("IMG_0001.JPG")
            .live_photo(
                "https://p01.icloud-content.com/first.mov",
                "first-motion",
                1000,
            )
            .build();
        let second_asset = TestPhotoAsset::new("SECOND")
            .filename("IMG_0001.JPG")
            .live_photo(
                "https://p01.icloud-content.com/second.mov",
                "second-motion",
                1000,
            )
            .build();
        let mut planner = TaskPlanner::for_reconciliation(Vec::new(), Vec::new()).unwrap();
        let first = planner
            .plan_reconciliation_asset(&first_asset, &config)
            .await
            .unwrap();
        let second = planner
            .plan_reconciliation_asset(&second_asset, &config)
            .await
            .unwrap();
        assert_eq!(first.tasks.len(), 2);
        assert_eq!(second.tasks.len(), 2);
        let paths: FxHashSet<_> = first
            .tasks
            .iter()
            .chain(&second.tasks)
            .map(|task| &task.download_path)
            .collect();
        assert_eq!(paths.len(), 4);
        for (asset, previous) in [(&second_asset, &second), (&first_asset, &first)] {
            let repeated = planner
                .plan_reconciliation_asset(asset, &config)
                .await
                .unwrap();
            assert_eq!(repeated.tasks.len(), previous.tasks.len());
            for (actual, expected) in repeated.tasks.iter().zip(&previous.tasks) {
                assert_eq!(actual.version_size, expected.version_size);
                assert_eq!(actual.download_path, expected.download_path);
            }
        }
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
        let initial = planner
            .plan_reconciliation_asset(&shared, &config)
            .await
            .unwrap();
        let shared_path = initial.tasks[0].download_path.clone();
        db.reserve_reconciliation_paths(planner.reconciliation_reservations())
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
}
