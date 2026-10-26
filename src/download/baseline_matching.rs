//! Read-only source-byte matching for the initial filesystem comparison.
//! Content equivalence never merges provider identities or grants write ownership.

use std::path::Path;
use std::sync::Arc;

use rustc_hash::FxHashMap;

use crate::state::VersionSizeKey;
use crate::state::db::BaselineReceipt;
use crate::state::db::provider_generations::SelectedCopyScope;

use crate::state::db::match_diagnostics::{MatchAttempt, MatchDiagnostics, MatchExplanation};

use super::file::{RetainedPendingFile, retain_pending_file};
use super::filter::DownloadTask;

/// Positive proof retains the file through the caller's conditional state commit.
/// The caller must recheck the receipt and validate the file before committing.
#[must_use]
#[derive(Debug)]
pub(in crate::download) enum BaselineMatch {
    Present {
        receipt: BaselineReceipt,
        file: Arc<RetainedPendingFile>,
    },
    /// Derived from a full retained-file v1 comparison, never loaded from state.
    ProviderPresent {
        receipt: BaselineReceipt,
        file: Arc<RetainedPendingFile>,
    },
    RecordedPresent {
        receipt: crate::state::db::download_receipts::DownloadReceipt,
        file: Arc<RetainedPendingFile>,
    },
    Missing,
    Uncertain(MatchExplanation),
}

/// Commit only positive matches through their original durable proof owner.
pub(in crate::download) async fn commit_match(
    decision: BaselineMatch,
    config: &super::DownloadConfig,
    asset: &crate::icloud::photos::PhotoAsset,
    task: &DownloadTask,
    shutdown: &tokio_util::sync::CancellationToken,
) -> anyhow::Result<()> {
    let db = config
        .state_db
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("Existing-file reuse requires durable state"))?;
    let record = super::planner::pending_record_for_task(config, asset, task);
    match decision {
        BaselineMatch::RecordedPresent { receipt, file } => {
            let _guard =
                super::file::lock_download_destination(&receipt.native_path.to_path(), shutdown)
                    .await?;
            let binding = task
                .publication_binding
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("Missing publication scope"))?
                .clone();
            let album = config
                .primary_layout_source
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("Missing authenticated source"))?;
            let (capture, scope, _) = album
                .publication_scope(&task.library)?
                .ok_or_else(|| anyhow::anyhow!("Missing authenticated scope"))?;
            anyhow::ensure!(scope == binding.scope, "Publication scope changed");
            capture
                .db
                .reuse_bound_download(
                    binding,
                    receipt,
                    record,
                    Box::new(move || {
                        file.validate_blocking()
                            // Filesystem errors may contain private archive paths.
                            .map_err(|_private_error| {
                                crate::state::error::StateError::ProviderSelectionInvalid
                            })
                    }),
                )
                .await?;
        }
        decision => {
            let provider = matches!(&decision, BaselineMatch::ProviderPresent { .. });
            let (receipt, file) = match decision {
                BaselineMatch::Present { receipt, file }
                | BaselineMatch::ProviderPresent { receipt, file } => (receipt, file),
                _ => anyhow::bail!("Nonpositive existing-file decision cannot be committed"),
            };
            let _guard =
                super::file::lock_download_destination(&receipt.local_path, shutdown).await?;
            let checksum = task.checksum.to_string();
            let size = task.size;
            let validate: Box<dyn FnOnce() -> Result<(), crate::state::error::StateError> + Send> =
                Box::new(move || {
                    let valid = if provider {
                        file.fingerprint.size == size
                            && matches!(
                                file.matches_provider_v1_blocking(&checksum),
                                Ok(Some(true))
                            )
                    } else {
                        file.validate_blocking().is_ok()
                    };
                    if valid {
                        Ok(())
                    } else {
                        Err(crate::state::error::StateError::ProviderSelectionInvalid)
                    }
                });
            if provider {
                db.adopt_provider_signature(&record, &receipt, validate)
                    .await?;
            } else {
                db.adopt_baseline_receipt(&record, &receipt, validate)
                    .await?;
            }
        }
    }
    Ok(())
}

/// Checked discovery only locates candidates; complete provider proof is still required.
#[derive(Debug)]
pub(in crate::download) struct BaselineCandidates {
    pub paths: Vec<std::path::PathBuf>,
    pub complete: bool,
}

impl Default for BaselineCandidates {
    fn default() -> Self {
        Self {
            paths: Vec::new(),
            complete: true,
        }
    }
}

/// Index once per comparison; labels and names are not content identity.
#[derive(Debug, Default)]
pub(in crate::download) struct BaselineMatcher {
    receipts: FxHashMap<(VersionSizeKey, String, u64), Vec<BaselineReceipt>>,
}

impl BaselineMatcher {
    pub(in crate::download) fn new(receipts: Vec<BaselineReceipt>) -> Self {
        let mut index = Self::default();
        for receipt in receipts {
            index
                .receipts
                .entry((
                    receipt.version_size,
                    receipt.provider_checksum.clone(),
                    receipt.provider_size,
                ))
                .or_default()
                .push(receipt);
        }
        for candidates in index.receipts.values_mut() {
            candidates.sort_by(|left, right| {
                right
                    .is_current_path
                    .cmp(&left.is_current_path)
                    .then_with(|| left.local_path.cmp(&right.local_path))
            });
        }
        index
    }

    /// Current provider facts select content candidates across historical labels.
    /// A verified source/local receipt is required; an imported local hash is not one.
    #[cfg(test)]
    pub(in crate::download) async fn match_task(
        &self,
        root: &Path,
        task: &DownloadTask,
        requested_path: &Path,
    ) -> BaselineMatch {
        self.match_task_in_scope(
            root,
            task,
            requested_path,
            false,
            &SelectedCopyScope {
                admitted: false,
                other: [].to_vec(),
            },
            &BaselineCandidates::default(),
        )
        .await
    }

