//! Private selection generation composition using existing selection and queues.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use anyhow::{Context, Result};
use reqwest::Client;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;

use super::config::{DownloadConfig, hash_download_config};
use super::delta::IncrementalDeltaSummary;
use super::models::{
    DownloadControls, DownloadOutcome, SyncResult, SyncStats,
    block_sync_token_for_incremental_delta, merge_download_outcomes,
};
use crate::commands::{AlbumPass, PassKind};
use crate::download::filter;
use crate::download::planner::{AssetTaskPlan, TaskPlanner};
use crate::icloud::photos::{
    PhotoAlbum, PhotoAsset, current_asset, inbox::ShadowCapture, same_selected_facts,
};
use crate::state::db::provider_generations::{
    ActiveDecision, ActiveGeneration, GenerationSpec, MAX_GENERATION_BYTES,
};
use crate::state::db::provider_selection::{
    SelectionDecision, SelectionDestination, SelectionOutcome, SelectionPath,
};

const MAX_REPLAY_DECISIONS: u32 = 64;
const DEFERRED_REASON: &str = "selection_generation_deferred";

#[derive(Debug, thiserror::Error)]
#[error("Selected identity retry is not due")]
pub(in crate::download) struct SelectionRetryDeferred;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::download) enum SelectionConfirmationErrorClass {
    SessionExpired,
    RetryDeferred,
    Refused,
}

/// Own typed confirmation classification before producer state mutation.
/// An existing retry deadline must not be rescheduled by another observation.
pub(in crate::download) fn classify_selection_confirmation_error(
    error: &anyhow::Error,
) -> SelectionConfirmationErrorClass {
    if crate::icloud::photos::session::is_session_error(error) {
        SelectionConfirmationErrorClass::SessionExpired
    } else if error.downcast_ref::<SelectionRetryDeferred>().is_some() {
        SelectionConfirmationErrorClass::RetryDeferred
    } else {
        SelectionConfirmationErrorClass::Refused
    }
}

fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

pub(crate) fn pass_key(pass: &AlbumPass) -> String {
    let mut excludes: Vec<_> = pass.exclude_ids.iter().collect();
    excludes.sort_unstable();
    digest(json!({"kind":format!("{:?}",pass.kind),"scope":pass.album.selection_scope(),"excludes":excludes}).to_string().as_bytes())
}

type SelectionRetrySources =
    rustc_hash::FxHashMap<super::url_refresh::RetryTaskKey, super::url_refresh::UrlRetrySource>;

#[derive(Clone)]
pub(crate) struct SelectionContext {
    capture: ShadowCapture,
    scope: String,
    zone: Value,
    config_hash: String,
    profile: Value,
    retry_sources: Arc<std::sync::Mutex<SelectionRetrySources>>,
}

/// Preserve already admitted writer dependencies before the next sync can
/// apply source relations or ordinary metadata changes. This does not create
/// or seal a generation and applies even when current selection is unsupported.
pub(super) async fn freeze_before_sync(
    passes: &[AlbumPass],
    config: &DownloadConfig,
    controls: DownloadControls,
) -> Result<()> {
    if !controls.run_mode.downloads_files() {
        return Ok(());
    }
    let Some(store) = &config.state_db else {
        return Ok(());
    };
    let mut scopes = std::collections::HashSet::new();
    for pass in passes {
        let Some((capture, scope, _zone)) = pass.album.owned_private_scope()? else {
            continue;
        };
        let concrete = Arc::clone(&capture.db) as Arc<dyn super::models::DownloadStore>;
        anyhow::ensure!(
            Arc::ptr_eq(store, &concrete),
            "Selection database context mismatch"
        );
        if scopes.insert(scope.clone()) {
            capture
                .db
                .freeze_interrupted_selection(capture.owner, scope)
                .await?;
        }
    }
    Ok(())
}

/// Existing selected obligations remain a checkpoint veto when a later
/// selector uses the legacy dispatcher. Eligibility changes cannot retire debt.
pub(super) async fn hold_retained_debt(
    passes: &[AlbumPass],
    config: &DownloadConfig,
    controls: DownloadControls,
    result: &mut SyncResult,
) -> Result<()> {
    if !controls.run_mode.downloads_files() {
        return Ok(());
    }
    let Some(store) = &config.state_db else {
        return Ok(());
    };
    let mut scopes = std::collections::HashSet::new();
    for pass in passes {
        let Some((capture, scope, _zone)) = pass.album.owned_private_scope()? else {
            continue;
        };
        let concrete = Arc::clone(&capture.db) as Arc<dyn super::models::DownloadStore>;
        anyhow::ensure!(
            Arc::ptr_eq(store, &concrete),
            "Selection database context mismatch"
        );
        if scopes.insert(scope.clone())
            && capture
                .db
                .unfinished_selection_debt(capture.owner, scope)
                .await?
        {
            result.block_incremental_token(DEFERRED_REASON);
            result.sync_token = None;
        }
    }
    Ok(())
}

