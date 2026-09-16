//! Durable pending-retry identity resolution and targeted provider revalidation.

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use rustc_hash::{FxHashMap, FxHashSet};
use tokio_util::sync::CancellationToken;

use super::{
    DownloadConfig, DownloadStore, DownloadTask, PENDING_RETRY_UNMATCHED_REASON, RecordedLocalFile,
    RetryTaskKey, UrlRetrySource, build_pass_configs_resolving_deferred_excludes, file, filter,
    pipeline, planner,
};
use crate::icloud::photos::{PhotoAsset, ProviderRecordId, RecordLookupRequest, RecordResolution};
use crate::state::{AssetVerificationState, VersionSizeKey};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) struct PendingRetryTarget {
    pub(super) library: Arc<str>,
    pub(super) asset_id: Arc<str>,
    pub(super) version_size: VersionSizeKey,
}

impl PendingRetryTarget {
    pub(super) fn from_record(record: &crate::state::AssetRecord) -> Self {
        Self {
            library: Arc::clone(&record.library),
            asset_id: Arc::from(record.id.as_ref()),
            version_size: record.version_size,
        }
    }

    pub(super) fn from_task(task: &DownloadTask) -> Self {
        Self {
            library: Arc::clone(&task.library),
            asset_id: Arc::clone(&task.asset_id),
            version_size: task.version_size,
        }
    }
}

#[derive(Debug)]
struct PendingRetryEvidence {
    checksum: Arc<str>,
    filename: Arc<str>,
    local_file: Option<RecordedLocalFile>,
    downloaded_at: Option<chrono::DateTime<chrono::Utc>>,
    size_bytes: u64,
    last_error: Option<Arc<str>>,
}

impl PendingRetryEvidence {
    fn from_record(record: &crate::state::AssetRecord) -> Self {
        Self {
            checksum: Arc::from(record.checksum.as_ref()),
            filename: Arc::from(record.filename.as_ref()),
            local_file: record.local_path.clone().map(|path| RecordedLocalFile {
                path,
                local_checksum: record.local_checksum.as_deref().map(Into::into),
                download_checksum: record.download_checksum.as_deref().map(Into::into),
            }),
            downloaded_at: record.downloaded_at,
            size_bytes: record.size_bytes,
            last_error: record.last_error.as_deref().map(Into::into),
        }
    }

    async fn truncated_repair_fingerprint(
        &self,
        enabled: bool,
    ) -> Result<Option<file::ExistingFileFingerprint>> {
        if !enabled
            || self.last_error.as_deref() != Some(crate::commands::reconcile::FILE_TRUNCATED_REASON)
            || self.downloaded_at.is_none()
        {
            return Ok(None);
        }
        let Some(local_file) = &self.local_file else {
            return Ok(None);
        };
        match tokio::fs::symlink_metadata(&local_file.path).await {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "Could not inspect truncated repair path {}",
                        local_file.path.display()
                    )
                });
            }
        }
        let fingerprint = file::fingerprint_regular_file(&local_file.path).await?;
        if self.size_bytes == 0 || fingerprint.size >= self.size_bytes {
            return Ok(None);
        }

        let metadata_changed_download = matches!(
            (
                local_file.local_checksum.as_deref(),
                local_file.download_checksum.as_deref()
            ),
            (Some(local), Some(download)) if local != download
        );
        if metadata_changed_download {
            let actual_checksum = data_encoding::HEXLOWER.encode(&fingerprint.sha256);
            if local_file.local_checksum.as_deref() == Some(actual_checksum.as_str()) {
                return Ok(None);
            }
        }
        Ok(Some(fingerprint))
    }

    fn matches_provider_version(&self, task: &DownloadTask) -> bool {
        self.checksum.as_ref() == task.checksum.as_ref() && self.size_bytes == task.size
    }

    fn local_path_under<'a>(&'a self, directory: &Path) -> Option<&'a Path> {
        self.local_file
            .as_ref()
            .map(|file| file.path.as_path())
            .filter(|path| path.starts_with(directory))
    }

    fn local_path_evidence_under<'a>(
        &'a self,
        directory: &Path,
    ) -> pipeline::PendingRetryLocalPath<'a> {
        let Some(file) = self
            .local_file
            .as_ref()
            .filter(|file| file.path.starts_with(directory))
        else {
            return pipeline::PendingRetryLocalPath::Unrecorded;
        };
        if self.downloaded_at.is_some() {
            pipeline::PendingRetryLocalPath::Current(file)
        } else {
            pipeline::PendingRetryLocalPath::Historical
        }
    }
}

#[derive(Debug)]
enum LegacyCandidateSelection {
    Selected(PhotoAsset),
    Missing,
    EvidenceMismatch { candidates: usize },
    Ambiguous { matches: usize },
}

