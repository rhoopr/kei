//! Existing and pending local-file evidence and adoption.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use rustc_hash::FxHashSet;

use crate::download::file::{LocalFileSizeExpectation, local_file_size_matches_state};
use crate::download::filter::{
    DerivedPath, DownloadTask, derive_expected_paths, determine_media_type,
};
use crate::download::planner::TaskPlanner;
use crate::download::{DownloadConfig, DownloadContext, DownloadStore, RecordedLocalFile, planner};
use crate::icloud::photos::PhotoAsset;
use crate::state::{AssetRecord, VersionSizeKey};

use super::task::capture_repair_requested;

pub(super) fn effective_asset_library<'a>(
    asset: &'a PhotoAsset,
    config: &'a DownloadConfig,
) -> &'a str {
    asset.source_zone().unwrap_or(config.library.as_ref())
}

pub(super) fn effective_asset_library_arc(asset: &PhotoAsset, config: &DownloadConfig) -> Arc<str> {
    asset
        .source_zone()
        .map(Arc::from)
        .unwrap_or_else(|| Arc::clone(&config.library))
}

pub(super) fn asset_record_for_derived_path(
    library: Arc<str>,
    asset: &PhotoAsset,
    derived: &DerivedPath,
    config: &DownloadConfig,
) -> AssetRecord {
    AssetRecord::new_pending(
        library,
        asset.state_id().to_string(),
        derived.version_size,
        derived.checksum.to_string(),
        derived.filename.clone(),
        asset.created(),
        Some(asset.added_date()),
        derived.size,
        determine_media_type(derived.version_size, asset),
    )
    .with_metadata_arc(crate::download::filter::metadata_for_selected_version(
        asset,
        config,
        derived.version_size,
    ))
}

fn pending_versions_for_asset<'a>(
    ctx: &'a DownloadContext,
    library: &str,
    asset: &PhotoAsset,
) -> Option<&'a FxHashSet<Box<str>>> {
    ctx.pending_ids
        .get(library)
        .and_then(|assets| assets.get(asset.state_id()))
}

fn pending_filename_for_asset_version<'a>(
    ctx: &'a DownloadContext,
    library: &str,
    asset: &PhotoAsset,
    version_size: VersionSizeKey,
) -> Option<&'a str> {
    ctx.pending_filenames
        .get(library)
        .and_then(|assets| assets.get(asset.state_id()))
        .and_then(|versions| versions.get(version_size.as_str()))
        .map(Box::as_ref)
}

fn pending_filename_matches_derived(pending_filename: &str, derived_filename: &str) -> bool {
    filenames_match_ampm_equivalent(pending_filename, derived_filename)
        || pending_filename.eq_ignore_ascii_case(derived_filename)
}

