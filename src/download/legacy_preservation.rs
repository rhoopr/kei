//! Preserve historical evidence without assigning it to a current provider child.
//! The sync owner supplies completion/bridge permission; this owner proves files
//! and current family receipts. State rechecks dependencies in the cursor transaction.

use std::path::{Path, PathBuf};

use anyhow::Context;
use base64::Engine as _;
use rustc_hash::FxHashSet;
use serde_json::json;
use tokio_util::sync::CancellationToken;

use super::{DownloadConfig, DownloadStore};
use crate::icloud::photos::CompleteLegacyInventory;
use crate::icloud::photos::PhotoAlbum;
use crate::state::{LegacyActivationProof, LegacyFileEvidence, LegacyPreservation};

/// Recovery scans a shared root, so its deny set must include every library.
pub(crate) async fn protected_replacement_paths(
    db: Option<&dyn DownloadStore>,
) -> anyhow::Result<Vec<PathBuf>> {
    let Some(db) = db else { return Ok(Vec::new()) };
    let libraries: FxHashSet<_> = db
        .get_protected_legacy_ids()
        .await?
        .into_iter()
        .map(|(library, _)| library)
        .collect();
    let mut paths = Vec::new();
    for library in libraries {
        for record in db.legacy_preservations(&library).await? {
            paths.extend(record.files.into_iter().map(|file| file.path));
        }
    }
    Ok(paths)
}

#[derive(Default)]
pub(crate) struct LegacyCycle {
    preserved: Vec<LegacyPreservation>,
    prepared_proofs: Vec<LegacyActivationProof>,
}

impl LegacyCycle {
    pub(crate) fn requires_inventory(&self) -> bool {
        self.preserved
            .iter()
            .any(|record| record.active_generation.is_none())
    }

    /// Preparation is durable protection only. It never authorizes a checkpoint.
    pub(crate) async fn begin(
        db: &dyn DownloadStore,
        config: &DownloadConfig,
        album: &PhotoAlbum,
        config_hash: &str,
        cancel: &CancellationToken,
    ) -> anyhow::Result<Self> {
        let mut preserved = db.legacy_preservations(&config.library).await?;
        for record in &mut preserved {
            let dependencies = db
                .legacy_dependency_evidence(&record.library, &record.asset_id)
                .await?;
            let files_valid = verify_files(&record.files, &config.temp_suffix, cancel).await;
            let current_files_valid = if let Some(evidence) = &record.provider_evidence {
                match provider_files(evidence) {
                    Ok(files) => verify_files(&files, &config.temp_suffix, cancel).await,
                    Err(error) => Err(error),
                }
            } else {
                Ok(())
            };
            if record.active_generation.is_some()
                && (record.config_hash.as_deref() != Some(config_hash)
                    || record.dependency_evidence.as_deref() != Some(dependencies.as_str())
                    || files_valid.is_err()
                    || current_files_valid.is_err())
            {
                db.reactivate_legacy_preservation(
                    &record.library,
                    &record.asset_id,
                    record.active_generation,
                )
                .await?;
                record.active_generation = None;
            }
            // Stop before ordinary replacement recovery can touch a protected path.
            files_valid?;
        }
        let candidates = db.legacy_preparation_snapshots(&config.library, 64).await?;
        if !candidates.is_empty() {
            let masters = candidates
                .iter()
                .map(|candidate| candidate.asset_id.clone())
                .collect();
            // Provider failures cannot relax existing capture/checkpoint guards.
            match album.complete_legacy_inventory(&masters, cancel).await {
                Ok(inventory) => {
                    for candidate in candidates {
                        if inventory
                            .children
                            .iter()
                            .filter(|child| child.id() == candidate.asset_id)
                            .count()
                            < 2
                        {
                            continue;
                        }
                        let mut files = Vec::new();
                        let mut valid = true;
                        for path in &candidate.paths {
                            for (path, required) in [(path.clone(), true), (sidecar(path), false)] {
                                match fingerprint(
                                    &config.directory,
                                    &path,
                                    required,
                                    &config.temp_suffix,
                                    cancel,
                                )
                                .await
                                {
                                    Ok(file) => files.push(file),
                                    Err(_) => {
                                        valid = false;
                                        break;
                                    }
                                }
                            }
                            if !valid {
                                break;
                            }
                        }
                        if valid {
                            db.prepare_legacy_preservation(&candidate, &files).await?;
                        }
                    }
                    preserved = db.legacy_preservations(&config.library).await?;
                }
                Err(_) => tracing::debug!(
                    "Legacy preservation inventory unavailable; retaining attribution checkpoint hold"
                ),
            }
        }
        Ok(Self {
            preserved,
            prepared_proofs: Vec::new(),
        })
    }