pub(crate) async fn context(
    passes: &[AlbumPass],
    config: &DownloadConfig,
    controls: DownloadControls,
) -> Result<Option<SelectionContext>> {
    if !controls.run_mode.downloads_files()
        || config.retry_only
        || passes.is_empty()
        || matches!(
            config.capture_timestamp_repair,
            crate::download::metadata_rewrite::CaptureTimestampRepair::ReplaceWithCaptureLocal
        )
    {
        return Ok(None);
    }
    let mut qualified = None;
    let mut profile = Vec::new();
    for pass in passes {
        if !matches!(pass.kind, PassKind::Album | PassKind::Unfiled) {
            return Ok(None);
        }
        let Some((capture, scope, zone)) = pass.album.private_selection_scope()? else {
            return Ok(None);
        };
        if zone.get("zoneName").and_then(Value::as_str) != Some(config.library.as_ref()) {
            return Ok(None);
        }
        let Some(store) = &config.state_db else {
            return Ok(None);
        };
        let concrete = Arc::clone(&capture.db) as Arc<dyn super::models::DownloadStore>;
        anyhow::ensure!(
            Arc::ptr_eq(store, &concrete),
            "Selection database context mismatch"
        );
        if let Some((previous, previous_scope, previous_zone)) = &qualified {
            let previous: &ShadowCapture = previous;
            if previous.owner != capture.owner
                || !Arc::ptr_eq(&previous.db, &capture.db)
                || previous_scope != &scope
                || previous_zone != &zone
            {
                return Ok(None);
            }
        } else {
            qualified = Some((capture, scope, zone));
        }
        let mut excludes: Vec<_> = pass.exclude_ids.iter().collect();
        excludes.sort_unstable();
        profile.push(json!({"key":pass_key(pass),"kind":format!("{:?}",pass.kind),"scope":pass.album.selection_scope(),"excludes":excludes}));
    }
    let Some((capture, scope, zone)) = qualified else {
        return Ok(None);
    };
    let mut excludes: Vec<_> = config.exclude_asset_ids.iter().collect();
    excludes.sort_unstable();
    let profile = json!({"format":1,"coverage":"observed_selection_window","passes":profile,
        "recent":config.recent,"recent_scope":format!("{:?}",config.recent_scope),"excludes":excludes});
    let config_hash=digest(serde_json::to_vec(&json!({"format":1,"profile":profile,"scope":scope,"zone":zone,
        "paths":hash_download_config(config),"anchored_root":SelectionPath::from_path(&std::path::absolute(&config.directory)?),
        "before":config.skip_created_before.map(crate::config::CreatedDateFilter::fingerprint),"after":config.skip_created_after.map(crate::config::CreatedDateFilter::fingerprint),
        "enum_hash":config.enum_config_hash.as_deref(),"metadata_flags":crate::download::pipeline::MetadataFlags::from(config).bits(),
        "capture_revision":crate::state::METADATA_CAPTURE_REVISION,"capture_repair":format!("{:?}",config.capture_timestamp_repair),
        "repair_truncated":config.repair_truncated,"attempt_limit":config.max_download_attempts,
    }))?.as_slice());
    Ok(Some(SelectionContext {
        capture,
        scope,
        zone,
        config_hash,
        profile,
        retry_sources: Arc::new(std::sync::Mutex::new(rustc_hash::FxHashMap::default())),
    }))
}

pub(in crate::download) struct SelectionConfirmation {
    retained: Vec<u8>,
    fresh: Option<Vec<u8>>,
}

pub(crate) struct SelectionRun {
    pub(crate) capture: ShadowCapture,
    pub(crate) root: ActiveGeneration,
    albums: BTreeMap<String, PhotoAlbum>,
    pub(crate) held: AtomicBool,
    pub(crate) session_expired: AtomicBool,
    fresh_confirmation: bool,
    retry_sources: Arc<std::sync::Mutex<SelectionRetrySources>>,
}

impl SelectionContext {
    pub(in crate::download) fn exact_retry_sources(
        &self,
    ) -> Result<
        rustc_hash::FxHashMap<super::url_refresh::RetryTaskKey, super::url_refresh::UrlRetrySource>,
    > {
        Ok(self
            .retry_sources
            .lock()
            .map_err(|_poisoned| anyhow::anyhow!("Selection retry evidence lock poisoned"))?
            .clone())
    }

    async fn current(&self) -> Result<Option<ActiveGeneration>> {
        Ok(self
            .capture
            .db
            .current_selection_generation(
                self.capture.owner.clone(),
                self.scope.clone(),
                self.config_hash.clone(),
            )
            .await?)
    }

    async fn start(
        &self,
        passes: &[AlbumPass],
        config: &DownloadConfig,
    ) -> Result<Arc<SelectionRun>> {
        let spec = GenerationSpec {
            format: 1,
            scope: self.scope.clone(),
            zone: self.zone.clone(),
            config_hash: self.config_hash.clone(),
            basis: self
                .capture
                .db
                .selection_basis(self.capture.owner.clone(), self.scope.clone())
                .await?,
            profile: self.profile.clone(),
            metadata_enabled: crate::download::pipeline::MetadataFlags::from(config)
                .has_any_write(),
            metadata_flags: crate::download::pipeline::MetadataFlags::from(config).bits(),
        };
        let root = if let Some(current) = self.current().await?
            && !current.sealed
        {
            current
        } else {
            self.capture
                .db
                .begin_selection_generation(self.capture.owner.clone(), spec, MAX_GENERATION_BYTES)
                .await?
        };
        Ok(Arc::new(SelectionRun {
            capture: self.capture.clone(),
            root,
            albums: passes
                .iter()
                .map(|pass| (pass_key(pass), pass.album.clone()))
                .collect(),
            held: AtomicBool::new(false),
            session_expired: AtomicBool::new(false),
            fresh_confirmation: false,
            retry_sources: Arc::clone(&self.retry_sources),
        }))
    }