pub(super) async fn adopt_pending_on_disk_skip(
    state_db: Option<&dyn DownloadStore>,
    config: &DownloadConfig,
    asset: &PhotoAsset,
    ctx: &DownloadContext,
    task_planner: &mut TaskPlanner,
) -> PendingOnDiskAdoptionSummary {
    let Some(db) = state_db else {
        return PendingOnDiskAdoptionSummary::default();
    };
    let library = effective_asset_library(asset, config);
    let pending_versions = pending_versions_for_asset(ctx, library, asset);
    let Some(pending_versions) = pending_versions else {
        return PendingOnDiskAdoptionSummary::default();
    };

    let mut summary = PendingOnDiskAdoptionSummary::default();
    for derived in derive_expected_paths(asset, config) {
        let version_size = derived.version_size.as_str();
        if !pending_versions.contains(version_size) {
            continue;
        }
        match adopt_pending_derived_path(
            db,
            config,
            asset,
            task_planner,
            &derived,
            ctx.pending_file_matching_checksum(
                library,
                asset.state_id(),
                derived.version_size,
                derived.checksum.as_ref(),
            ),
        )
        .await
        {
            Some(PendingOnDiskAdoption::Adopted(_)) => {}
            Some(PendingOnDiskAdoption::StateWriteFailed(_)) => {
                summary.state_write_failures += 1;
            }
            None => {}
        }
    }

    summary
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(super) struct PendingOnDiskAdoptionSummary {
    pub(super) state_write_failures: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum PendingOnDiskAdoption {
    Adopted(PathBuf),
    StateWriteFailed(PathBuf),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::download) enum PendingRetryAdoption {
    NotFound,
    Adopted,
    StateWriteFailed,
}

impl From<PendingOnDiskAdoption> for PendingRetryAdoption {
    fn from(adoption: PendingOnDiskAdoption) -> Self {
        match adoption {
            PendingOnDiskAdoption::Adopted(_) => Self::Adopted,
            PendingOnDiskAdoption::StateWriteFailed(_) => Self::StateWriteFailed,
        }
    }
}

pub(in crate::download) struct PendingRetryFileEvidence<'a> {
    pub(in crate::download) version_size: VersionSizeKey,
    pub(in crate::download) filename: &'a str,
    pub(in crate::download) checksum: &'a str,
    pub(in crate::download) local_path: PendingRetryLocalPath<'a>,
    pub(in crate::download) size: u64,
}

pub(in crate::download) enum PendingRetryLocalPath<'a> {
    Unrecorded,
    Current(&'a RecordedLocalFile),
    Historical,
}

pub(in crate::download) async fn adopt_pending_on_disk_for_retry(
    db: &dyn DownloadStore,
    config: &DownloadConfig,
    asset: &PhotoAsset,
    task_planner: &mut TaskPlanner,
    planned_tasks: &[DownloadTask],
    evidence: PendingRetryFileEvidence<'_>,
) -> PendingRetryAdoption {
    // A content change can leave historical catalog evidence after the new
    // reserved sibling was published but finalization failed. Its immutable
    // content-specific reservation and verified bytes permit adoption; the old
    // recorded file still cannot stand in for the new provider generation.
    for task in planned_tasks.iter().filter(|task| {
        task.version_size == evidence.version_size
            && task.checksum.as_ref() == evidence.checksum
            && task.size == evidence.size
    }) {
        if task_planner.has_durable_destination(task)
            && let Some(adoption) =
                adopt_pending_task_path(db, config, asset, task_planner, task, None).await
        {
            return adoption.into();
        }
    }
    if matches!(evidence.local_path, PendingRetryLocalPath::Historical) {
        return PendingRetryAdoption::NotFound;
    }

    if let PendingRetryLocalPath::Current(recorded_file) = evidence.local_path {
        let local_path = &recorded_file.path;
        let recorded_filename_matches = local_path
            .file_name()
            .and_then(|filename| filename.to_str())
            .is_some_and(|filename| pending_filename_matches_derived(evidence.filename, filename));
        if !recorded_filename_matches {
            return PendingRetryAdoption::NotFound;
        }

        task_planner.prepare_path_parent(local_path).await;
        for task in planned_tasks.iter().filter(|task| {
            task.version_size == evidence.version_size
                && task.checksum.as_ref() == evidence.checksum
                && task.size == evidence.size
        }) {
            let mut recorded_task = task.clone();
            recorded_task.download_path = local_path.to_path_buf();
            if let Some(adoption) = adopt_pending_task_path(
                db,
                config,
                asset,
                task_planner,
                &recorded_task,
                Some(recorded_file),
            )
            .await
            {
                return adoption.into();
            }
        }

        for derived in derive_expected_paths(asset, config)
            .into_iter()
            .filter(|derived| {
                derived.version_size == evidence.version_size
                    && derived.checksum.as_ref() == evidence.checksum
                    && derived.size == evidence.size
                    && pending_filename_matches_derived(evidence.filename, &derived.filename)
            })
        {
            if let Some(adoption) = adopt_pending_derived_path_at(
                db,
                config,
                asset,
                task_planner,
                &derived,
                local_path,
                Some(recorded_file),
            )
            .await
            {
                return adoption.into();
            }
        }
        return PendingRetryAdoption::NotFound;
    }

    for task in planned_tasks.iter().filter(|task| {
        task.version_size == evidence.version_size
            && task.checksum.as_ref() == evidence.checksum
            && task.size == evidence.size
    }) {
        if let Some(adoption) =
            adopt_pending_task_path(db, config, asset, task_planner, task, None).await
        {
            return adoption.into();
        }
    }

    for derived in derive_expected_paths(asset, config)
        .into_iter()
        .filter(|derived| {
            derived.version_size == evidence.version_size
                && derived.checksum.as_ref() == evidence.checksum
                && derived.size == evidence.size
                && pending_filename_matches_derived(evidence.filename, &derived.filename)
        })
    {
        if let Some(adoption) =
            adopt_pending_derived_path(db, config, asset, task_planner, &derived, None).await
        {
            return adoption.into();
        }
    }

    PendingRetryAdoption::NotFound
}

pub(super) async fn adopt_pending_on_disk_task(
    state_db: Option<&dyn DownloadStore>,
    config: &DownloadConfig,
    asset: &PhotoAsset,
    ctx: &DownloadContext,
    task_planner: &mut TaskPlanner,
    task: &DownloadTask,
) -> Option<PendingOnDiskAdoption> {
    let db = state_db?;
    let library = effective_asset_library(asset, config);
    let pending_versions = pending_versions_for_asset(ctx, library, asset)?;
    if !pending_versions.contains(task.version_size.as_str()) {
        return None;
    }

    if let Some(adoption) = adopt_pending_task_path(
        db,
        config,
        asset,
        task_planner,
        task,
        ctx.pending_file_matching_checksum(
            library,
            asset.state_id(),
            task.version_size,
            task.checksum.as_ref(),
        ),
    )
    .await
    {
        return Some(adoption);
    }

    for derived in derive_expected_paths(asset, config) {
        if derived.version_size != task.version_size {
            continue;
        }
        let pending_filename =
            pending_filename_for_asset_version(ctx, library, asset, task.version_size);
        if !pending_filename.is_some_and(|filename| {
            pending_filename_matches_derived(filename, derived.filename.as_str())
        }) {
            continue;
        }
        if let Some(adoption) = adopt_pending_derived_path(
            db,
            config,
            asset,
            task_planner,
            &derived,
            ctx.pending_file_matching_checksum(
                library,
                asset.state_id(),
                derived.version_size,
                derived.checksum.as_ref(),
            ),
        )
        .await
        {
            return Some(adoption);
        }
    }

    None
}

fn path_matches_recorded_file(path: &Path, recorded_path: &Path) -> bool {
    if path == recorded_path {
        return true;
    }
    if path.parent() != recorded_path.parent() {
        return false;
    }

    match (
        path.file_name().and_then(|name| name.to_str()),
        recorded_path.file_name().and_then(|name| name.to_str()),
    ) {
        (Some(path), Some(recorded)) => filenames_match_ampm_equivalent(path, recorded),
        _ => false,
    }
}

async fn pending_file_size_allows_adoption(
    asset: &PhotoAsset,
    version_size: &str,
    path: &Path,
    actual_size: u64,
    provider_size: u64,
    recorded_file: Option<&RecordedLocalFile>,
) -> bool {
    let recorded_file =
        recorded_file.filter(|recorded| path_matches_recorded_file(path, &recorded.path));
    let result = local_file_size_matches_state(
        path,
        actual_size,
        LocalFileSizeExpectation::ExactProvider(provider_size),
        recorded_file.and_then(|recorded| recorded.local_checksum.as_deref()),
        recorded_file.and_then(|recorded| recorded.download_checksum.as_deref()),
    )
    .await;
    match result {
        Ok(matches) => matches,
        Err(error) => {
            tracing::warn!(target: "kei::download::pipeline",
                asset_id = %asset.id(),
                version_size,
                path = %path.display(),
                error = %error,
                "Failed to verify size-divergent pending file"
            );
            false
        }
    }
}

async fn adopt_pending_task_path(
    db: &dyn DownloadStore,
    config: &DownloadConfig,
    asset: &PhotoAsset,
    task_planner: &mut TaskPlanner,
    task: &DownloadTask,
    recorded_file: Option<&RecordedLocalFile>,
) -> Option<PendingOnDiskAdoption> {
    let version_size = task.version_size.as_str();
    let (existing_path, existing_size) =
        task_planner.existing_path_with_size(&task.download_path)?;
    if !task_planner.retry_path_allowed(
        &task.library,
        &task.asset_id,
        task.version_size,
        &task.checksum,
        task.size,
        &existing_path,
    ) {
        return None;
    }
    if !pending_file_size_allows_adoption(
        asset,
        version_size,
        &existing_path,
        existing_size,
        task.size,
        recorded_file,
    )
    .await
    {
        return None;
    }

    if let Err(e) = planner::upsert_seen_for_task(db, config, asset, task).await {
        tracing::warn!(target: "kei::download::pipeline",
            asset_id = %asset.id(),
            version_size,
            error = %e,
            "Failed to refresh pending asset before adopting planned on-disk file"
        );
        return Some(PendingOnDiskAdoption::StateWriteFailed(existing_path));
    }

    mark_pending_downloaded_from_existing_path(
        db,
        task.library.as_ref(),
        asset,
        version_size,
        existing_path,
        capture_repair_requested(config),
    )
    .await
}

async fn adopt_pending_derived_path(
    db: &dyn DownloadStore,
    config: &DownloadConfig,
    asset: &PhotoAsset,
    task_planner: &mut TaskPlanner,
    derived: &DerivedPath,
    recorded_file: Option<&RecordedLocalFile>,
) -> Option<PendingOnDiskAdoption> {
    adopt_pending_derived_path_at(
        db,
        config,
        asset,
        task_planner,
        derived,
        &derived.path,
        recorded_file,
    )
    .await
}

async fn adopt_pending_derived_path_at(
    db: &dyn DownloadStore,
    config: &DownloadConfig,
    asset: &PhotoAsset,
    task_planner: &mut TaskPlanner,
    derived: &DerivedPath,
    path: &Path,
    recorded_file: Option<&RecordedLocalFile>,
) -> Option<PendingOnDiskAdoption> {
    let library = effective_asset_library(asset, config);
    let version_size = derived.version_size.as_str();
    let (existing_path, existing_size) = task_planner.existing_path_with_size(path)?;
    if !task_planner.retry_path_allowed(
        library,
        asset.state_id(),
        derived.version_size,
        &derived.checksum,
        derived.size,
        &existing_path,
    ) {
        return None;
    }
    if !pending_file_size_allows_adoption(
        asset,
        version_size,
        &existing_path,
        existing_size,
        derived.size,
        recorded_file,
    )
    .await
    {
        return None;
    }

    let record = asset_record_for_derived_path(Arc::from(library), asset, derived, config);
    if let Err(e) = db.upsert_seen(&record).await {
        tracing::warn!(target: "kei::download::pipeline",
            asset_id = %asset.id(),
            version_size,
            error = %e,
            "Failed to refresh pending asset before adopting on-disk file"
        );
        return Some(PendingOnDiskAdoption::StateWriteFailed(existing_path));
    }

    mark_pending_downloaded_from_existing_path(
        db,
        library,
        asset,
        version_size,
        existing_path,
        capture_repair_requested(config),
    )
    .await
}

async fn mark_pending_downloaded_from_existing_path(
    db: &dyn DownloadStore,
    library: &str,
    asset: &PhotoAsset,
    version_size: &str,
    existing_path: PathBuf,
    mark_capture_repair: bool,
) -> Option<PendingOnDiskAdoption> {
    let local_checksum = match crate::download::file::compute_sha256(&existing_path).await {
        Ok(checksum) => checksum,
        Err(e) => {
            tracing::warn!(target: "kei::download::pipeline",
                asset_id = %asset.id(),
                version_size,
                path = %existing_path.display(),
                error = %e,
                "Failed to hash on-disk file for pending asset"
            );
            return None;
        }
    };
    if let Err(e) = db
        .mark_downloaded_with_capture_repair(
            library,
            asset.state_id(),
            version_size,
            &existing_path,
            &local_checksum,
            None,
            mark_capture_repair,
        )
        .await
    {
        tracing::warn!(target: "kei::download::pipeline",
            asset_id = %asset.id(),
            version_size,
            path = %existing_path.display(),
            error = %e,
            "Failed to mark pending asset downloaded from on-disk file"
        );
        return Some(PendingOnDiskAdoption::StateWriteFailed(existing_path));
    }
    tracing::info!(target: "kei::download::pipeline",
        asset_id = %asset.id(),
        version_size,
        path = %existing_path.display(),
        "Resolved pending asset from existing on-disk file"
    );
    Some(PendingOnDiskAdoption::Adopted(existing_path))
}

async fn state_path_size_allows_skip(
    asset: &PhotoAsset,
    version_size: VersionSizeKey,
    path: &Path,
    on_disk_size: u64,
    expected_size: u64,
    recorded_file: &RecordedLocalFile,
) -> bool {
    let result = local_file_size_matches_state(
        path,
        on_disk_size,
        LocalFileSizeExpectation::AtLeastProvider(expected_size),
        recorded_file.local_checksum.as_deref(),
        recorded_file.download_checksum.as_deref(),
    )
    .await;
    match result {
        Ok(true) => true,
        Ok(false) => {
            tracing::warn!(target: "kei::download::pipeline",
                asset_id = %asset.id(),
                version_size = %version_size.as_str(),
                path = %path.display(),
                on_disk_size,
                expected_size,
                "State path is smaller than expected; re-downloading instead of skipping"
            );
            false
        }
        Err(error) => {
            tracing::warn!(target: "kei::download::pipeline",
                asset_id = %asset.id(),
                version_size = %version_size.as_str(),
                path = %path.display(),
                error = %error,
                "Failed to verify size-divergent state path; re-downloading instead of skipping"
            );
            false
        }
    }
}

fn stored_path_matches_current_collision_family(
    asset_id: &str,
    derived: &DerivedPath,
    derived_paths: &[DerivedPath],
    config: &DownloadConfig,
    stored_path: &Path,
) -> bool {
    if stored_path.parent() != derived.path.parent() {
        return false;
    }

    let Some(stored_filename) = stored_path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };

    collision_family_base_filenames(asset_id, derived, derived_paths, config)
        .iter()
        .any(|base| stored_filename_matches_base_family(stored_filename, base, asset_id))
}