    /// Run after normal inventory and before replay from the retained cursor.
    /// The subsequent bridge may change dependencies, in which case activation
    /// is rejected and a later complete inventory must prove the new family.
    pub(crate) async fn certify(
        &mut self,
        db: &dyn DownloadStore,
        config: &DownloadConfig,
        album: &PhotoAlbum,
        config_hash: &str,
        prior_cursor: &str,
        cancel: &CancellationToken,
    ) -> anyhow::Result<()> {
        if !self.requires_inventory() {
            return Ok(());
        }
        let masters = self
            .preserved
            .iter()
            .filter(|r| r.active_generation.is_none())
            .map(|r| r.asset_id.clone())
            .collect();
        let inventory = album.complete_legacy_inventory(&masters, cancel).await?;
        anyhow::ensure!(
            inventory.library == config.library.as_ref(),
            "Legacy preservation scope changed"
        );
        for record in self
            .preserved
            .iter()
            .filter(|record| record.active_generation.is_none())
        {
            verify_files(&record.files, &config.temp_suffix, cancel).await?;
            let before = db
                .legacy_dependency_evidence(&record.library, &record.asset_id)
                .await?;
            let provider_evidence = qualify_family(db, config, record, &inventory, cancel).await?;
            anyhow::ensure!(
                before
                    == db
                        .legacy_dependency_evidence(&record.library, &record.asset_id)
                        .await?,
                "Legacy current receipts changed during qualification"
            );
            let mut expected_metadata = Vec::new();
            for key in [
                "enum_config_hash",
                "pending_enum_config_hash",
                "config_hash",
                "pending_download_config_hash",
            ] {
                expected_metadata.push((key.to_owned(), db.get_metadata(key).await?));
            }
            self.prepared_proofs.push(LegacyActivationProof {
                library: record.library.clone(),
                asset_id: record.asset_id.clone(),
                expected_original: record.original_evidence.clone(),
                expected_dependencies: before,
                provider_evidence,
                expected_metadata,
                expected_active_generation: record.active_generation,
                config_hash: config_hash.to_owned(),
                prior_cursor: prior_cursor.to_owned(),
                next_cursor: String::new(),
            });
        }
        Ok(())
    }

    /// Every prepared original needs a proof. Existing activated records are
    /// revalidated too, so new evidence cannot borrow a previous generation.
    pub(crate) async fn finish(
        &self,
        db: &dyn DownloadStore,
        config: &DownloadConfig,
        token: &str,
        cancel: &CancellationToken,
    ) -> anyhow::Result<Vec<LegacyActivationProof>> {
        let mut proofs = Vec::new();
        for record in &self.preserved {
            verify_files(&record.files, &config.temp_suffix, cancel).await?;
            let current = db
                .legacy_dependency_evidence(&record.library, &record.asset_id)
                .await?;
            if record.active_generation.is_some() {
                let files = provider_files(
                    record
                        .provider_evidence
                        .as_deref()
                        .context("Missing legacy current file evidence")?,
                )?;
                verify_files(&files, &config.temp_suffix, cancel).await?;
                anyhow::ensure!(
                    record.dependency_evidence.as_deref() == Some(current.as_str()),
                    "Legacy current coverage changed"
                );
            } else {
                let mut proof = self
                    .prepared_proofs
                    .iter()
                    .find(|proof| proof.asset_id == record.asset_id)
                    .context("Legacy current coverage is incomplete")?
                    .clone();
                anyhow::ensure!(
                    proof.expected_dependencies == current,
                    "Legacy current receipts changed after inventory"
                );
                verify_files(
                    &provider_files(&proof.provider_evidence)?,
                    &config.temp_suffix,
                    cancel,
                )
                .await?;
                proof.next_cursor = token.to_owned();
                proofs.push(proof);
            }
        }
        Ok(proofs)
    }
}