    /// Pass-specific copies cannot reuse another directory's receipt as coverage.
    pub(in crate::download) async fn match_task_in_scope(
        &self,
        root: &Path,
        task: &DownloadTask,
        requested_path: &Path,
        require_requested_parent: bool,
        selected: &SelectedCopyScope,
        discovered: &BaselineCandidates,
    ) -> BaselineMatch {
        if task.checksum.trim().is_empty() {
            return uncertain("provider_signature_missing", None);
        }
        let key = (task.version_size, task.checksum.to_string(), task.size);
        let candidates = self.receipts.get(&key).map_or(&[][..], Vec::as_slice);
        let separate_destination_admitted = selected.admitted;
        let mut other_selected_copies = selected.other.clone();
        for (native, compatible) in &mut other_selected_copies {
            if let Some(receipt) = compatible
                && receipt.local_path != *native
                && !matches!(
                    tokio::fs::symlink_metadata(&receipt.local_path).await,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound
                )
            {
                // A lossy spelling may also name a real UTF-8 sibling. Ledger
                // uniqueness alone cannot attribute that file to the native copy.
                *compatible = None;
            }
        }
        let belongs_to_other_copy = |receipt: &BaselineReceipt| {
            other_selected_copies.iter().any(|(native, compatible)| {
                receipt.local_path == *native
                    || compatible.as_ref().is_some_and(|other| {
                        receipt.library == other.library
                            && receipt.id == other.id
                            && receipt.local_path == other.local_path
                            && receipt.local_checksum == other.local_checksum
                            && receipt.source_checksum == other.source_checksum
                    })
            })
        };
        let mut source_digest: Option<&str> = None;
        let mut conflicting_sources = false;
        for receipt in candidates
            .iter()
            .filter(|receipt| !belongs_to_other_copy(receipt))
        {
            if let Some(source) = receipt
                .source_checksum
                .as_deref()
                .filter(|value| valid_sha256(value))
            {
                if source_digest.is_some_and(|prior| prior != source) {
                    conflicting_sources = true;
                }
                source_digest = Some(source);
            }
        }
        let mut explanation = MatchExplanation::default();
        if conflicting_sources {
            explanation.add(None, "conflicting_receipt_source_hashes");
        }
        if !discovered.complete {
            explanation.add(None, "candidate_discovery_incomplete");
        }
        let mut uncertain = conflicting_sources || !discovered.complete;
        let is_identity = |receipt: &&BaselineReceipt| {
            receipt.library == task.library.as_ref() && receipt.id == task.asset_id.as_ref()
        };
        let ordered = candidates
            .iter()
            .filter(is_identity)
            .chain(candidates.iter().filter(|receipt| !is_identity(receipt)));
        for receipt in ordered.filter(|receipt| !belongs_to_other_copy(receipt)) {
            let other_parent = if require_requested_parent {
                let parent_key = |path: &Path| {
                    path.parent()
                        .and_then(|parent| crate::state::db::primary_layout::path_key(parent).ok())
                };
                let requested_parent = parent_key(requested_path);
                if requested_parent.is_none() {
                    uncertain = true;
                    explanation.add(
                        Some(&receipt.local_path),
                        "path_or_receipt_validation_failed",
                    );
                    continue;
                }
                parent_key(&receipt.local_path) != requested_parent
            } else {
                false
            };
            if !super::file::supported_provider_signature(&task.checksum)
                && (conflicting_sources
                    || receipt
                        .source_checksum
                        .as_deref()
                        .filter(|source| valid_sha256(source))
                        .is_none_or(|source| receipt.local_checksum.as_deref() != Some(source)))
            {
                match super::file::inspect_pending_candidate(root, &receipt.local_path).await {
                    Ok(false) => {}
                    Ok(true) => {
                        explanation.add(
                            Some(&receipt.local_path),
                            "no_valid_receipt_or_supported_provider_proof",
                        );
                        uncertain = true;
                    }
                    Err(_) => {
                        explanation.add(
                            Some(&receipt.local_path),
                            "candidate_unreadable_unsafe_or_changed",
                        );
                        uncertain = true;
                    }
                }
                continue;
            }
            let file = match retain_pending_file(root, &receipt.local_path).await {
                Ok(Some(file)) => file,
                Ok(None) => continue,
                Err(_) => {
                    uncertain = true;
                    explanation.add(
                        Some(&receipt.local_path),
                        "path_or_receipt_validation_failed",
                    );
                    continue;
                }
            };
            let actual = data_encoding::HEXLOWER.encode(&file.fingerprint.sha256);
            // Old generic adoption could retain a genuine source hash while
            // replacing the local hash with unrelated same-size bytes. A legacy
            // source/local pair therefore proves reuse only when today's bytes
            // still match the original source hash. Metadata-edited files need
            // an independently bound publication/rewrite receipt.
            let verified = !conflicting_sources
                && receipt
                    .source_checksum
                    .as_deref()
                    .filter(|source| valid_sha256(source))
                    .is_some_and(|source| source == actual && file.fingerprint.size == task.size)
                && receipt.local_checksum.as_deref() == Some(actual.as_str());
            // A supported current signature also checks legacy receipts:
            // contradictory provider bytes cannot be overridden by old state.
            let provider_comparison =
                if receipt.local_path.to_str().is_some() && file.fingerprint.size == task.size {
                    file.matches_provider_v1(&task.checksum).await
                } else {
                    Ok(None)
                };
            let provider_verified = match provider_comparison {
                Ok(Some(true)) => !verified,
                Ok(None) => false,
                Ok(Some(false)) => {
                    uncertain = true;
                    explanation.add(
                        Some(&receipt.local_path),
                        "provider_content_signature_mismatch",
                    );
                    continue;
                }
                Err(_) => {
                    uncertain = true;
                    explanation.add(
                        Some(&receipt.local_path),
                        "candidate_unreadable_unsafe_or_changed",
                    );
                    continue;
                }
            };
            if !verified && !provider_verified {
                explanation.add(
                    Some(&receipt.local_path),
                    "no_valid_receipt_or_supported_provider_proof",
                );
                uncertain = true;
                continue;
            }
            if other_parent {
                // A verified A cannot fulfill independently admitted B. It
                // also cannot make an absent B ambiguous. Without that exact
                // admission, preserve possible historical-layout ambiguity.
                uncertain |= !separate_destination_admitted;
                if !separate_destination_admitted {
                    explanation.add(
                        Some(&receipt.local_path),
                        "different_selected_destination_requires_confirmation",
                    );
                }
                continue;
            }
            return if provider_verified {
                BaselineMatch::ProviderPresent {
                    receipt: provider_receipt(task, &receipt.local_path, &actual),
                    file,
                }
            } else {
                BaselineMatch::Present {
                    receipt: receipt.clone(),
                    file,
                }
            };
        }
        // An occupied path is only a candidate. Supported provider signatures
        // can prove its complete bytes even when historical receipts are absent.
        let mut visited = std::collections::HashSet::new();
        for path in [requested_path, task.download_path.as_path()]
            .into_iter()
            .chain(discovered.paths.iter().map(std::path::PathBuf::as_path))
        {
            if !visited.insert(path.to_path_buf()) {
                continue;
            }
            if other_selected_copies.iter().any(|(native, compatible)| {
                native == path
                    || compatible
                        .as_ref()
                        .is_some_and(|old| old.local_path == path)
            }) {
                continue;
            }
            // No known content proof can result from hashing this unreceipted
            // candidate. Still inspect confinement/existence so absence and
            // unsafe occupied paths retain their original distinct decisions.
            if !super::file::supported_provider_signature(&task.checksum) || path.to_str().is_none()
            {
                match super::file::inspect_pending_candidate(root, path).await {
                    Ok(false) => {}
                    Ok(true) => {
                        explanation.add(
                            Some(path),
                            "unsupported_provider_proof_without_valid_receipt",
                        );
                        uncertain = true;
                    }
                    Err(_) => {
                        explanation.add(Some(path), "candidate_unreadable_unsafe_or_changed");
                        uncertain = true;
                    }
                }
                continue;
            }
            match retain_pending_file(root, path).await {
                Ok(None) => {}
                Ok(Some(file)) => {
                    if path.to_str().is_some()
                        && file.fingerprint.size == task.size
                        && matches!(
                            file.matches_provider_v1(&task.checksum).await,
                            Ok(Some(true))
                        )
                    {
                        let local = data_encoding::HEXLOWER.encode(&file.fingerprint.sha256);
                        return BaselineMatch::ProviderPresent {
                            receipt: provider_receipt(task, path, &local),
                            file,
                        };
                    }
                    explanation.add(
                        Some(path),
                        "candidate_bytes_not_verified_for_current_resource",
                    );
                    uncertain = true;
                }
                Err(_) => {
                    explanation.add(Some(path), "candidate_unreadable_unsafe_or_changed");
                    uncertain = true;
                }
            }
        }
        if uncertain {
            BaselineMatch::Uncertain(explanation)
        } else {
            BaselineMatch::Missing
        }
    }
}

