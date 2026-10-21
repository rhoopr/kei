//! Selected sparse targets use exact provider identity and the normal pipeline.
use crate::download::{DownloadConfig, DownloadControls};
use crate::icloud::photos::asset::SparseShareEvidence;
use crate::icloud::photos::{PhotoAlbum, PhotoAsset, PhotoLibrary, RecordResolution};
use crate::state::{db::SparseTargetState, error::StateError};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

#[derive(Clone)]
pub(crate) struct SelectedSparseTarget {
    pub(crate) library: PhotoLibrary,
    pub(crate) album: PhotoAlbum,
    pub(crate) config: Arc<DownloadConfig>,
    pub(crate) client: reqwest::Client,
    pub(crate) controls: DownloadControls,
}
fn outcome(decision: &'static str, reason: &'static str) {
    tracing::info!(
        diagnostic = "recovery_decision_v1",
        mechanism = "sparse_target",
        decision,
        reason,
        "Sparse target decision; source checkpoint still requires complete proof"
    );
}
fn unsupported(reason: &'static str) -> Option<SparseTargetState> {
    outcome("unsupported", reason);
    None
}
async fn receipts(
    target: &SelectedSparseTarget,
    asset: &PhotoAsset,
    cancel: &CancellationToken,
) -> anyhow::Result<Option<SparseTargetState>> {
    let config = &target.config;
    let Some(db) = &config.state_db else {
        return Ok(None);
    };
    let Some(before) = db
        .sparse_target_state(&config.library, asset.asset_record_name(), asset.id())
        .await?
    else {
        return Ok(None);
    };
    let versions = super::filter::extract_skip_candidates(asset, config.as_ref());
    if versions.is_empty() {
        return Ok(None);
    }
    let rows = db
        .legacy_child_receipts(&config.library, asset.asset_record_name())
        .await?;
    let catalog = db.get_reconciliation_catalog_paths().await?;
    let mut retained = Vec::new();
    let mut checked_paths = Vec::new();
    for (version, checksum) in versions {
        let Some(row) = rows.iter().find(|r| r.version_size == version) else {
            return Ok(None);
        };
        if row.checksum.as_ref() != checksum
            || Some(row.created_at) != asset.asset_date_evidence()
            || row.added_at != asset.added_date_evidence()
            || row.metadata.compute_hash()
                != super::filter::metadata_for_selected_version(asset, config.as_ref(), version)
                    .compute_hash()
            || !matches!(
                db.legacy_child_provenance(
                    &config.library,
                    asset.asset_record_name(),
                    version.as_str()
                )
                .await?,
                Some(crate::state::db::trusted_legacy::LegacyReceiptProvenance::VerifiedSource)
            )
        {
            return Ok(None);
        }
        let Some(path) = &row.local_path else {
            return Ok(None);
        };
        let key = crate::fs_util::confined_path_key(path)?;
        for owner in &catalog {
            if crate::fs_util::confined_path_key(&owner.path)? == key
                && (owner.library.as_ref() != config.library.as_ref()
                    || owner.asset_id.as_ref() != asset.asset_record_name())
            {
                return Ok(None);
            }
        }
        db.guard_primary_writer(path).await?;
        let file =
            super::legacy_preservation::strict_current_file(config, path, true, cancel).await?;
        if file.sha256 != row.local_checksum {
            return Ok(None);
        }
        let Some(held) = super::file::retain_pending_file(&config.directory, path).await? else {
            return Ok(None);
        };
        if Some(data_encoding::HEXLOWER.encode(&held.fingerprint.sha256)) != row.local_checksum {
            return Ok(None);
        }
        retained.push(held);
        checked_paths.push((path.clone(), true));
        let mut sidecar = path.as_os_str().to_owned();
        sidecar.push(".xmp");
        let sidecar = std::path::PathBuf::from(sidecar);
        #[cfg(feature = "xmp")]
        let required = config.metadata.xmp_sidecar;
        #[cfg(not(feature = "xmp"))]
        let required = false;
        db.guard_primary_writer(&sidecar).await?;
        let sidecar_key = crate::fs_util::confined_path_key(&sidecar)?;
        for owner in &catalog {
            if crate::fs_util::confined_path_key(&owner.path)? == sidecar_key {
                return Ok(None);
            }
        }
        super::legacy_preservation::strict_current_file(config, &sidecar, required, cancel).await?;
        let held = super::file::retain_pending_file(&config.directory, &sidecar).await?;
        checked_paths.push((sidecar, held.is_some()));
        if let Some(held) = held {
            retained.push(held);
        }
    }
    if cancel.is_cancelled()
        || db
            .sparse_target_state(&config.library, asset.asset_record_name(), asset.id())
            .await?
            .as_ref()
            != Some(&before)
    {
        return Ok(None);
    }
    let cancel = cancel.clone();
    let root = config.directory.to_path_buf();
    let suffix = config.temp_suffix.clone();
    Ok(Some(before.with_file_validation(Arc::new(move || {
        if cancel.is_cancelled() {
            return Err(StateError::Invariant {
                operation: "sparse_target_files",
                detail: "target validation cancelled".into(),
            });
        }
        validate_paths(&root, &checked_paths, &suffix).map_err(|_private_error| {
            StateError::Invariant {
                operation: "sparse_target_files",
                detail: "target publication state changed".into(),
            }
        })?;
        for file in &retained {
            if file.link_count().ok() != Some(1) {
                return Err(StateError::Invariant {
                    operation: "sparse_target_files",
                    detail: "target link policy changed".into(),
                });
            }
            file.validate_blocking()
                .map_err(|_private_error| StateError::Invariant {
                    operation: "sparse_target_files",
                    detail: "qualified target file changed".into(),
                })?;
        }
        Ok(())
    }))))
}
pub(crate) async fn recover(
    evidence: &SparseShareEvidence,
    config: &DownloadConfig,
    cancel: &CancellationToken,
    stats: &mut crate::download::SyncStats,
) -> anyhow::Result<Option<SparseTargetState>> {
    let SparseShareEvidence::Linked(link) = evidence else {
        return Ok(unsupported("target_scope_unqualified"));
    };
    let mut matched = None;
    for target in config.sparse_targets.iter() {
        if target.album.sparse_link_matches(link) {
            if matched.is_some() {
                return Ok(unsupported("target_scope_unqualified"));
            }
            matched = Some(target);
        }
    }
    let Some(target) = matched else {
        return Ok(unsupported("target_not_selected"));
    };
    let Some(db) = &target.config.state_db else {
        return Ok(unsupported("target_receipt_unqualified"));
    };
    if cancel.is_cancelled() || !target.controls.run_mode.downloads_files() {
        return Ok(None);
    }
    if !target.library.check_indexing().await?
        || db
            .get_metadata(&crate::sync_cycle::indexing_reconciliation_key(
                &target.config.library,
            ))
            .await?
            .is_some()
        || db.has_primary_layouts(&target.config.library).await?
    {
        return Ok(unsupported("target_policy_unsupported"));
    }
    let asset = match target.album.resolve_sparse_target(link).await {
        RecordResolution::Present(asset) => asset,
        RecordResolution::TransientFailure(error) => return Err(error.into()),
        _ => return Ok(unsupported("target_pair_unresolved")),
    };
    if asset.metadata().is_hidden
        || asset.metadata().is_deleted
        || cancel.is_cancelled()
        || super::filter::is_asset_filtered(&asset, target.config.as_ref()).is_some()
        || asset.asset_record_name() == asset.id()
        || super::filter::extract_skip_candidates(&asset, target.config.as_ref()).is_empty()
    {
        return Ok(unsupported("target_policy_unsupported"));
    }
    match receipts(target, &asset, cancel).await {
        Ok(Some(proof)) => {
            outcome("partial", "target_receipt_ready");
            return Ok(Some(proof));
        }
        Err(_) => return Ok(unsupported("target_file_unverified")),
        Ok(None) => {}
    }
    let child = Arc::from(asset.asset_record_name());
    let asset = asset
        .with_state_record_name(child)
        .with_source_zone(Arc::clone(&target.config.library));
    let verify_asset = asset.clone();
    let result = super::pipeline::stream_and_download_from_stream(
        &target.client,
        futures_util::stream::iter(vec![Ok(asset)]),
        &target.config,
        target.controls,
        1,
        cancel.clone(),
        super::pipeline::StreamRuntime::new(None, None),
    )
    .await?;
    stats.downloaded += result.downloaded;
    stats.failed += result.failed.len();
    stats.exif_failures += result.exif_failures;
    stats.state_write_failures += result.state_write_failures;
    stats.bytes_downloaded += result.bytes_downloaded;
    stats.disk_bytes_written += result.disk_bytes_written;
    stats.photos_downloaded += result.photos_downloaded;
    stats.videos_downloaded += result.videos_downloaded;
    stats.enumeration_errors += result.enumeration_errors;
    if result.provider_auth_errors > 0 || result.auth_errors > 0 {
        return Err(crate::icloud::photos::ProviderLookupError::Authentication {
            status: 401,
            message: "sparse target authentication failed".into(),
        }
        .into());
    }
    if !result.failed.is_empty()
        || result.state_write_failures > 0
        || result.enumeration_errors > 0
        || result.exif_failures > 0
        || cancel.is_cancelled()
    {
        outcome("partial", "target_materialization_failed");
        return Ok(None);
    }
    match receipts(target, &verify_asset, cancel).await {
        Ok(Some(proof)) => {
            outcome("partial", "target_receipt_ready");
            Ok(Some(proof))
        }
        Ok(None) => Ok(unsupported("target_receipt_unqualified")),
        Err(_) => Ok(unsupported("target_file_unverified")),
    }
}