    fn replay_run(
        &self,
        root: ActiveGeneration,
        passes: &[AlbumPass],
        fresh_confirmation: bool,
    ) -> Arc<SelectionRun> {
        Arc::new(SelectionRun {
            capture: self.capture.clone(),
            root,
            albums: passes
                .iter()
                .map(|pass| (pass_key(pass), pass.album.clone()))
                .collect(),
            held: AtomicBool::new(false),
            session_expired: AtomicBool::new(false),
            fresh_confirmation,
            retry_sources: Arc::clone(&self.retry_sources),
        })
    }
}

impl SelectionRun {
    /// Capture only failed tasks' original selected provenance. This transient
    /// map supplements the exact frozen task, never changes its path or metadata,
    /// and ends with this sync invocation. Durable replay rebuilds it from SQLite.
    pub(in crate::download) async fn remember_retry_sources(
        &self,
        tasks: &[filter::DownloadTask],
        passes: &[AlbumPass],
        config: &DownloadConfig,
    ) -> Result<()> {
        let mut sources = Vec::new();
        for task in tasks {
            for (pass_index, pass) in passes.iter().enumerate() {
                let key = pass_key(pass);
                let Some(manifest) = self
                    .capture
                    .db
                    .selection_decision(
                        self.capture.owner.clone(),
                        self.root.id.clone(),
                        key,
                        task.asset_record_name.to_string(),
                    )
                    .await?
                else {
                    continue;
                };
                if manifest.state_id != task.asset_id.as_ref()
                    || !manifest.decision.destinations.iter().any(|destination| {
                        destination.version_size == task.version_size.as_str()
                            && destination.path.to_path() == task.download_path
                            && destination.checksum == task.checksum.as_ref()
                            && destination.size == task.size
                    })
                {
                    continue;
                }
                let (Some(body), Some(master)) =
                    (&manifest.decision.confirmation, &manifest.decision.master)
                else {
                    continue;
                };
                let asset =
                    current_asset(body, &manifest.decision.child, master, &self.root.spec.zone)?;
                sources.push((
                    super::url_refresh::RetryTaskKey::from(task),
                    super::url_refresh::UrlRetrySource {
                        asset_record_name: asset.asset_record_name_arc(),
                        master_record_name: Arc::from(master.as_str()),
                        provider_version: filter::provider_version_for_selected(
                            &asset,
                            &config.with_pass(pass),
                            task.version_size,
                        ),
                        pass_index,
                    },
                ));
                break;
            }
        }
        self.retry_sources
            .lock()
            .map_err(|_poisoned| anyhow::anyhow!("Selection retry evidence lock poisoned"))?
            .extend(sources);
        Ok(())
    }

    /// Confirm before any canonical producer mutation. Retained current proof is
    /// reused only for this root's unchanged dependency/configuration basis.
    pub(in crate::download) async fn confirm(
        &self,
        key: &str,
        observed: PhotoAsset,
    ) -> Result<(PhotoAsset, SelectionConfirmation)> {
        if !self
            .capture
            .db
            .selection_attempt_due(
                self.capture.owner.clone(),
                self.root.id.clone(),
                key.to_owned(),
                observed.asset_record_name().to_owned(),
            )
            .await?
        {
            return Err(SelectionRetryDeferred.into());
        }
        if let Some(previous) = self
            .capture
            .db
            .selection_decision(
                self.capture.owner.clone(),
                self.root.id.clone(),
                key.to_owned(),
                observed.asset_record_name().to_owned(),
            )
            .await?
            && let (Some(body), Some(master)) =
                (previous.decision.confirmation, previous.decision.master)
        {
            let retained = current_asset(
                &body,
                &previous.decision.child,
                &master,
                &self.root.spec.zone,
            )?;
            anyhow::ensure!(
                same_selected_facts(&retained, &observed),
                "Retained current proof differs from observed selection"
            );
            if self.fresh_confirmation {
                let album = self
                    .albums
                    .get(key)
                    .context("Missing retained selection pass owner")?;
                let current = album
                    .confirm_catalog_asset(&previous.decision.child)
                    .await?;
                anyhow::ensure!(
                    same_selected_facts(&retained, &current.asset),
                    "Retained selected generation differs from current proof"
                );
                return Ok((
                    current
                        .asset
                        .with_state_record_name(Arc::from(previous.state_id)),
                    SelectionConfirmation {
                        retained: body,
                        fresh: Some(current.body),
                    },
                ));
            }
            return Ok((
                retained.with_state_record_name(Arc::from(previous.state_id)),
                SelectionConfirmation {
                    retained: body,
                    fresh: None,
                },
            ));
        }
        let album = self
            .albums
            .get(key)
            .context("Missing selection pass owner")?;
        let current = album
            .confirm_catalog_asset(observed.asset_record_name())
            .await?;
        anyhow::ensure!(
            same_selected_facts(&current.asset, &observed),
            "Current confirmation changed selection facts"
        );
        Ok((
            current.asset,
            SelectionConfirmation {
                retained: current.body.clone(),
                fresh: Some(current.body),
            },
        ))
    }

