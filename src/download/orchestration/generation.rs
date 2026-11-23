//! Current selection generation composition using existing selection and queues.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use anyhow::{Context, Result};
use reqwest::Client;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;

use super::config::{DownloadConfig, IndexingWindow, hash_download_config};
use super::delta::{IncrementalDeltaSummary, MembershipDelta};
use super::models::{
    DownloadControls, DownloadOutcome, SyncResult, SyncStats,
    block_sync_token_for_incremental_delta, merge_download_outcomes,
};
use crate::commands::{AlbumPass, PassKind};
use crate::download::filter;
use crate::download::planner::{AssetTaskPlan, SelectionPathEvidence, TaskPlanner};
use crate::icloud::photos::{
    PhotoAlbum, PhotoAsset, current_asset, inbox::ShadowCapture, same_selected_facts,
};
use crate::state::ReconciliationStateStore;
use crate::state::db::provider_generations::{
    ActiveDecision, ActiveGeneration, AdmissionChecks, GenerationSpec, INDEPENDENT_ALBUM_COPIES,
    MAX_GENERATION_BYTES,
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
    album_directories: BTreeMap<String, String>,
    album_files:
        BTreeMap<String, Vec<crate::state::db::primary_layout::directory::AlbumDirectoryFile>>,
    retry_sources: Arc<std::sync::Mutex<SelectionRetrySources>>,
    inventory_membership_delta: Arc<std::sync::Mutex<Option<Box<MembershipDelta>>>>,
}