// Recheck both present and absent companion files without reopening the database.
fn validate_paths(
    root: &std::path::Path,
    paths: &[(std::path::PathBuf, bool)],
    suffix: &str,
) -> anyhow::Result<()> {
    for (path, present) in paths {
        anyhow::ensure!(
            !super::file::has_replacement_journal_blocking(root, path)?,
            "target replacement is pending"
        );
        for suffix in [suffix, ".part"] {
            let mut temporary = path.as_os_str().to_owned();
            temporary.push(suffix);
            anyhow::ensure!(
                !crate::fs_util::ConfinedPath::open(
                    root,
                    std::path::Path::new(&temporary),
                    crate::fs_util::ConfinedParents::Existing
                )?
                .entry_exists()?,
                "target temporary write is pending"
            );
        }
        let file = crate::fs_util::ConfinedPath::open(
            root,
            path,
            crate::fs_util::ConfinedParents::Existing,
        )?
        .open_optional_regular()?;
        anyhow::ensure!(
            file.is_some() == *present,
            "target companion presence changed"
        );
        if let Some(file) = file {
            anyhow::ensure!(
                crate::fs_util::file_link_count(&file)? == 1,
                "target hardlink policy changed"
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::validate_paths;
    #[test]
    fn sparse_target_file_gate_rechecks_publication_and_companion_absence() {
        let root = tempfile::tempdir().unwrap();
        let media = root.path().join("photo.jpg");
        let sidecar = root.path().join("photo.jpg.xmp");
        std::fs::write(&media, b"retained bytes").unwrap();
        let paths = vec![(media.clone(), true), (sidecar.clone(), false)];
        validate_paths(root.path(), &paths, ".download").unwrap();
        for path in [
            root.path().join("photo.jpg.part"),
            root.path().join("photo.jpg.download"),
            root.path().join("photo.jpg.xmp.part"),
            root.path().join("photo.jpg.xmp.download"),
            sidecar,
        ] {
            std::fs::write(&path, b"pending").unwrap();
            assert!(validate_paths(root.path(), &paths, ".download").is_err());
            std::fs::remove_file(path).unwrap();
        }
        let alias = root.path().join("alias.jpg");
        std::fs::hard_link(&media, &alias).unwrap();
        assert!(validate_paths(root.path(), &paths, ".download").is_err());
        std::fs::remove_file(alias).unwrap();
        validate_paths(root.path(), &paths, ".download").unwrap();
    }
    #[cfg(target_os = "linux")]
    #[test]
    fn sparse_target_file_gate_rechecks_replacement_journal() {
        use sha2::{Digest, Sha256};
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("photo.jpg");
        std::fs::write(&path, b"retained bytes").unwrap();
        let paths = vec![(path, true)];
        validate_paths(root.path(), &paths, ".download").unwrap();
        let digest = data_encoding::HEXLOWER.encode(&Sha256::digest(b"photo.jpg"));
        let journal = root.path().join(format!(".kei-replace-{digest}"));
        std::fs::create_dir(&journal).unwrap();
        assert!(validate_paths(root.path(), &paths, ".download").is_err());
        std::fs::remove_dir(journal).unwrap();
        validate_paths(root.path(), &paths, ".download").unwrap();
    }
}