fn collision_family_base_filenames(
    asset_id: &str,
    derived: &DerivedPath,
    derived_paths: &[DerivedPath],
    config: &DownloadConfig,
) -> Vec<String> {
    let mut bases = vec![
        derived.filename.clone(),
        crate::download::paths::add_dedup_suffix(&derived.filename, derived.size),
        crate::download::paths::insert_asset_identity_suffix(&derived.filename, asset_id),
    ];

    if derived.version_size.is_live_photo_motion()
        && let Some(primary) = primary_derived_path(derived_paths)
    {
        let primary_collision_filenames = [
            crate::download::paths::add_dedup_suffix(&primary.filename, primary.size),
            crate::download::paths::insert_asset_identity_suffix(&primary.filename, asset_id),
        ];
        for primary_filename in primary_collision_filenames {
            bases.push(live_photo_motion_filename_for_primary(
                &primary_filename,
                config,
            ));
        }
    }

    bases.sort();
    bases.dedup();
    bases
}

fn primary_derived_path(derived_paths: &[DerivedPath]) -> Option<&DerivedPath> {
    derived_paths
        .iter()
        .find(|derived| derived.version_size.is_primary_media())
}

fn live_photo_motion_filename_for_primary(
    primary_filename: &str,
    config: &DownloadConfig,
) -> String {
    match config.live_photo_mov_filename_policy {
        crate::types::LivePhotoMovFilenamePolicy::Suffix => {
            crate::download::paths::live_photo_mov_path_suffix(primary_filename)
        }
        crate::types::LivePhotoMovFilenamePolicy::Original => {
            crate::download::paths::live_photo_mov_path_original(primary_filename)
        }
    }
}

