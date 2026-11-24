//! Selected sparse targets use exact provider identity and the normal pipeline.
use crate::download::{DownloadConfig, DownloadControls};
use crate::icloud::photos::asset::{SparseShareEvidence, SparseShareLink};
use crate::icloud::photos::{PhotoAlbum, PhotoAsset, PhotoLibrary, RecordResolution};
use crate::state::{
    db::{SparseTargetHold, SparseTargetProof, SparseTargetState},
    error::StateError,
};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// Scheduling hints only. Recovery still needs matching fresh source evidence.
pub(crate) async fn due_primary_links(
    db: &dyn super::DownloadStore,
) -> Result<Vec<SparseShareLink>, StateError> {
    let now = chrono::Utc::now();
    Ok(db
        .sparse_identities(crate::icloud::photos::PRIMARY_ZONE_NAME)
        .await?
        .into_iter()
        .filter(|row| row.next_retry.is_none_or(|retry| retry <= now))
        .filter_map(
            |row| match SparseShareEvidence::from_durable_key(&row.observed) {
                Some(SparseShareEvidence::Linked(link)) => Some(link),
                _ => None,
            },
        )
        .collect())
}

#[derive(Clone)]
pub(crate) struct SelectedSparseTarget {
    pub(crate) hidden_only: bool,
    pub(crate) scan: Option<String>,
    pub(crate) library: PhotoLibrary,
    pub(crate) album: PhotoAlbum,
    pub(crate) config: Arc<DownloadConfig>,
    pub(crate) client: reqwest::Client,
    pub(crate) controls: DownloadControls,
}
impl SelectedSparseTarget {
    fn requires_metadata_receipts(&self) -> bool {
        !self.hidden_only || super::metadata_rewrite::writers_enabled(&self.config.metadata)
    }
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
fn unsupported<T>(reason: &'static str) -> Option<T> {
    outcome("unsupported", reason);
    None
}
enum ReceiptFailure {
    NotObserved,
    PairUnresolved,
    FileUnverified,
}

impl ReceiptFailure {
    fn diagnostic(self) -> &'static str {
        match self {
            Self::NotObserved => "target_membership_unobserved",
            Self::PairUnresolved => "target_pair_unresolved",
            Self::FileUnverified => "target_file_unverified",
        }
    }
}
async fn receipts(
    target: &SelectedSparseTarget,
    asset: &PhotoAsset,
    cancel: &CancellationToken,
) -> Result<Option<SparseTargetState>, ReceiptFailure> {
    let config = &target.config;
    let Some(db) = &config.state_db else {
        return Ok(None);
    };
    let before = match db
        .sparse_target_state(
            &config.library,
            asset.asset_record_name(),
            asset.id(),
            target.scan.as_deref(),
            target.requires_metadata_receipts(),
        )
        .await
        .map_err(|_error| ReceiptFailure::FileUnverified)?
    {
        Ok(state) => state,
        Err(SparseTargetHold::ReceiptUnqualified) => return Ok(None),
        Err(SparseTargetHold::MembershipUnobserved) => return Err(ReceiptFailure::NotObserved),
        Err(SparseTargetHold::PairUnresolved) => return Err(ReceiptFailure::PairUnresolved),
    };
    verify_receipts(target, asset, cancel, before, db.as_ref())
        .await
        .map_err(|_error| ReceiptFailure::FileUnverified)
}