    /// Account for an unresolved current identity without creating queue work.
    /// Original source links remain available and same-root recovery may add a
    /// strict current proof atomically; backoff never means completion.
    pub(in crate::download) async fn defer_confirmation(
        &self,
        key: &str,
        child: String,
    ) -> Result<()> {
        let previous = self
            .capture
            .db
            .selection_decision(
                self.capture.owner.clone(),
                self.root.id.clone(),
                key.to_owned(),
                child.clone(),
            )
            .await?;
        if previous
            .as_ref()
            .is_some_and(|old| old.decision.confirmation.is_some())
        {
            self.capture
                .db
                .defer_selection_confirmation(
                    self.capture.owner.clone(),
                    self.root.id.clone(),
                    key.to_owned(),
                    child,
                )
                .await?;
            return Ok(());
        }
        // A failed retry preserves the original unresolved intent. New raw
        // observations remain retained independently; a later strict proof may
        // bind their additional links in the atomic resolving transaction.
        let sources = if let Some(previous) = previous {
            previous.sources
        } else {
            self.capture
                .db
                .selection_rank_sources(
                    self.capture.owner.clone(),
                    self.root.id.clone(),
                    key.to_owned(),
                    child.clone(),
                )
                .await?
        };
        let manifest = ActiveDecision {
            decision: SelectionDecision {
                pass_key: key.to_owned(),
                child: child.clone(),
                master: None,
                confirmation: None,
                outcome: SelectionOutcome::Deferred,
                reason: "current_lookup_unresolved".to_owned(),
                destinations: Vec::new(),
            },
            sources,
            state_id: child,
        };
        self.capture
            .db
            .project_selection_decision(
                self.capture.owner.clone(),
                self.root.id.clone(),
                manifest,
                Vec::new(),
                MAX_GENERATION_BYTES,
            )
            .await?;
        Ok(())
    }