pub(in crate::download) fn uncertain(reason: &str, path: Option<&Path>) -> BaselineMatch {
    let mut explanation = MatchExplanation::default();
    explanation.add(path, reason);
    BaselineMatch::Uncertain(explanation)
}

pub(in crate::download) fn pending_record(
    config: &super::DownloadConfig,
    asset: &crate::icloud::photos::PhotoAsset,
    task: &DownloadTask,
    explanation: MatchExplanation,
) -> crate::state::AssetRecord {
    let mut record = super::planner::pending_record_for_task(config, asset, task);
    let report = MatchDiagnostics {
        last_transfer_error: None,
        attempts: vec![MatchAttempt {
            generation: config.selection_run.as_ref().map(|run| run.root.id.clone()),
            pass: config.primary_layout_pass.as_ref().map(ToString::to_string),
            child: task.asset_record_name.to_string(),
            destination: crate::state::db::provider_selection::SelectionPath::from_path(
                &std::path::absolute(&task.download_path)
                    .unwrap_or_else(|_| task.download_path.clone()),
            ),
            checksum: task.checksum.to_string(),
            observed_at: chrono::Utc::now().timestamp(),
            explanation,
        }],
        omitted: 0,
    };
    record.last_error = report.encode();
    record
}

fn provider_receipt(task: &DownloadTask, path: &Path, local: &str) -> BaselineReceipt {
    BaselineReceipt {
        is_current_path: true,
        library: task.library.to_string(),
        id: task.asset_id.to_string(),
        version_size: task.version_size,
        provider_checksum: task.checksum.to_string(),
        provider_size: task.size,
        local_path: path.to_owned(),
        local_checksum: Some(local.to_owned()),
        source_checksum: Some(local.to_owned()),
    }
}

fn valid_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

#[cfg(test)]
mod tests {
    use super::{BaselineMatch, BaselineMatcher};
    use crate::download::{DownloadConfig, filter, paths};
    use crate::state::db::BaselineReceipt;
    use crate::test_helpers::TestPhotoAsset;
    use rustc_hash::FxHashMap;
    use sha2::{Digest, Sha256};
    use std::path::Path;

    pub(super) fn task(root: &Path) -> filter::DownloadTask {
        let mut config = DownloadConfig::test_default();
        config.directory = root.to_path_buf().into();
        let asset = TestPhotoAsset::new("current-child")
            .filename("IMG_0001.JPG")
            .orig_size(4)
            .orig_checksum("current-generation")
            .build();
        let mut tasks = filter::filter_asset_to_tasks(
            &asset,
            &config,
            &mut FxHashMap::default(),
            &mut paths::DirCache::new(),
            filter::PathPlanningMode::Download,
        )
        .unwrap();
        let mut task = tasks.remove(0);
        task.library = "SharedSync".into();
        task.asset_id = "current-child".into();
        task.download_path = root.join("IMG_0001.JPG");
        task
    }

    pub(super) fn receipt(task: &filter::DownloadTask, path: &Path) -> BaselineReceipt {
        let digest = data_encoding::HEXLOWER.encode(&Sha256::digest(b"good"));
        BaselineReceipt {
            is_current_path: true,
            library: "PrimarySync".into(),
            id: "old-master".into(),
            version_size: task.version_size,
            provider_checksum: task.checksum.to_string(),
            provider_size: task.size,
            local_path: path.to_path_buf(),
            local_checksum: Some(digest.clone()),
            source_checksum: Some(digest),
        }
    }

    #[tokio::test]
    async fn mixed_labels_reuse_exact_source_bytes_without_merging_identities() {
        let tmp = tempfile::tempdir().unwrap();
        let task = task(tmp.path());
        let path = tmp.path().join("old-renamed-4096.JPG");
        std::fs::write(&path, b"good").unwrap();
        let modified = std::fs::metadata(&path).unwrap().modified().unwrap();
        let matcher = BaselineMatcher::new(vec![receipt(&task, &path)]);
        for current_id in ["current-child", "distinct-child-with-same-bytes"] {
            let mut selected = task.clone();
            selected.asset_id = current_id.into();
            let BaselineMatch::Present { receipt, file } = matcher
                .match_task(tmp.path(), &selected, &selected.download_path)
                .await
            else {
                panic!("receipt-backed content should be reusable");
            };
            assert_eq!(receipt.library, "PrimarySync");
            assert_eq!(receipt.id, "old-master");
            assert_eq!(receipt.local_path, path);
            assert_eq!(selected.asset_id.as_ref(), current_id);
            assert_eq!(selected.library.as_ref(), "SharedSync");
            file.validate().await.unwrap();
        }
        assert_eq!(std::fs::read(&path).unwrap(), b"good");
        assert_eq!(
            std::fs::metadata(&path).unwrap().modified().unwrap(),
            modified
        );
        assert!(!task.download_path.exists());
    }