async fn verify_receipts(
    target: &SelectedSparseTarget,
    asset: &PhotoAsset,
    cancel: &CancellationToken,
    before: SparseTargetState,
    db: &dyn super::DownloadStore,
) -> anyhow::Result<Option<SparseTargetState>> {
    let config = &target.config;
    let versions = super::filter::extract_skip_candidates(asset, config.as_ref());
    if versions.is_empty() {
        return Ok(None);
    }
    let rows = if target.scan.is_none() {
        db.qualified_child_receipts(
            &config.library,
            asset.asset_record_name(),
            target.requires_metadata_receipts(),
        )
        .await?
    } else {
        Vec::new()
    };
    let catalog = db.get_reconciliation_catalog_paths().await?;
    let mut retained = Vec::new();
    let mut checked_paths = Vec::new();
    if before.copies.iter().any(|copy| {
        !versions
            .iter()
            .any(|(v, c)| v.as_str() == copy.version && *c == copy.provider_checksum)
    }) {
        return Ok(None);
    }
    let mut copies: Vec<(&std::path::PathBuf, &str)> = Vec::new();
    for (version, checksum) in versions {
        let metadata_hash =
            super::filter::metadata_for_selected_version(asset, config.as_ref(), version)
                .compute_hash();
        if target.scan.is_some() {
            let mut seen = false;
            for copy in before
                .copies
                .iter()
                .filter(|copy| copy.version == version.as_str())
            {
                if copy.metadata_hash != metadata_hash
                    || Some(copy.created) != asset.asset_date_evidence()
                    || copy.added != asset.added_date_evidence()
                {
                    return Ok(None);
                }
                let local = copy.local_checksum.as_str();
                seen = true;
                if let Some((_, checked)) = copies.iter().find(|(p, _)| **p == copy.path) {
                    if *checked != local {
                        return Ok(None);
                    }
                } else {
                    copies.push((&copy.path, local));
                }
            }
            if !seen {
                return Ok(None);
            }
        } else {
            let Some(row) = rows.iter().find(|r| r.version_size == version) else {
                return Ok(None);
            };
            if row.checksum.as_ref() != checksum
                || Some(row.created_at) != asset.asset_date_evidence()
                || row.added_at != asset.added_date_evidence()
                || row.metadata.compute_hash() != metadata_hash
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
            let (Some(path), Some(local)) = (&row.local_path, row.local_checksum.as_deref()) else {
                return Ok(None);
            };
            if target.hidden_only {
                let paths = super::filter::derive_expected_paths(asset, config.as_ref());
                if !paths.iter().any(|p| {
                    p.version_size == version
                        && super::filter::stored_path_matches_current_collision_family(
                            asset.asset_record_name(),
                            p,
                            &paths,
                            config,
                            path,
                        )
                }) {
                    return Ok(None);
                }
            }
            copies.push((path, local));
        }
    }
    for (path, local_checksum) in copies {
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
        if file.sha256.as_deref() != Some(local_checksum) {
            return Ok(None);
        }
        let Some(held) = super::file::retain_pending_file(&config.directory, path).await? else {
            return Ok(None);
        };
        if data_encoding::HEXLOWER.encode(&held.fingerprint.sha256) != local_checksum {
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
            .sparse_target_state(
                &config.library,
                asset.asset_record_name(),
                asset.id(),
                target.scan.as_deref(),
                target.requires_metadata_receipts(),
            )
            .await?
            .as_ref()
            != Ok(&before)
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
) -> anyhow::Result<Option<SparseTargetProof>> {
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
        return Ok(unsupported("target_selection_evidence_missing"));
    };
    let Some(db) = &target.config.state_db else {
        return Ok(unsupported("target_receipt_unqualified"));
    };
    if cancel.is_cancelled() || !target.controls.run_mode.downloads_files() {
        return Ok(None);
    }
    let finished = tokio::select! {
        biased;
        () = cancel.cancelled() => return Ok(None),
        result = target.library.check_indexing_once() => result?,
    };
    if !finished
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
    let child = asset.asset_record_name_arc();
    let asset = asset
        .with_state_record_name(child)
        .with_source_zone(Arc::clone(&target.config.library));
    let outside_hidden = target.hidden_only && asset.hidden_membership() == Some(false);
    if (asset.metadata().is_hidden && target.scan.is_none())
        || asset.metadata().is_deleted
        || cancel.is_cancelled()
        || asset.asset_record_name() == asset.id()
        || (!outside_hidden
            && (super::filter::is_asset_filtered(&asset, target.config.as_ref()).is_some()
                || super::filter::extract_skip_candidates(&asset, target.config.as_ref())
                    .is_empty()))
    {
        return Ok(unsupported("target_policy_unsupported"));
    }
    match receipts(target, &asset, cancel).await {
        Ok(Some(proof)) => {
            if outside_hidden {
                return Ok(unsupported("target_membership_unobserved"));
            }
            outcome("partial", "target_receipt_ready");
            return Ok(Some(SparseTargetProof::Selected(proof)));
        }
        Err(ReceiptFailure::NotObserved) if target.hidden_only => {
            let Some(scan) = &target.scan else {
                return Ok(unsupported("target_selection_evidence_missing"));
            };
            let direct_proof = |selected| SparseTargetProof::DirectHidden {
                child: asset.asset_record_name().into(),
                master: asset.id().into(),
                library: target.config.library.to_string(),
                scan: scan.clone(),
                selected,
            };
            match asset.hidden_membership() {
                Some(false) => {
                    outcome("partial", "target_explicitly_not_hidden");
                    return Ok(Some(direct_proof(None)));
                }
                None => return Ok(unsupported("target_membership_unobserved")),
                Some(true) => {}
            }
            let direct = SelectedSparseTarget {
                scan: None,
                ..target.clone()
            };
            let selected = match receipts(&direct, &asset, cancel).await {
                Ok(Some(proof)) => Some(proof),
                Ok(None) => materialize(&direct, &asset, cancel, stats).await?,
                Err(reason) => return Ok(unsupported(reason.diagnostic())),
            };
            return Ok(selected.map(|proof| direct_proof(Some(proof))));
        }
        Err(reason) => return Ok(unsupported(reason.diagnostic())),
        Ok(None) if target.scan.is_some() => {
            return Ok(unsupported("target_selected_copies_unverified"));
        }
        Ok(None) => {}
    }
    Ok(materialize(target, &asset, cancel, stats)
        .await?
        .map(SparseTargetProof::Selected))
}

async fn materialize(
    target: &SelectedSparseTarget,
    asset: &PhotoAsset,
    cancel: &CancellationToken,
    stats: &mut crate::download::SyncStats,
) -> anyhow::Result<Option<SparseTargetState>> {
    let result = super::pipeline::stream_and_download_from_stream(
        &target.client,
        futures_util::stream::iter(vec![Ok(asset.clone())]),
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
    stats.storage_failed |= result.storage_failed;
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
    match receipts(target, asset, cancel).await {
        Ok(Some(proof)) => {
            outcome("partial", "target_receipt_ready");
            Ok(Some(proof))
        }
        Ok(None) => Ok(unsupported("target_receipt_unqualified")),
        Err(reason) => Ok(unsupported(reason.diagnostic())),
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
    use super::{recover, validate_paths};
    use crate::download::{DownloadConfig, DownloadControls, SyncStats};
    use crate::icloud::error::ICloudError;
    use crate::icloud::photos::asset::SparseShareEvidence;
    use crate::icloud::photos::{PhotoLibrary, PhotosSession};
    use crate::retry::RetryConfig;
    use serde_json::{Value, json};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;
    use tokio_util::sync::CancellationToken;

    const SHARED_ZONE: &str = "SharedSync-sparse-regression";
    const SHARED_OWNER: &str = "owner-sparse-regression";

    /// The constructor sees FINISHED. Later probes see UNKNOWN.
    #[derive(Clone)]
    struct ReadinessSession(Arc<AtomicUsize>);

    #[async_trait::async_trait]
    impl PhotosSession for ReadinessSession {
        async fn post(
            &self,
            _url: &str,
            body: String,
            _headers: &[(&str, &str)],
        ) -> anyhow::Result<Value> {
            // No sparse lookup or other provider work may get past readiness.
            let request: Value = serde_json::from_str(&body)?;
            assert_eq!(request["query"]["recordType"], "CheckIndexingState");
            assert_eq!(request["zoneID"]["zoneName"], SHARED_ZONE);
            assert_eq!(request["zoneID"]["ownerRecordName"], SHARED_OWNER);
            let probe = self.0.fetch_add(1, Ordering::SeqCst);
            let state = if probe == 0 { "FINISHED" } else { "UNKNOWN" };
            Ok(json!({"records": [{"fields": {"state": {"value": state}}}]}))
        }

        fn clone_box(&self) -> Box<dyn PhotosSession> {
            Box::new(self.clone())
        }
    }

    fn linked_observation(child: &str) -> SparseShareEvidence {
        let evidence = SparseShareEvidence::from_fields(&json!({
            "isSparsePrivateRecord": {"type": "INT64", "value": 1},
            "linkedShareRecordName": {"type": "STRING", "value": child},
            "linkedShareZoneName": {"type": "STRING", "value": SHARED_ZONE},
            "linkedShareZoneOwner": {"type": "REFERENCE", "value": {"recordName": SHARED_OWNER}},
        }))
        .expect("linked sparse share evidence");
        assert!(matches!(evidence, SparseShareEvidence::Linked(_)));
        evidence
    }

    #[tokio::test(start_paused = true)]
    async fn sparse_target_recovery_probes_readiness_once_per_linked_observation() {
        let probes = Arc::new(AtomicUsize::new(0));
        let library = PhotoLibrary::new(
            "https://example.invalid".into(),
            Arc::default(),
            Box::new(ReadinessSession(probes.clone())),
            Arc::new(json!({"zoneName": SHARED_ZONE, "ownerRecordName": SHARED_OWNER})),
            "shared".into(),
            RetryConfig::default(),
        )
        .await
        .unwrap();
        assert_eq!(probes.load(Ordering::SeqCst), 1);
        assert!(library.last_indexing_finished());
        let album = library.all();

        let mut config = DownloadConfig::test_default();
        config.library = Arc::from(SHARED_ZONE);
        config.state_db = Some(Arc::new(
            crate::state::SqliteStateDb::open_in_memory().unwrap(),
        ));
        let target_config = Arc::new(config.clone());
        config.sparse_targets = Arc::from([super::SelectedSparseTarget {
            hidden_only: false,
            scan: None,
            library,
            album,
            config: target_config,
            client: reqwest::Client::new(),
            controls: DownloadControls::download_hidden(),
        }]);

        let cancel = CancellationToken::new();
        let mut stats = SyncStats::default();
        for child in ["sparse-child-one", "sparse-child-two"] {
            let before = probes.load(Ordering::SeqCst);
            let started = tokio::time::Instant::now();
            let error =
                match recover(&linked_observation(child), &config, &cancel, &mut stats).await {
                    Ok(_) => panic!("UNKNOWN readiness must not yield a target proof"),
                    Err(error) => error,
                };
            assert!(
                matches!(
                    error.downcast_ref::<ICloudError>(),
                    Some(ICloudError::IndexingUnavailable(_))
                ),
                "unknown readiness must surface as a retryable indexing hold: {error}"
            );
            assert_eq!(
                started.elapsed(),
                Duration::ZERO,
                "one recovery observation must not wait through the 5s/15s indexing backoff"
            );
            assert_eq!(
                probes.load(Ordering::SeqCst),
                before + 1,
                "exactly one readiness request per recovery observation"
            );
        }

        cancel.cancel();
        assert!(
            recover(
                &linked_observation("sparse-child-three"),
                &config,
                &cancel,
                &mut stats,
            )
            .await
            .expect("cancellation is not a recovery error")
            .is_none(),
            "a cancelled recovery must not produce a target proof"
        );
        assert_eq!(
            probes.load(Ordering::SeqCst),
            3,
            "a cancelled recovery must short-circuit before the readiness probe"
        );
    }

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
