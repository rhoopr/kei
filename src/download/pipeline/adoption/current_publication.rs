//! Reuse a still-current publication whose rendition was left pending.

#[cfg(target_os = "linux")]
use std::path::Path;
use std::sync::Arc;

use anyhow::{Result, anyhow, ensure};
use tokio_util::sync::CancellationToken;

use crate::download::filter::{
    derive_expected_paths, is_asset_filtered, stored_path_matches_download_family,
};
use crate::download::planner::TaskPlanner;
use crate::download::retry::PreparedReplaceAuthorization;
use crate::download::{DownloadConfig, DownloadStore};
use crate::icloud::photos::PhotoAsset;
use crate::state::VersionSizeKey;
use crate::state::db::PreparedPublication;

use super::{PendingOnDiskAdoption, asset_record_for_derived_path, effective_asset_library};

fn authorization_matches(
    authorization: &PreparedReplaceAuthorization,
    proof: &PreparedPublication,
    library: &str,
    asset: &PhotoAsset,
    version: VersionSizeKey,
) -> bool {
    authorization.version_size == version
        && authorization.library.as_ref() == library
        && proof.library == authorization.library.as_ref()
        && proof.id == authorization.asset_id.as_ref()
        && proof.id == asset.state_id()
        && proof.version == authorization.version_size.as_str()
        && proof.provider_checksum == authorization.provider_checksum
        && proof.provider_size == authorization.provider_size
        && proof.destination == authorization.native_path
        && proof.binding.as_ref() == Some(&authorization.binding)
        && authorization.binding.library == library
        && authorization.binding.child == asset.asset_record_name()
        && authorization.binding.master == asset.id()
        && authorization.binding.state_id == asset.state_id()
}

#[cfg(all(test, target_os = "linux"))]
pub(super) mod completed_link_test_hook {
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::sync::Mutex;
    use tokio::sync::Notify;

    #[derive(Clone, Copy, Debug, PartialEq)]
    pub(super) enum Stage {
        CleanupRetained,
        CleanupPrechecked,
        ReplaceReady,
    }
    type Pause = (PathBuf, Stage, Arc<Notify>, Arc<Notify>);
    static PAUSES: Mutex<Vec<Pause>> = Mutex::new(Vec::new());

    pub(super) fn arm_pause(path: &Path, stage: Stage) -> (Arc<Notify>, Arc<Notify>) {
        let reached = Arc::new(Notify::new());
        let resume = Arc::new(Notify::new());
        PAUSES
            .lock()
            .unwrap()
            .push((path.into(), stage, reached.clone(), resume.clone()));
        (reached, resume)
    }

    pub(super) async fn pause(path: &Path, stage: Stage) {
        let pause = {
            let mut pauses = PAUSES.lock().unwrap();
            pauses
                .iter()
                .position(|p| p.0 == path && p.1 == stage)
                .map(|i| pauses.remove(i))
        };
        if let Some((_, _, reached, resume)) = pause {
            reached.notify_one();
            resume.notified().await;
        }
    }

    static FAIL_PARENT_SYNC: Mutex<Option<PathBuf>> = Mutex::new(None);

    pub(super) fn arm(target: &Path) {
        if let Ok(mut guard) = FAIL_PARENT_SYNC.lock() {
            *guard = Some(target.to_path_buf());
        }
    }

    pub(super) fn disarm() {
        if let Ok(mut guard) = FAIL_PARENT_SYNC.lock() {
            *guard = None;
        }
    }

    pub(super) fn should_fail(target: &Path) -> bool {
        FAIL_PARENT_SYNC
            .lock()
            .is_ok_and(|guard| guard.as_deref() == Some(target))
    }
}