fn sidecar(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".xmp");
    PathBuf::from(name)
}

async fn fingerprint(
    root: &Path,
    path: &Path,
    required: bool,
    temp_suffix: &str,
    cancel: &CancellationToken,
) -> anyhow::Result<LegacyFileEvidence> {
    anyhow::ensure!(!cancel.is_cancelled(), "Legacy preservation cancelled");
    anyhow::ensure!(
        !super::file::has_replacement_journal(root, path).await?,
        "Legacy replacement recovery is pending"
    );
    let root_owned = root.to_path_buf();
    let path_owned = path.to_path_buf();
    let suffix = temp_suffix.to_owned();
    let present = tokio::task::spawn_blocking(move || -> anyhow::Result<bool> {
        let confined = crate::fs_util::ConfinedPath::open(
            &root_owned,
            &path_owned,
            crate::fs_util::ConfinedParents::Existing,
        )?;
        for suffix in [suffix.as_str(), ".part"] {
            let mut temporary = path_owned.as_os_str().to_owned();
            temporary.push(suffix);
            anyhow::ensure!(
                !crate::fs_util::ConfinedPath::open(
                    &root_owned,
                    Path::new(&temporary),
                    crate::fs_util::ConfinedParents::Existing
                )?
                .entry_exists()?,
                "Legacy temporary write is pending"
            );
        }
        let file = confined.open_optional_regular()?;
        if let Some(file) = &file {
            anyhow::ensure!(
                crate::fs_util::file_link_count(file)? == 1,
                "Legacy preservation requires independent file objects"
            );
        }
        Ok(file.is_some())
    })
    .await??;
    anyhow::ensure!(present || !required, "Legacy original is missing");
    let value = if present {
        Some(super::file::fingerprint_downloaded_path(root, path).await?)
    } else {
        None
    };
    anyhow::ensure!(!cancel.is_cancelled(), "Legacy preservation cancelled");
    Ok(LegacyFileEvidence {
        root: root.to_path_buf(),
        path: path.to_path_buf(),
        sha256: value.map(|value| data_encoding::HEXLOWER.encode(&value.sha256)),
        size: value.map(|value| value.size),
    })
}

async fn verify_files(
    files: &[LegacyFileEvidence],
    temp_suffix: &str,
    cancel: &CancellationToken,
) -> anyhow::Result<()> {
    for expected in files {
        let actual = fingerprint(
            &expected.root,
            &expected.path,
            expected.sha256.is_some(),
            temp_suffix,
            cancel,
        )
        .await?;
        anyhow::ensure!(
            &actual == expected,
            "Preserved legacy bytes or sidecar presence changed"
        );
    }
    Ok(())
}