    #[tokio::test]
    async fn wrong_same_size_bytes_and_absent_source_provenance_remain_uncertain() {
        let tmp = tempfile::tempdir().unwrap();
        let task = task(tmp.path());
        let path = tmp.path().join("old.JPG");
        std::fs::write(&path, b"evil").unwrap();
        assert!(matches!(
            BaselineMatcher::new(vec![receipt(&task, &path)])
                .match_task(tmp.path(), &task, &task.download_path)
                .await,
            BaselineMatch::Uncertain(_)
        ));
        std::fs::write(&path, b"good").unwrap();
        let mut imported = receipt(&task, &path);
        imported.source_checksum = None;
        assert!(matches!(
            BaselineMatcher::new(vec![imported])
                .match_task(tmp.path(), &task, &task.download_path)
                .await,
            BaselineMatch::Uncertain(_)
        ));
        assert!(matches!(
            BaselineMatcher::default()
                .match_task(tmp.path(), &task, &task.download_path)
                .await,
            BaselineMatch::Missing
        ));
        std::fs::write(&task.download_path, b"good").unwrap();
        assert!(matches!(
            BaselineMatcher::default()
                .match_task(tmp.path(), &task, &task.download_path)
                .await,
            BaselineMatch::Uncertain(_)
        ));
    }

    #[tokio::test]
    async fn contradictory_receipts_do_not_choose_one_source_digest() {
        let tmp = tempfile::tempdir().unwrap();
        let task = task(tmp.path());
        let path = tmp.path().join("old.JPG");
        std::fs::write(&path, b"good").unwrap();
        let first = receipt(&task, &path);
        let mut second = first.clone();
        second.id = "another-child".into();
        second.source_checksum = Some(data_encoding::HEXLOWER.encode(&Sha256::digest(b"evil")));
        assert!(matches!(
            BaselineMatcher::new(vec![first, second])
                .match_task(tmp.path(), &task, &task.download_path)
                .await,
            BaselineMatch::Uncertain(_)
        ));
    }

    #[tokio::test]
    async fn changed_generation_and_missing_companion_do_not_reuse_receipt() {
        let tmp = tempfile::tempdir().unwrap();
        let task = task(tmp.path());
        let path = tmp.path().join("old.JPG");
        std::fs::write(&path, b"good").unwrap();
        let old = receipt(&task, &path);
        let matcher = BaselineMatcher::new(vec![old]);
        let mut changed = task.clone();
        changed.checksum = "new-generation".into();
        assert!(matches!(
            matcher
                .match_task(tmp.path(), &changed, &changed.download_path)
                .await,
            BaselineMatch::Missing
        ));
        changed = task.clone();
        changed.version_size = crate::state::VersionSizeKey::LiveOriginal;
        assert!(matches!(
            matcher
                .match_task(tmp.path(), &changed, &changed.download_path)
                .await,
            BaselineMatch::Missing
        ));
        changed = task.clone();
        changed.size += 1;
        assert!(matches!(
            matcher
                .match_task(tmp.path(), &changed, &changed.download_path)
                .await,
            BaselineMatch::Missing
        ));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn receipt_symlink_is_uncertain_and_changed_retained_file_is_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let task = task(tmp.path());
        let path = tmp.path().join("old.JPG");
        let source = tmp.path().join("source.JPG");
        std::fs::write(&source, b"good").unwrap();
        std::os::unix::fs::symlink(&source, &path).unwrap();
        assert!(matches!(
            BaselineMatcher::new(vec![receipt(&task, &path)])
                .match_task(tmp.path(), &task, &task.download_path)
                .await,
            BaselineMatch::Uncertain(_)
        ));
        let matcher = BaselineMatcher::new(vec![receipt(&task, &source)]);
        let BaselineMatch::Present { file, .. } = matcher
            .match_task(tmp.path(), &task, &task.download_path)
            .await
        else {
            panic!("valid file");
        };
        std::fs::write(&source, b"evil").unwrap();
        assert!(file.validate().await.is_err());
    }