#[cfg(target_os = "linux")]
fn prepared_owner_matches(
    proof: &PreparedPublication,
    library: &str,
    asset: &PhotoAsset,
    version: VersionSizeKey,
) -> bool {
    let Some(binding) = proof.binding.as_ref() else {
        return false;
    };
    proof.library == library
        && proof.id == asset.state_id()
        && proof.version == version.as_str()
        && binding.library == library
        && binding.child == asset.asset_record_name()
        && binding.master == asset.id()
        && binding.state_id == asset.state_id()
        && !binding.account_key.trim().is_empty()
        && !binding.provider_key.trim().is_empty()
        && !binding.scope.trim().is_empty()
        && !binding.origin_pass.trim().is_empty()
}

/// Idempotent directory durability for the completed prepared publication.
/// The proof identifies both parents; a retry after a successful unlink must
/// still prove those parents durable before retiring the proof.
#[cfg(target_os = "linux")]
async fn sync_completed_parents(root: &Path, part: &Path, target: &Path) -> Result<()> {
    let (root, part, target) = (root.to_path_buf(), part.to_path_buf(), target.to_path_buf());
    tokio::task::spawn_blocking(move || {
        let part_path = crate::fs_util::ConfinedPath::open(
            &root,
            &part,
            crate::fs_util::ConfinedParents::Existing,
        )?;
        let target_path = crate::fs_util::ConfinedPath::open(
            &root,
            &target,
            crate::fs_util::ConfinedParents::Existing,
        )?;
        part_path.sync_parent()?;
        target_path.sync_parent()?;
        #[cfg(test)]
        if completed_link_test_hook::should_fail(target_path.path()) {
            return Err(std::io::Error::other("synthetic parent durability failure").into());
        }
        Ok(())
    })
    .await?
}

/// Remove only the exact published hardlink, then prove both parents durable.
#[cfg(target_os = "linux")]
async fn finish_completed_published_link(
    root: &Path,
    proof: &PreparedPublication,
    retained: &Arc<crate::download::file::RetainedPendingFile>,
    shutdown: &CancellationToken,
) -> Result<()> {
    let expected = retained.fingerprint;
    let retained = Arc::clone(retained);
    let part_path = proof.temporary.to_path();
    let target_path = proof.destination.to_path();
    #[cfg(test)]
    completed_link_test_hook::pause(
        &target_path,
        completed_link_test_hook::Stage::CleanupRetained,
    )
    .await;
    let (precheck_root, precheck_part) = (root.to_path_buf(), part_path.clone());
    #[cfg(test)]
    let precheck_target = target_path.clone();
    let precheck = tokio::task::spawn_blocking(move || {
        let part = crate::fs_util::ConfinedPath::open(
            &precheck_root,
            &precheck_part,
            crate::fs_util::ConfinedParents::Existing,
        )?;
        let Some(file) = part.open_optional_regular()? else {
            return Ok(None);
        };
        let links = crate::fs_util::file_link_count(&file)?;
        anyhow::ensure!(
            links == 2,
            "Prepared part is not the exact published link; retaining evidence"
        );
        // Authority stays with the retained target, never a newly opened part.
        let identity = retained.identity()?;
        anyhow::ensure!(
            crate::fs_util::file_identity(&file)? == identity,
            "Prepared part no longer belongs to the retained target; retaining evidence"
        );
        #[cfg(test)]
        tokio::runtime::Handle::current().block_on(completed_link_test_hook::pause(
            &precheck_target,
            completed_link_test_hook::Stage::CleanupPrechecked,
        ));
        Ok(Some((file, identity)))
    })
    .await??;
    ensure!(
        !shutdown.is_cancelled(),
        "Prepared publication cleanup cancelled"
    );
    let Some((_part_handle, identity)) = precheck else {
        return sync_completed_parents(root, &part_path, &target_path).await;
    };
    let layout = crate::state::db::primary_layout::LayoutFingerprint {
        size: expected.size,
        sha256: expected.sha256,
        identity,
    };
    crate::download::file::primary_layout::recover_prepared_link(
        root,
        &part_path,
        &target_path,
        &layout,
    )
    .await?;
    let (check_root, check_part) = (root.to_path_buf(), part_path.clone());
    let exists = tokio::task::spawn_blocking(move || {
        crate::fs_util::ConfinedPath::open(
            &check_root,
            &check_part,
            crate::fs_util::ConfinedParents::Existing,
        )?
        .entry_exists()
    })
    .await??;
    anyhow::ensure!(
        !exists,
        "Prepared part remained after verified completion; retaining evidence"
    );
    sync_completed_parents(root, &part_path, &target_path).await
}