async fn qualify_family(
    db: &dyn DownloadStore,
    config: &DownloadConfig,
    record: &LegacyPreservation,
    inventory: &CompleteLegacyInventory,
    cancel: &CancellationToken,
) -> anyhow::Result<String> {
    let children: Vec<_> = inventory
        .children
        .iter()
        .filter(|child| child.id() == record.asset_id)
        .collect();
    anyhow::ensure!(children.len() >= 2, "Legacy family is no longer ambiguous");
    let mut paths: FxHashSet<_> = record
        .files
        .iter()
        .map(|file| crate::fs_util::confined_path_key(&file.path))
        .collect::<Result<_, _>>()?;
    let catalog = db.get_reconciliation_catalog_paths().await?;
    let mut evidence = Vec::new();
    for child in children {
        anyhow::ensure!(
            db.get_master_record_name_for_asset(&record.library, child.asset_record_name())
                .await?
                .as_deref()
                == Some(record.asset_id.as_str()),
            "Legacy current child mapping is not durable"
        );
        anyhow::ensure!(
            child.asset_record_name() != record.asset_id,
            "Legacy identity conflicts with current child"
        );
        anyhow::ensure!(
            super::filter::is_asset_filtered(child, config).is_none(),
            "Legacy family includes policy-excluded work"
        );
        let versions = super::filter::extract_skip_candidates(child, config);
        anyhow::ensure!(
            !versions.is_empty(),
            "Legacy family has no eligible renditions"
        );
        let receipts = db
            .legacy_child_receipts(&record.library, child.asset_record_name())
            .await?;
        for (version, checksum) in versions {
            let receipt = receipts
                .iter()
                .find(|receipt| receipt.version_size == version)
                .context("Legacy child rendition is not durably downloaded")?;
            anyhow::ensure!(
                receipt.checksum.as_ref() == checksum
                    && Some(receipt.created_at) == child.asset_date_evidence()
                    && receipt.added_at == child.added_date_evidence(),
                "Legacy child receipt does not match current provider evidence"
            );
            let metadata = super::filter::metadata_for_selected_version(child, config, version);
            anyhow::ensure!(
                receipt.metadata.compute_hash() == metadata.compute_hash(),
                "Legacy child metadata is not current"
            );
            let path = receipt
                .local_path
                .as_deref()
                .context("Legacy child receipt has no output")?;
            anyhow::ensure!(
                paths.insert(crate::fs_util::confined_path_key(path)?),
                "Legacy and current children share an output path"
            );
            let key = crate::fs_util::confined_path_key(path)?;
            for owner in &catalog {
                anyhow::ensure!(
                    crate::fs_util::confined_path_key(&owner.path)? != key
                        || (owner.library.as_ref() == record.library
                            && owner.asset_id.as_ref() == child.asset_record_name()),
                    "Legacy current output has conflicting catalog ownership"
                );
            }
            let file =
                fingerprint(&config.directory, path, true, &config.temp_suffix, cancel).await?;
            anyhow::ensure!(
                file.sha256 == receipt.local_checksum
                    && verified_current_content(
                        checksum,
                        file.sha256.as_deref(),
                        receipt.download_checksum.as_deref()
                    ),
                "Legacy child output has no independent verified receipt"
            );
            #[cfg(feature = "xmp")]
            let sidecar_required = config.metadata.xmp_sidecar;
            #[cfg(not(feature = "xmp"))]
            let sidecar_required = false;
            let sidecar_file = fingerprint(
                &config.directory,
                &sidecar(path),
                sidecar_required,
                &config.temp_suffix,
                cancel,
            )
            .await?;
            anyhow::ensure!(
                paths.insert(crate::fs_util::confined_path_key(&sidecar_file.path)?),
                "Legacy current sidecar aliases another output"
            );
            evidence.push(json!({"child":child.asset_record_name(),"version":version.as_str(),"provider_checksum":checksum,"created_at":child.asset_date_evidence(),"added_at":child.added_date_evidence(),"metadata_hash":metadata.compute_hash(),"files":[file,sidecar_file]}));
        }
    }
    Ok(json!({"version":1,"library":inventory.library,"inventory_cursor":inventory.cursor,"master":record.asset_id,"children":evidence}).to_string())
}

fn provider_files(evidence: &str) -> anyhow::Result<Vec<LegacyFileEvidence>> {
    let value: serde_json::Value = serde_json::from_str(evidence)?;
    anyhow::ensure!(
        value.get("version").and_then(serde_json::Value::as_u64) == Some(1),
        "Unsupported legacy current proof version"
    );
    let mut files = Vec::new();
    for child in value
        .get("children")
        .and_then(serde_json::Value::as_array)
        .context("Missing legacy current children")?
    {
        for file in child
            .get("files")
            .and_then(serde_json::Value::as_array)
            .context("Missing legacy current files")?
        {
            files.push(serde_json::from_value(file.clone())?);
        }
    }
    anyhow::ensure!(!files.is_empty(), "Missing legacy current file evidence");
    Ok(files)
}

// This proves bytes for an independently identified current child, never the
// historical original's owner. A post-publication state failure may leave no
// pre-metadata receipt; exact provider SHA-256 equality still verifies the file.
fn verified_current_content(
    provider: &str,
    current: Option<&str>,
    downloaded: Option<&str>,
) -> bool {
    let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(provider) else {
        return false;
    };
    if bytes.len() != 32 {
        return false;
    }
    let hash = data_encoding::HEXLOWER.encode(&bytes);
    current == Some(hash.as_str()) || downloaded == Some(hash.as_str())
}

#[cfg(test)]
mod tests {
    use super::{fingerprint, provider_files, sidecar, verified_current_content, verify_files};
    use base64::Engine as _;
    use tokio_util::sync::CancellationToken;