    pub(in crate::download) async fn project(
        &self,
        key: &str,
        asset: &PhotoAsset,
        confirmation: SelectionConfirmation,
        plan: &mut AssetTaskPlan,
        planner: &mut TaskPlanner,
        config: &DownloadConfig,
    ) -> Result<bool> {
        let SelectionConfirmation {
            retained: body,
            fresh,
        } = confirmation;
        let reason = if plan.malformed_resource.is_some() {
            "malformed_current_resource"
        } else if plan.filter_reason.is_some() {
            "currently_filtered"
        } else {
            ""
        };
        let previous = self
            .capture
            .db
            .selection_decision(
                self.capture.owner.clone(),
                self.root.id.clone(),
                key.to_owned(),
                asset.asset_record_name().to_owned(),
            )
            .await?;
        let mut destinations = Vec::new();
        let mut records = Vec::new();
        if reason.is_empty() {
            let derived_paths = filter::derive_expected_paths(asset, config);
            for derived in &derived_paths {
                let mut retained = None;
                let candidates = self
                    .capture
                    .db
                    .verified_selection_paths(
                        self.capture.owner.clone(),
                        self.root.spec.scope.clone(),
                        key.to_owned(),
                        asset.asset_record_name().to_owned(),
                        asset.id().to_owned(),
                        derived.version_size.as_str().to_owned(),
                        (derived.checksum.to_string(), derived.size),
                    )
                    .await?;
                for candidate in candidates {
                    let path = candidate.to_path();
                    if filter::stored_path_matches_download_family(
                        asset.state_id(),
                        derived,
                        &derived_paths,
                        config,
                        &path,
                    ) && let Ok(fingerprint) =
                        crate::download::file::fingerprint_downloaded_path(&config.directory, &path)
                            .await
                        && self
                            .capture
                            .db
                            .selection_path_matches_local(
                                self.capture.owner.clone(),
                                self.root.spec.scope.clone(),
                                key.to_owned(),
                                asset.asset_record_name().to_owned(),
                                candidate.clone(),
                                (
                                    data_encoding::HEXLOWER.encode(&fingerprint.sha256),
                                    fingerprint.size,
                                ),
                            )
                            .await?
                    {
                        retained = Some(path);
                        break;
                    }
                }
                let path = if planner.managed_layout_session().is_some() {
                    plan.tasks
                        .iter()
                        .find(|task| task.version_size == derived.version_size)
                        .context("Managed layout lost its pinned rendition path")?
                        .download_path
                        .clone()
                } else if let Some(previous) = &previous
                    && previous.decision.confirmation.is_some()
                {
                    previous
                        .decision
                        .destinations
                        .iter()
                        .find(|destination| {
                            destination.version_size == derived.version_size.as_str()
                        })
                        .context("Frozen selection lost a rendition")?
                        .path
                        .to_path()
                } else if let Some(path) = retained {
                    path
                } else if let Some(path) = planner
                    .verified_downloaded_path(asset, config, derived.version_size)
                    .await
                {
                    path
                } else if let Some(task) = plan
                    .tasks
                    .iter()
                    .find(|task| task.version_size == derived.version_size)
                {
                    task.download_path.clone()
                } else {
                    derived.path.clone()
                };
                let metadata =
                    filter::metadata_for_selected_version(asset, config, derived.version_size);
                let metadata_hash = metadata
                    .metadata_hash
                    .clone()
                    .context("Missing selected metadata fingerprint")?;
                records.push(
                    crate::state::AssetRecord::new_pending(
                        Arc::clone(&config.library),
                        asset.state_id().to_owned(),
                        derived.version_size,
                        derived.checksum.to_string(),
                        path.file_name()
                            .and_then(|name| name.to_str())
                            .unwrap_or("")
                            .to_owned(),
                        asset.created(),
                        Some(asset.added_date()),
                        derived.size,
                        filter::determine_media_type(derived.version_size, asset),
                    )
                    .with_metadata_arc(metadata),
                );
                destinations.push(SelectionDestination {
                    version_size: derived.version_size.as_str().to_owned(),
                    path: SelectionPath::from_path(&std::path::absolute(&path)?),
                    checksum: derived.checksum.to_string(),
                    size: derived.size,
                    metadata_hash,
                });
            }
        }
        let sources = if let Some(previous) = &previous
            && previous.decision.confirmation.is_some()
        {
            previous.sources.clone()
        } else {
            self.capture
                .db
                .selection_rank_sources(
                    self.capture.owner.clone(),
                    self.root.id.clone(),
                    key.to_owned(),
                    asset.asset_record_name().to_owned(),
                )
                .await?
        };
        let manifest = ActiveDecision {
            state_id: asset.state_id().to_owned(),
            sources,
            decision: SelectionDecision {
                pass_key: key.to_owned(),
                child: asset.asset_record_name().to_owned(),
                master: Some(asset.id().to_owned()),
                confirmation: Some(body),
                outcome: if reason.is_empty() {
                    SelectionOutcome::Selected
                } else if reason == "currently_filtered" {
                    SelectionOutcome::Excluded
                } else {
                    SelectionOutcome::Deferred
                },
                reason: reason.to_owned(),
                destinations,
            },
        };
        let projection = self
            .capture
            .db
            .project_selection_decision_with_proof(
                self.capture.owner.clone(),
                self.root.id.clone(),
                manifest.clone(),
                records,
                fresh,
                MAX_GENERATION_BYTES,
            )
            .await?;
        if !projection.admitted && manifest.decision.outcome != SelectionOutcome::Excluded {
            tracing::debug!(reason = %projection.reason, "Selection destination admission deferred");
            self.held.store(true, Ordering::Relaxed);
            return Ok(false);
        }
        for destination in &manifest.decision.destinations {
            if let Ok(fingerprint) = crate::download::file::fingerprint_downloaded_path(
                &config.directory,
                &destination.path.to_path(),
            )
            .await
            {
                self.capture
                    .db
                    .reuse_selection_publication(
                        self.capture.owner.clone(),
                        self.root.id.clone(),
                        key.to_owned(),
                        asset.asset_record_name().to_owned(),
                        destination.path.clone(),
                        (
                            data_encoding::HEXLOWER.encode(&fingerprint.sha256),
                            fingerprint.size,
                        ),
                    )
                    .await?;
            }
        }
        let mut retained_tasks = Vec::with_capacity(plan.tasks.len());
        for mut task in std::mem::take(&mut plan.tasks) {
            if self
                .capture
                .db
                .selection_destination_verified(
                    self.capture.owner.clone(),
                    self.root.id.clone(),
                    key.to_owned(),
                    asset.asset_record_name().to_owned(),
                    task.version_size.as_str().to_owned(),
                )
                .await?
            {
                continue;
            }
            let destination = manifest
                .decision
                .destinations
                .iter()
                .find(|destination| destination.version_size == task.version_size.as_str())
                .context("Task has no frozen destination")?;
            let frozen = destination.path.to_path();
            if task.download_path != frozen {
                if !planner.claim_recorded_repair_path(&frozen, &task).await {
                    self.held.store(true, Ordering::Relaxed);
                    continue;
                }
                task.download_path = frozen;
            }
            retained_tasks.push(task);
        }
        plan.tasks = retained_tasks;
        Ok(true)
    }
}

pub(crate) async fn has_due(
    passes: &[AlbumPass],
    config: &DownloadConfig,
    controls: DownloadControls,
) -> Result<Option<bool>> {
    let Some(context) = context(passes, config, controls).await? else {
        return Ok(None);
    };
    let Some(root) = context.current().await? else {
        return Ok(Some(true));
    };
    if !root.sealed
        && !context
            .capture
            .db
            .selection_waiting_for_retry(context.capture.owner.clone(), root.id.clone())
            .await?
    {
        return Ok(Some(true));
    }
    let pending = context
        .capture
        .db
        .has_pending_selection(context.capture.owner.clone(), root.id.clone())
        .await?;
    let retained = context
        .capture
        .db
        .retained_selection_root(
            context.capture.owner.clone(),
            context.scope,
            context.config_hash,
            root.id,
        )
        .await?
        .is_some();
    Ok(Some(pending || retained))
}