fn candidate_matches_durable_evidence(
    asset: &PhotoAsset,
    target: &PendingRetryTarget,
    evidence: &PendingRetryEvidence,
) -> bool {
    asset.versions().iter().any(|(version_size, version)| {
        VersionSizeKey::from(*version_size) == target.version_size
            && version.size == evidence.size_bytes
            && version.checksum.as_ref() == evidence.checksum.as_ref()
    })
}

fn select_legacy_candidate(
    mut candidates: Vec<PhotoAsset>,
    targets: &[&PendingRetryTarget],
    evidence: &FxHashMap<PendingRetryTarget, PendingRetryEvidence>,
    owner_asset_record_name: Option<&str>,
) -> LegacyCandidateSelection {
    if let Some(owner) = owner_asset_record_name {
        candidates.retain(|asset| asset.asset_record_name() == owner);
    }
    if candidates.is_empty() {
        return LegacyCandidateSelection::Missing;
    }

    let candidate_count = candidates.len();
    let mut matching = candidates.into_iter().filter(|asset| {
        targets.iter().any(|target| {
            evidence
                .get(*target)
                .is_some_and(|evidence| candidate_matches_durable_evidence(asset, target, evidence))
        })
    });
    let Some(selected) = matching.next() else {
        return LegacyCandidateSelection::EvidenceMismatch {
            candidates: candidate_count,
        };
    };
    if matching.next().is_none() {
        return LegacyCandidateSelection::Selected(selected);
    }
    LegacyCandidateSelection::Ambiguous {
        matches: 2 + matching.count(),
    }
}

pub(super) fn take_matching_pending_retry_tasks<I>(
    tasks: I,
    pending_targets: &mut FxHashSet<PendingRetryTarget>,
    out: &mut Vec<DownloadTask>,
) where
    I: IntoIterator<Item = DownloadTask>,
{
    for task in tasks {
        let target = PendingRetryTarget::from_task(&task);
        if pending_targets.remove(&target) {
            out.push(task);
            if pending_targets.is_empty() {
                break;
            }
        }
    }
}

struct PendingRetryPlanning<'a> {
    db: &'a dyn DownloadStore,
    pass_configs: &'a [Arc<DownloadConfig>],
    pending_evidence: &'a FxHashMap<PendingRetryTarget, PendingRetryEvidence>,
    pending_targets: &'a mut FxHashSet<PendingRetryTarget>,
    task_planner: &'a mut planner::TaskPlanner,
    tasks: &'a mut Vec<DownloadTask>,
    retry_sources: &'a mut FxHashMap<RetryTaskKey, UrlRetrySource>,
}

