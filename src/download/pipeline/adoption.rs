//! Existing and pending local-file evidence and adoption.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use rustc_hash::FxHashSet;

use crate::download::filter::{
    DerivedPath, DownloadTask, derive_expected_paths, determine_media_type,
};
use crate::download::planner::TaskPlanner;
use crate::download::{DownloadConfig, DownloadContext, DownloadStore};
use crate::icloud::photos::PhotoAsset;
use crate::state::{AssetRecord, VersionSizeKey};

mod current_publication;
pub(in crate::download) use current_publication::recover_current_pending_publication;

pub(super) fn effective_asset_library<'a>(
    asset: &'a PhotoAsset,
    config: &'a DownloadConfig,
) -> &'a str {
    asset.source_zone().unwrap_or(config.library.as_ref())
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

/// A saved cross-parent alias protects the whole pending rendition even if
/// the current enumerating pass happens to render a same-parent alias.
async fn validate_reserved_pending_destination(
    db: &dyn DownloadStore,
    config: &DownloadConfig,
    asset: &PhotoAsset,
    ctx: &DownloadContext,
    planner: &TaskPlanner,
    derived: &DerivedPath,
) -> Result<bool> {
    let library = effective_asset_library(asset, config);
    if !pending_versions_for_asset(ctx, library, asset)
        .is_some_and(|versions| versions.contains(derived.version_size.as_str()))
    {
        return Ok(false);
    }
    let destinations = planner.cross_parent_retry_destinations(
        library,
        asset.state_id(),
        derived.version_size,
        &derived.checksum,
        derived.size,
    )?;
    for saved in &destinations {
        let destination = crate::fs_util::reserved_path_under_root(&config.directory, saved)?;
        anyhow::ensure!(
            crate::download::legacy_preservation::current_path_is_independent(db, &destination)
                .await?,
            "Reserved pending file overlaps a protected historical original"
        );
        anyhow::ensure!(
            planner.retry_path_allowed(
                library,
                asset.state_id(),
                derived.version_size,
                &derived.checksum,
                derived.size,
                &destination
            ),
            "Reserved cross-parent pending destination has incompatible ownership"
        );
    }
    // A reservation owns a destination, not its bytes. Prepared-publication
    // recovery and the shared matcher decide content reuse and revalidate it
    // inside their state transaction. Historical unequal hashes prove nothing.
    Ok(!destinations.is_empty())
}

/// Preserve finalization proof only for the already-pending rendition and its
/// current-content saved cross-parent aliases, including direct dispatch paths.
pub(in crate::download) fn mark_reserved_pending_task(
    ctx: &DownloadContext,
    planner: &TaskPlanner,
    config: &DownloadConfig,
    task: &mut DownloadTask,
) -> Result<()> {
    let pending = ctx
        .pending_ids
        .get(task.library.as_ref())
        .and_then(|assets| assets.get(task.asset_id.as_ref()))
        .is_some_and(|versions| versions.contains(task.version_size.as_str()));
    if pending
        && !planner
            .cross_parent_retry_destinations(
                &task.library,
                &task.asset_id,
                task.version_size,
                &task.checksum,
                task.size,
            )?
            .is_empty()
    {
        task.pending_cross_parent_root = Some(Arc::new(config.directory.to_path_buf()));
    }
    Ok(())
}

pub(in crate::download) async fn validate_enumerated_reserved_pending(
    db: &dyn DownloadStore,
    config: &DownloadConfig,
    asset: &PhotoAsset,
    ctx: &DownloadContext,
    planner: &TaskPlanner,
) -> Result<bool> {
    let mut affected = false;
    for derived in derive_expected_paths(asset, config) {
        affected |=
            validate_reserved_pending_destination(db, config, asset, ctx, planner, &derived)
                .await?;
    }
    Ok(affected)
}

/// Resolve current publications before a fresh plan can save another destination.
/// Both completed and failed finalizations must stay out of dispatch and saving.
#[derive(Default)]
pub(super) struct PendingCurrentPublications {
    pub(super) retained_versions: FxHashSet<VersionSizeKey>,
    pub(super) state_write_failures: usize,
}

pub(super) async fn recover_pending_current_publications(
    state_db: Option<&dyn DownloadStore>,
    config: &DownloadConfig,
    asset: &PhotoAsset,
    ctx: &DownloadContext,
    task_planner: &mut TaskPlanner,
    planned_tasks: &[DownloadTask],
    shutdown: &tokio_util::sync::CancellationToken,
) -> Result<PendingCurrentPublications> {
    let mut recovered = PendingCurrentPublications::default();
    let Some(db) = state_db else {
        return Ok(recovered);
    };
    let library = effective_asset_library(asset, config);
    let pending_versions = pending_versions_for_asset(ctx, library, asset);
    for derived in derive_expected_paths(asset, config) {
        let pending = pending_versions
            .is_some_and(|versions| versions.contains(derived.version_size.as_str()));
        // A healthy current publication must not acquire a fresh different
        // choice that would preclude its next retry. Do not reinterpret an
        // already durable destination or hash healthy files without a choice.
        let fresh_choice = planned_tasks.iter().any(|task| {
            if task.library.as_ref() != library
                || task.asset_id.as_ref() != asset.state_id()
                || task.version_size != derived.version_size
                || task.checksum != derived.checksum
                || task.size != derived.size
                || task_planner.has_durable_destination(task)
            {
                return false;
            }
            let Ok(destination) = crate::fs_util::confined_path_key(&task.download_path) else {
                return false;
            };
            task_planner
                .reconciliation_reservations()
                .iter()
                .any(|reservation| {
                    reservation.library == task.library
                        && reservation.asset_id.as_ref() == task.asset_id.as_ref()
                        && reservation.version_size == task.version_size
                        && reservation.content.as_ref().is_some_and(|content| {
                            content.checksum == task.checksum && content.size == task.size
                        })
                        && reservation.destination_path_key.0 == destination
                        && crate::fs_util::confined_path_key(&reservation.destination_path)
                            .is_ok_and(|key| key == destination)
                })
        });
        if !pending && !fresh_choice {
            continue;
        }
        if let Some(adoption) = recover_current_pending_publication(
            db,
            config,
            asset,
            task_planner,
            derived.version_size,
            shutdown,
        )
        .await?
        {
            recovered.retained_versions.insert(derived.version_size);
            if matches!(adoption, PendingOnDiskAdoption::StateWriteFailed(_)) {
                recovered.state_write_failures += 1;
            }
        }
    }
    Ok(recovered)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::download) enum PendingOnDiskAdoption {
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

#[cfg(test)]
mod tests;

#[cfg(test)]
mod cross_parent_validation_tests;