pub(super) async fn inventory(
    client: &Client,
    passes: &[AlbumPass],
    config: &Arc<DownloadConfig>,
    controls: DownloadControls,
    cancel: CancellationToken,
) -> Result<SyncResult> {
    let context = config
        .selection_context
        .as_ref()
        .context("Missing selection context")?;
    let run = context.start(passes, config).await?;
    let captured_passes: Vec<_> = passes
        .iter()
        .map(|pass| AlbumPass {
            album: pass
                .album
                .clone()
                .with_rank_capture(&run.root.id, &pass_key(pass)),
            ..pass.clone()
        })
        .collect();
    let captured_config = Arc::new(DownloadConfig {
        selection_run: Some(run.clone()),
        ..config.as_ref().clone()
    });
    let retained = Box::pin(recover_retained(
        client,
        passes,
        config,
        controls,
        cancel.clone(),
        &run.root,
    ))
    .await?;
    if retained.provider_auth_errors > 0 {
        return crate::download::pipeline::build_download_result(
            client,
            passes,
            config,
            controls,
            retained,
            Instant::now(),
            cancel,
        )
        .await;
    }
    let mut result = Box::pin(super::full::download_photos_full_with_token(
        client,
        &captured_passes,
        &captured_config,
        controls,
        cancel.clone(),
    ))
    .await?;
    let retained = Box::pin(crate::download::pipeline::build_download_result(
        client,
        passes,
        config,
        controls,
        retained,
        Instant::now(),
        cancel.clone(),
    ))
    .await?;
    let query_checkpoint_ready = result.sync_token.is_some()
        && !result.checkpoint.sync_token_blocked
        && !result.checkpoint.identity_incomplete;
    let query_veto = result.stats.sync_token_blocked_reason.map(str::to_owned);
    let covered = !result.checkpoint.enumeration_incomplete
        && result.checkpoint.enumeration_errors == 0
        && result.checkpoint.state_write_failures == 0
        && !result.checkpoint.interrupted
        && !cancel.is_cancelled();
    // The new rank coverage receipt belongs to this query, independently of
    // retained older work. Historical failure still composes into the returned
    // checkpoint and the whole-scope debt gate below; it cannot un-cover a
    // complete current query or seal the older root.
    result.outcome = merge_download_outcomes(&result.outcome, &retained.outcome);
    result.accumulate(&retained);
    if covered {
        context
            .capture
            .db
            .seal_selection_generation(
                context.capture.owner.clone(),
                run.root.id.clone(),
                query_checkpoint_ready,
                query_veto,
            )
            .await?;
    }
    context
        .capture
        .db
        .freeze_interrupted_selection(context.capture.owner.clone(), context.scope.clone())
        .await?;
    let metadata_failures = metadata_tail(config, &cancel, context, &run.root.id).await?;
    result.stats.exif_failures += metadata_failures;
    if metadata_failures > 0 {
        result.outcome = merge_download_outcomes(
            &result.outcome,
            &DownloadOutcome::PartialFailure {
                failed_count: metadata_failures,
            },
        );
    }
    context
        .capture
        .db
        .complete_selection_metadata(context.capture.owner.clone(), run.root.id.clone())
        .await?;
    if run.held.load(Ordering::Relaxed) {
        result.checkpoint.identity_incomplete = true;
        result.block_incremental_token(DEFERRED_REASON)
    }
    if context
        .capture
        .db
        .unfinished_selection_debt(context.capture.owner.clone(), context.scope.clone())
        .await?
    {
        result.block_incremental_token(DEFERRED_REASON)
    }
    result.checkpoint.project(&mut result.stats);
    Ok(result)
}

/// Keep the existing 500-row writer budget while rotating past retained failed
/// or option-disabled physical writers. This cursor schedules retries only;
/// debt/progress remain in the exact publication and metadata receipts.
async fn metadata_tail(
    config: &DownloadConfig,
    cancel: &CancellationToken,
    context: &SelectionContext,
    generation: &str,
) -> Result<usize> {
    let Some(db) = &config.state_db else {
        return Ok(0);
    };
    let flags = crate::download::pipeline::MetadataFlags::from(config);
    if !flags.has_any_write() {
        return Ok(0);
    }
    let mut offset = context
        .capture
        .db
        .selection_metadata_retry_offset(context.capture.owner.clone())
        .await?;
    // Reserve at most half the existing 500-row budget for current exact
    // destinations. Older failed work cannot occupy this lane, and repeated
    // inventory never resets the independent retained/legacy continuation.
    let current = context
        .capture
        .db
        .pending_selection_metadata(context.capture.owner.clone(), generation.to_owned(), 250)
        .await?;
    let priority = crate::download::metadata_rewrite::run_pending_rows(
        db.as_ref(),
        flags,
        crate::download::metadata_rewrite::CaptureTimestampRepair::Preserve,
        config.temp_suffix.clone(),
        cancel,
        current,
    )
    .await;
    let remaining = 500_usize.saturating_sub(priority.fetched);
    let original = offset;
    let mut result = crate::download::metadata_rewrite::run_pending_budget(
        db.as_ref(),
        flags,
        crate::download::metadata_rewrite::CaptureTimestampRepair::Preserve,
        config.temp_suffix.clone(),
        cancel,
        None,
        (offset, remaining),
    )
    .await;
    if result.fetched == 0 && offset > 0 && !cancel.is_cancelled() {
        offset = 0;
        result = crate::download::metadata_rewrite::run_pending_budget(
            db.as_ref(),
            flags,
            crate::download::metadata_rewrite::CaptureTimestampRepair::Preserve,
            config.temp_suffix.clone(),
            cancel,
            None,
            (0, remaining),
        )
        .await;
    }
    let next = offset
        .saturating_add(result.fetched)
        .saturating_sub(result.retired_from_selected_queue);
    if next != original {
        context
            .capture
            .db
            .set_selection_metadata_retry_offset(context.capture.owner.clone(), next)
            .await?;
    }
    Ok(priority.failed.saturating_add(result.failed))
}