impl PendingRetryPlanning<'_> {
    async fn plan_resolved_asset(&mut self, asset: &PhotoAsset, state_id: &str) -> Result<()> {
        let mut malformed_targets = FxHashSet::default();
        let mut state_write_failed_targets = FxHashSet::default();
        let mut filter_reasons = Vec::<filter::FilterReason>::new();
        for (pass_index, pass_config) in self.pass_configs.iter().enumerate() {
            let plan = self
                .task_planner
                .plan_pending_retry_asset(asset, pass_config)
                .await?;
            let targets: Vec<PendingRetryTarget> = self
                .pending_targets
                .iter()
                .filter(|target| target.asset_id.as_ref() == state_id)
                .cloned()
                .collect();
            for target in targets {
                let Some(evidence) = self.pending_evidence.get(&target) else {
                    continue;
                };
                match pipeline::adopt_pending_on_disk_for_retry(
                    self.db,
                    pass_config,
                    asset,
                    self.task_planner,
                    &plan.tasks,
                    pipeline::PendingRetryFileEvidence {
                        version_size: target.version_size,
                        filename: &evidence.filename,
                        checksum: &evidence.checksum,
                        local_path: evidence.local_path_evidence_under(&pass_config.directory),
                        size: evidence.size_bytes,
                    },
                )
                .await
                {
                    pipeline::PendingRetryAdoption::Adopted => {
                        self.pending_targets.remove(&target);
                        self.db
                            .clear_asset_verification(
                                &target.library,
                                &target.asset_id,
                                target.version_size.as_str(),
                            )
                            .await?;
                    }
                    pipeline::PendingRetryAdoption::StateWriteFailed => {
                        state_write_failed_targets.insert(target);
                    }
                    pipeline::PendingRetryAdoption::NotFound => {}
                }
            }
            if let Some(reason) = plan.filter_reason {
                if !filter_reasons.contains(&reason) {
                    filter_reasons.push(reason);
                }
                continue;
            }
            if plan.malformed_resource.is_some() {
                malformed_targets.extend(
                    self.pending_targets
                        .iter()
                        .filter(|target| target.asset_id.as_ref() == state_id)
                        .cloned(),
                );
            }
            let mut retry_tasks = Vec::with_capacity(plan.tasks.len());
            for mut task in plan.tasks.into_iter().filter(|task| {
                !state_write_failed_targets.contains(&PendingRetryTarget::from_task(task))
            }) {
                let target = PendingRetryTarget::from_task(&task);
                if let Some(evidence) = self.pending_evidence.get(&target)
                    && let Some(local_path) = evidence.local_path_under(&pass_config.directory)
                    && self.task_planner.retry_path_allowed(
                        &task.library,
                        &task.asset_id,
                        task.version_size,
                        local_path,
                    )
                {
                    if evidence.matches_provider_version(&task)
                        && let Some(fingerprint) = evidence
                            .truncated_repair_fingerprint(pass_config.repair_truncated)
                            .await?
                    {
                        if !self
                            .task_planner
                            .claim_recorded_repair_path(local_path, &task)
                            .await
                        {
                            tracing::warn!(
                                asset_id = %task.asset_id,
                                version_size = %task.version_size.as_str(),
                                path = %local_path.display(),
                                "Could not reserve the recorded truncated path; retaining pending work"
                            );
                            continue;
                        }
                        task.download_path = local_path.to_path_buf();
                        task.publication = file::FinalPublication::ReplaceTruncated(fingerprint);
                    } else {
                        let Some(retry_path) = self
                            .task_planner
                            .resolve_recorded_retry_path(local_path, &task)
                            .await
                        else {
                            tracing::warn!(
                                asset_id = %task.asset_id,
                                version_size = %task.version_size.as_str(),
                                path = %local_path.display(),
                                "Could not choose a safe sibling for the recorded retry path; retaining pending work"
                            );
                            continue;
                        };
                        task.download_path = retry_path;
                    }
                }
                self.task_planner.retain_retry_claim(&task)?;
                retry_tasks.push(task);
            }
            let queued_targets: Vec<PendingRetryTarget> = retry_tasks
                .iter()
                .map(PendingRetryTarget::from_task)
                .filter(|target| self.pending_targets.contains(target))
                .collect();
            let first_new_task = self.tasks.len();
            take_matching_pending_retry_tasks(retry_tasks, self.pending_targets, self.tasks);
            for target in queued_targets {
                if !self.pending_targets.contains(&target) {
                    self.db
                        .clear_asset_verification(
                            &target.library,
                            &target.asset_id,
                            target.version_size.as_str(),
                        )
                        .await?;
                }
            }
            for task in self.tasks.iter().skip(first_new_task) {
                self.retry_sources.insert(
                    RetryTaskKey::from(task),
                    UrlRetrySource {
                        asset_record_name: asset.asset_record_name_arc(),
                        pass_index,
                    },
                );
            }
        }

        let deferred_targets: Vec<PendingRetryTarget> = self
            .pending_targets
            .iter()
            .filter(|target| target.asset_id.as_ref() == state_id)
            .filter(|target| {
                !malformed_targets.contains(*target)
                    && !state_write_failed_targets.contains(*target)
            })
            .cloned()
            .collect();
        for target in deferred_targets {
            let transitioned = self
                .db
                .mark_policy_excluded(
                    &target.library,
                    &target.asset_id,
                    target.version_size.as_str(),
                )
                .await?;
            if transitioned {
                self.pending_targets.remove(&target);
            }
            tracing::info!(
                library = %target.library,
                asset_id = %target.asset_id,
                version_size = target.version_size.as_str(),
                filter_reasons = ?filter_reasons,
                transitioned,
                "Pending asset excluded: current sync policy did not produce a retry task"
            );
        }
        for target in state_write_failed_targets {
            if !self.pending_targets.contains(&target) {
                continue;
            }
            self.db
                .set_asset_verification(
                    &target.library,
                    &target.asset_id,
                    target.version_size.as_str(),
                    AssetVerificationState::TransientFailure,
                    "failed to persist on-disk pending asset adoption",
                )
                .await?;
        }
        for target in malformed_targets {
            if !self.pending_targets.contains(&target) {
                continue;
            }
            self.db
                .set_asset_verification(
                    &target.library,
                    &target.asset_id,
                    target.version_size.as_str(),
                    AssetVerificationState::Unknown,
                    "provider record did not contain a usable retry resource",
                )
                .await?;
        }

        Ok(())
    }
}

async fn set_verification_for_state_id(
    db: &dyn DownloadStore,
    pending_targets: &FxHashSet<PendingRetryTarget>,
    state_id: &str,
    state: AssetVerificationState,
    reason: &str,
) -> Result<()> {
    for target in pending_targets
        .iter()
        .filter(|target| target.asset_id.as_ref() == state_id)
    {
        db.set_asset_verification(
            &target.library,
            &target.asset_id,
            target.version_size.as_str(),
            state,
            reason,
        )
        .await?;
    }
    Ok(())
}

#[derive(Debug, Default)]
pub(super) struct PendingRetryPlan {
    pub(super) tasks: Vec<DownloadTask>,
    pub(super) retry_sources: FxHashMap<RetryTaskKey, UrlRetrySource>,
    pub(super) pass_configs: Vec<Arc<DownloadConfig>>,
    pub(super) unmatched_targets: Vec<PendingRetryTarget>,
    pub(super) requested: usize,
}