#[cfg(target_os = "linux")]
async fn prepared_part_exists(root: &Path, proof: &PreparedPublication) -> Result<bool> {
    let (root, part) = (root.to_path_buf(), proof.temporary.to_path());
    Ok(tokio::task::spawn_blocking(move || {
        crate::fs_util::ConfinedPath::open(&root, &part, crate::fs_util::ConfinedParents::Existing)?
            .entry_exists()
    })
    .await??)
}

pub(in crate::download) async fn recover_current_pending_publication(
    db: &dyn DownloadStore,
    config: &DownloadConfig,
    asset: &PhotoAsset,
    planner: &mut TaskPlanner,
    version: VersionSizeKey,
    replace: Option<&PreparedReplaceAuthorization>,
    shutdown: &CancellationToken,
) -> Result<Option<PendingOnDiskAdoption>> {
    ensure!(
        !shutdown.is_cancelled(),
        "Current publication recovery cancelled"
    );
    if !planner.allows_existing_mutation(config) {
        // A retained hash is not evidence that this operation created the
        // destination. Let independent read-only matching run; keep journals.
        return Ok(None);
    }
    if is_asset_filtered(asset, config).is_none() {
        let library = effective_asset_library(asset, config);
        let derived_paths = derive_expected_paths(asset, config);
        if let Some(derived) = derived_paths
            .iter()
            .find(|path| path.version_size == version)
        {
            for proof in db
                .get_prepared_publications(library, asset.state_id(), version)
                .await?
            {
                let destination = proof.destination.to_path();
                if proof.provider_checksum != derived.checksum.as_ref()
                    || proof.provider_size != derived.size
                    || !stored_path_matches_download_family(
                        asset.state_id(),
                        derived,
                        &derived_paths,
                        config,
                        &destination,
                    )
                    || !planner.retry_path_allowed(
                        library,
                        asset.state_id(),
                        asset.asset_record_name(),
                        version,
                        &proof.provider_checksum,
                        proof.provider_size,
                        &destination,
                    )
                {
                    continue;
                }
                let _guard =
                    crate::download::file::lock_download_destination(&destination, shutdown)
                        .await?;
                let mut published_replace = false;
                let retained = match crate::download::file::retain_pending_file(
                    &config.directory,
                    &destination,
                )
                .await?
                {
                    Some(retained) => {
                        if data_encoding::HEXLOWER.encode(&retained.fingerprint.sha256)
                            == proof.local_checksum
                        {
                            #[cfg(target_os = "linux")]
                            if proof.binding.is_some() {
                                anyhow::ensure!(
                                    prepared_owner_matches(&proof, library, asset, version),
                                    "Prepared publication owner changed; retaining evidence"
                                );
                                let temporary = proof.temporary.to_path();
                                // Revalidate the durable owner before any unlink.
                                // prepare_temp_publication alone does not compare
                                // child/master fields; prepared_owner_matches does.
                                db.prepare_temp_publication(&temporary, &proof).await?;
                                retained.validate().await?;
                                ensure!(
                                    !shutdown.is_cancelled(),
                                    "Prepared publication cleanup cancelled"
                                );
                                finish_completed_published_link(
                                    &config.directory,
                                    &proof,
                                    &retained,
                                    shutdown,
                                )
                                .await?;
                                retained.validate().await?;
                            } else {
                                anyhow::ensure!(
                                    !prepared_part_exists(&config.directory, &proof).await?,
                                    "Unbound prepared part cannot be cleaned; retaining evidence"
                                );
                            }
                            retained
                        } else {
                            // The replacement journal may have restored the exact
                            // truncated original after the prepared part was
                            // verified. Only an explicitly authorized repair for
                            // this recorded path can replace those exact bytes.
                            let Some(replace) = replace else {
                                continue;
                            };
                            if !config.repair_truncated
                                || !authorization_matches(replace, &proof, library, asset, version)
                                || retained.fingerprint != replace.expected
                            {
                                continue;
                            }
                            let temporary = proof.temporary.to_path();
                            // Revalidate the durable owner and current provider
                            // generation before publishing. The conditional
                            // publisher rechecks the old target immediately and
                            // restores displaced bytes on failure.
                            let mut completed = async {
                                db.prepare_temp_publication(&temporary, &proof).await?;
                                let completed =
                                    crate::download::file::DownloadedFile::reopen_prepared(
                                        &config.directory,
                                        &temporary,
                                        &proof.local_checksum,
                                    )
                                    .await?;
                                #[cfg(all(test, target_os = "linux"))]
                                completed_link_test_hook::pause(
                                    &destination,
                                    completed_link_test_hook::Stage::ReplaceReady,
                                )
                                .await;
                                Ok::<_, anyhow::Error>(completed)
                            }
                            .await?;
                            ensure!(
                                !shutdown.is_cancelled(),
                                "Prepared publication recovery cancelled"
                            );
                            completed
                                .publish(
                                    &destination,
                                    crate::download::file::FinalPublication::ReplaceTruncated(
                                        replace.expected,
                                    ),
                                )
                                .await?;
                            let retained = completed
                                .retain_pending_publication(&config.directory, &destination)
                                .await?;
                            published_replace = true;
                            retained
                        }
                    }
                    None => {
                        let temporary = proof.temporary.to_path();
                        // Recheck the existing owner and current provider generation
                        // before publishing. This is idempotent and never claims a
                        // different part or discards older unfinished evidence.
                        db.prepare_temp_publication(&temporary, &proof).await?;
                        let mut completed = crate::download::file::DownloadedFile::reopen_prepared(
                            &config.directory,
                            &temporary,
                            &proof.local_checksum,
                        )
                        .await?;
                        ensure!(
                            !shutdown.is_cancelled(),
                            "Prepared publication recovery cancelled"
                        );
                        completed
                            .publish(
                                &destination,
                                crate::download::file::FinalPublication::NoReplace,
                            )
                            .await?;
                        completed
                            .retain_pending_publication(&config.directory, &destination)
                            .await?
                    }
                };
                if data_encoding::HEXLOWER.encode(&retained.fingerprint.sha256)
                    != proof.local_checksum
                {
                    continue;
                }
                retained.validate().await?;
                if !published_replace {
                    ensure!(
                        !shutdown.is_cancelled(),
                        "Prepared publication recovery cancelled"
                    );
                }
                match db
                    .mark_verified_download(
                        library,
                        asset.state_id(),
                        version.as_str(),
                        &destination,
                        &proof.local_checksum,
                        Some(&proof.source_checksum),
                        false,
                    )
                    .await
                {
                    Ok(()) => {
                        planner.add_downloaded_paths(vec![crate::state::DownloadedFileRecord {
                            added_at: None,
                            is_current_path: true,
                            is_preserved_history: false,
                            library: library.to_owned(),
                            id: asset.state_id().to_owned(),
                            version_size: version,
                            checksum: proof.provider_checksum,
                            local_path: Some(destination.clone()),
                            local_checksum: Some(proof.local_checksum.clone()),
                            download_checksum: Some(proof.local_checksum),
                        }]);
                        return Ok(Some(PendingOnDiskAdoption::Adopted(destination)));
                    }
                    Err(_) => {
                        return Ok(Some(PendingOnDiskAdoption::StateWriteFailed(destination)));
                    }
                }
            }
        }
    }
    // Managed selection and layout journals keep their publication owner.
    if config.selection_run.is_some() || is_asset_filtered(asset, config).is_some() {
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
