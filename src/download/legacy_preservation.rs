//! Preserve historical evidence without assigning it to a current provider child.
//! The sync owner supplies completion/bridge permission; this owner proves files
//! and current family receipts. State rechecks dependencies in the cursor transaction.

use std::path::{Path, PathBuf};

use anyhow::Context;
use base64::Engine as _;
use rustc_hash::FxHashSet;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
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

const INVENTORY_RETRY_SECONDS: i64 = 60 * 60;
const INVENTORY_RETRY_PREFIX: &str = "legacy_preservation_inventory_retry:";

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct InventoryRetry {
    version: u8,
    signature: String,
    next_attempt_at: i64,
}

impl InventoryRetry {
    fn deferred(&self, signature: &str, now: i64) -> bool {
        self.version == 1
            && self.signature == signature
            && self.next_attempt_at > now
            && self.next_attempt_at <= now.saturating_add(INVENTORY_RETRY_SECONDS)
    }
}

fn inventory_retry_signature(
    config_hash: &str,
    candidates: &[crate::state::db::LegacyPreparationSnapshot],
) -> anyhow::Result<String> {
    let mut evidence = Vec::with_capacity(candidates.len());
    for candidate in candidates {
        let mut original: serde_json::Value = serde_json::from_str(&candidate.evidence)?;
        let tables = original
            .get_mut("evidence")
            .and_then(serde_json::Value::as_object_mut)
            .context("Missing legacy preparation evidence")?;
        // Retry timestamps and cycle counters do not change the historical
        // family. All original receipt, owner, path and relationship facts stay
        // in the signature; preparation still compares the entire snapshot.
        tables.remove("metadata_capture_retries");
        tables.remove("capture_state_at_preparation");
        if let Some(history) = tables.get_mut("family_history")
            && let Some(index) = history
                .get("columns")
                .and_then(serde_json::Value::as_array)
                .and_then(|columns| {
                    columns
                        .iter()
                        .position(|column| column.as_str() == Some("updated_at"))
                })
        {
            // Replaying an unchanged child refreshes this bookkeeping timestamp.
            // Preserve relation identities in the signature, and retain the full
            // unmodified table in the eventual preparation snapshot.
            let rows = history
                .get_mut("rows")
                .and_then(serde_json::Value::as_array_mut)
                .context("Missing legacy family history rows")?;
            for row in rows {
                *row.as_array_mut()
                    .and_then(|values| values.get_mut(index))
                    .context("Invalid legacy family history row")? = serde_json::Value::Null;
            }
        }
        evidence.push(original);
    }
    let encoded = serde_json::to_vec(&(
        1,
        config_hash,
        crate::state::METADATA_CAPTURE_REVISION,
        evidence,
    ))?;
    Ok(data_encoding::HEXLOWER.encode(&Sha256::digest(encoded)))
}

#[derive(Debug, thiserror::Error)]
enum LegacyFileError {
    #[error("Legacy preservation requires independent file objects")]
    SharedFileLinks,
}

fn classify_legacy_file_error(error: &anyhow::Error) -> Option<&'static str> {
    match error.downcast_ref::<LegacyFileError>() {
        Some(LegacyFileError::SharedFileLinks) => Some("shared_file_links"),
        None => None,
    }
}