    #[tokio::test]
    async fn preservation_file_evidence_detects_changes_and_never_writes() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let path = root.join("original.jpg");
        std::fs::write(&path, b"synthetic original").unwrap();
        let cancel = CancellationToken::new();
        let media = fingerprint(root, &path, true, ".kei-tmp", &cancel)
            .await
            .unwrap();
        let absent = fingerprint(root, &sidecar(&path), false, ".kei-tmp", &cancel)
            .await
            .unwrap();
        let files = vec![media, absent];
        verify_files(&files, ".kei-tmp", &cancel).await.unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"synthetic original");
        assert!(!sidecar(&path).exists());
        std::fs::write(sidecar(&path), b"new sidecar").unwrap();
        assert!(verify_files(&files, ".kei-tmp", &cancel).await.is_err());
        assert_eq!(std::fs::read(sidecar(&path)).unwrap(), b"new sidecar");
        std::fs::write(&path, b"changed original").unwrap();
        assert!(verify_files(&files, ".kei-tmp", &cancel).await.is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"changed original");
        cancel.cancel();
        assert!(
            fingerprint(root, &path, true, ".kei-tmp", &cancel)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn preservation_file_evidence_rejects_links_and_pending_writes() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let path = root.join("original.jpg");
        std::fs::write(&path, b"original").unwrap();
        let cancel = CancellationToken::new();
        let linked = root.join("alias.jpg");
        std::fs::hard_link(&path, &linked).unwrap();
        assert!(
            fingerprint(root, &path, true, ".kei-tmp", &cancel)
                .await
                .is_err()
        );
        std::fs::remove_file(&linked).unwrap();
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&path, &linked).unwrap();
            assert!(
                fingerprint(root, &linked, true, ".kei-tmp", &cancel)
                    .await
                    .is_err()
            );
        }
        let temporary = root.join("original.jpg.kei-tmp");
        std::fs::write(&temporary, b"pending").unwrap();
        assert!(
            fingerprint(root, &path, true, ".kei-tmp", &cancel)
                .await
                .is_err()
        );
        assert_eq!(std::fs::read(&temporary).unwrap(), b"pending");
        assert_eq!(std::fs::read(&path).unwrap(), b"original");
    }

    #[test]
    fn preservation_current_content_requires_exact_source_bytes_or_verified_prewrite_receipt() {
        use sha2::{Digest, Sha256};
        let raw = Sha256::digest(b"synthetic source");
        let provider = base64::engine::general_purpose::STANDARD.encode(raw);
        let hash = data_encoding::HEXLOWER.encode(&raw);
        let changed = data_encoding::HEXLOWER.encode(&Sha256::digest(b"changed bytes"));
        assert!(verified_current_content(&provider, Some(&hash), None));
        assert!(verified_current_content(
            &provider,
            Some(&changed),
            Some(&hash)
        ));
        assert!(!verified_current_content(&provider, Some(&changed), None));
        assert!(!verified_current_content(
            &provider,
            Some(&changed),
            Some(&changed)
        ));
        assert!(!verified_current_content(
            "unsupported",
            Some(&hash),
            Some(&hash)
        ));
    }

    #[test]
    fn preservation_file_evidence_roundtrip_retains_absent_sidecars_and_rejects_unknown_fields() {
        let files = vec![crate::state::LegacyFileEvidence {
            root: "/synthetic".into(),
            path: "/synthetic/image.jpg.xmp".into(),
            sha256: None,
            size: None,
        }];
        let encoded = serde_json::to_string(&files).unwrap();
        let decoded: Vec<crate::state::LegacyFileEvidence> =
            serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded, files);
        let mut value = serde_json::to_value(&files[0]).unwrap();
        value["unknown"] = serde_json::json!(true);
        assert!(serde_json::from_value::<crate::state::LegacyFileEvidence>(value).is_err());
    }

    #[test]
    fn preservation_current_file_evidence_rejects_unsupported_shapes() {
        for evidence in [
            "{}",
            "null",
            "[]",
            "{\"version\":2,\"children\":[]}",
            "{\"version\":1,\"children\":[]}",
            "{\"version\":1,\"children\":[{}]}",
        ] {
            assert!(provider_files(evidence).is_err(), "{evidence}");
        }
    }
}
