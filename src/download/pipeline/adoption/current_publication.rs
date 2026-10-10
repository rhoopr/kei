//! Reuse a still-current publication whose rendition was left pending.

use std::sync::Arc;

use anyhow::{Result, anyhow, ensure};
use tokio_util::sync::CancellationToken;

use crate::download::filter::{
    derive_expected_paths, is_asset_filtered, stored_path_matches_download_family,
};
use crate::download::planner::TaskPlanner;
use crate::download::{DownloadConfig, DownloadStore};
use crate::icloud::photos::PhotoAsset;
use crate::state::VersionSizeKey;

use super::{PendingOnDiskAdoption, asset_record_for_derived_path, effective_asset_library};

pub(in crate::download) async fn recover_current_pending_publication(
    db: &dyn DownloadStore,
    config: &DownloadConfig,
    asset: &PhotoAsset,
    planner: &mut TaskPlanner,
    version: VersionSizeKey,
    shutdown: &CancellationToken,
) -> Result<Option<PendingOnDiskAdoption>> {
    ensure!(
        !shutdown.is_cancelled(),
        "Current publication recovery cancelled"
    );
    // Managed selection and layout journals keep their publication owner.
    if config.selection_run.is_some()
        || planner.managed_layout_session().is_some()
        || is_asset_filtered(asset, config).is_some()
    {
        return Ok(None);
    }
    let derived_paths = derive_expected_paths(asset, config);
    let Some(derived) = derived_paths
        .iter()
        .find(|path| path.version_size == version)
    else {
        return Ok(None);
    };
    let library = effective_asset_library(asset, config);
    // Exact saved cross-parent recovery owns its destinations and keeps priority.
    if !planner
        .cross_parent_retry_destinations(
            library,
            asset.state_id(),
            version,
            &derived.checksum,
            derived.size,
        )?
        .is_empty()
    {
        return Ok(None);
    }
    let candidate = db
        .get_pending_publication(library, asset.state_id(), version)
        .await?;
    ensure!(
        !shutdown.is_cancelled(),
        "Current publication recovery cancelled"
    );
    let Some(proof) = candidate else {
        return Ok(None);
    };
    if proof.checksum != derived.checksum.as_ref()
        || proof.size != derived.size
        || !stored_path_matches_download_family(
            asset.state_id(),
            derived,
            &derived_paths,
            config,
            &proof.local_path,
        )
    {
        return Ok(None);
    }
    ensure!(
        planner.pending_publication_path_owned(&proof),
        "Current publication ownership is incomplete or incompatible; retaining pending work"
    );
    let _destination_guard =
        crate::download::file::lock_download_destination(&proof.local_path, shutdown)
            .await
            .map_err(|_private_error| {
                anyhow!("Current publication destination could not be acquired")
            })?;
    ensure!(
        !shutdown.is_cancelled(),
        "Current publication recovery cancelled"
    );
    let retained = crate::download::file::retain_pending_file(&config.directory, &proof.local_path)
        .await
        .map_err(|_private_error| anyhow!("Current publication cannot be inspected safely"))?;
    ensure!(
        !shutdown.is_cancelled(),
        "Current publication recovery cancelled"
    );
    let Some(retained) = retained else {
        return Ok(None);
    };
    if retained.fingerprint.size != derived.size
        || data_encoding::HEXLOWER.encode(&retained.fingerprint.sha256) != proof.local_checksum
    {
        return Ok(None);
    }
    let selected = asset_record_for_derived_path(Arc::from(library), asset, derived, config);
    retained.validate().await.map_err(|_private_error| {
        anyhow!("Current publication changed before recovery finalization")
    })?;
    ensure!(
        !shutdown.is_cancelled(),
        "Current publication recovery cancelled"
    );
    // Await the real DB outcome even if cancellation arrives while its blocking
    // worker runs. Dropping this future would not cancel that transaction.
    let finalized = db
        .recover_pending_publication(
            &proof,
            &selected,
            asset.asset_record_name(),
            asset.id(),
            shutdown,
        )
        .await;
    drop(retained);
    match finalized {
        Ok(true) => {
            planner.remember_recovered_publication(&proof);
            Ok(Some(PendingOnDiskAdoption::Adopted(proof.local_path)))
        }
        Ok(false) => {
            ensure!(
                !shutdown.is_cancelled(),
                "Current publication recovery cancelled"
            );
            anyhow::bail!(
                "Current publication state changed before recovery; retaining pending work"
            )
        }
        Err(error) => {
            tracing::warn!(%error, "Failed to recover current publication; skipping re-download");
            Ok(Some(PendingOnDiskAdoption::StateWriteFailed(
                proof.local_path,
            )))
        }
    }
}

#[cfg(test)]
mod tests;