pub(crate) fn log_preservation_hold(stage: &'static str, error: &anyhow::Error) {
    if let Some(diagnostic) = crate::icloud::photos::classify_legacy_inventory_error(error) {
        tracing::warn!(
            stage,
            reason = diagnostic.reason,
            diagnostic = "legacy_inventory_failure_v2",
            phase = diagnostic.phase,
            subreason = diagnostic.subreason,
            eof_observed = diagnostic.eof_observed,
            family_context = diagnostic.family_context,
            child_soft_deleted = diagnostic.child_soft_deleted,
            pages = diagnostic.pages,
            records = diagnostic.records,
            transferred_bytes = diagnostic.transferred_bytes,
            retained_bytes = diagnostic.retained_bytes,
            "Legacy preservation inventory unavailable; retaining attribution checkpoint hold"
        );
    } else if let Some(reason) = classify_legacy_file_error(error) {
        tracing::warn!(
            stage,
            reason,
            "Legacy preservation files unavailable; retaining attribution checkpoint hold"
        );
    } else {
        // Provider errors and file/state errors can contain private identifiers,
        // URLs or paths. Only a fixed category reaches ordinary logs.
        tracing::warn!(
            stage,
            reason = "current_evidence_incomplete",
            "Legacy preservation evidence incomplete; retaining attribution checkpoint hold"
        );
    }
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
            let retry_key = format!("{INVENTORY_RETRY_PREFIX}{}", config.library);
            let signature = inventory_retry_signature(config_hash, &candidates)?;
            let now = chrono::Utc::now().timestamp();
            let deferred = db
                .get_metadata(&retry_key)
                .await?
                .and_then(|encoded| serde_json::from_str::<InventoryRetry>(&encoded).ok())
                .is_some_and(|retry| retry.deferred(&signature, now));
            if deferred {
                tracing::info!(
                    stage = "preparation",
                    reason = "inventory_retry_deferred",
                    candidates = candidates.len(),
                    "Legacy preservation discovery deferred; retaining attribution checkpoint hold"
                );
                return Ok(Self {
                    preserved,
                    prepared_proofs: Vec::new(),
                });
            }
            let masters = candidates
                .iter()
                .map(|candidate| candidate.asset_id.clone())
                .collect();
            // Provider failures cannot relax existing capture/checkpoint guards.
            match album.complete_legacy_inventory(&masters, cancel).await {
                Ok(inventory) => {
                    // A successful scan cannot borrow an earlier failure delay.
                    db.set_metadata(&retry_key, "").await?;
                    let mut no_current_child = 0usize;
                    let mut invalid_original_files = 0usize;
                    let mut shared_file_links = 0usize;
                    let mut stale_candidates = 0usize;
                    for candidate in candidates {
                        if !inventory
                            .children
                            .iter()
                            .any(|child| child.id() == candidate.asset_id)
                        {
                            no_current_child += 1;
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
                                    Err(error) => {
                                        if classify_legacy_file_error(&error).is_some() {
                                            shared_file_links += 1;
                                        }
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
                            if !db.prepare_legacy_preservation(&candidate, &files).await? {
                                stale_candidates += 1;
                            }
                        } else {
                            invalid_original_files += 1;
                        }
                    }
                    if no_current_child + invalid_original_files + stale_candidates > 0 {
                        tracing::warn!(
                            stage = "preparation",
                            no_current_child,
                            invalid_original_files,
                            shared_file_links,
                            stale_candidates,
                            "Legacy preservation candidates remain unresolved; retaining attribution checkpoint hold"
                        );
                    }
                    preserved = db.legacy_preservations(&config.library).await?;
                }
                Err(error) => {
                    log_preservation_hold("preparation", &error);
                    if !cancel.is_cancelled() {
                        let retry = InventoryRetry {
                            version: 1,
                            signature,
                            next_attempt_at: chrono::Utc::now()
                                .timestamp()
                                .saturating_add(INVENTORY_RETRY_SECONDS),
                        };
                        db.set_metadata(&retry_key, &serde_json::to_string(&retry)?)
                            .await?;
                    }
                }
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
            let links = crate::fs_util::file_link_count(file)?;
            anyhow::ensure!(links <= 1, LegacyFileError::SharedFileLinks);
            anyhow::ensure!(
                links == 1,
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
    anyhow::ensure!(
        !children.is_empty(),
        "Legacy family has no current children"
    );
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

    #[tokio::test]
    async fn preservation_shared_file_links_are_typed_and_redacted() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let cancel = CancellationToken::new();
        // The same helper guards originals and independently owned current outputs.
        for name in [
            "private-original.jpg",
            "private-current-child.jpg",
            "private-sidecar.xmp",
        ] {
            let path = root.join(name);
            std::fs::write(&path, b"verified synthetic bytes").unwrap();
            let evidence = fingerprint(root, &path, true, ".kei-tmp", &cancel)
                .await
                .unwrap();
            let alias = root.join(format!("{name}.alias"));
            std::fs::hard_link(&path, &alias).unwrap();
            let error = verify_files(&[evidence], ".kei-tmp", &cancel)
                .await
                .unwrap_err()
                .context("private-provider-id/private-path");
            assert_eq!(
                super::classify_legacy_file_error(&error),
                Some("shared_file_links")
            );
            let log = root.join("warning.log");
            let subscriber = tracing_subscriber::fmt()
                .with_ansi(false)
                .without_time()
                .with_max_level(tracing::Level::WARN)
                .with_writer(std::sync::Mutex::new(std::fs::File::create(&log).unwrap()))
                .finish();
            tracing::subscriber::with_default(subscriber, || {
                super::log_preservation_hold("certification", &error);
                super::log_preservation_hold("checkpoint", &error);
            });
            let output = std::fs::read_to_string(log).unwrap();
            assert!(output.contains("stage=\"certification\""));
            assert!(output.contains("stage=\"checkpoint\""));
            assert_eq!(output.matches("reason=\"shared_file_links\"").count(), 2);
            assert!(!output.contains("private"));
            assert!(!output.contains(&root.display().to_string()));
            assert_eq!(std::fs::read(&path).unwrap(), b"verified synthetic bytes");
            assert_eq!(std::fs::read(&alias).unwrap(), b"verified synthetic bytes");
        }
        assert_eq!(
            super::classify_legacy_file_error(&anyhow::anyhow!("shared_file_links")),
            None
        );
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
    #[tokio::test]
    async fn preservation_inventory_retry_roundtrip_is_bounded_and_evidence_fenced() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        let key = format!("{}PrimarySync", super::INVENTORY_RETRY_PREFIX);
        let retry = super::InventoryRetry {
            version: 1,
            signature: "original-evidence".into(),
            next_attempt_at: 4600,
        };
        {
            let db = crate::state::SqliteStateDb::open(&path).await.unwrap();
            db.set_metadata(&key, &serde_json::to_string(&retry).unwrap())
                .await
                .unwrap();
            db.set_metadata("sync_token:PrimarySync", "held")
                .await
                .unwrap();
        }
        let db = crate::state::SqliteStateDb::open(&path).await.unwrap();
        let decoded: super::InventoryRetry =
            serde_json::from_str(&db.get_metadata(&key).await.unwrap().unwrap()).unwrap();
        assert!(decoded.deferred("original-evidence", 1000));
        assert!(!decoded.deferred("changed-evidence", 1000));
        assert!(!decoded.deferred("original-evidence", 4600));
        assert!(!decoded.deferred("original-evidence", 999));
        let unsupported = super::InventoryRetry {
            version: 2,
            ..decoded
        };
        assert!(!unsupported.deferred("original-evidence", 1000));
        assert_eq!(
            db.get_metadata("sync_token:PrimarySync")
                .await
                .unwrap()
                .as_deref(),
            Some("held")
        );
        assert!(
            serde_json::from_str::<super::InventoryRetry>(
                r#"{"version":1,"signature":"x","next_attempt_at":1,"unexpected":true}"#
            )
            .is_err()
        );
    }

    #[test]
    fn preservation_inventory_retry_signature_keeps_original_facts_and_config() {
        let original = serde_json::json!({"version":1,"library":"PrimarySync","asset_id":"master","evidence":{
            "assets":{"added_at":123,"checksum":"original"},
            "family_history":{"columns":["library","child","master","updated_at"],"rows":[["PrimarySync","child-a","master",1000],["PrimarySync","child-b","master",1000]]},
            "owners":[],
            "metadata_capture_retries":{"next_retry_at":1000},
            "capture_state_at_preparation":{"processed":5}
        }});
        let snapshot = |value: serde_json::Value| crate::state::db::LegacyPreparationSnapshot {
            library: "PrimarySync".into(),
            asset_id: "master".into(),
            evidence: value.to_string(),
            paths: Vec::new(),
        };
        let baseline =
            super::inventory_retry_signature("config-a", &[snapshot(original.clone())]).unwrap();
        let mut counters = original.clone();
        counters["evidence"]["metadata_capture_retries"]["next_retry_at"] = serde_json::json!(2000);
        counters["evidence"]["capture_state_at_preparation"]["processed"] = serde_json::json!(6);
        counters["evidence"]["family_history"]["rows"][0][3] = serde_json::json!(2000);
        assert_eq!(
            baseline,
            super::inventory_retry_signature("config-a", &[snapshot(counters)]).unwrap()
        );
        assert_ne!(
            baseline,
            super::inventory_retry_signature("config-b", &[snapshot(original.clone())]).unwrap()
        );
        for field in ["assets", "family_history", "owners"] {
            let mut changed = original.clone();
            changed["evidence"][field] = serde_json::json!("changed");
            assert_ne!(
                baseline,
                super::inventory_retry_signature("config-a", &[snapshot(changed)]).unwrap(),
                "{field}"
            );
        }
    }
}