async fn recover_retained(
    client: &Client,
    passes: &[AlbumPass],
    config: &Arc<DownloadConfig>,
    controls: DownloadControls,
    cancel: CancellationToken,
    current: &ActiveGeneration,
) -> Result<crate::download::pipeline::StreamingResult> {
    let context = config
        .selection_context
        .as_ref()
        .context("Missing retained selection context")?;
    let Some(root) = context
        .capture
        .db
        .retained_selection_root(
            context.capture.owner.clone(),
            context.scope.clone(),
            context.config_hash.clone(),
            current.id.clone(),
        )
        .await?
    else {
        return Ok(crate::download::pipeline::StreamingResult {
            enumeration_complete: true,
            ..crate::download::pipeline::StreamingResult::default()
        });
    };
    anyhow::ensure!(
        root.spec.profile == current.spec.profile
            && root.spec.zone == current.spec.zone
            && root.spec.metadata_flags == current.spec.metadata_flags,
        "Retained selection configuration disagrees"
    );
    let decisions = context
        .capture
        .db
        .retained_selection_decisions(context.capture.owner.clone(), root.id.clone())
        .await?;
    let run = context.replay_run(root.clone(), passes, true);
    let mut combined = crate::download::pipeline::StreamingResult {
        enumeration_complete: true,
        ..crate::download::pipeline::StreamingResult::default()
    };
    for pass in passes {
        if cancel.is_cancelled() || run.session_expired.load(Ordering::Relaxed) {
            break;
        }
        let key = pass_key(pass);
        let mut assets = Vec::new();
        for decision in decisions
            .iter()
            .filter(|decision| decision.decision.pass_key == key)
        {
            let asset = match (&decision.decision.confirmation, &decision.decision.master) {
                (Some(body), Some(master)) => Some(current_asset(
                    body,
                    &decision.decision.child,
                    master,
                    &root.spec.zone,
                )?),
                _ => {
                    match context
                        .capture
                        .db
                        .observed_selection_asset(
                            context.capture.owner.clone(),
                            root.id.clone(),
                            key.clone(),
                            decision.decision.child.clone(),
                        )
                        .await
                    {
                        Ok(asset) => asset,
                        Err(crate::state::error::StateError::ProviderSelectionFull) => None,
                        Err(error) => return Err(error.into()),
                    }
                }
            };
            if let Some(asset) = asset {
                assets.push(Ok(
                    asset.with_state_record_name(Arc::from(decision.state_id.clone()))
                ));
            } else {
                run.defer_confirmation(&key, decision.decision.child.clone())
                    .await?;
            }
        }
        if assets.is_empty() {
            continue;
        }
        let mut effective = config.with_pass(pass);
        effective.selection_run = Some(run.clone());
        effective.selection_pass = Some(key);
        let result = crate::download::pipeline::stream_and_download_from_stream(
            client,
            futures_util::stream::iter(assets),
            &Arc::new(effective),
            controls,
            0,
            cancel.clone(),
            crate::download::pipeline::StreamRuntime::new(None, None).deferring_metadata_drain(),
        )
        .await?;
        run.remember_retry_sources(&result.failed, passes, config)
            .await?;
        super::models::merge_streaming_result(&mut combined, result);
    }
    context
        .capture
        .db
        .complete_selection_metadata(context.capture.owner.clone(), root.id)
        .await?;
    Ok(combined)
}