    #[tokio::test]
    async fn baseline_receipt_query_preserves_source_provenance_across_reopen() {
        use crate::state::db::{DownloadContextStateStore, DownloadStateStore, SqliteStateDb};
        use crate::test_helpers::TestAssetRecord;

        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("state.db");
        let db = SqliteStateDb::open(&db_path).await.unwrap();
        let selected = task(tmp.path());
        for (id, verified) in [("downloaded-old", true), ("imported-old", false)] {
            let path = tmp.path().join(format!("{id}.JPG"));
            std::fs::write(&path, b"good").unwrap();
            let row = TestAssetRecord::new(id)
                .checksum(&selected.checksum)
                .size(4)
                .build();
            db.upsert_seen(&row).await.unwrap();
            let digest = data_encoding::HEXLOWER.encode(&Sha256::digest(b"good"));
            if verified {
                db.mark_verified_download(
                    "PrimarySync",
                    id,
                    selected.version_size.as_str(),
                    &path,
                    &digest,
                    Some(&digest),
                    false,
                )
                .await
                .unwrap();
            } else {
                db.mark_downloaded(
                    "PrimarySync",
                    id,
                    selected.version_size.as_str(),
                    &path,
                    &digest,
                    Some(&digest),
                )
                .await
                .unwrap();
            }
        }
        drop(db);
        let db = SqliteStateDb::open(&db_path).await.unwrap();
        let receipts = db.get_baseline_receipts().await.unwrap();
        assert_eq!(receipts.len(), 2);
        let imported = receipts
            .iter()
            .find(|receipt| receipt.id == "imported-old")
            .unwrap();
        assert_eq!(imported.source_checksum, None);
        assert!(matches!(
            BaselineMatcher::new(vec![imported.clone()])
                .match_task(tmp.path(), &selected, &selected.download_path)
                .await,
            BaselineMatch::Uncertain(_)
        ));
        let matcher = BaselineMatcher::new(receipts);
        let BaselineMatch::Present { receipt, file } = matcher
            .match_task(tmp.path(), &selected, &selected.download_path)
            .await
        else {
            panic!("verified receipt");
        };
        assert_eq!(receipt.id, "downloaded-old");
        file.validate().await.unwrap();
        drop(db);
        let db = SqliteStateDb::open(&db_path).await.unwrap();
        assert_eq!(db.get_baseline_receipts().await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn v027_renamed_legacy_path_without_current_receipt_remains_uncertain() {
        use crate::state::db::{DownloadContextStateStore, DownloadStateStore, SqliteStateDb};
        use crate::test_helpers::TestAssetRecord;

        for stale_receipt in [false, true] {
            let tmp = tempfile::tempdir().unwrap();
            let db_path = tmp.path().join("state.db");
            let db = SqliteStateDb::open(&db_path).await.unwrap();
            let mut selected = task(tmp.path());
            selected.checksum = "opaque-mmcs-generation+/=".into();
            let requested = selected.download_path.clone();
            selected.download_path = tmp.path().join("IMG_0001-4.JPG");
            let old_path = tmp.path().join("renamed-historical-2.JPG");
            let bytes = b"good with historical embedded metadata";
            std::fs::write(&old_path, bytes).unwrap();
            let modified = std::fs::metadata(&old_path).unwrap().modified().unwrap();
            let local = data_encoding::HEXLOWER.encode(&Sha256::digest(bytes));
            let source = data_encoding::HEXLOWER.encode(&Sha256::digest(b"good"));
            db.upsert_seen(
                &TestAssetRecord::new("legacy")
                    .checksum(&selected.checksum)
                    .size(4)
                    .build(),
            )
            .await
            .unwrap();
            db.mark_verified_download(
                "PrimarySync",
                "legacy",
                selected.version_size.as_str(),
                &old_path,
                &local,
                Some(&source),
                false,
            )
            .await
            .unwrap();
            {
                let conn = db
                    .acquire_lock("simulate historical absent or stale path receipt")
                    .unwrap();
                if stale_receipt {
                    conn.execute("UPDATE asset_metadata_paths SET provider_checksum='older-opaque-generation'", []).unwrap();
                } else {
                    conn.execute("DELETE FROM asset_metadata_paths", [])
                        .unwrap();
                }
            }
            drop(db);
            for _ in 0..2 {
                let db = SqliteStateDb::open(&db_path).await.unwrap();
                let receipts = db.get_baseline_receipts().await.unwrap();
                let prior = receipts
                    .iter()
                    .find(|receipt| receipt.local_path == old_path)
                    .expect("the legacy recorded path remains a useful unverified candidate");
                assert_eq!(prior.provider_checksum, selected.checksum.as_ref());
                assert_eq!(
                    prior.source_checksum, None,
                    "assets download checksum is not source provenance"
                );
                assert!(matches!(
                    BaselineMatcher::new(receipts)
                        .match_task(tmp.path(), &selected, &requested)
                        .await,
                    BaselineMatch::Uncertain(_)
                ));
                assert_eq!(std::fs::read(&old_path).unwrap(), bytes);
                assert_eq!(
                    std::fs::metadata(&old_path).unwrap().modified().unwrap(),
                    modified
                );
                assert!(!requested.exists());
                assert!(!selected.download_path.exists());
            }
            std::fs::remove_file(&old_path).unwrap();
            let db = SqliteStateDb::open(&db_path).await.unwrap();
            assert!(matches!(
                BaselineMatcher::new(db.get_baseline_receipts().await.unwrap())
                    .match_task(tmp.path(), &selected, &requested)
                    .await,
                BaselineMatch::Missing
            ));
        }
    }

    #[tokio::test]
    async fn occupied_requested_path_and_empty_generation_are_uncertain() {
        let tmp = tempfile::tempdir().unwrap();
        let mut task = task(tmp.path());
        let requested = task.download_path.clone();
        std::fs::write(&requested, b"good").unwrap();
        task.download_path = tmp.path().join("IMG_0001-4.JPG");
        assert!(matches!(
            BaselineMatcher::default()
                .match_task(tmp.path(), &task, &requested)
                .await,
            BaselineMatch::Uncertain(_)
        ));
        task.checksum = " ".into();
        assert!(matches!(
            BaselineMatcher::default()
                .match_task(tmp.path(), &task, &task.download_path)
                .await,
            BaselineMatch::Uncertain(_)
        ));
    }

    #[tokio::test]
    async fn metadata_rewrite_without_independent_binding_is_not_distinguishable_from_old_readoption()
     {
        use crate::state::AssetMetadata;
        use crate::state::db::{DownloadContextStateStore, DownloadStateStore, SqliteStateDb};
        use crate::state::db::{MetadataRewriteCompletion, MetadataRewriteQueue};
        use crate::test_helpers::TestAssetRecord;

        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("state.db");
        let db = SqliteStateDb::open(&db_path).await.unwrap();
        let selected = task(tmp.path());
        let path = tmp.path().join("historical-suffixed.JPG");
        let row = TestAssetRecord::new("legacy")
            .checksum(&selected.checksum)
            .size(4)
            .metadata(AssetMetadata {
                metadata_hash: Some("metadata".into()),
                ..AssetMetadata::default()
            })
            .build();
        let original = data_encoding::HEXLOWER.encode(&Sha256::digest(b"good"));
        db.upsert_seen(&row).await.unwrap();
        std::fs::write(&path, b"good").unwrap();
        db.mark_verified_download(
            "PrimarySync",
            "legacy",
            selected.version_size.as_str(),
            &path,
            &original,
            Some(&original),
            false,
        )
        .await
        .unwrap();
        db.record_metadata_write_failure("PrimarySync", "legacy", selected.version_size.as_str())
            .await
            .unwrap();
        let pending = db
            .get_pending_metadata_rewrites_page_for_queue(
                MetadataRewriteQueue::Ordinary,
                None,
                0,
                10,
            )
            .await
            .unwrap();
        assert_eq!(pending.len(), 1);
        let rewritten = b"good with embedded metadata";
        let local = data_encoding::HEXLOWER.encode(&Sha256::digest(rewritten));
        std::fs::write(&path, rewritten).unwrap();
        db.finish_metadata_rewrite(
            &pending[0],
            MetadataRewriteQueue::Ordinary,
            Some(&local),
            Some(&original),
            MetadataRewriteCompletion::Ordinary,
        )
        .await
        .unwrap();
        drop(db);
        let before = std::fs::metadata(&path).unwrap().modified().unwrap();
        let db = SqliteStateDb::open(&db_path).await.unwrap();
        let receipts = db.get_baseline_receipts().await.unwrap();
        assert_eq!(
            receipts[0].source_checksum.as_deref(),
            Some(original.as_str())
        );
        assert_eq!(receipts[0].local_checksum.as_deref(), Some(local.as_str()));
        // This fixture really performed a metadata rewrite, but the resulting
        // legacy columns are indistinguishable from v0.24/v0.25 re-adoption.
        // Do not infer the missing independently bound provenance from them.
        assert!(matches!(
            BaselineMatcher::new(receipts)
                .match_task(tmp.path(), &selected, &selected.download_path)
                .await,
            BaselineMatch::Uncertain(_)
        ));
        assert_eq!(std::fs::read(&path).unwrap(), rewritten);
        assert_eq!(
            std::fs::metadata(&path).unwrap().modified().unwrap(),
            before
        );

        let changed = TestAssetRecord::new("legacy")
            .checksum("different-generation")
            .size(4)
            .build();
        db.upsert_seen(&changed).await.unwrap();
        assert!(db.get_baseline_receipts().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn managed_layout_provenance_is_not_promoted_by_baseline_query() {
        use crate::state::db::{DownloadContextStateStore, DownloadStateStore, SqliteStateDb};
        use crate::test_helpers::TestAssetRecord;

        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("state.db");
        let db = SqliteStateDb::open(&db_path).await.unwrap();
        let selected = task(tmp.path());
        let path = tmp.path().join("layout.JPG");
        std::fs::write(&path, b"good").unwrap();
        let row = TestAssetRecord::new("legacy")
            .checksum(&selected.checksum)
            .size(4)
            .build();
        let digest = data_encoding::HEXLOWER.encode(&Sha256::digest(b"good"));
        db.upsert_seen(&row).await.unwrap();
        db.mark_verified_download(
            "PrimarySync",
            "legacy",
            selected.version_size.as_str(),
            &path,
            &digest,
            Some(&digest),
            false,
        )
        .await
        .unwrap();
        drop(db);
        {
            let conn = rusqlite::Connection::open(&db_path).unwrap();
            conn.execute("INSERT INTO primary_layout_bindings(family,library,source_library,child,generation,decision,policy,binding) VALUES ('family','PrimarySync','PrimarySync','legacy',1,'decision','policy',X'00')", []).unwrap();
        }
        let db = SqliteStateDb::open(&db_path).await.unwrap();
        let receipts = db.get_baseline_receipts().await.unwrap();
        assert_eq!(receipts.len(), 1);
        assert_eq!(receipts[0].source_checksum, None);
        assert!(matches!(
            BaselineMatcher::new(receipts)
                .match_task(tmp.path(), &selected, &selected.download_path)
                .await,
            BaselineMatch::Uncertain(_)
        ));
    }

    #[tokio::test]
    async fn current_recorded_path_precedes_additional_copies() {
        let tmp = tempfile::tempdir().unwrap();
        let task = task(tmp.path());
        let current = tmp.path().join("z-recorded.JPG");
        let extra = tmp.path().join("a-additional.JPG");
        std::fs::write(&current, b"good").unwrap();
        std::fs::write(&extra, b"good").unwrap();
        let mut primary = receipt(&task, &current);
        primary.library = task.library.to_string();
        primary.id = task.asset_id.to_string();
        let mut additional = primary.clone();
        additional.local_path = extra;
        additional.is_current_path = false;
        let BaselineMatch::Present { receipt, .. } =
            BaselineMatcher::new(vec![additional, primary])
                .match_task(tmp.path(), &task, &task.download_path)
                .await
        else {
            panic!("current path");
        };
        assert_eq!(receipt.local_path, current);
    }
}

#[cfg(test)]
mod selected_destination_tests {
    use super::tests::{receipt, task};
    use super::{BaselineCandidates, BaselineMatch, BaselineMatcher, SelectedCopyScope};

    #[tokio::test]
    async fn admitted_missing_album_copy_is_distinct_but_occupied_copy_stays_uncertain() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("A/original.JPG");
        std::fs::create_dir_all(source.parent().unwrap()).unwrap();
        std::fs::write(&source, b"good").unwrap();
        let mut target = task(root.path());
        target.download_path = root.path().join("B/original.JPG");
        let matcher = BaselineMatcher::new(vec![receipt(&target, &source)]);
        assert!(matches!(
            matcher
                .match_task_in_scope(
                    root.path(),
                    &target,
                    &target.download_path,
                    true,
                    &SelectedCopyScope {
                        admitted: false,
                        other: [].to_vec()
                    },
                    &BaselineCandidates::default()
                )
                .await,
            BaselineMatch::Uncertain(_)
        ));
        assert!(matches!(
            matcher
                .match_task_in_scope(
                    root.path(),
                    &target,
                    &target.download_path,
                    true,
                    &SelectedCopyScope {
                        admitted: true,
                        other: [].to_vec()
                    },
                    &BaselineCandidates::default()
                )
                .await,
            BaselineMatch::Missing
        ));
        std::fs::create_dir_all(target.download_path.parent().unwrap()).unwrap();
        std::fs::write(&target.download_path, b"evil").unwrap();
        assert!(matches!(
            matcher
                .match_task_in_scope(
                    root.path(),
                    &target,
                    &target.download_path,
                    true,
                    &SelectedCopyScope {
                        admitted: true,
                        other: [].to_vec()
                    },
                    &BaselineCandidates::default()
                )
                .await,
            BaselineMatch::Uncertain(_)
        ));
        assert_eq!(std::fs::read(&source).unwrap(), b"good");
        assert_eq!(std::fs::read(&target.download_path).unwrap(), b"evil");
    }
}

#[cfg(all(test, unix))]
mod selected_copy_scope_tests {
    use super::tests::{receipt, task};
    use super::{BaselineCandidates, BaselineMatch, BaselineMatcher, SelectedCopyScope};
    use std::os::unix::ffi::OsStringExt;

    #[tokio::test]
    async fn v027_ambiguous_native_compatibility_receipt_stays_uncertain() {
        let dir = tempfile::tempdir().unwrap();
        let target = task(dir.path());
        let native = dir
            .path()
            .join(std::ffi::OsString::from_vec(b"other-\xff.JPG".to_vec()));
        let compatible = std::path::PathBuf::from(native.to_string_lossy().as_ref());
        std::fs::write(&native, b"good").unwrap();
        std::fs::write(&compatible, b"evil").unwrap();
        let candidate = receipt(&target, &compatible);
        let matcher = BaselineMatcher::new(vec![candidate.clone()]);
        // Multiple native bindings must make the owner withhold a compatibility
        // exclusion. The still-uncertain TEXT candidate cannot disappear.
        assert!(matches!(
            matcher
                .match_task_in_scope(
                    dir.path(),
                    &target,
                    &target.download_path,
                    false,
                    &SelectedCopyScope {
                        admitted: true,
                        other: [(native.clone(), None)].to_vec()
                    },
                    &BaselineCandidates::default()
                )
                .await,
            BaselineMatch::Uncertain(_)
        ));
        // Even a unique other-copy binding with different receipts cannot hide
        // a same-spelled candidate whose provenance is not that binding.
        // A matching ledger receipt still cannot identify an existing UTF-8
        // sibling as the native file that supplied its lossy TEXT path.
        assert!(matches!(
            matcher
                .match_task_in_scope(
                    dir.path(),
                    &target,
                    &target.download_path,
                    false,
                    &SelectedCopyScope {
                        admitted: true,
                        other: [(native.clone(), Some(candidate.clone()))].to_vec()
                    },
                    &BaselineCandidates::default()
                )
                .await,
            BaselineMatch::Uncertain(_)
        ));
        let mut unrelated = candidate;
        unrelated.local_checksum = Some("0".repeat(64));
        unrelated.source_checksum = Some("0".repeat(64));
        assert!(matches!(
            matcher
                .match_task_in_scope(
                    dir.path(),
                    &target,
                    &target.download_path,
                    false,
                    &SelectedCopyScope {
                        admitted: true,
                        other: [(native.clone(), Some(unrelated))].to_vec()
                    },
                    &BaselineCandidates::default()
                )
                .await,
            BaselineMatch::Uncertain(_)
        ));
        assert!(!target.download_path.exists());
        assert_eq!(std::fs::read(native).unwrap(), b"good");
        assert_eq!(std::fs::read(compatible).unwrap(), b"evil");
    }
}

#[cfg(test)]
mod historical_writer_tests {
    use super::tests::task;
    use super::{BaselineMatch, BaselineMatcher};
    use crate::state::db::{DownloadContextStateStore, DownloadStateStore, SqliteStateDb};
    use crate::test_helpers::TestAssetRecord;
    use sha2::{Digest, Sha256};

    #[tokio::test]
    async fn v027_v024_same_identity_readoption_cannot_inherit_metadata_edit_provenance() {
        let dir = tempfile::tempdir().unwrap();
        let database = dir.path().join("state.sqlite");
        let path = dir.path().join("historical.JPG");
        let mut selected = task(dir.path());
        selected.library = "PrimarySync".into();
        let db = SqliteStateDb::open(&database).await.unwrap();
        let row = TestAssetRecord::new(selected.asset_id.as_ref())
            .checksum(&selected.checksum)
            .size(4)
            .build();
        db.upsert_seen(&row).await.unwrap();
        let source = data_encoding::HEXLOWER.encode(&Sha256::digest(b"good"));
        std::fs::write(&path, b"good").unwrap();
        db.mark_verified_download(
            selected.library.as_ref(),
            selected.asset_id.as_ref(),
            selected.version_size.as_str(),
            &path,
            &source,
            Some(&source),
            false,
        )
        .await
        .unwrap();

        // v0.24.1 and v0.25.2 pending_file_size_allows_adoption accepted
        // exact-size bytes without checking the recorded local hash. Their
        // generic mark_downloaded writer supplied no source checksum, while
        // record_metadata_path retained an existing source for equal provider
        // checksum even when the local fingerprint changed. Replay that
        // historical durable result; today's writer correctly clears it.
        std::fs::write(&path, b"evil").unwrap();
        let changed = data_encoding::HEXLOWER.encode(&Sha256::digest(b"evil"));
        {
            let conn = db.acquire_lock("replay v024 generic readoption").unwrap();
            let tx = conn.unchecked_transaction().unwrap();
            tx.execute(
                "UPDATE assets SET status='downloaded',local_checksum=?1
                 WHERE library=?2 AND id=?3 AND version_size=?4",
                rusqlite::params![
                    changed,
                    selected.library.as_ref(),
                    selected.asset_id.as_ref(),
                    selected.version_size.as_str()
                ],
            )
            .unwrap();
            tx.execute(
                "INSERT INTO asset_metadata_paths
                    (library,id,version_size,local_path,provider_checksum,
                     local_checksum,download_checksum)
                 SELECT library,id,version_size,local_path,checksum,
                        local_checksum,download_checksum FROM assets
                 WHERE library=?1 AND id=?2 AND version_size=?3
                 ON CONFLICT(library,id,version_size,local_path) DO UPDATE SET
                     provider_checksum=excluded.provider_checksum,
                     local_checksum=excluded.local_checksum,
                     download_checksum=excluded.download_checksum,
                     source_checksum=CASE
                         WHEN asset_metadata_paths.provider_checksum=excluded.provider_checksum
                         THEN asset_metadata_paths.source_checksum ELSE NULL END",
                rusqlite::params![
                    selected.library.as_ref(),
                    selected.asset_id.as_ref(),
                    selected.version_size.as_str()
                ],
            )
            .unwrap();
            tx.commit().unwrap();
        }
        drop(db);
        let mtime = std::fs::metadata(&path).unwrap().modified().unwrap();
        for _ in 0..2 {
            let db = SqliteStateDb::open(&database).await.unwrap();
            let receipts = db.get_baseline_receipts().await.unwrap();
            assert_eq!(receipts.len(), 1);
            assert_eq!(
                receipts[0].source_checksum.as_deref(),
                Some(source.as_str())
            );
            assert_eq!(
                receipts[0].local_checksum.as_deref(),
                Some(changed.as_str())
            );
            assert_eq!(receipts[0].id, selected.asset_id.as_ref());
            assert!(
                matches!(
                    BaselineMatcher::new(receipts)
                        .match_task(dir.path(), &selected, &selected.download_path)
                        .await,
                    BaselineMatch::Uncertain(_)
                ),
                "historical source/local mismatch alone cannot establish a metadata-only edit"
            );
            assert_eq!(std::fs::read(&path).unwrap(), b"evil");
            assert_eq!(std::fs::metadata(&path).unwrap().modified().unwrap(), mtime);
            assert!(!selected.download_path.exists());
        }
    }
}

#[cfg(test)]
mod provider_v1_matching_tests {
    use super::{BaselineMatch, BaselineMatcher, provider_receipt};
    use crate::state::SqliteStateDb;
    use crate::state::db::DownloadContextStateStore;
    use crate::test_helpers::TestAssetRecord;
    use sha1::{Digest, Sha1};

    fn signature(bytes: &[u8]) -> String {
        let mut hash = Sha1::new();
        hash.update(b"com.apple.XattrObjectSalt\0com.apple.DataObjectSalt\0");
        hash.update(bytes);
        data_encoding::BASE64.encode(&[vec![1], hash.finalize().to_vec()].concat())
    }

    #[tokio::test]
    async fn v027_provider_v1_reuses_renamed_missing_receipt_without_changing_historical_owner() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("renamed-legacy-1.JPG");
        std::fs::write(&path, b"good").unwrap();
        let before = std::fs::metadata(&path).unwrap().modified().unwrap();
        let state = dir.path().join("state.sqlite");
        let db = SqliteStateDb::open(&state).await.unwrap();
        let mut task = super::tests::task(dir.path());
        task.checksum = signature(b"good").into();
        let local = crate::download::file::compute_sha256(&path).await.unwrap();
        let old = TestAssetRecord::new("old-master")
            .library("PrimarySync")
            .checksum(&task.checksum)
            .size(4)
            .build();
        db.upsert_seen(&old).await.unwrap();
        db.mark_downloaded("PrimarySync", "old-master", "original", &path, &local, None)
            .await
            .unwrap();
        let receipts = db.get_baseline_receipts().await.unwrap();
        assert_eq!(receipts[0].source_checksum, None);
        let BaselineMatch::ProviderPresent { receipt, file } = BaselineMatcher::new(receipts)
            .match_task(dir.path(), &task, &task.download_path)
            .await
        else {
            panic!("complete supported provider signature should verify missing source receipt");
        };
        assert_eq!(receipt.id, "current-child");
        assert_eq!(receipt.library, "SharedSync");
        let current = TestAssetRecord::new("current-child")
            .library("SharedSync")
            .checksum(&task.checksum)
            .size(4)
            .build();
        let expected = task.checksum.to_string();
        db.adopt_provider_signature(
            &current,
            &receipt,
            Box::new(move || {
                if file.matches_provider_v1_blocking(&expected).ok() != Some(Some(true)) {
                    return Err(crate::state::error::StateError::ProviderSelectionInvalid);
                }
                Ok(())
            }),
        )
        .await
        .unwrap();
        drop(db);
        for _ in 0..2 {
            let db = SqliteStateDb::open(&state).await.unwrap();
            let receipts = db.get_baseline_receipts().await.unwrap();
            let old = receipts.iter().find(|r| r.id == "old-master").unwrap();
            assert_eq!(
                old.source_checksum, None,
                "old provenance must not be rewritten"
            );
            assert_eq!(old.local_path, path);
            assert!(matches!(
                BaselineMatcher::new(receipts)
                    .match_task(dir.path(), &task, &task.download_path)
                    .await,
                BaselineMatch::Present { .. }
            ));
            assert_eq!(std::fs::read(&path).unwrap(), b"good");
            assert_eq!(
                std::fs::metadata(&path).unwrap().modified().unwrap(),
                before
            );
            assert!(!task.download_path.exists());
        }
    }

    #[tokio::test]
    async fn v027_provider_v1_mismatch_cannot_be_overridden_by_legacy_receipt() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("legacy.JPG");
        std::fs::write(&path, b"evil").unwrap();
        let mut task = super::tests::task(dir.path());
        task.checksum = signature(b"good").into();
        let local = crate::download::file::compute_sha256(&path).await.unwrap();
        let receipt = provider_receipt(&task, &path, &local);
        assert!(matches!(
            BaselineMatcher::new(vec![receipt])
                .match_task(dir.path(), &task, &task.download_path)
                .await,
            BaselineMatch::Uncertain(_)
        ));
        assert_eq!(std::fs::read(&path).unwrap(), b"evil");
        assert!(!task.download_path.exists());
    }

    #[tokio::test]
    async fn v027_provider_v1_resolves_conflicting_legacy_hashes_from_complete_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("renamed-raw.JPG");
        std::fs::write(&path, b"good").unwrap();
        let before = std::fs::metadata(&path).unwrap().modified().unwrap();
        let mut task = super::tests::task(dir.path());
        task.checksum = signature(b"good").into();
        let local = crate::download::file::compute_sha256(&path).await.unwrap();
        let mut first = provider_receipt(&task, &path, &local);
        first.library = "PrimarySync".into();
        first.id = "old-master".into();
        let mut contradictory = first.clone();
        contradictory.id = "other-old-master".into();
        contradictory.source_checksum = Some("a".repeat(64));
        let matcher = BaselineMatcher::new(vec![first.clone(), contradictory.clone()]);
        assert!(matches!(
            matcher
                .match_task(dir.path(), &task, &task.download_path)
                .await,
            BaselineMatch::ProviderPresent { .. }
        ));
        // Disagreement still blocks legacy-only proof for opaque signatures.
        task.checksum = "opaque-resource".into();
        first.provider_checksum = task.checksum.to_string();
        contradictory.provider_checksum = task.checksum.to_string();
        assert!(matches!(
            BaselineMatcher::new(vec![first, contradictory])
                .match_task(dir.path(), &task, &task.download_path)
                .await,
            BaselineMatch::Uncertain(_)
        ));
        assert_eq!(std::fs::read(&path).unwrap(), b"good");
        assert_eq!(
            std::fs::metadata(&path).unwrap().modified().unwrap(),
            before
        );
        assert!(!task.download_path.exists());
    }