fn stored_filename_matches_base_family(stored_filename: &str, base: &str, asset_id: &str) -> bool {
    let base = crate::download::paths::clean_filename(base);
    let base = base.as_ref();
    filenames_match_ampm_equivalent(stored_filename, base)
        || crate::download::paths::filename_matches_identity_collision(
            base,
            asset_id,
            stored_filename,
        )
        || crate::download::paths::filename_matches_identity_collision(
            &crate::download::paths::normalize_ampm(base),
            asset_id,
            &crate::download::paths::normalize_ampm(stored_filename),
        )
}

fn filenames_match_ampm_equivalent(a: &str, b: &str) -> bool {
    a == b || crate::download::paths::normalize_ampm(a) == crate::download::paths::normalize_ampm(b)
}

pub(in crate::download) async fn recorded_current_path_exists(
    config: &DownloadConfig,
    asset: &PhotoAsset,
    version_size: VersionSizeKey,
    task_planner: &mut TaskPlanner,
    recorded_file: &RecordedLocalFile,
) -> Option<PathBuf> {
    let stored_path = &recorded_file.path;
    let derived_paths = derive_expected_paths(asset, config);

    for derived in &derived_paths {
        if derived.version_size != version_size {
            continue;
        }
        let Some((existing_path, existing_size)) =
            task_planner.existing_path_with_size(&derived.path)
        else {
            continue;
        };
        if existing_path.as_path() == stored_path.as_path() {
            if state_path_size_allows_skip(
                asset,
                derived.version_size,
                &existing_path,
                existing_size,
                derived.size,
                recorded_file,
            )
            .await
            {
                return Some(existing_path);
            }
            return None;
        }
    }

    for derived in &derived_paths {
        if derived.version_size != version_size {
            continue;
        }
        if !stored_path_matches_current_collision_family(
            asset.state_id(),
            derived,
            &derived_paths,
            config,
            stored_path,
        ) {
            continue;
        }
        let (existing_path, existing_size) = task_planner.existing_path_with_size(stored_path)?;
        if state_path_size_allows_skip(
            asset,
            version_size,
            &existing_path,
            existing_size,
            derived.size,
            recorded_file,
        )
        .await
        {
            return Some(existing_path);
        }
        return None;
    }

    None
}

pub(in crate::download) async fn state_confirmed_current_path_exists(
    ctx: &DownloadContext,
    config: &DownloadConfig,
    asset: &PhotoAsset,
    task: &DownloadTask,
    task_planner: &mut TaskPlanner,
) -> Option<PathBuf> {
    let recorded_file = ctx.downloaded_file(&task.library, &task.asset_id, task.version_size)?;
    recorded_current_path_exists(
        config,
        asset,
        task.version_size,
        task_planner,
        recorded_file,
    )
    .await
}

#[cfg(test)]
mod tests;