async fn replay(
    client: &Client,
    passes: &[AlbumPass],
    config: &Arc<DownloadConfig>,
    controls: DownloadControls,
    cancel: CancellationToken,
    root: ActiveGeneration,
) -> Result<SyncResult> {
    let context = config
        .selection_context
        .as_ref()
        .context("Missing replay context")?;
    let run = context.replay_run(root.clone(), passes, false);
    let started = Instant::now();
    let mut combined = Box::pin(recover_retained(
        client,
        passes,
        config,
        controls,
        cancel.clone(),
        &root,
    ))
    .await?;
    for pass in passes {
        if cancel.is_cancelled() || combined.provider_auth_errors > 0 {
            combined.enumeration_complete = false;
            break;
        }
        let key = pass_key(pass);
        let mut after = None;
        loop {
            let decisions = context
                .capture
                .db
                .pending_selection_decisions(
                    context.capture.owner.clone(),
                    root.id.clone(),
                    key.clone(),
                    after.clone(),
                    MAX_REPLAY_DECISIONS,
                )
                .await?;
            if decisions.is_empty() {
                break;
            }
            after = decisions.last().map(|d| d.decision.child.clone());
            let mut assets = Vec::new();
            for decision in decisions {
                if let (Some(body), Some(master)) =
                    (&decision.decision.confirmation, &decision.decision.master)
                {
                    assets.push(Ok(current_asset(
                        body,
                        &decision.decision.child,
                        master,
                        &root.spec.zone,
                    )?
                    .with_state_record_name(Arc::from(decision.state_id))));
                } else {
                    run.held.store(true, Ordering::Relaxed);
                }
            }
            let mut effective = config.with_pass(pass);
            effective.selection_run = Some(run.clone());
            effective.selection_pass = Some(key.clone());
            let result = crate::download::pipeline::stream_and_download_from_stream(
                client,
                futures_util::stream::iter(assets),
                &Arc::new(effective),
                controls,
                0,
                cancel.clone(),
                crate::download::pipeline::StreamRuntime::new(None, None)
                    .deferring_metadata_drain(),
            )
            .await?;
            run.remember_retry_sources(&result.failed, passes, config)
                .await?;
            super::models::merge_streaming_result(&mut combined, result);
        }
    }
    combined.exif_failures += metadata_tail(config, &cancel, context, &run.root.id).await?;
    let mut result = crate::download::pipeline::build_download_result(
        client, passes, config, controls, combined, started, cancel,
    )
    .await?;
    context
        .capture
        .db
        .complete_selection_metadata(context.capture.owner.clone(), root.id.clone())
        .await?;
    if !root.checkpoint_ready || run.held.load(Ordering::Relaxed) {
        result.block_incremental_token(match root.checkpoint_veto.as_deref() {
            Some(super::models::RECENT_LIMITED_FULL_ENUMERATION_REASON) => {
                super::models::RECENT_LIMITED_FULL_ENUMERATION_REASON
            }
            Some(super::models::DATE_BOUNDED_FULL_ENUMERATION_REASON) => {
                super::models::DATE_BOUNDED_FULL_ENUMERATION_REASON
            }
            _ => DEFERRED_REASON,
        });
    }
    if context
        .capture
        .db
        .unfinished_selection_debt(context.capture.owner.clone(), context.scope.clone())
        .await?
    {
        result.block_incremental_token(DEFERRED_REASON)
    }
    Ok(result)
}

/// Reuse the existing completed-delta/full-selection veto composition. The
/// only offered successor is the actual completed source stream's successor.
pub(super) async fn after_delta(
    client: &Client,
    passes: &[AlbumPass],
    config: &Arc<DownloadConfig>,
    controls: DownloadControls,
    cancel: CancellationToken,
    delta: IncrementalDeltaSummary,
    prior_cursor: &str,
) -> Result<SyncResult> {
    let context = config
        .selection_context
        .as_ref()
        .context("Missing selection context")?;
    let recent = super::recent::active(config)
        .await
        .map_err(super::recent::RecentSelectionError)?;
    let repeat_inventory = recent
        && super::recent::needs_inventory(passes, config, prior_cursor, false)
            .await
            .map_err(super::recent::RecentSelectionError)?;
    if recent {
        super::recent::record(passes, config, prior_cursor, false)
            .await
            .map_err(super::recent::RecentSelectionError)?;
    }
    let root = context.current().await?;
    let selection = match root {
        Some(root)
            if !repeat_inventory
                && (root.sealed
                    || context
                        .capture
                        .db
                        .selection_waiting_for_retry(context.capture.owner.clone(), root.id.clone())
                        .await?) =>
        {
            replay(client, passes, config, controls, cancel.clone(), root).await
        }
        _ => inventory(client, passes, config, controls, cancel.clone()).await,
    };
    let mut selected = if recent {
        selection.map_err(|error| {
            if crate::icloud::photos::session::is_session_error(&error) {
                error
            } else {
                super::recent::RecentSelectionError(error).into()
            }
        })?
    } else {
        selection?
    };
    let mut stats = SyncStats {
        state_write_failures: delta.state_transition_failures,
        identity_incomplete: delta.identity_incomplete,
        interrupted: cancel.is_cancelled(),
        ..SyncStats::default()
    };
    if let Some(reason) = delta.token_unsafe_reason {
        block_sync_token_for_incremental_delta(&mut stats, reason)
    }
    let completed_delta_replay = delta.sync_token.is_some();
    let delta_result = SyncResult::from_incremental_execution(
        if delta.state_transition_failures > 0 {
            DownloadOutcome::PartialFailure {
                failed_count: delta.state_transition_failures,
            }
        } else {
            DownloadOutcome::Success
        },
        None,
        stats,
        delta.sparse_identity_proofs,
        completed_delta_replay,
    );
    selected.outcome = merge_download_outcomes(&selected.outcome, &delta_result.outcome);
    selected.accumulate(&delta_result);
    if let Some(reason) = delta.token_unsafe_reason {
        selected.block_incremental_token(reason)
    }
    selected.sync_token = None;
    if delta.sync_token.is_some()
        && !selected.checkpoint.sync_token_blocked
        && !selected.checkpoint.identity_incomplete
        && !selected.checkpoint.interrupted
        && !selected.checkpoint.enumeration_incomplete
        && selected.checkpoint.enumeration_errors == 0
        && selected.checkpoint.state_write_failures == 0
    {
        selected.sync_token = delta.sync_token;
        if recent && let Some(successor) = selected.sync_token.as_deref() {
            super::recent::record(
                passes,
                config,
                successor,
                matches!(selected.outcome, DownloadOutcome::Success),
            )
            .await
            .map_err(super::recent::RecentSelectionError)?;
        }
    }
    Ok(selected)
}