    #[tokio::test]
    async fn v027_provider_v1_unrecorded_requested_file_requires_full_matching_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let mut task = super::tests::task(dir.path());
        let bytes = vec![41_u8; 65_537];
        task.size = bytes.len() as u64;
        task.checksum = signature(&bytes).into();
        std::fs::write(&task.download_path, &bytes).unwrap();
        assert!(matches!(
            BaselineMatcher::default()
                .match_task(dir.path(), &task, &task.download_path)
                .await,
            BaselineMatch::ProviderPresent { .. }
        ));
        let mut wrong = bytes;
        *wrong.last_mut().unwrap() ^= 1;
        std::fs::write(&task.download_path, &wrong).unwrap();
        assert!(matches!(
            BaselineMatcher::default()
                .match_task(dir.path(), &task, &task.download_path)
                .await,
            BaselineMatch::Uncertain(_)
        ));
        let mut unsupported = data_encoding::BASE64
            .decode(signature(&wrong).as_bytes())
            .unwrap();
        unsupported[0] = 4;
        task.checksum = data_encoding::BASE64.encode(&unsupported).into();
        assert!(matches!(
            BaselineMatcher::default()
                .match_task(dir.path(), &task, &task.download_path)
                .await,
            BaselineMatch::Uncertain(_)
        ));
    }
}