struct ProviderLookupPlan {
    requests: Vec<RecordLookupRequest>,
    master_by_state_id: FxHashMap<String, String>,
}

async fn build_provider_lookup_plan(
    db: &dyn DownloadStore,
    library: &str,
    state_ids: &[&str],
) -> Result<ProviderLookupPlan> {
    let mut requests = Vec::new();
    let mut seen_requests = FxHashSet::default();
    let mut master_by_state_id = FxHashMap::default();
    for &state_id in state_ids {
        let mapped_master = db
            .get_master_record_name_for_asset(library, state_id)
            .await?;
        let master = mapped_master.as_deref().unwrap_or(state_id).to_string();
        let asset_record_names = if mapped_master.is_some() {
            vec![state_id.to_string()]
        } else {
            db.get_asset_record_names_for_master(library, &master)
                .await?
        };
        master_by_state_id.insert(state_id.to_string(), master.clone());
        if asset_record_names.is_empty() {
            let request_key = (state_id.to_string(), master.clone(), None);
            if seen_requests.insert(request_key.clone()) {
                requests.push(RecordLookupRequest::master_only(
                    ProviderRecordId::new(request_key.0),
                    ProviderRecordId::new(request_key.1),
                ));
            }
            continue;
        }
        for asset_record_name in asset_record_names {
            let request_key = (
                state_id.to_string(),
                master.clone(),
                Some(asset_record_name.clone()),
            );
            if seen_requests.insert(request_key.clone()) {
                requests.push(RecordLookupRequest::paired(
                    ProviderRecordId::new(request_key.0),
                    ProviderRecordId::new(request_key.1),
                    ProviderRecordId::new(asset_record_name),
                ));
            }
        }
    }

    Ok(ProviderLookupPlan {
        requests,
        master_by_state_id,
    })
}

async fn apply_policy_excluded_resolutions(
    db: &dyn DownloadStore,
    library: &str,
    master_by_state_id: &FxHashMap<String, String>,
    resolutions: Vec<(ProviderRecordId, RecordResolution)>,
    shutdown_token: &CancellationToken,
) -> Result<usize> {
    let mut source_deleted = 0usize;
    for (state_id, resolution) in resolutions {
        if shutdown_token.is_cancelled() {
            break;
        }
        // CONTRACT: POLICY_EXCLUDED_REQUIRES_EXPLICIT_SOURCE_DELETION
        if let RecordResolution::Deleted {
            deleted_at,
            master_family,
        } = resolution
        {
            let state_id = state_id.as_str();
            let resolved = if master_family {
                let master = master_by_state_id
                    .get(state_id)
                    .map(String::as_str)
                    .unwrap_or(state_id);
                db.resolve_master_family_source_deleted_affected(library, master, deleted_at)
                    .await?
            } else {
                db.resolve_source_deleted_affected(library, state_id, deleted_at)
                    .await?
            };
            source_deleted = source_deleted.saturating_add(resolved);
        }
    }
    Ok(source_deleted)
}

async fn revalidate_policy_excluded_assets(
    passes: &[crate::commands::AlbumPass],
    config: &DownloadConfig,
    shutdown_token: &CancellationToken,
) -> Result<()> {
    let Some(db) = &config.state_db else {
        return Ok(());
    };
    let state_ids = db
        .get_policy_excluded_ids_for_revalidation(&config.library)
        .await?;
    if state_ids.is_empty() || shutdown_token.is_cancelled() {
        return Ok(());
    }
    let Some(pass) = passes.first() else {
        return Ok(());
    };
    let state_id_refs: Vec<&str> = state_ids.iter().map(String::as_str).collect();
    let ProviderLookupPlan {
        requests,
        master_by_state_id,
    } = build_provider_lookup_plan(db.as_ref(), &config.library, &state_id_refs).await?;

    let requested = state_ids.len();
    let batch = pass.album.resolve_records(&requests).await;
    let complete = batch.complete;
    let source_deleted = apply_policy_excluded_resolutions(
        db.as_ref(),
        &config.library,
        &master_by_state_id,
        batch.results,
        shutdown_token,
    )
    .await?;
    if source_deleted > 0 {
        tracing::info!(
            library = %config.library,
            requested,
            source_deleted,
            complete,
            "Provider deletion superseded policy exclusion"
        );
    } else {
        tracing::debug!(
            library = %config.library,
            requested,
            complete,
            "Policy-excluded provider revalidation retained current state"
        );
    }
    Ok(())
}