/// Begin a synthetic historical generation through the normal admission owner.
/// Tests supply frozen decisions separately, rather than enabling retired policy.
#[cfg(test)]
pub(super) async fn begin_frozen_fixture(
    passes: &[AlbumPass],
    config: &DownloadConfig,
    controls: DownloadControls,
) -> Result<ActiveGeneration> {
    let context = context(passes, config, controls)
        .await?
        .context("fixture selection context")?;
    Ok(context.start(passes, config).await?.root.clone())
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
        let Some((capture, scope, _zone)) = pass.album.owned_selection_scope()? else {
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

/// Compose only the authenticated current selection's checkpoint debt.
/// Historical physical publication protection stays with admission and journals.
pub(super) async fn hold_retained_debt(
    passes: &[AlbumPass],
    config: &DownloadConfig,
    controls: DownloadControls,
    result: &mut SyncResult,
) -> Result<()> {
    if !controls.run_mode.downloads_files() || passes.is_empty() {
        return Ok(());
    }
    let Some(context) = &config.selection_context else {
        return Ok(());
    };
    if context.checkpoint_debt(config).await? {
        result.block_incremental_token(DEFERRED_REASON);
        result.stats.selected_work_remaining = true;
        result.sync_token = None;
    }
    Ok(())
}

pub(crate) async fn context(
    passes: &[AlbumPass],
    config: &DownloadConfig,
    controls: DownloadControls,
) -> Result<Option<SelectionContext>> {
    if !controls.run_mode.downloads_files() || passes.is_empty() {
        return Ok(None);
    }
    let mut qualified = None;
    let mut profile = Vec::new();
    let mut album_directories = BTreeMap::new();
    let mut album_files = BTreeMap::new();
    let mut album_projection_generations = BTreeMap::new();
    for pass in passes {
        if pass.kind == PassKind::SmartFolder && !pass.album.is_supported_smart_folder() {
            return Ok(None);
        }
        let Some((capture, scope, zone)) = pass.album.current_selection_scope()? else {
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
        if let Some(container) = pass.album.container_id()
            && let Some(binding) = capture
                .db
                .album_directory_binding(
                    capture.owner.clone(),
                    config.library.to_string(),
                    container.to_owned(),
                )
                .await?
        {
            if binding.projection.is_some() || binding.additional_projection.is_some() {
                album_projection_generations.insert(container.to_owned(), binding.generation);
            }
            album_files.insert(
                container.to_owned(),
                capture
                    .db
                    .album_projection_files(
                        capture.owner.clone(),
                        binding.clone(),
                        SelectionPath::from_path(&std::path::absolute(&config.directory)?),
                    )
                    .await?,
            );
            if binding.projection.is_none()
                && let Some(parent) = super::album_directory::template_parent(config)?
                && binding.root.to_path() == parent
            {
                let path = binding.path.to_path();
                let name = path
                    .strip_prefix(&parent)?
                    .to_str()
                    .context("Album directory requires a UTF-8 relative path")?;
                // Folder templates use '/' between components on every platform.
                let template = name
                    .replace(std::path::MAIN_SEPARATOR, "/")
                    .replace('%', "%%");
                album_directories.insert(container.to_owned(), template);
            }
        }
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
        let mut selected = json!({"key":pass_key(pass),"kind":format!("{:?}",pass.kind),"scope":pass.album.selection_scope(),"excludes":excludes});
        if pass.kind == PassKind::Album {
            selected
                .as_object_mut()
                .context("Invalid selection profile")?
                .insert(
                    "selector_components".into(),
                    serde_json::to_value(pass.album.selector_components())?,
                );
        }
        let media_sources = pass.album.selection_media_sources()?;
        if !media_sources.is_empty() {
            selected
                .as_object_mut()
                .context("Invalid selection profile")?
                .insert(
                    "media_sources".to_owned(),
                    serde_json::to_value(media_sources)?,
                );
        }
        profile.push(selected);
    }
    let Some((capture, scope, zone)) = qualified else {
        return Ok(None);
    };
    let mut excludes: Vec<_> = config.exclude_asset_ids.iter().collect();
    excludes.sort_unstable();
    let mut profile = json!({"format":1,"coverage":"observed_selection_window","passes":profile,
        "selection_config":config.enum_config_hash.as_deref(),
        "recent":null,"recent_scope":"Global","excludes":excludes});
    let mut identity = json!({"format":1,"profile":profile,"scope":scope,"zone":zone,
        "paths":hash_download_config(config),"anchored_root":SelectionPath::from_path(&std::path::absolute(&config.directory)?),
        "before":config.skip_created_before.map(crate::config::CreatedDateFilter::fingerprint),"after":config.skip_created_after.map(crate::config::CreatedDateFilter::fingerprint),
        "enum_hash":config.enum_config_hash.as_deref(),"metadata_flags":crate::download::pipeline::MetadataFlags::from(config).bits(),
        "metadata_policy":crate::download::pipeline::MetadataFlags::POLICY_REVISION,
        "capture_revision":crate::state::METADATA_CAPTURE_REVISION,"capture_repair":format!("{:?}",config.capture_timestamp_repair),
        // Retain the historical fingerprint key; lifetime admission is retired.
        "repair_truncated":config.repair_truncated,"attempt_limit":0,
    });
    // Keep historical fingerprints byte-for-byte when there is no exclusive
    // directory binding. A new binding must not replay an older frozen path.
    if !album_directories.is_empty() {
        identity
            .as_object_mut()
            .context("Invalid selection identity")?
            .insert(
                "album_directories".to_owned(),
                serde_json::to_value(&album_directories)?,
            );
    }
    if !album_projection_generations.is_empty() {
        identity
            .as_object_mut()
            .context("Invalid selection identity")?
            .insert(
                "album_projection_generations".to_owned(),
                serde_json::to_value(album_projection_generations)?,
            );
    }
    let config_hash = digest(&serde_json::to_vec(&identity)?);
    // Execution policy changes the declaration, not its configuration identity.
    profile
        .as_object_mut()
        .context("Selection profile is not an object")?
        .insert(INDEPENDENT_ALBUM_COPIES.to_owned(), Value::Bool(true));
    Ok(Some(SelectionContext {
        capture,
        scope,
        zone,
        config_hash,
        profile,
        album_directories,
        album_files,
        retry_sources: Arc::new(std::sync::Mutex::new(rustc_hash::FxHashMap::default())),
        inventory_membership_delta: Arc::default(),
    }))
}

pub(in crate::download) struct SelectionConfirmation {
    retained: Vec<u8>,
    fresh: Option<Vec<u8>>,
    retry_observation: Option<Arc<PhotoAsset>>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum SelectionPhase {
    Inventory,
    Execution,
}

pub(crate) struct SelectionRun {
    pub(crate) capture: ShadowCapture,
    pub(crate) root: ActiveGeneration,
    pub(crate) phase: SelectionPhase,
    albums: BTreeMap<String, PhotoAlbum>,
    pub(crate) held: AtomicBool,
    pub(crate) session_expired: AtomicBool,
    fresh_confirmation: bool,
    retry_sources: Arc<std::sync::Mutex<SelectionRetrySources>>,
}

impl SelectionContext {
    async fn checkpoint_debt(&self, config: &DownloadConfig) -> Result<bool> {
        let (owner, scope, hash) = (
            self.capture.owner.clone(),
            self.scope.clone(),
            self.config_hash.clone(),
        );
        Ok(if config.post_sync_metadata {
            self.capture
                .db
                .provider_selection_debt(owner, scope, hash)
                .await?
        } else {
            self.capture
                .db
                .unfinished_selection_debt(owner, scope, hash)
                .await?
        })
    }
    async fn include_durable_membership_deletions(
        &self,
        delta: &mut MembershipDelta,
    ) -> Result<()> {
        let Some(config) = self.profile.get("selection_config").and_then(Value::as_str) else {
            return Ok(());
        };
        for container in self
            .capture
            .db
            .pending_deleted_membership_containers(
                self.capture.owner.clone(),
                self.scope.clone(),
                config.to_owned(),
            )
            .await?
        {
            delta.containers.entry(container).or_insert(true);
        }
        Ok(())
    }

    async fn reconcile_membership(
        &self,
        passes: &[AlbumPass],
        config: &DownloadConfig,
        delta: &MembershipDelta,
        cancel: &CancellationToken,
    ) -> Result<bool> {
        if !delta.relations.values().any(|deleted| *deleted)
            && !delta.containers.values().any(|deleted| *deleted)
        {
            return Ok(true);
        }
        let Some(root) = self.current().await? else {
            return Ok(false);
        };
        Box::pin(super::primary_layout::membership::reconcile(
            passes,
            config,
            &self.capture,
            &root,
            delta,
            None,
            cancel,
        ))
        .await
    }

    pub(super) fn album_directory(&self, container: &str) -> Option<&str> {
        self.album_directories.get(container).map(String::as_str)
    }

    pub(in crate::download) fn album_file(
        &self,
        container: &str,
        library: &str,
        child: &str,
        version: &str,
    ) -> Option<std::path::PathBuf> {
        self.album_files
            .get(container)?
            .iter()
            .find(|file| file.library == library && file.child == child && file.version == version)
            .map(|file| file.next.to_path())
    }

    pub(in crate::download) fn album_parent(
        &self,
        container: &str,
        library: &str,
        child: &str,
    ) -> Option<std::path::PathBuf> {
        let mut files = self
            .album_files
            .get(container)?
            .iter()
            .filter(|file| file.library == library && file.child == child);
        let parent = files.next()?.next.to_path().parent()?.to_path_buf();
        files
            .all(|file| file.next.to_path().parent() == Some(parent.as_path()))
            .then_some(parent)
    }
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

    pub(super) async fn current(&self) -> Result<Option<ActiveGeneration>> {
        Ok(self
            .capture
            .db
            .current_selection_generation(
                self.capture.owner.clone(),
                self.scope.clone(),
                self.config_hash.clone(),
            )
            .await?
            .filter(|root| {
                root.spec.profile.get(INDEPENDENT_ALBUM_COPIES) == Some(&Value::Bool(true))
            }))
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
            phase: SelectionPhase::Inventory,
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
            phase: SelectionPhase::Execution,
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

/// Empty selected coverage uses an authenticated exact lookup source, never a
/// synthetic unfiled selection or a provider checkpoint candidate.
pub(crate) async fn recover_deleted_album_selection(
    source: &PhotoAlbum,
    bindings: &[crate::state::db::provider_generations::BoundAlbumSelection],
    config: &DownloadConfig,
    cancel: &CancellationToken,
) -> Result<bool> {
    if bindings.is_empty() || cancel.is_cancelled() {
        return Ok(false);
    }
    let Some((capture, scope, zone)) = source.current_selection_scope()? else {
        return Ok(false);
    };
    let Some(store) = &config.state_db else {
        return Ok(false);
    };
    let concrete = Arc::clone(&capture.db) as Arc<dyn super::models::DownloadStore>;
    anyhow::ensure!(
        Arc::ptr_eq(store, &concrete),
        "Deleted selection database context mismatch"
    );
    anyhow::ensure!(
        zone.get("zoneName").and_then(Value::as_str) == Some(config.library.as_ref()),
        "Deleted selection source scope mismatch"
    );
    let mut bindings = bindings.to_vec();
    bindings.sort_by(|a, b| a.container_id.cmp(&b.container_id));
    let profile = json!({"format":1,"coverage":"observed_selection_window","passes":[],(INDEPENDENT_ALBUM_COPIES):true,"selection_config":config.enum_config_hash.as_deref(),"deleted_album_bindings":bindings});
    let config_hash = digest(
        serde_json::to_vec(&json!([
            profile,
            scope,
            zone,
            hash_download_config(config),
            SelectionPath::from_path(&std::path::absolute(&config.directory)?)
        ]))?
        .as_slice(),
    );
    let context = SelectionContext {
        capture,
        scope,
        zone,
        config_hash,
        profile,
        retry_sources: Arc::default(),
        album_directories: BTreeMap::new(),
        album_files: BTreeMap::new(),
        inventory_membership_delta: Arc::default(),
    };
    let mut root = match context.current().await? {
        Some(root) => root,
        None => context.start(&[], config).await?.root.clone(),
    };
    if !root.sealed {
        context
            .capture
            .db
            .seal_selection_generation(context.capture.owner.clone(), root.id.clone(), true, None)
            .await?;
        root = context
            .current()
            .await?
            .context("Missing deleted selection coverage")?;
    }
    let mut delta = MembershipDelta::default();
    for binding in bindings {
        delta.containers.insert(binding.container_id, true);
    }
    Box::pin(super::primary_layout::membership::reconcile(
        &[],
        config,
        &context.capture,
        &root,
        &delta,
        Some(source),
        cancel,
    ))
    .await
}

impl SelectionRun {
    #[must_use]
    pub(in crate::download) fn independent_album(&self, config: &DownloadConfig) -> bool {
        self.root.spec.profile.get(INDEPENDENT_ALBUM_COPIES) == Some(&Value::Bool(true))
            && matches!(
                config.primary_layout_pass_kind,
                Some(PassKind::Album | PassKind::SmartFolder)
            )
    }

    /// Qualify a later album before concurrent producers can choose the same
    /// fresh filename. Durable same-pass paths still take precedence over this
    /// current-template hint, and admission rechecks actual path ownership.
    #[must_use]
    pub(super) fn colliding_album_parent(
        &self,
        asset: &PhotoAsset,
        config: &DownloadConfig,
    ) -> bool {
        if !self.independent_album(config) {
            return false;
        }
        let Some(key) = config.primary_layout_pass.as_deref() else {
            return false;
        };
        let parent = crate::download::paths::local_download_dir(
            &config.directory,
            &config.folder_structure,
            &asset.created_local(),
            config.album_name.as_deref(),
        );
        let Some(passes) = self
            .root
            .spec
            .profile
            .get("passes")
            .and_then(Value::as_array)
        else {
            return false;
        };
        passes.iter().any(|pass| {
            let Some(other_key) = pass.get("key").and_then(Value::as_str) else {
                return false;
            };
            let kind = match pass.get("kind").and_then(Value::as_str) {
                Some("Album") => PassKind::Album,
                Some("SmartFolder") => PassKind::SmartFolder,
                _ => return false,
            };
            if other_key >= key {
                return false;
            }
            let Some(album) = self.albums.get(other_key) else {
                return false;
            };
            let other = config.with_pass(&AlbumPass {
                kind,
                album: album.clone(),
                exclude_ids: Arc::default(),
            });
            parent
                == crate::download::paths::local_download_dir(
                    &other.directory,
                    &other.folder_structure,
                    &asset.created_local(),
                    other.album_name.as_deref(),
                )
        })
    }

    pub(in crate::download) async fn album_path_claimed(
        &self,
        config: &DownloadConfig,
        path: &std::path::Path,
    ) -> Result<bool> {
        if !self.independent_album(config) {
            return Ok(false);
        }
        let key = config
            .primary_layout_pass
            .as_deref()
            .context("Album destination has no pass owner")?;
        Ok(self
            .capture
            .db
            .selection_album_path_claimed(
                self.capture.owner.clone(),
                self.root.spec.scope.clone(),
                key.to_owned(),
                SelectionPath::from_path(&std::path::absolute(path)?),
            )
            .await?)
    }

    /// Capture only failed tasks' original selected provenance. This transient
    /// map supplements the exact frozen task, never changes its path or metadata,
    /// and ends with this sync invocation. Durable replay rebuilds it from SQLite.
    pub(in crate::download) async fn remember_retry_sources(
        &self,
        tasks: &[filter::DownloadTask],
        passes: &[AlbumPass],
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
                let asset = current_asset(
                    body,
                    &manifest.decision.child,
                    master,
                    manifest.media_zone(&self.root),
                )?;
                let Some(provider_version) = filter::provider_version_for_frozen_resource(
                    &asset,
                    task.version_size,
                    &task.checksum,
                    task.size,
                ) else {
                    continue;
                };
                sources.push((
                    super::url_refresh::RetryTaskKey::from(task),
                    super::url_refresh::UrlRetrySource {
                        asset_record_name: asset.asset_record_name_arc(),
                        master_record_name: Arc::from(master.as_str()),
                        provider_version,
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
        observed: &PhotoAsset,
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
        let previous = self
            .capture
            .db
            .selection_decision(
                self.capture.owner.clone(),
                self.root.id.clone(),
                key.to_owned(),
                observed.asset_record_name().to_owned(),
            )
            .await?;
        let retry_inventory = self.phase == SelectionPhase::Inventory && previous.is_some();
        if let Some(previous) = previous
            && let (Some(body), Some(master)) =
                (previous.decision.confirmation, previous.decision.master)
        {
            let retained = current_asset(
                &body,
                &previous.decision.child,
                &master,
                previous
                    .decision
                    .media_source
                    .as_ref()
                    .map_or(&self.root.spec.zone, |source| &source.zone),
            )?;
            anyhow::ensure!(
                same_selected_facts(&retained, observed),
                "Retained current proof differs from observed selection"
            );
            if self.fresh_confirmation {
                let album = self
                    .albums
                    .get(key)
                    .context("Missing retained selection pass owner")?;
                let current = album.confirm_selected_asset(&retained).await?;
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
                        retry_observation: None,
                    },
                ));
            }
            return Ok((
                retained.with_state_record_name(Arc::from(previous.state_id)),
                SelectionConfirmation {
                    retained: body,
                    fresh: None,
                    retry_observation: None,
                },
            ));
        }
        let album = self
            .albums
            .get(key)
            .context("Missing selection pass owner")?;
        let current = album.confirm_selected_asset(observed).await?;
        anyhow::ensure!(
            same_selected_facts(&current.asset, observed),
            "Current confirmation changed selection facts"
        );
        Ok((
            current.asset,
            SelectionConfirmation {
                retained: current.body.clone(),
                fresh: Some(current.body),
                retry_observation: retry_inventory.then(|| Arc::new(observed.clone())),
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
        observed: Option<&PhotoAsset>,
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
        let (sources, media_source) = if let Some(previous) = previous {
            (previous.sources, previous.decision.media_source)
        } else {
            let media_source = observed
                .map(|asset| {
                    self.albums
                        .get(key)
                        .context("Missing selected media owner")?
                        .selected_media_source(asset)
                })
                .transpose()?;
            (
                self.capture
                    .db
                    .selection_rank_sources(
                        self.capture.owner.clone(),
                        self.root.id.clone(),
                        key.to_owned(),
                        child.clone(),
                    )
                    .await?,
                media_source,
            )
        };
        let manifest = ActiveDecision {
            decision: SelectionDecision {
                pass_key: key.to_owned(),
                child: child.clone(),
                master: None,
                confirmation: None,
                media_source,
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
            retry_observation,
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
        let mut archive_paths = Vec::new();
        // A frozen historical RAW pair may use the opposite logical keys.
        // Recover that mapping from exact resource evidence, never current policy.
        let frozen_key = |provider, checksum: &str, size| -> Result<crate::state::VersionSizeKey> {
            use crate::state::VersionSizeKey;
            let Some(previous) = previous
                .as_ref()
                .filter(|p| p.decision.confirmation.is_some())
            else {
                return Ok(provider);
            };
            if !matches!(
                provider,
                VersionSizeKey::Original | VersionSizeKey::Alternative
            ) {
                return Ok(provider);
            }
            let mut candidates = previous.decision.destinations.iter().filter(|d| {
                matches!(d.version_size.as_str(), "original" | "alternative")
                    && d.checksum == checksum
                    && d.size == size
            });
            let destination = candidates
                .next()
                .context("Frozen RAW resource is missing")?;
            anyhow::ensure!(
                candidates.next().is_none(),
                "Frozen RAW resource is ambiguous"
            );
            VersionSizeKey::from_str(&destination.version_size).context("Invalid frozen RAW key")
        };
        for task in &mut plan.tasks {
            task.version_size = frozen_key(task.version_size, &task.checksum, task.size)?;
        }
        if reason.is_empty() {
            let mut derived_paths = filter::derive_expected_paths(asset, config);
            for derived in &mut derived_paths {
                derived.version_size =
                    frozen_key(derived.version_size, &derived.checksum, derived.size)?;
            }
            for derived in &derived_paths {
                let retained = planner
                    .recorded_selection_path(
                        asset,
                        config,
                        derived,
                        &derived_paths,
                        &SelectionPathEvidence {
                            db: self.capture.db.as_ref(),
                            owner: &self.capture.owner,
                            scope: &self.root.spec.scope,
                            pass_key: key,
                        },
                    )
                    .await?;
                let has_retained = retained.is_some();
                let mut catalog_path = false;
                let mut path = if planner.managed_layout_session().is_some() {
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
                    catalog_path = true;
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
                if planner.managed_layout_session().is_none()
                    && previous
                        .as_ref()
                        .is_none_or(|previous| previous.decision.confirmation.is_none())
                    && !has_retained
                    && self.album_path_claimed(config, &path).await?
                {
                    // A peer's canonical receipt supplies content proof, not
                    // this album's destination or an extra filename qualifier.
                    path = derived.path.clone();
                }
                if planner.managed_layout_session().is_none()
                    && previous
                        .as_ref()
                        .is_none_or(|previous| previous.decision.confirmation.is_none())
                    && (self.album_path_claimed(config, &path).await?
                        || (!has_retained && self.colliding_album_parent(asset, config)))
                {
                    path = planner.independent_album_path(&path, key, config).await?;
                }
                if config.primary_layout_pass_kind == Some(PassKind::Unfiled)
                    && crate::icloud::photos::is_shared_zone(
                        asset.source_zone().unwrap_or(&config.library),
                    )
                    && planner.managed_layout_session().is_none()
                    && !has_retained
                    && !catalog_path
                    && path != derived.path
                    && tokio::fs::try_exists(&derived.path).await?
                {
                    archive_paths.push((derived.version_size, derived.path.clone()));
                }
                let metadata = if previous
                    .as_ref()
                    .is_some_and(|p| p.decision.confirmation.is_some())
                {
                    let provider = filter::provider_version_for_frozen_resource(
                        asset,
                        derived.version_size,
                        &derived.checksum,
                        derived.size,
                    )
                    .context("Frozen rendition resource is unresolved")?;
                    asset.metadata_arc(provider)
                } else {
                    filter::metadata_for_selected_version(asset, config, derived.version_size)
                };
                let metadata_hash = metadata
                    .metadata_hash
                    .clone()
                    .context("Missing selected metadata fingerprint")?;
                records.push(
                    crate::state::AssetRecord::new_pending(
                        Arc::from(asset.source_zone().unwrap_or(&config.library)),
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
        let mut manifest = ActiveDecision {
            state_id: asset.state_id().to_owned(),
            sources,
            decision: SelectionDecision {
                pass_key: key.to_owned(),
                child: asset.asset_record_name().to_owned(),
                master: Some(asset.id().to_owned()),
                confirmation: Some(body),
                media_source: match previous.as_ref() {
                    Some(previous) => previous.decision.media_source.clone(),
                    None => Some(
                        self.albums
                            .get(key)
                            .context("Missing selected media owner")?
                            .selected_media_source(asset)?,
                    ),
                },
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
        let layouts = if planner.managed_layout_session().is_some() {
            let layouts = self
                .capture
                .db
                .cancelled_selection_layouts(
                    self.capture.owner.clone(),
                    self.root.id.clone(),
                    manifest.clone(),
                )
                .await?;
            super::primary_layout::validate_cancelled_selection_layouts(&layouts).await?;
            layouts
        } else {
            Vec::new()
        };
        let mut retried_collision = false;
        let projection = loop {
            let projected = self
                .capture
                .db
                .project_selection_decision_with_layout_proof(
                    self.capture.owner.clone(),
                    self.root.id.clone(),
                    manifest.clone(),
                    records.clone(),
                    fresh.clone(),
                    AdmissionChecks {
                        inventory_observation: retry_observation
                            .as_ref()
                            .filter(|_| {
                                matches!(config.sync_mode, crate::download::SyncMode::Full)
                                    && !config.inventory_reconciliation
                            })
                            .cloned(),
                        capacity: MAX_GENERATION_BYTES,
                        layouts: layouts.clone(),
                        archive_paths: archive_paths.clone(),
                    },
                )
                .await;
            match projected {
                Ok(projection) => break projection,
                Err(error @ crate::state::error::StateError::ProviderWorkConflict(_))
                    if !retried_collision
                        && previous
                            .as_ref()
                            .is_none_or(|previous| previous.decision.confirmation.is_none())
                        && planner.managed_layout_session().is_none() =>
                {
                    let mut changed = false;
                    for (destination, record) in
                        manifest.decision.destinations.iter_mut().zip(&mut records)
                    {
                        if self
                            .album_path_claimed(config, &destination.path.to_path())
                            .await?
                        {
                            let path = planner
                                .independent_album_path(&destination.path.to_path(), key, config)
                                .await?;
                            record.filename = path
                                .file_name()
                                .and_then(|name| name.to_str())
                                .context("Album destination has no filename")?
                                .into();
                            destination.path =
                                SelectionPath::from_path(&std::path::absolute(path)?);
                            changed = true;
                        }
                    }
                    if !changed {
                        return Err(error.into());
                    }
                    retried_collision = true;
                }
                Err(error) => return Err(error.into()),
            }
        };
        if !projection.admitted && manifest.decision.outcome != SelectionOutcome::Excluded {
            tracing::debug!(reason = %projection.reason, "Selection destination admission deferred");
            self.held.store(true, Ordering::Relaxed);
            return Ok(false);
        }
        for destination in &manifest.decision.destinations {
            if let Some(confirmed) = self
                .capture
                .db
                .selection_confirmed_repair(
                    self.capture.owner.clone(),
                    self.root.id.clone(),
                    key.to_owned(),
                    asset.asset_record_name().to_owned(),
                    destination.path.clone(),
                )
                .await?
            {
                // A retained catalog lookup proves the asset still exists, not
                // that it belongs to this pass today. Only the current pass
                // may act on a destination's explicit replacement authority.
                if self.fresh_confirmation {
                    return Ok(false);
                }
                if planner.managed_layout_session().is_none()
                    && let Ok(actual) = crate::download::file::inspect_integrity_fingerprint(
                        &config.directory,
                        &destination.path.to_path(),
                    )
                    .await
                    && actual != confirmed
                    && actual.1 == destination.size
                    && data_encoding::BASE64
                        .decode(destination.checksum.as_bytes())
                        .is_ok_and(|source| {
                            source.len() == 32
                                && data_encoding::HEXLOWER.encode(&source) == actual.0
                        })
                {
                    crate::fs_util::fsync_parent_dir_async(&destination.path.to_path()).await?;
                    // Recovery still requires the existing owner's independent
                    // source/publication proof. A changed fingerprint alone
                    // cannot discharge confirmed repair.
                    self.capture
                        .db
                        .reuse_selection_publication(
                            self.capture.owner.clone(),
                            self.root.id.clone(),
                            key.to_owned(),
                            asset.asset_record_name().to_owned(),
                            destination.path.clone(),
                            actual,
                        )
                        .await?;
                }
                if !plan
                    .tasks
                    .iter()
                    .any(|task| task.version_size.as_str() == destination.version_size)
                {
                    let mut task = filter::selected_resource_tasks(asset, config)
                        .into_iter()
                        .find(|task| {
                            task.checksum.as_ref() == destination.checksum
                                && task.size == destination.size
                                && task.version_size.as_str() == destination.version_size
                        })
                        .context("Confirmed repair has no current selected resource")?;
                    anyhow::ensure!(
                        planner
                            .bind_reserved_destination(&mut task, &destination.path.to_path())
                            .await,
                        "Confirmed repair destination ownership changed"
                    );
                    plan.tasks.push(task);
                }
                continue;
            }
            if planner.managed_layout_session().is_some() {
                // A matching file is not completion of a managed transition:
                // preservation, alias retirement and explicit refresh belong
                // to the journal owner, which acknowledges its binding below.
                continue;
            }
            if let Some(retained) = crate::download::file::retain_pending_file(
                &config.directory,
                &destination.path.to_path(),
            )
            .await?
            {
                let fingerprint = retained.fingerprint;
                crate::fs_util::fsync_parent_dir_async(&destination.path.to_path()).await?;
                let reused = self
                    .capture
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
                if reused {
                    retained.validate().await?;
                    continue;
                }
                let established = self
                    .capture
                    .db
                    .established_selection_path(
                        self.capture.owner.clone(),
                        self.root.spec.scope.clone(),
                        key.to_owned(),
                        asset.asset_record_name().to_owned(),
                        asset.id().to_owned(),
                        destination.version_size.clone(),
                    )
                    .await?;
                if established.as_ref() != Some(&destination.path) {
                    continue;
                }
                let unchanged = self
                    .capture
                    .db
                    .selection_path_matches_local(
                        self.capture.owner.clone(),
                        self.root.spec.scope.clone(),
                        key.to_owned(),
                        (
                            asset.asset_record_name().to_owned(),
                            asset.source_zone().unwrap_or(&config.library).to_owned(),
                        ),
                        destination.path.clone(),
                        (
                            data_encoding::HEXLOWER.encode(&fingerprint.sha256),
                            fingerprint.size,
                        ),
                    )
                    .await?;
                let version = crate::state::VersionSizeKey::from_str(&destination.version_size)
                    .context("Invalid selected local rendition")?;
                let cloud_newer = asset
                    .media_modified_at(version)
                    .zip(retained.modified.map(chrono::DateTime::<chrono::Utc>::from))
                    .is_some_and(|(cloud, local)| cloud > local);
                retained.validate().await?;
                if unchanged || cloud_newer {
                    if let Some(task) = plan
                        .tasks
                        .iter_mut()
                        .find(|task| task.version_size == version)
                    {
                        task.replacement_fingerprint = Some(Arc::new(
                            crate::download::file::FinalPublication::ReplaceLocal {
                                fingerprint,
                                modified: (!unchanged).then_some(retained.modified).flatten(),
                            },
                        ));
                    }
                } else {
                    self.capture
                        .db
                        .preserve_selection_local(
                            self.capture.owner.clone(),
                            self.root.id.clone(),
                            key.to_owned(),
                            asset.asset_record_name().to_owned(),
                            destination.path.clone(),
                            (
                                data_encoding::HEXLOWER.encode(&fingerprint.sha256),
                                fingerprint.size,
                                retained.modified.map(chrono::DateTime::<chrono::Utc>::from),
                            ),
                        )
                        .await?;
                }
            }
        }
        let mut retained_tasks = Vec::with_capacity(plan.tasks.len());
        for mut task in std::mem::take(&mut plan.tasks) {
            if self
                .capture
                .db
                .selection_destination_resolved(
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
            if task.download_path != frozen
                && !planner.bind_reserved_destination(&mut task, &frozen).await
            {
                self.held.store(true, Ordering::Relaxed);
                continue;
            }
            if let Some((checksum, size)) = self
                .capture
                .db
                .selection_confirmed_repair(
                    self.capture.owner.clone(),
                    self.root.id.clone(),
                    key.to_owned(),
                    asset.asset_record_name().to_owned(),
                    destination.path.clone(),
                )
                .await?
            {
                if planner.managed_layout_session().is_some() {
                    // The layout journal owns its pinned input, preservation,
                    // publication and interrupted-publication recovery.
                    retained_tasks.push(task);
                    continue;
                }
                let fingerprint =
                    crate::download::file::fingerprint_downloaded_path(&config.directory, &frozen)
                        .await?;
                anyhow::ensure!(
                    fingerprint.size == size
                        && data_encoding::HEXLOWER.encode(&fingerprint.sha256) == checksum,
                    "Confirmed damaged destination changed; replacement refused"
                );
                task.replacement_fingerprint = Some(Arc::new(
                    crate::download::file::FinalPublication::ReplaceTruncated(fingerprint),
                ));
            }
            retained_tasks.push(task);
        }
        plan.tasks = retained_tasks;
        Ok(true)
    }
}

impl SelectionRun {
    /// Explicit per-copy repair uses the ordinary conditional-publication queue.
    /// Managed destinations remain with their layout journal owner.
    pub(in crate::download) async fn confirmed_repair_plan(
        &self,
        key: &str,
        asset: &PhotoAsset,
        config: &DownloadConfig,
        planner: &mut TaskPlanner,
    ) -> Result<Option<AssetTaskPlan>> {
        if config.primary_layout_active
            || config.edited_naming == crate::types::EditedNaming::Primary
            || planner.managed_layout_session().is_some()
            || filter::is_asset_filtered(asset, config).is_some()
        {
            return Ok(None);
        }
        let Some(previous) = self
            .capture
            .db
            .selection_decision(
                self.capture.owner.clone(),
                self.root.id.clone(),
                key.to_owned(),
                asset.asset_record_name().to_owned(),
            )
            .await?
        else {
            return Ok(None);
        };
        let mut tasks = Vec::new();
        for destination in &previous.decision.destinations {
            if self
                .capture
                .db
                .selection_confirmed_repair(
                    self.capture.owner.clone(),
                    self.root.id.clone(),
                    key.to_owned(),
                    asset.asset_record_name().to_owned(),
                    destination.path.clone(),
                )
                .await?
                .is_none()
            {
                continue;
            }
            let mut task = filter::selected_resource_tasks(asset, config)
                .into_iter()
                .find(|task| {
                    task.version_size.as_str() == destination.version_size
                        && task.checksum.as_ref() == destination.checksum
                        && task.size == destination.size
                })
                .context(
                    "Confirmed repair provider resource changed; inspect again before replacement",
                )?;
            task.download_path = destination.path.to_path();
            self.capture
                .db
                .guard_primary_slot(
                    &task.library,
                    &task.asset_id,
                    task.version_size.as_str(),
                    &task.checksum,
                    &task.download_path,
                )
                .await?;
            anyhow::ensure!(
                super::super::legacy_preservation::current_path_is_independent(
                    self.capture.db.as_ref(),
                    &task.download_path,
                )
                .await?,
                "Confirmed repair path is protected historical media"
            );
            anyhow::ensure!(
                planner
                    .bind_reserved_destination(&mut task, &destination.path.to_path())
                    .await,
                "Confirmed repair destination ownership changed"
            );
            tasks.push(task);
        }
        if tasks.is_empty() {
            return Ok(None);
        }
        Ok(Some(AssetTaskPlan {
            tasks,
            filter_reason: None,
            malformed_resource: None,
        }))
    }
}

async fn discover_missing_files(
    context: &SelectionContext,
    generation: &str,
    directory: &std::path::Path,
    cancel: Option<&CancellationToken>,
) -> Result<()> {
    let mut after = None;
    loop {
        if cancel.is_some_and(CancellationToken::is_cancelled) {
            return Ok(());
        }
        let files = context
            .capture
            .db
            .completed_selection_files(context.capture.owner.clone(), generation.to_owned(), after)
            .await?;
        let Some(last) = files.last() else {
            return Ok(());
        };
        after = Some((
            last.pass_key.clone(),
            last.child.clone(),
            last.version.clone(),
            last.path.key()?,
        ));
        for file in files {
            if cancel.is_some_and(CancellationToken::is_cancelled) {
                return Ok(());
            }
            let Ok(path) =
                crate::fs_util::reserved_path_under_root(directory, &file.path.to_path())
            else {
                continue;
            };
            match tokio::fs::symlink_metadata(&path).await {
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    context
                        .capture
                        .db
                        .reopen_missing_selection_file(
                            context.capture.owner.clone(),
                            generation.to_owned(),
                            file,
                        )
                        .await?;
                }
                Err(error) => {
                    return Err(error).context("Could not inspect recorded selected file");
                }
            }
        }
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
    if context
        .capture
        .db
        .has_album_directory_debt(
            context.capture.owner.clone(),
            config.library.to_string(),
            passes
                .iter()
                .filter(|pass| pass.kind == PassKind::Album)
                .filter_map(|pass| pass.album.container_id().map(str::to_owned))
                .collect(),
        )
        .await?
    {
        return Ok(Some(true));
    }
    let Some(root) = context.current().await? else {
        return Ok(Some(true));
    };
    discover_missing_files(&context, &root.id, &config.directory, None).await?;
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
            context.scope.clone(),
            context.config_hash.clone(),
            root.id,
        )
        .await?
        .is_some();
    Ok(Some(
        (pending || retained)
            && (!config.post_sync_metadata || context.checkpoint_debt(config).await?),
    ))
}

pub(super) async fn inventory(
    client: &Client,
    passes: &[AlbumPass],
    config: &Arc<DownloadConfig>,
    controls: DownloadControls,
    cancel: CancellationToken,
) -> Result<SyncResult> {
    if config.primary_layout_active
        && let Err(error) = super::primary_layout::validate_recorded_sources(config).await
    {
        tracing::warn!(%error, "Managed source recovery holds selected inventory");
        let mut result = SyncResult::from_execution(
            DownloadOutcome::PartialFailure { failed_count: 1 },
            None,
            SyncStats {
                failed: 1,
                enumeration_incomplete: true,
                ..SyncStats::default()
            },
        );
        result.block_incremental_token(DEFERRED_REASON);
        return Ok(result);
    }
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
    let mut retained = if config.primary_layout_active {
        crate::download::pipeline::StreamingResult {
            enumeration_complete: true,
            ..crate::download::pipeline::StreamingResult::default()
        }
    } else {
        Box::pin(recover_retained(
            client,
            passes,
            config,
            controls,
            cancel.clone(),
            &run.root,
        ))
        .await?
    };
    if retained.provider_auth_errors > 0 || retained.storage_failed {
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
    if config.primary_layout_active
        && !result.stats.storage_failed
        && !cancel.is_cancelled()
        && !run.session_expired.load(Ordering::Relaxed)
    {
        // Current authenticated selection can discharge only exactly proven
        // cancelled layout obligations before the historical scheduler runs.
        retained = Box::pin(recover_retained(
            client,
            passes,
            config,
            controls,
            cancel.clone(),
            &run.root,
        ))
        .await?;
    }
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
    if config.primary_layout_active
        && !cancel.is_cancelled()
        && !run.session_expired.load(Ordering::Relaxed)
        && !result.stats.storage_failed
    {
        let execution = context.replay_run(run.root.clone(), passes, false);
        let mut completed = crate::download::pipeline::StreamingResult {
            enumeration_complete: true,
            ..crate::download::pipeline::StreamingResult::default()
        };
        replay_destinations(
            client,
            passes,
            config,
            controls,
            &cancel,
            &execution,
            &mut completed,
        )
        .await?;
        let completed = crate::download::pipeline::build_download_result(
            client,
            passes,
            config,
            controls,
            completed,
            Instant::now(),
            cancel.clone(),
        )
        .await?;
        result.outcome = merge_download_outcomes(&result.outcome, &completed.outcome);
        result.accumulate(&completed);
        if execution.held.load(Ordering::Relaxed) {
            run.held.store(true, Ordering::Relaxed);
        }
    }
    let metadata = if result.stats.storage_failed || config.post_sync_metadata {
        MetadataTailOutcome::default()
    } else {
        metadata_tail(config, &cancel, context, &run.root.id).await?
    };
    result.stats.exif_failures += metadata.failed;
    if metadata.storage_failed {
        // The checkpoint projection below owns the reported flag, so record
        // the metadata drain's storage evidence before it runs.
        result.checkpoint.storage_failed = true;
        result.stats.storage_failed = true;
    }
    if metadata.failed > 0 {
        result.outcome = merge_download_outcomes(
            &result.outcome,
            &DownloadOutcome::PartialFailure {
                failed_count: metadata.failed,
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
    result.stats.selected_work_remaining = context.checkpoint_debt(config).await?;
    if result.stats.selected_work_remaining {
        result.block_incremental_token(DEFERRED_REASON)
    }
    result.checkpoint.project(&mut result.stats);
    let mut membership_delta = context
        .inventory_membership_delta
        .lock()
        .map_err(|_poisoned| anyhow::anyhow!("Membership observation lock poisoned"))?
        .take()
        .unwrap_or_default();
    context
        .include_durable_membership_deletions(&mut membership_delta)
        .await?;
    if !result.checkpoint.sync_token_blocked
        && !result.checkpoint.identity_incomplete
        && !result.checkpoint.storage_failed
        && covered
        && !context
            .reconcile_membership(passes, config, &membership_delta, &cancel)
            .await?
    {
        result.sync_token = None;
        result.block_incremental_token("membership_layout_unresolved");
    }
    if covered
        && query_checkpoint_ready
        && !result.checkpoint.sync_token_blocked
        && result.stats.same_cycle_recovery_attempts == 0
        && !config.provisional
    {
        result.checkpoint.scanned_generation = Some((
            super::config::inventory_scope(passes, config),
            run.root.id.clone(),
        ));
    }
    Ok(result)
}

const METADATA_WRITER_BUDGET: usize = 500;

#[cfg(test)]
tokio::task_local! {
    // Scoped to one fixture future, so parallel tests retain production limits.
    pub(super) static TEST_METADATA_WRITER_BUDGET: usize;
    pub(super) static TEST_AFTER_WRITER_SNAPSHOT: fn(&std::path::Path);
}

fn metadata_writer_limits() -> (usize, usize) {
    let budget = METADATA_WRITER_BUDGET;
    #[cfg(test)]
    let budget = TEST_METADATA_WRITER_BUDGET
        .try_with(|budget| *budget)
        .unwrap_or(budget);
    (budget / 2, budget)
}

#[cfg(test)]
#[test]
fn metadata_writer_default_limits_reserve_250_within_500() {
    assert_eq!(metadata_writer_limits(), (250, 500));
}

#[derive(Debug, Default)]
struct MetadataTailOutcome {
    failed: usize,
    /// At least one metadata-only write hit operational storage failure.
    storage_failed: bool,
}

/// Keep the existing 500-row writer budget while rotating past current failed
/// or option-disabled physical writers. This cursor schedules retries only;
/// debt/progress remain in the exact publication and metadata receipts.
async fn metadata_tail(
    config: &DownloadConfig,
    cancel: &CancellationToken,
    context: &SelectionContext,
    generation: &str,
) -> Result<MetadataTailOutcome> {
    let Some(db) = &config.state_db else {
        return Ok(MetadataTailOutcome::default());
    };
    let flags = crate::download::pipeline::MetadataFlags::from(config);
    if !flags.has_any_write() {
        return Ok(MetadataTailOutcome::default());
    }
    if !config.primary_layout_active
        && !config.refresh_metadata
        && config.capture_timestamp_repair == crate::download::CaptureTimestampRepair::Preserve
    {
        reuse_writer_outputs(
            config,
            cancel,
            context,
            generation,
            flags,
            None,
            &mut Default::default(),
        )
        .await?;
    }
    let mut offset = context
        .capture
        .db
        .selection_metadata_retry_offset(context.capture.owner.clone())
        .await?;
    // Reserve at most half the existing 500-row budget for current exact
    // destinations. The remaining budget rotates through the same current
    // selection, or the explicit capture-repair queue when requested.
    let (current_reserve, budget) = metadata_writer_limits();
    let current = context
        .capture
        .db
        .pending_selection_metadata(
            context.capture.owner.clone(),
            generation.to_owned(),
            current_reserve,
        )
        .await?;
    let attempted_paths: std::collections::HashSet<_> = current
        .iter()
        .filter_map(|pending| pending.asset.local_path.clone())
        .collect();
    let priority = Box::pin(crate::download::metadata_rewrite::run_pending_rows(
        db.as_ref(),
        flags,
        config.capture_timestamp_repair,
        config.temp_suffix.clone(),
        cancel,
        current,
    ))
    .await;
    let remaining = budget.saturating_sub(priority.fetched);
    let original = offset;
    let library_scope = [config.library.as_ref()];
    let repair_scope = matches!(
        config.capture_timestamp_repair,
        crate::download::CaptureTimestampRepair::ReplaceWithCaptureLocal
    )
    .then_some(library_scope.as_slice());
    let attempted_paths = &attempted_paths;
    let remaining_page = |offset| async move {
        if attempted_paths.is_empty() && repair_scope.is_some() {
            return crate::download::metadata_rewrite::run_pending_budget(
                db.as_ref(),
                flags,
                config.capture_timestamp_repair,
                config.temp_suffix.clone(),
                cancel,
                repair_scope,
                (offset, remaining),
            )
            .await;
        }
        let queue = if repair_scope.is_some() {
            crate::state::db::MetadataRewriteQueue::CaptureRepair
        } else {
            crate::state::db::MetadataRewriteQueue::Ordinary
        };
        let pending = if repair_scope.is_some() {
            db.get_pending_metadata_rewrites_page_for_queue(queue, repair_scope, offset, remaining)
                .await
        } else {
            context
                .capture
                .db
                .pending_selection_metadata_page(
                    context.capture.owner.clone(),
                    generation.to_owned(),
                    offset,
                    remaining,
                )
                .await
        };
        let mut pending = match pending {
            Ok(pending) => pending,
            Err(error) => {
                tracing::warn!(%error, "Failed to load current metadata rewrites");
                return crate::download::metadata_rewrite::RewritePass {
                    failed: 1,
                    ..Default::default()
                };
            }
        };
        let fetched = pending.len();
        // A failed priority write remains queued, but is not a second attempt
        // in this invocation. A newer marker on that path also stays durable.
        pending.retain(|row| {
            row.asset
                .local_path
                .as_ref()
                .is_none_or(|path| !attempted_paths.contains(path))
        });
        let mut result = Box::pin(crate::download::metadata_rewrite::run_pending_rows(
            db.as_ref(),
            flags,
            config.capture_timestamp_repair,
            config.temp_suffix.clone(),
            cancel,
            pending,
        ))
        .await;
        result.fetched = fetched;
        result
    };
    let mut result = remaining_page(offset).await;
    if result.fetched == 0 && offset > 0 && !cancel.is_cancelled() {
        offset = 0;
        result = remaining_page(0).await;
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
    Ok(MetadataTailOutcome {
        failed: priority.failed.saturating_add(result.failed),
        storage_failed: priority.storage_failed || result.storage_failed,
    })
}

async fn reuse_writer_outputs(
    config: &DownloadConfig,
    cancel: &CancellationToken,
    context: &SelectionContext,
    generation: &str,
    flags: crate::download::pipeline::MetadataFlags,
    deadline: Option<Instant>,
    after: &mut crate::state::db::provider_generations::WriterKey,
) -> Result<()> {
    use crate::state::db::provider_generations::WriterOutput;
    let db = &context.capture.db;
    let owner = &context.capture.owner;
    while !cancel.is_cancelled() && deadline.is_none_or(|due| Instant::now() < due) {
        let page = db
            .writer_output_candidates(owner.clone(), generation.to_owned(), after.clone())
            .await?;
        if page.is_empty() {
            break;
        }
        for key in page {
            if cancel.is_cancelled() || deadline.is_some_and(|due| Instant::now() >= due) {
                return Ok(());
            }
            *after = key.clone();
            let path = key.native_path()?;
            // Missing, changed or unreadable files go through the ordinary bounded writer.
            let Ok(media) =
                crate::download::file::WriterSnapshot::read(&config.directory, &path).await
            else {
                continue;
            };
            let sidecar = if flags.contains(crate::download::pipeline::MetadataFlags::XMP_SIDECAR) {
                let mut sidecar = path.as_os_str().to_owned();
                sidecar.push(".xmp");
                let Ok(packet) = crate::download::file::WriterSnapshot::read(
                    &config.directory,
                    std::path::Path::new(&sidecar),
                )
                .await
                else {
                    continue;
                };
                Some(packet)
            } else {
                None
            };
            #[cfg(test)]
            let _ = TEST_AFTER_WRITER_SNAPSHOT.try_with(|edit| edit(&path));
            let packet = sidecar.as_ref().map_or(WriterOutput::EmbeddedOnly, |file| {
                WriterOutput::Sidecar(file.fingerprint.sha256)
            });
            db.reuse_writer_output(
                owner.clone(),
                generation.to_owned(),
                key,
                data_encoding::HEXLOWER.encode(&media.fingerprint.sha256),
                packet,
                move || {
                    sidecar
                        .as_ref()
                        .is_none_or(|file| file.still_matches().is_ok())
                        && media.still_matches().is_ok()
                },
            )
            .await?;
        }
    }
    Ok(())
}

/// Scheduling hints only. Losing this cursor cannot settle a receipt.
#[derive(Default)]
pub(crate) struct BackfillCursor {
    generation: Option<String>,
    reuse_after: crate::state::db::provider_generations::WriterKey,
    offset: usize,
}

/// Reconstruct writer scope locally, including after a quiet restart.
pub(crate) async fn drain_local_backfill(
    passes: &[AlbumPass],
    config: &DownloadConfig,
    db: &crate::state::SqliteStateDb,
    controls: DownloadControls,
    cancel: &CancellationToken,
    deadline: Option<Instant>,
    cursor: &mut BackfillCursor,
) -> Result<super::maintenance::MetadataDrainOutcome> {
    use super::maintenance::MetadataDrainOutcome;
    let mut outcome = MetadataDrainOutcome::default();
    let flags = crate::download::pipeline::MetadataFlags::from(config);
    if !config.post_sync_metadata
        || !flags.has_any_write()
        || !controls.run_mode.downloads_files()
        || cancel.is_cancelled()
    {
        return Ok(outcome);
    }
    let context = context(passes, config, controls).await?;
    let selection = if let Some(context) = context {
        context
            .capture
            .db
            .local_writer_generation(
                context.capture.owner.clone(),
                context.scope.clone(),
                context.config_hash.clone(),
            )
            .await?
            .map(|root| (context, root))
    } else {
        None
    };
    let generation = selection.as_ref().map(|(_, root)| root.id.clone());
    if generation != cursor.generation {
        *cursor = BackfillCursor {
            generation,
            ..Default::default()
        };
    }
    if cursor.offset == 0
        && let Some((context, root)) = &selection
        && config.edited_naming != crate::types::EditedNaming::Primary
        && !db.has_primary_layouts(&config.library).await?
    {
        reuse_writer_outputs(
            config,
            cancel,
            context,
            &root.id,
            flags,
            deadline,
            &mut cursor.reuse_after,
        )
        .await?;
    }
    let mut applied = 0usize;
    let mut started = false;
    let root_directory = crate::fs_util::absolute_confined_path(&config.directory)?;
    let (_, limit) = metadata_writer_limits();
    while limit > 0 && !cancel.is_cancelled() && deadline.is_none_or(|due| Instant::now() < due) {
        let mut rows = if let Some((context, root)) = &selection {
            db.pending_local_metadata_page(
                context.capture.owner.clone(),
                root.id.clone(),
                config.library.to_string(),
                cursor.offset,
                limit,
            )
            .await?
        } else {
            db.pending_legacy_metadata_page(config.library.to_string(), cursor.offset, limit)
                .await?
        };
        if rows.is_empty() {
            cursor.offset = 0;
            cursor.reuse_after = Default::default();
            break;
        }
        if !started {
            tracing::info!("Primary sync finished; local metadata backfill pending");
            started = true;
        }
        let fetched = rows.len();
        rows.retain(|row| {
            row.asset.local_path.as_deref().is_some_and(|path| {
                crate::fs_util::absolute_confined_path(path)
                    .is_ok_and(|path| path.starts_with(&root_directory))
            })
        });
        let pass = Box::pin(crate::download::metadata_rewrite::run_backfill_rows(
            db,
            flags,
            config.temp_suffix.clone(),
            cancel,
            rows,
            cursor.generation.as_deref(),
        ))
        .await;
        applied += pass.retired_from_selected_queue;
        outcome.failed += fetched.saturating_sub(pass.retired_from_selected_queue);
        outcome.storage_failed |= pass.storage_failed;
        anyhow::ensure!(
            !pass.state_failed,
            "Metadata backfill stopped after a state error; unfinished receipts remain queued"
        );
        if outcome.storage_failed {
            break;
        }
        // Retired rows disappeared. Move past retained rows without wrapping
        // this invocation, even if an entire page failed to make progress.
        cursor.offset += fetched.saturating_sub(pass.retired_from_selected_queue);
    }
    if applied > 0 || outcome.failed > 0 {
        tracing::info!(
            applied,
            remaining_attempted = outcome.failed,
            "Local metadata backfill pass finished"
        );
    }
    Ok(outcome)
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
    let mut retained_profile = root.spec.profile.clone();
    retained_profile
        .as_object_mut()
        .context("Retained selection profile is not an object")?
        .insert(INDEPENDENT_ALBUM_COPIES.to_owned(), Value::Bool(true));
    anyhow::ensure!(
        retained_profile == current.spec.profile
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
        'decision: for decision in decisions
            .iter()
            .filter(|decision| decision.decision.pass_key == key)
        {
            // A managed planner can persist its journal before projection. A
            // catalog lookup cannot establish today's album membership, so
            // leave confirmed repair to the current pass before planning.
            for destination in &decision.decision.destinations {
                if context
                    .capture
                    .db
                    .selection_confirmed_repair(
                        context.capture.owner.clone(),
                        root.id.clone(),
                        key.clone(),
                        decision.decision.child.clone(),
                        destination.path.clone(),
                    )
                    .await?
                    .is_some()
                {
                    continue 'decision;
                }
            }
            let mut asset = match (&decision.decision.confirmation, &decision.decision.master) {
                (Some(body), Some(master)) => Some(current_asset(
                    body,
                    &decision.decision.child,
                    master,
                    decision.media_zone(&root),
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
            if asset.is_none()
                && let Some(binding) = &decision.decision.media_source
            {
                let current = tokio::select! {
                    biased;
                    () = cancel.cancelled() => {
                        combined.enumeration_complete = false;
                        break;
                    }
                    current = pass.album.confirm_selection_identity(&decision.decision.child, binding) => current,
                };
                match current {
                    Ok(current) => asset = Some(current.asset),
                    Err(error) => {
                        combined.enumeration_errors += 1;
                        combined.enumeration_complete = false;
                        run.held.store(true, Ordering::Relaxed);
                        if matches!(
                            classify_selection_confirmation_error(&error),
                            SelectionConfirmationErrorClass::SessionExpired
                        ) {
                            combined.provider_auth_errors += 1;
                            run.session_expired.store(true, Ordering::Relaxed);
                            break;
                        }
                    }
                }
            }
            if let Some(asset) = asset {
                assets.push(Ok(
                    asset.with_state_record_name(Arc::from(decision.state_id.clone()))
                ));
            } else {
                run.defer_confirmation(&key, decision.decision.child.clone(), None)
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
        run.remember_retry_sources(&result.failed, passes).await?;
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
    let local_only = config.post_sync_metadata
        && root.sealed
        && root.checkpoint_ready
        && !context.checkpoint_debt(config).await?;
    let mut combined = if local_only {
        crate::download::pipeline::StreamingResult {
            enumeration_complete: true,
            ..Default::default()
        }
    } else {
        let mut primary = Box::pin(recover_retained(
            client,
            passes,
            config,
            controls,
            cancel.clone(),
            &root,
        ))
        .await?;
        replay_destinations(
            client,
            passes,
            config,
            controls,
            &cancel,
            &run,
            &mut primary,
        )
        .await?;
        primary
    };
    if !combined.storage_failed && !config.post_sync_metadata {
        let metadata = metadata_tail(config, &cancel, context, &run.root.id).await?;
        combined.exif_failures += metadata.failed;
        combined.storage_failed |= metadata.storage_failed;
    }
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
            Some(super::models::DATE_BOUNDED_FULL_ENUMERATION_REASON) => {
                super::models::DATE_BOUNDED_FULL_ENUMERATION_REASON
            }
            _ => DEFERRED_REASON,
        });
    }
    result.stats.selected_work_remaining = context.checkpoint_debt(config).await?;
    if result.stats.selected_work_remaining {
        result.block_incremental_token(DEFERRED_REASON)
    }
    Ok(result)
}

async fn replay_destinations(
    client: &Client,
    passes: &[AlbumPass],
    config: &Arc<DownloadConfig>,
    controls: DownloadControls,
    cancel: &CancellationToken,
    run: &Arc<SelectionRun>,
    combined: &mut crate::download::pipeline::StreamingResult,
) -> Result<()> {
    let root = &run.root;
    for pass in passes {
        if cancel.is_cancelled() || combined.provider_auth_errors > 0 || combined.storage_failed {
            combined.enumeration_complete = false;
            break;
        }
        let key = pass_key(pass);
        let mut after = None;
        loop {
            if cancel.is_cancelled() || combined.provider_auth_errors > 0 || combined.storage_failed
            {
                combined.enumeration_complete = false;
                break;
            }
            let decisions = run
                .capture
                .db
                .pending_selection_decisions(
                    run.capture.owner.clone(),
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
                        decision.media_zone(root),
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
            run.remember_retry_sources(&result.failed, passes).await?;
            super::models::merge_streaming_result(combined, result);
        }
    }
    Ok(())
}

/// Reuse the existing completed-delta/full-selection veto composition. The
/// only offered successor is the actual completed source stream's successor.
pub(super) async fn after_delta(
    client: &Client,
    passes: &[AlbumPass],
    config: &Arc<DownloadConfig>,
    controls: DownloadControls,
    cancel: CancellationToken,
    mut delta: IncrementalDeltaSummary,
) -> Result<SyncResult> {
    let context = config
        .selection_context
        .as_ref()
        .context("Missing selection context")?;
    let mut stats = SyncStats {
        state_write_failures: delta.state_transition_failures,
        identity_incomplete: delta.identity_incomplete,
        interrupted: cancel.is_cancelled(),
        ..SyncStats::default()
    };
    stats.accumulate(&delta.sparse_target_stats);
    if let Some(reason) = delta.token_unsafe_reason {
        block_sync_token_for_incremental_delta(&mut stats, reason)
    }
    let completed_delta_replay = delta.raw_records_observed.zip(delta.sync_token.clone());
    let mut delta_result = SyncResult::from_incremental_execution(
        if delta.state_transition_failures > 0 {
            DownloadOutcome::PartialFailure {
                failed_count: delta.state_transition_failures,
            }
        } else {
            DownloadOutcome::Success
        },
        None,
        stats,
        std::mem::take(&mut delta.sparse_identity_proofs),
        completed_delta_replay,
    );
    context
        .include_durable_membership_deletions(&mut delta.membership)
        .await?;
    if delta.membership.incomplete
        || ((!delta.membership.relations.is_empty() || !delta.membership.containers.is_empty())
            && (delta.sync_token.is_none() || delta.token_unsafe_reason.is_some()))
    {
        delta_result.block_incremental_token("membership_delta_incomplete");
        return Ok(delta_result);
    }
    let delta_safe = !delta_result.checkpoint.sync_token_blocked
        && !delta_result.checkpoint.identity_incomplete
        && !delta_result.checkpoint.interrupted
        && delta_result.checkpoint.state_write_failures == 0;
    if config.inventory_reconciliation {
        // The dispatcher already owns exactly one current inventory between
        // these two delta passes. Do not start another implicit generation scan
        // or replay here. Its durable request and retained source cursor remain
        // until full selected coverage plus a raw-empty replay qualify; a
        // changed replay cannot advance past newly observed unadmitted work.
        if delta_safe {
            delta_result.sync_token = delta.sync_token.clone();
            *context
                .inventory_membership_delta
                .lock()
                .map_err(|_poisoned| anyhow::anyhow!("Membership observation lock poisoned"))? =
                Some(delta.membership);
        }
        return Ok(delta_result);
    }
    let root = context.current().await?;
    if let Some(root) = &root {
        discover_missing_files(context, &root.id, &config.directory, Some(&cancel)).await?;
    }
    // Smart membership and foreign media facts are not covered by the owner's
    // zone delta. Refresh their current queries through the same inventory path.
    let refresh_selection = passes.iter().any(|pass| {
            pass.kind == PassKind::SmartFolder || pass.album.has_cross_zone_hydration()
        })
        || !delta.membership.relations.is_empty()
        || !delta.membership.containers.is_empty()
        || (config.primary_layout_active
            && (config.refresh_metadata
                || match config.state_db.as_deref() {
                    Some(db) => !db
                        .primary_layout_operations(config.library.to_string())
                        .await?
                        .is_empty(),
                    None => false,
                }))
        // A due source can refresh a completed target, but cannot restart
        // a deferred target generation or bypass its local recovery work.
        || (config.sparse_membership_due && !config.primary_layout_active
            && root.as_ref().is_some_and(|root| root.sealed && root.checkpoint_ready)
            && !context.capture.db.unfinished_selection_debt(
                context.capture.owner.clone(), context.scope.clone(), context.config_hash.clone(),
            ).await?);
    let indexing_replay = matches!(
        config.indexing_inventory_cursor,
        Some(IndexingWindow::AwaitReplay)
    );
    // Replaying a sealed generation cannot supply a missing pass token.
    let retry_pass_tokens = indexing_replay
        && root.as_ref().is_some_and(|root| {
            matches!(
                root.checkpoint_veto.as_deref(),
                Some(
                    "icloud_sync_token_mismatch"
                        | "icloud_sync_token_missing"
                        | "icloud_blank_sync_token"
                        | "kei_internal_token_receiver_dropped"
                )
            )
        });
    let mut selected = match root {
        Some(root)
            if !refresh_selection
                && !retry_pass_tokens
                && (root.sealed
                    || context
                        .capture
                        .db
                        .selection_waiting_for_retry(context.capture.owner.clone(), root.id.clone())
                        .await?) =>
        {
            replay(client, passes, config, controls, cancel.clone(), root).await?
        }
        _ => {
            let mut config = Arc::clone(config);
            if indexing_replay
                && delta_safe
                && delta_result.checkpoint.completed_delta_replay
                && let Some(cursor) = &delta.sync_token
            {
                Arc::make_mut(&mut config).indexing_inventory_cursor =
                    Some(IndexingWindow::ReplayedTo(cursor.clone()));
            }
            inventory(client, passes, &config, controls, cancel.clone()).await?
        }
    };
    selected.outcome = merge_download_outcomes(&selected.outcome, &delta_result.outcome);
    selected.accumulate(&delta_result);
    if let Some(reason) = delta.token_unsafe_reason {
        // The observed delta refusal is more specific than a missing token
        // caused by the subsequent selected inventory. Keep every safety gate.
        selected.stats.sync_token_blocked_reason = None;
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
        if context
            .reconcile_membership(passes, config, &delta.membership, &cancel)
            .await?
        {
            selected.sync_token = delta.sync_token;
        } else {
            selected.block_incremental_token("membership_layout_unresolved");
        }
    }
    Ok(selected)
}