pub(super) async fn build_pending_retry_download_tasks(
    passes: &[crate::commands::AlbumPass],
    config: &DownloadConfig,
    shutdown_token: CancellationToken,
) -> Result<PendingRetryPlan> {
    let Some(db) = &config.state_db else {
        return Ok(PendingRetryPlan::default());
    };

    revalidate_policy_excluded_assets(passes, config, &shutdown_token).await?;

    let pending = db.get_pending().await?;
    let mut pending_targets: FxHashSet<PendingRetryTarget> = pending
        .iter()
        .filter(|record| record.library.as_ref() == config.library.as_ref())
        .map(PendingRetryTarget::from_record)
        .collect();
    if pending_targets.is_empty() {
        return Ok(PendingRetryPlan::default());
    }
    let pending_evidence: FxHashMap<PendingRetryTarget, PendingRetryEvidence> = pending
        .iter()
        .filter(|record| record.library.as_ref() == config.library.as_ref())
        .map(|record| {
            (
                PendingRetryTarget::from_record(record),
                PendingRetryEvidence::from_record(record),
            )
        })
        .collect();
    let legacy_master_state_owners: FxHashMap<String, String> = db
        .get_legacy_master_state_owners()
        .await?
        .into_iter()
        .filter(|(library, _, _)| library == config.library.as_ref())
        .map(|(_, master_record_name, asset_record_name)| (master_record_name, asset_record_name))
        .collect();

    let backfilled = db
        .backfill_asset_master_mappings_from_album_memberships()
        .await?;
    if backfilled > 0 {
        tracing::info!(
            inserted = backfilled,
            library = %config.library,
            "Backfilled asset/master mappings before pending retry"
        );
    }

    let requested = pending_targets.len();
    let pass_configs = build_pass_configs_resolving_deferred_excludes(passes, config).await?;
    let mut tasks: Vec<DownloadTask> = Vec::with_capacity(requested);
    let mut retry_sources: FxHashMap<RetryTaskKey, UrlRetrySource> = FxHashMap::default();
    let mut task_planner = planner::TaskPlanner::for_pending_retry(db.as_ref()).await?;
    let pending_state_ids: Vec<&str> = pending
        .iter()
        .filter(|record| record.library.as_ref() == config.library.as_ref())
        .map(|record| record.id.as_ref())
        .collect();
    let ProviderLookupPlan {
        requests: lookup_requests,
        master_by_state_id,
    } = build_provider_lookup_plan(db.as_ref(), &config.library, &pending_state_ids).await?;

    let requested_state_ids: FxHashSet<&str> = lookup_requests
        .iter()
        .map(|request| request.state_id.as_str())
        .collect();
    for target in &pending_targets {
        if !requested_state_ids.contains(target.asset_id.as_ref()) {
            db.set_asset_verification(
                &target.library,
                &target.asset_id,
                target.version_size.as_str(),
                AssetVerificationState::Unknown,
                "stable provider asset/master mapping is unavailable",
            )
            .await?;
        }
    }

    let resolutions = if let Some(pass) = passes.first() {
        let batch = pass.album.resolve_records(&lookup_requests).await;
        if !batch.complete {
            tracing::warn!(
                library = %config.library,
                requested = lookup_requests.len(),
                "Pending provider revalidation completed with inconclusive results"
            );
        }
        batch.results
    } else {
        Vec::new()
    };
    let mut legacy_present_state_ids = FxHashSet::default();
    for (state_id, resolution) in resolutions {
        if pending_targets.is_empty() || shutdown_token.is_cancelled() {
            break;
        }
        match resolution {
            RecordResolution::Present(asset) => {
                PendingRetryPlanning {
                    db: db.as_ref(),
                    pass_configs: &pass_configs,
                    pending_evidence: &pending_evidence,
                    pending_targets: &mut pending_targets,
                    task_planner: &mut task_planner,
                    tasks: &mut tasks,
                    retry_sources: &mut retry_sources,
                }
                .plan_resolved_asset(&asset, state_id.as_str())
                .await?;
            }
            RecordResolution::MasterPresent => {
                legacy_present_state_ids.insert(state_id.as_str().to_string());
            }
            RecordResolution::Deleted {
                deleted_at,
                master_family,
            } => {
                let state_id = state_id.as_str();
                let resolved = if master_family {
                    let master = master_by_state_id
                        .get(state_id)
                        .map(String::as_str)
                        .unwrap_or(state_id);
                    db.resolve_master_family_source_deleted_affected(
                        &config.library,
                        master,
                        deleted_at,
                    )
                    .await?
                } else {
                    db.resolve_source_deleted_affected(&config.library, state_id, deleted_at)
                        .await?
                };
                tracing::info!(
                    library = %config.library,
                    state_id,
                    resolved,
                    master_family,
                    "Pending asset cleared: provider confirmed source deletion"
                );
            }
            RecordResolution::AssetPresent { .. } | RecordResolution::Unknown => {
                // CONTRACT: UNKNOWN_PROVIDER_IDENTITY_REMAINS_PENDING
                set_verification_for_state_id(
                    db.as_ref(),
                    &pending_targets,
                    state_id.as_str(),
                    AssetVerificationState::Unknown,
                    "provider lookup omitted or could not parse the requested record",
                )
                .await?;
                tracing::warn!(
                    library = %config.library,
                    state_id = state_id.as_str(),
                    "Pending asset retained: provider lookup was inconclusive"
                );
            }
            RecordResolution::TransientFailure(error) => {
                set_verification_for_state_id(
                    db.as_ref(),
                    &pending_targets,
                    state_id.as_str(),
                    AssetVerificationState::TransientFailure,
                    &error.to_string(),
                )
                .await?;
                tracing::warn!(
                    library = %config.library,
                    state_id = state_id.as_str(),
                    error = %error,
                    "Pending asset retained: provider lookup failed transiently"
                );
            }
        }
    }

    if !legacy_present_state_ids.is_empty() && !shutdown_token.is_cancelled() {
        tracing::info!(
            library = %config.library,
            masters = legacy_present_state_ids.len(),
            "Hydrating missing CPLAsset identities for live legacy pending masters"
        );
        let (hydrated, hydration_failed) = match passes.first() {
            Some(pass) => match pass
                .album
                .hydrate_matching_master_assets_from_changes(
                    &legacy_present_state_ids,
                    &shutdown_token,
                )
                .await
            {
                Ok(assets) => (assets, false),
                Err(error) => {
                    let reason = error.to_string();
                    for state_id in &legacy_present_state_ids {
                        set_verification_for_state_id(
                            db.as_ref(),
                            &pending_targets,
                            state_id,
                            AssetVerificationState::TransientFailure,
                            &reason,
                        )
                        .await?;
                    }
                    tracing::warn!(
                        library = %config.library,
                        error = %error,
                        "Pending legacy asset hydration failed transiently"
                    );
                    (Vec::new(), true)
                }
            },
            None => (Vec::new(), false),
        };
        let mut candidates_by_master: FxHashMap<String, Vec<PhotoAsset>> = FxHashMap::default();
        for asset in hydrated {
            candidates_by_master
                .entry(asset.id().to_string())
                .or_default()
                .push(asset);
        }

        for state_id in legacy_present_state_ids {
            if shutdown_token.is_cancelled() {
                break;
            }
            if hydration_failed {
                continue;
            }
            let matching_targets: Vec<&PendingRetryTarget> = pending_targets
                .iter()
                .filter(|target| target.asset_id.as_ref() == state_id)
                .collect();
            let candidates = candidates_by_master.remove(&state_id).unwrap_or_default();
            let persisted_owner = legacy_master_state_owners
                .get(&state_id)
                .map(String::as_str);
            match select_legacy_candidate(
                candidates,
                &matching_targets,
                &pending_evidence,
                persisted_owner,
            ) {
                LegacyCandidateSelection::Selected(asset) => {
                    if persisted_owner.is_none()
                        && !db
                            .claim_legacy_master_state_owner(
                                &config.library,
                                asset.id(),
                                asset.asset_record_name(),
                            )
                            .await?
                    {
                        set_verification_for_state_id(
                            db.as_ref(),
                            &pending_targets,
                            &state_id,
                            AssetVerificationState::Unknown,
                            "a different provider asset claimed the legacy master state",
                        )
                        .await?;
                        tracing::warn!(
                            library = %config.library,
                            state_id,
                            asset_record_name = %asset.asset_record_name(),
                            "Pending asset retained: legacy master owner changed concurrently"
                        );
                        continue;
                    }
                    db.upsert_asset_master_mapping(
                        &config.library,
                        asset.asset_record_name(),
                        asset.id(),
                    )
                    .await?;
                    tracing::info!(
                        library = %config.library,
                        state_id,
                        asset_record_name = %asset.asset_record_name(),
                        "Recovered legacy pending asset/master mapping"
                    );
                    let asset = asset.with_state_record_name(Arc::from(state_id.as_str()));
                    PendingRetryPlanning {
                        db: db.as_ref(),
                        pass_configs: &pass_configs,
                        pending_evidence: &pending_evidence,
                        pending_targets: &mut pending_targets,
                        task_planner: &mut task_planner,
                        tasks: &mut tasks,
                        retry_sources: &mut retry_sources,
                    }
                    .plan_resolved_asset(&asset, &state_id)
                    .await?;
                }
                LegacyCandidateSelection::Missing => {
                    set_verification_for_state_id(
                        db.as_ref(),
                        &pending_targets,
                        &state_id,
                        AssetVerificationState::Unknown,
                        "provider confirmed the master exists but no current CPLAsset pair was found",
                    )
                    .await?;
                    tracing::warn!(
                        library = %config.library,
                        state_id,
                        "Pending asset retained: live master had no current CPLAsset pair"
                    );
                }
                LegacyCandidateSelection::EvidenceMismatch { candidates } => {
                    set_verification_for_state_id(
                        db.as_ref(),
                        &pending_targets,
                        &state_id,
                        AssetVerificationState::Unknown,
                        "no current provider asset matched the pending version, size, and checksum",
                    )
                    .await?;
                    tracing::warn!(
                        library = %config.library,
                        state_id,
                        candidates,
                        "Pending asset retained: current CPLAsset records did not match durable evidence"
                    );
                }
                LegacyCandidateSelection::Ambiguous { matches } => {
                    set_verification_for_state_id(
                        db.as_ref(),
                        &pending_targets,
                        &state_id,
                        AssetVerificationState::Unknown,
                        "multiple provider asset records matched the legacy master",
                    )
                    .await?;
                    tracing::warn!(
                        library = %config.library,
                        state_id,
                        matches,
                        "Pending asset retained: legacy master resolved to ambiguous CPLAsset siblings"
                    );
                }
            }
        }
    }

    // Explicit deletion tombstones leave catalog history in place but remove
    // those rows from the actionable pending reader. Present rows remain
    // actionable until their retry succeeds.
    let still_pending: FxHashSet<PendingRetryTarget> = db
        .get_pending()
        .await?
        .iter()
        .filter(|record| record.library.as_ref() == config.library.as_ref())
        .map(PendingRetryTarget::from_record)
        .collect();
    pending_targets.retain(|target| still_pending.contains(target));

    if !pending_targets.is_empty() {
        tracing::warn!(
            requested,
            refreshed = tasks.len(),
            missing = pending_targets.len(),
            diagnostic = PENDING_RETRY_UNMATCHED_REASON,
            "Targeted retry could not refresh every pending asset; retaining durable retry work"
        );
    }

    Ok(PendingRetryPlan {
        tasks,
        retry_sources,
        pass_configs,
        unmatched_targets: pending_targets.into_iter().collect(),
        requested,
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::test_helpers::TestAssetRecord;

    fn candidate(master: &str, asset: &str, checksum: &str, size: u64) -> PhotoAsset {
        PhotoAsset::new(
            json!({
                "recordName": master,
                "recordType": "CPLMaster",
                "fields": {
                    "filenameEnc": {"value": "legacy.jpg", "type": "STRING"},
                    "itemType": {"value": "public.jpeg"},
                    "resOriginalFileType": {"value": "public.jpeg"},
                    "resOriginalRes": {"value": {
                        "downloadURL": "https://p01.icloud-content.com/legacy.jpg",
                        "fileChecksum": checksum,
                        "size": size,
                    }},
                },
            }),
            json!({
                "recordName": asset,
                "recordType": "CPLAsset",
                "fields": {
                    "masterRef": {"value": {"recordName": master}},
                    "assetDate": {"value": 1700000000000i64},
                },
            }),
        )
    }

    #[tokio::test]
    async fn contract_policy_excluded_requires_explicit_source_deletion() {
        let db = crate::state::SqliteStateDb::open_in_memory().unwrap();
        for id in ["PRESENT", "OMITTED", "MALFORMED", "TRANSIENT", "DELETED"] {
            let record = TestAssetRecord::new(id).build();
            db.upsert_seen(&record).await.unwrap();
            assert!(
                db.mark_policy_excluded("PrimarySync", id, "original")
                    .await
                    .unwrap()
            );
        }
        let resolutions = vec![
            (
                ProviderRecordId::new("PRESENT"),
                RecordResolution::Present(candidate(
                    "PRESENT",
                    "asset-PRESENT",
                    "present-checksum",
                    100,
                )),
            ),
            (ProviderRecordId::new("OMITTED"), RecordResolution::Unknown),
            (
                ProviderRecordId::new("MALFORMED"),
                RecordResolution::TransientFailure(
                    crate::icloud::photos::ProviderLookupError::Malformed(
                        "missing records array".to_string(),
                    ),
                ),
            ),
            (
                ProviderRecordId::new("TRANSIENT"),
                RecordResolution::TransientFailure(
                    crate::icloud::photos::ProviderLookupError::Request(
                        "temporary lookup failure".to_string(),
                    ),
                ),
            ),
            (
                ProviderRecordId::new("DELETED"),
                RecordResolution::Deleted {
                    deleted_at: None,
                    master_family: false,
                },
            ),
        ];

        let source_deleted = apply_policy_excluded_resolutions(
            &db,
            "PrimarySync",
            &FxHashMap::default(),
            resolutions,
            &CancellationToken::new(),
        )
        .await
        .unwrap();

        assert_eq!(source_deleted, 1);
        let summary = db.get_summary().await.unwrap();
        assert_eq!(summary.policy_excluded, 4);
        assert_eq!(summary.source_deleted, 1);
    }

    #[test]
    fn legacy_candidate_selection_uses_unique_durable_fingerprint() {
        let record = TestAssetRecord::new("legacy-master")
            .checksum("checksum-b")
            .size(200)
            .build();
        let target = PendingRetryTarget::from_record(&record);
        let evidence =
            FxHashMap::from_iter([(target.clone(), PendingRetryEvidence::from_record(&record))]);
        let selection = select_legacy_candidate(
            vec![
                candidate("legacy-master", "asset-a", "checksum-a", 100),
                candidate("legacy-master", "asset-b", "checksum-b", 200),
            ],
            &[&target],
            &evidence,
            None,
        );

        let LegacyCandidateSelection::Selected(selected) = selection else {
            panic!("unique durable fingerprint should select one sibling");
        };
        assert_eq!(selected.asset_record_name(), "asset-b");
    }

    #[test]
    fn legacy_candidate_selection_rejects_candidates_without_durable_match() {
        let record = TestAssetRecord::new("legacy-master")
            .checksum("missing-checksum")
            .size(300)
            .build();
        let target = PendingRetryTarget::from_record(&record);
        let evidence =
            FxHashMap::from_iter([(target.clone(), PendingRetryEvidence::from_record(&record))]);
        let selection = select_legacy_candidate(
            vec![
                candidate("legacy-master", "asset-a", "checksum-a", 100),
                candidate("legacy-master", "asset-b", "checksum-b", 200),
            ],
            &[&target],
            &evidence,
            None,
        );

        assert!(matches!(
            selection,
            LegacyCandidateSelection::EvidenceMismatch { candidates: 2 }
        ));
    }

    #[test]
    fn legacy_candidate_selection_rejects_single_candidate_without_durable_match() {
        let record = TestAssetRecord::new("legacy-master")
            .checksum("pending-checksum")
            .size(300)
            .build();
        let target = PendingRetryTarget::from_record(&record);
        let evidence =
            FxHashMap::from_iter([(target.clone(), PendingRetryEvidence::from_record(&record))]);

        let selection = select_legacy_candidate(
            vec![candidate(
                "legacy-master",
                "asset-current",
                "current-checksum",
                200,
            )],
            &[&target],
            &evidence,
            None,
        );

        assert!(matches!(
            selection,
            LegacyCandidateSelection::EvidenceMismatch { candidates: 1 }
        ));
    }

    #[test]
    fn legacy_candidate_selection_retains_multiple_durable_matches() {
        let record = TestAssetRecord::new("legacy-master")
            .checksum("shared-checksum")
            .size(300)
            .build();
        let target = PendingRetryTarget::from_record(&record);
        let evidence =
            FxHashMap::from_iter([(target.clone(), PendingRetryEvidence::from_record(&record))]);

        let selection = select_legacy_candidate(
            vec![
                candidate("legacy-master", "asset-a", "shared-checksum", 300),
                candidate("legacy-master", "asset-b", "shared-checksum", 300),
            ],
            &[&target],
            &evidence,
            None,
        );

        assert!(matches!(
            selection,
            LegacyCandidateSelection::Ambiguous { matches: 2 }
        ));
    }

    #[test]
    fn legacy_candidate_selection_uses_persisted_owner_to_resolve_matching_siblings() {
        let record = TestAssetRecord::new("legacy-master")
            .checksum("shared-checksum")
            .size(300)
            .build();
        let target = PendingRetryTarget::from_record(&record);
        let evidence =
            FxHashMap::from_iter([(target.clone(), PendingRetryEvidence::from_record(&record))]);

        let selection = select_legacy_candidate(
            vec![
                candidate("legacy-master", "asset-a", "shared-checksum", 300),
                candidate("legacy-master", "asset-b", "shared-checksum", 300),
            ],
            &[&target],
            &evidence,
            Some("asset-b"),
        );

        let LegacyCandidateSelection::Selected(selected) = selection else {
            panic!("persisted owner should resolve matching siblings");
        };
        assert_eq!(selected.asset_record_name(), "asset-b");
    }

    #[tokio::test]
    async fn truncated_repair_requires_marker_and_rejects_intact_metadata_rewrite() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("photo.jpg");
        tokio::fs::write(&path, b"short metadata bytes")
            .await
            .unwrap();
        let actual_checksum = file::compute_sha256(&path).await.unwrap();
        let mut evidence = PendingRetryEvidence {
            checksum: Arc::from("provider-checksum"),
            filename: Arc::from("photo.jpg"),
            local_file: Some(RecordedLocalFile {
                path,
                local_checksum: Some(actual_checksum.into()),
                download_checksum: Some("pre-metadata-checksum".into()),
            }),
            downloaded_at: Some(chrono::Utc::now()),
            size_bytes: 100,
            last_error: Some(Arc::from(crate::commands::reconcile::FILE_TRUNCATED_REASON)),
        };

        assert!(
            evidence
                .truncated_repair_fingerprint(true)
                .await
                .unwrap()
                .is_none()
        );

        evidence.local_file.as_mut().unwrap().local_checksum =
            Some("expected-intact-checksum".into());
        evidence.last_error = None;
        assert!(
            evidence
                .truncated_repair_fingerprint(true)
                .await
                .unwrap()
                .is_none()
        );

        evidence.last_error = Some(Arc::from(crate::commands::reconcile::FILE_TRUNCATED_REASON));
        assert!(
            evidence
                .truncated_repair_fingerprint(true)
                .await
                .unwrap()
                .is_some()
        );
    }
}
