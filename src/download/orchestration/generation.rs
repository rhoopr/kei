//! Private selection generation composition using existing selection and queues.

use crate::state::db::provider_generations::observation::ObservationProof;
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
use crate::state::db::MetadataRewriteStore;
use crate::state::db::provider_generations::{
    ActiveDecision, ActiveGeneration, GenerationSpec, MAX_GENERATION_BYTES,
};
use crate::state::db::provider_selection::{
    SelectionDecision, SelectionDestination, SelectionOutcome, SelectionPath,
};

const MAX_REPLAY_DECISIONS: u32 = 64;
const DEFERRED_REASON: &str = "selection_generation_deferred";

#[cfg(test)]
tokio::task_local! {
    pub(super) static TEST_GENERATION_FORMAT: u8;
}

fn generation_format() -> u8 {
    #[cfg(test)]
    if let Ok(format) = TEST_GENERATION_FORMAT.try_with(|format| *format) {
        return format;
    }
    2
}

#[derive(Debug, thiserror::Error)]
#[error("Selected identity retry is not due")]
pub(in crate::download) struct SelectionRetryDeferred;

#[derive(Debug, thiserror::Error)]
#[error("Current retry lacks selected rendition")]
struct SelectionReferenceUnavailable;

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
    format: u8,
    capture: ShadowCapture,
    scope: String,
    zone: Value,
    config_hash: String,
    profile: Value,
    source_contexts: Vec<SelectionContext>,
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
        if let Some((capture, scope, zone)) = pass.album.download_scope()? {
            capture
                .db
                .validate_download_scope(
                    capture.owner,
                    scope,
                    zone.get("zoneName")
                        .and_then(Value::as_str)
                        .context("Missing provider zone name")?
                        .to_owned(),
                )
                .await?;
        }
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
    if config
        .selection_context
        .as_ref()
        .is_some_and(|context| context.format == 2)
    {
        return Ok(());
    }
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
    let format = generation_format();
    let Some(mut context) = context_for_format(passes, config, controls, format).await? else {
        return Ok(None);
    };
    if format == 2 {
        let mut sources = BTreeMap::new();
        for pass in passes {
            for (_, scope, zone) in pass.album.selector_sources()? {
                let name = zone
                    .get("zoneName")
                    .and_then(Value::as_str)
                    .context("Missing selected source zone")?;
                if name != config.library.as_ref() {
                    sources.insert(name.to_owned(), scope);
                }
            }
        }
        for (name, _) in sources {
            let mut selected = Vec::new();
            for pass in passes {
                if pass
                    .album
                    .selector_sources()?
                    .iter()
                    .any(|(_, _, zone)| zone.get("zoneName").and_then(Value::as_str) == Some(&name))
                {
                    selected.push(pass.clone());
                }
            }
            let mut child = context_for_source(&selected, config, controls, format, Some(&name))
                .await?
                .context("Selected source could not bind download ledger")?;
            child.retry_sources = Arc::clone(&context.retry_sources);
            context.source_contexts.push(child);
        }
    }
    Ok(Some(context))
}

/// Compare the exact current selector/source plans with an existing checkpoint.
/// Constructing contexts here neither admits work nor grants coverage.
pub(crate) async fn has_current_observation(
    passes: &[AlbumPass],
    config: &DownloadConfig,
    controls: DownloadControls,
    token_key: &str,
) -> Result<bool> {
    let Some(context) = context(passes, config, controls).await? else {
        return Ok(false);
    };
    let specs = std::iter::once(&context)
        .chain(context.source_contexts.iter())
        .map(|source| source.spec(config, source.config_hash.clone()))
        .collect();
    Ok(context
        .capture
        .db
        .current_observation_matches(context.capture.owner.clone(), token_key.to_owned(), specs)
        .await?)
}

async fn context_for_format(
    passes: &[AlbumPass],
    config: &DownloadConfig,
    controls: DownloadControls,
    format: u8,
) -> Result<Option<SelectionContext>> {
    context_for_source(passes, config, controls, format, None).await
}

async fn context_for_source(
    passes: &[AlbumPass],
    config: &DownloadConfig,
    controls: DownloadControls,
    format: u8,
    source: Option<&str>,
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
    let mut selector_binding = None;
    let mut profile = Vec::new();
    for pass in passes {
        if format == 1 && !matches!(pass.kind, PassKind::Album | PassKind::Unfiled) {
            return Ok(None);
        }
        let scope_owner = if format == 1 {
            pass.album.private_selection_scope()?
        } else {
            let scopes = pass.album.selector_sources()?;
            if let Some(source) = source {
                let (selector, selector_scope, selector_zone) =
                    scopes.first().context("Missing selector owner")?;
                anyhow::ensure!(
                    selector_zone.get("zoneName").and_then(Value::as_str)
                        == Some(config.library.as_ref()),
                    "Selector library mismatch"
                );
                let (account, provider) = selector.owner.authenticated_keys()?;
                let binding = json!({"version":2,"account_key":account,"provider_key":provider,
                    "scope":serde_json::from_str::<Value>(selector_scope)?,"zone":selector_zone});
                anyhow::ensure!(
                    selector_binding
                        .as_ref()
                        .is_none_or(|previous| previous == &binding),
                    "Mixed selector owners"
                );
                selector_binding = Some(binding);
                scopes.into_iter().find(|(_, _, zone)| {
                    zone.get("zoneName").and_then(Value::as_str) == Some(source)
                })
            } else {
                scopes.into_iter().next()
            }
        };
        let Some((capture, scope, zone)) = scope_owner else {
            return Ok(None);
        };
        if zone.get("zoneName").and_then(Value::as_str)
            != Some(source.unwrap_or(config.library.as_ref()))
        {
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
    let mut profile = json!({"format":format,"coverage":if format == 1 { "observed_selection_window" } else { "selected_downloads" },"passes":profile,
        "recent":config.recent,"recent_scope":format!("{:?}",config.recent_scope),"excludes":excludes});
    if let Some(binding) = &selector_binding {
        profile
            .as_object_mut()
            .context("Invalid selection profile")?
            .insert("selector_binding".into(), binding.clone());
    }
    let anchored_root = SelectionPath::from_path(&std::path::absolute(&config.directory)?);
    let policy = |paths: String| {
        let mut policy = json!({"format":1,"scope":scope,"zone":zone,
            "paths":paths,"anchored_root":anchored_root,
            "before":config.skip_created_before.map(crate::config::CreatedDateFilter::fingerprint),"after":config.skip_created_after.map(crate::config::CreatedDateFilter::fingerprint),
            "enum_hash":config.enum_config_hash.as_deref(),"metadata_flags":crate::download::pipeline::MetadataFlags::from(config).bits(),
            "capture_revision":crate::state::METADATA_CAPTURE_REVISION,"capture_repair":format!("{:?}",config.capture_timestamp_repair),
            "repair_truncated":config.repair_truncated,"attempt_limit":config.max_download_attempts,
        });
        if let (Some(binding), Some(object)) = (&selector_binding, policy.as_object_mut()) {
            object.insert("selector_binding".into(), binding.clone());
        }
        policy
    };
    if format == 2 {
        // Folder templates cannot revoke already admitted destinations. Keep
        // all eligibility, rendition, metadata and archive-root guards.
        let mut retained_policy = config.clone();
        retained_policy.folder_structure.clear();
        retained_policy.folder_structure_albums = Arc::from("");
        retained_policy.folder_structure_smart_folders = Arc::from("");
        profile
            .as_object_mut()
            .ok_or_else(|| anyhow::anyhow!("Invalid selection profile"))?
            .insert(
                "retry_policy".into(),
                json!(digest(&serde_json::to_vec(&policy(hash_download_config(
                    &retained_policy
                )))?)),
            );
        // Admission freezes destinations, not current selection eligibility.
        // Keep root/rendition/metadata policy strict while allowing membership,
        // date and filename filters to change. Current filters are checked on
        // replay and can defer, never retire, an older obligation.
        retained_policy.skip_created_before = None;
        retained_policy.skip_created_after = None;
        retained_policy.filename_exclude = Arc::from([]);
        let mut retry_policy = policy(hash_download_config(&retained_policy));
        for key in ["before", "after", "enum_hash"] {
            *retry_policy
                .get_mut(key)
                .ok_or_else(|| anyhow::anyhow!("Invalid retry policy"))? = Value::Null;
        }
        profile
            .as_object_mut()
            .ok_or_else(|| anyhow::anyhow!("Invalid selection profile"))?
            .insert(
                "admitted_retry_policy".into(),
                json!(digest(&serde_json::to_vec(&retry_policy)?)),
            );
    }
    if format == 2 {
        profile
            .as_object_mut()
            .context("Invalid selection profile")?
            .insert("current_download_metadata".into(), json!(1));
    }
    if format == 2 && config.refresh_metadata {
        // Admission records the explicit request. Its unfinished writers remain
        // authorized on later normal retries under the same source/flags policy.
        profile
            .as_object_mut()
            .context("Invalid selection profile")?
            .insert("explicit_metadata_refresh".into(), json!(1));
    }
    let mut fingerprint = policy(hash_download_config(config));
    fingerprint
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("Invalid selection policy"))?
        .insert("profile".into(), profile.clone());
    let config_hash = digest(&serde_json::to_vec(&fingerprint)?);
    Ok(Some(SelectionContext {
        format,
        capture,
        scope,
        zone,
        config_hash,
        profile,
        source_contexts: Vec::new(),
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
    pub(crate) held: Arc<AtomicBool>,
    pub(crate) session_expired: Arc<AtomicBool>,
    source_runs: BTreeMap<String, Arc<SelectionRun>>,
    fresh_confirmation: bool,
    pub(in crate::download) retained_retry: bool,
    retry_sources: Arc<std::sync::Mutex<SelectionRetrySources>>,
}

impl SelectionContext {
    fn spec(&self, config: &DownloadConfig, basis: String) -> GenerationSpec {
        GenerationSpec {
            format: self.format,
            scope: self.scope.clone(),
            zone: self.zone.clone(),
            config_hash: self.config_hash.clone(),
            basis,
            profile: self.profile.clone(),
            metadata_enabled: crate::download::pipeline::MetadataFlags::from(config)
                .has_any_write(),
            metadata_flags: crate::download::pipeline::MetadataFlags::from(config).bits(),
        }
    }

    pub(super) fn current_sparse_scope(&self) -> Option<&str> {
        (self.format == 2).then_some(self.scope.as_str())
    }

    async fn inventory_proof_matches(
        &self,
        config: &DownloadConfig,
        proof: &ObservationProof,
    ) -> Result<bool> {
        let specs = std::iter::once(self)
            .chain(self.source_contexts.iter())
            .map(|source| source.spec(config, source.config_hash.clone()))
            .collect();
        self.capture
            .db
            .inventory_proof_matches(self.capture.owner.clone(), proof.clone(), specs)
            .await
            .map_err(Into::into)
    }
    pub(super) fn uses_download_ledger(&self) -> bool {
        self.format == 2
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

    async fn current(&self) -> Result<Option<ActiveGeneration>> {
        if self.format == 2 {
            return Ok(self
                .capture
                .db
                .current_download_generation(
                    self.capture.owner.clone(),
                    self.scope.clone(),
                    self.config_hash.clone(),
                )
                .await?);
        }
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
        let mut run = self.start_single(passes, config).await?;
        for context in &self.source_contexts {
            let selected = context.selected_passes(passes);
            let mut child = context.start_single(&selected, config).await?;
            let child_mut =
                Arc::get_mut(&mut child).context("Shared source run before admission")?;
            child_mut.held = Arc::clone(&run.held);
            child_mut.session_expired = Arc::clone(&run.session_expired);
            let zone = context
                .zone
                .get("zoneName")
                .and_then(Value::as_str)
                .context("Missing source zone")?
                .to_owned();
            Arc::get_mut(&mut run)
                .context("Shared owner run before admission")?
                .source_runs
                .insert(zone, child);
        }
        Ok(run)
    }

    fn selected_passes(&self, passes: &[AlbumPass]) -> Vec<AlbumPass> {
        passes
            .iter()
            .filter(|pass| {
                self.profile
                    .get("passes")
                    .and_then(Value::as_array)
                    .is_some_and(|items| {
                        items.iter().any(|item| {
                            item.get("key").and_then(Value::as_str) == Some(pass_key(pass).as_str())
                        })
                    })
            })
            .cloned()
            .collect()
    }

    async fn start_single(
        &self,
        passes: &[AlbumPass],
        config: &DownloadConfig,
    ) -> Result<Arc<SelectionRun>> {
        let basis = if self.format == 2 {
            self.config_hash.clone()
        } else {
            self.capture
                .db
                .selection_basis(self.capture.owner.clone(), self.scope.clone())
                .await?
        };
        let spec = self.spec(config, basis);
        let root = if self.format == 1
            && let Some(current) = self.current().await?
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
            held: Arc::new(AtomicBool::new(false)),
            session_expired: Arc::new(AtomicBool::new(false)),
            source_runs: BTreeMap::new(),
            fresh_confirmation: false,
            retained_retry: false,
            retry_sources: Arc::clone(&self.retry_sources),
        }))
    }

    fn replay_run(
        &self,
        root: ActiveGeneration,
        passes: &[AlbumPass],
        fresh_confirmation: bool,
        retained_retry: bool,
    ) -> Arc<SelectionRun> {
        Arc::new(SelectionRun {
            capture: self.capture.clone(),
            root,
            albums: passes
                .iter()
                .map(|pass| (pass_key(pass), pass.album.clone()))
                .collect(),
            held: Arc::new(AtomicBool::new(false)),
            session_expired: Arc::new(AtomicBool::new(false)),
            source_runs: BTreeMap::new(),
            fresh_confirmation,
            retained_retry,
            retry_sources: Arc::clone(&self.retry_sources),
        })
    }
}

// Expected reference-transition refusals retain the original task for backoff.
// Other errors remain fatal to admission and cannot become a deferred success.
fn classify_selection_reference_deferral(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<crate::state::error::StateError>()
        .is_some_and(|error| {
            matches!(
                error,
                crate::state::error::StateError::ProviderWorkConflict
                    | crate::state::error::StateError::ProviderSelectionFull
            )
        })
        || error
            .downcast_ref::<SelectionReferenceUnavailable>()
            .is_some()
}

impl SelectionRun {
    pub(in crate::download) fn for_asset(
        self: &Arc<Self>,
        asset: &PhotoAsset,
    ) -> Result<Arc<Self>> {
        let source = asset.source_zone().unwrap_or(
            self.root
                .spec
                .zone
                .get("zoneName")
                .and_then(Value::as_str)
                .unwrap_or(""),
        );
        if self.root.spec.zone.get("zoneName").and_then(Value::as_str) == Some(source) {
            return Ok(Arc::clone(self));
        }
        self.source_runs
            .get(source)
            .cloned()
            .context("Observed asset has no authenticated source ledger")
    }

    async fn prepare_replay_or_retain(
        self: &Arc<Self>,
        decision: &ActiveDecision,
        asset: PhotoAsset,
        config: &DownloadConfig,
    ) -> Result<(Arc<Self>, PhotoAsset)> {
        match self.prepare_replay(decision, asset.clone(), config).await {
            Ok(value) => Ok(value),
            Err(error) if classify_selection_reference_deferral(&error) => {
                // The existing producer refusal path owns durable backoff and
                // incomplete reporting for this still-original task.
                Ok((self.clone(), asset))
            }
            Err(error) => Err(error),
        }
    }

    async fn prepare_replay(
        self: &Arc<Self>,
        decision: &ActiveDecision,
        asset: PhotoAsset,
        config: &DownloadConfig,
    ) -> Result<(Arc<Self>, PhotoAsset)> {
        if self.root.spec.format != 2
            || !self.fresh_confirmation
            || decision.is_deferred_observation()
        {
            return Ok((self.clone(), asset));
        }
        let key = &decision.decision.pass_key;
        if !self
            .capture
            .db
            .selection_attempt_due(
                self.capture.owner.clone(),
                self.root.id.clone(),
                key.clone(),
                decision.decision.child.clone(),
            )
            .await?
        {
            return Ok((self.clone(), asset));
        }
        let album = self.albums.get(key).context("Missing current retry pass")?;
        if let Some(witness) = asset.selector_membership()
            && let Err(error) = album
                .confirm_selector_membership(witness, &decision.decision.child)
                .await
        {
            if crate::icloud::photos::session::is_session_error(&error) {
                return Err(error);
            }
            return Ok((self.clone(), asset));
        }
        let mut current = match album
            .confirm_layout_asset(
                &decision.decision.child,
                asset.source_zone().context("Missing retry source")?,
            )
            .await
        {
            Ok(current) => current,
            Err(error) if crate::icloud::photos::session::is_session_error(&error) => {
                return Err(error);
            }
            Err(_) => return Ok((self.clone(), asset)),
        };
        if current.asset.id() == asset.id()
            || filter::is_asset_filtered(&current.asset, config).is_some()
        {
            return Ok((self.clone(), asset));
        }
        if let Some(witness) = asset.selector_membership() {
            let mut body: Value = serde_json::from_slice(&current.body)?;
            body.as_object_mut()
                .context("Invalid current retry proof")?
                .insert("selector_membership".into(), witness.clone());
            current.body = serde_json::to_vec(&body)?;
            current.asset = current.asset.with_selector_membership(witness.clone());
        }
        let mut replacement = decision.clone();
        replacement.decision.master = Some(current.asset.id().to_owned());
        replacement.decision.confirmation = Some(current.body);
        replacement.decision.destinations.clear();
        let paths = filter::derive_expected_paths(&current.asset, config);
        let previous_paths = filter::derive_expected_paths(&asset, config);
        let selected_versions: Vec<_> = paths
            .iter()
            .map(|path| {
                (
                    path.version_size.as_str().to_owned(),
                    filter::provider_version_for_selected(&asset, config, path.version_size),
                    filter::provider_version_for_selected(
                        &current.asset,
                        config,
                        path.version_size,
                    ),
                )
            })
            .collect();
        if selected_versions.iter().any(|(version, _, _)| {
            !previous_paths
                .iter()
                .any(|path| path.version_size.as_str() == version)
        }) {
            return Err(SelectionReferenceUnavailable.into());
        }

        let mut records = Vec::new();
        for old in &decision.decision.destinations {
            if self
                .capture
                .db
                .selection_destination_verified(
                    self.capture.owner.clone(),
                    self.root.id.clone(),
                    key.clone(),
                    decision.decision.child.clone(),
                    old.version_size.clone(),
                )
                .await?
            {
                continue;
            }
            let derived = paths
                .iter()
                .find(|path| path.version_size.as_str() == old.version_size)
                .ok_or(SelectionReferenceUnavailable)?;
            let metadata =
                filter::metadata_for_selected_version(&current.asset, config, derived.version_size);
            let path = old.path.to_path();
            records.push(
                crate::state::AssetRecord::new_pending(
                    Arc::from(
                        current
                            .asset
                            .source_zone()
                            .context("Missing current source")?,
                    ),
                    decision.state_id.clone(),
                    derived.version_size,
                    derived.checksum.to_string(),
                    path.file_name()
                        .and_then(|s| s.to_str())
                        .unwrap_or("")
                        .to_owned(),
                    current.asset.created(),
                    Some(current.asset.added_date()),
                    derived.size,
                    filter::determine_media_type(derived.version_size, &current.asset),
                )
                .with_metadata_arc(metadata.clone()),
            );
            replacement
                .decision
                .destinations
                .push(SelectionDestination {
                    version_size: old.version_size.clone(),
                    path: old.path.clone(),
                    checksum: derived.checksum.to_string(),
                    size: derived.size,
                    metadata_hash: metadata
                        .metadata_hash
                        .clone()
                        .context("Missing current retry metadata")?,
                });
        }
        let root = self
            .capture
            .db
            .refresh_selection_reference(
                self.capture.owner.clone(),
                self.root.id.clone(),
                decision.clone(),
                replacement,
                (records, selected_versions),
                MAX_GENERATION_BYTES,
            )
            .await?;
        let run = Arc::new(Self {
            capture: self.capture.clone(),
            root,
            albums: self.albums.clone(),
            held: self.held.clone(),
            session_expired: self.session_expired.clone(),
            source_runs: BTreeMap::new(),
            fresh_confirmation: true,
            retained_retry: self.retained_retry,
            retry_sources: self.retry_sources.clone(),
        });
        Ok((
            run,
            current
                .asset
                .with_state_record_name(Arc::from(decision.state_id.clone())),
        ))
    }

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
        let mut deferred = std::collections::HashSet::new();
        for task in tasks {
            let run = if self.root.spec.zone.get("zoneName").and_then(Value::as_str)
                == Some(task.library.as_ref())
            {
                self
            } else {
                self.source_runs
                    .get(task.library.as_ref())
                    .map(AsRef::as_ref)
                    .context("Failed task has no authenticated source root")?
            };
            for (pass_index, pass) in passes.iter().enumerate() {
                let key = pass_key(pass);
                let Some(manifest) = run
                    .capture
                    .db
                    .selection_decision(
                        run.capture.owner.clone(),
                        run.root.id.clone(),
                        key.clone(),
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
                if run.root.spec.format == 2
                    && deferred.insert((key.clone(), task.asset_record_name.to_string()))
                {
                    self.capture
                        .db
                        .defer_selection_confirmation(
                            run.capture.owner.clone(),
                            run.root.id.clone(),
                            key,
                            task.asset_record_name.to_string(),
                        )
                        .await?;
                }
                let (Some(body), Some(master)) =
                    (&manifest.decision.confirmation, &manifest.decision.master)
                else {
                    continue;
                };
                let asset =
                    current_asset(body, &manifest.decision.child, master, &run.root.spec.zone)?;
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
        let membership = if self.root.spec.profile.get("selector_binding").is_some() {
            Some(
                observed
                    .selector_membership()
                    .cloned()
                    .context("Missing current selecting album membership")?,
            )
        } else {
            None
        };
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
            && let (Some(body), Some(master)) = (
                previous.decision.confirmation.clone(),
                previous.decision.master.clone(),
            )
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
            if self.fresh_confirmation || previous.is_deferred_observation() {
                let album = self
                    .albums
                    .get(key)
                    .context("Missing retained selection pass owner")?;
                if let Some(witness) = &membership {
                    album
                        .confirm_selector_membership(witness, &previous.decision.child)
                        .await?;
                }
                let mut current = album
                    .confirm_layout_asset(
                        &previous.decision.child,
                        self.root
                            .spec
                            .zone
                            .get("zoneName")
                            .and_then(Value::as_str)
                            .context("Missing source zone")?,
                    )
                    .await?;
                anyhow::ensure!(
                    same_selected_facts(&retained, &current.asset),
                    "Retained selected generation differs from current proof"
                );
                if let Some(witness) = &membership {
                    let mut fresh: Value = serde_json::from_slice(&current.body)?;
                    fresh
                        .as_object_mut()
                        .context("Invalid fresh source proof")?
                        .insert("selector_membership".into(), witness.clone());
                    current.body = serde_json::to_vec(&fresh)?;
                    current.asset = current.asset.with_selector_membership(witness.clone());
                }
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
        let mut current = album
            .confirm_layout_asset(
                observed.asset_record_name(),
                self.root
                    .spec
                    .zone
                    .get("zoneName")
                    .and_then(Value::as_str)
                    .context("Missing source zone")?,
            )
            .await?;
        anyhow::ensure!(
            same_selected_facts(&current.asset, &observed),
            "Current confirmation changed selection facts"
        );
        if let Some(witness) = membership {
            let mut body: Value = serde_json::from_slice(&current.body)?;
            body.as_object_mut()
                .context("Invalid current source proof")?
                .insert("selector_membership".into(), witness.clone());
            current.body = serde_json::to_vec(&body)?;
            current.asset = current.asset.with_selector_membership(witness);
        }
        Ok((
            current.asset,
            SelectionConfirmation {
                retained: current.body.clone(),
                fresh: Some(current.body),
            },
        ))
    }

    /// Keep the original selected facts for a failed first lookup in the same
    /// ledger. The deferred outcome never grants a destination or a media write.
    pub(in crate::download) async fn defer_observed_confirmation(
        &self,
        key: &str,
        observed: &PhotoAsset,
    ) -> Result<()> {
        let child = observed.asset_record_name().to_owned();
        if self.root.spec.format != 2
            || self
                .capture
                .db
                .selection_decision(
                    self.capture.owner.clone(),
                    self.root.id.clone(),
                    key.to_owned(),
                    child.clone(),
                )
                .await?
                .is_some()
        {
            return self.defer_confirmation(key, child).await;
        }
        let mut body = observed
            .observed_pair()
            .cloned()
            .context("Missing selected observation pair")?;
        if let Some(witness) = observed.selector_membership() {
            body.as_object_mut()
                .context("Invalid selected observation")?
                .insert("selector_membership".into(), witness.clone());
        }
        let body = serde_json::to_vec(&body)?;
        let decoded = current_asset(&body, &child, observed.id(), &self.root.spec.zone)?;
        anyhow::ensure!(
            same_selected_facts(&decoded, observed),
            "Selected observation lost required facts"
        );
        self.capture
            .db
            .project_selection_decision(
                self.capture.owner.clone(),
                self.root.id.clone(),
                ActiveDecision {
                    state_id: child.clone(),
                    sources: Vec::new(),
                    decision: SelectionDecision {
                        pass_key: key.to_owned(),
                        child,
                        master: Some(observed.id().to_owned()),
                        confirmation: Some(body),
                        outcome: SelectionOutcome::Deferred,
                        reason: "current_lookup_unresolved".into(),
                        destinations: Vec::new(),
                    },
                },
                Vec::new(),
                MAX_GENERATION_BYTES,
            )
            .await?;
        Ok(())
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
        anyhow::ensure!(
            self.root.spec.format == 1,
            "Unconfirmed observation could not be retained; keep the source cursor"
        );
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
        cancel: &CancellationToken,
    ) -> Result<bool> {
        let SelectionConfirmation {
            retained: body,
            fresh,
        } = confirmation;
        if self.retained_retry && plan.filter_reason.is_some() {
            self.defer_confirmation(key, asset.asset_record_name().to_owned())
                .await?;
            tracing::warn!(
                reason = "retained_selection_currently_filtered",
                "Previously selected work remains pending under the current filters"
            );
            return Ok(false);
        }
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
        // Choose verified historical reuse before freezing the physical
        // destination. This is a read-only candidate; the producer rechecks
        // its receipt and retained file inside the final adoption transaction.
        if reason.is_empty()
            && previous.as_ref().is_none_or(|value| {
                (value.decision.confirmation.is_none() || value.is_deferred_observation())
                    && value.decision.outcome != SelectionOutcome::Selected
            })
        {
            for task in &mut plan.tasks {
                if let crate::download::baseline_matching::BaselineMatch::Present {
                    receipt, ..
                }
                | crate::download::baseline_matching::BaselineMatch::ProviderPresent {
                    receipt,
                    ..
                } = planner.match_existing(asset, config, task).await
                    && task.download_path != receipt.local_path
                {
                    planner.release_unpublished_task_claim(task);
                    task.download_path = receipt.local_path;
                }
            }
        }
        let mut destinations = Vec::new();
        let mut records = Vec::new();
        if reason.is_empty() {
            let derived_paths = filter::derive_expected_paths(asset, config);
            for derived in &derived_paths {
                if self.fresh_confirmation
                    && previous.as_ref().is_some_and(|old| {
                        old.decision.confirmation.is_some()
                            && !old.is_deferred_observation()
                            && !old
                                .decision
                                .destinations
                                .iter()
                                .any(|d| d.version_size == derived.version_size.as_str())
                    })
                {
                    continue;
                }
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
                    let fulfilled = self
                        .capture
                        .db
                        .selection_path_reusable_for_pass(
                            self.capture.owner.clone(),
                            self.root.id.clone(),
                            self.root.spec.scope.clone(),
                            key.to_owned(),
                            asset.asset_record_name().to_owned(),
                            candidate.clone(),
                        )
                        .await?;
                    if (fulfilled
                        || filter::stored_path_matches_download_family(
                            asset.state_id(),
                            derived,
                            &derived_paths,
                            config,
                            &path,
                        ))
                        && let Ok(fingerprint) = crate::download::file::fingerprint_downloaded_path(
                            &config.directory,
                            &path,
                        )
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
                    && !previous.is_deferred_observation()
                    && (previous.decision.confirmation.is_some()
                        || previous.decision.outcome == SelectionOutcome::Selected)
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
        let sources = if self.root.spec.format == 2 {
            Vec::new()
        } else if let Some(previous) = &previous
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
        let freshly_confirmed = fresh.is_some();
        let fresh_for_trust = fresh.clone();
        let mut projection = self
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
        if freshly_confirmed {
            self.capture
                .db
                .source_revalidation_seen(
                    self.capture.owner.clone(),
                    config.library.to_string(),
                    self.root.spec.scope.clone(),
                    asset.state_id().to_owned(),
                )
                .await?;
        }
        if !projection.admitted
            && projection.reason == "identity_conflict"
            && let Some(fresh) = fresh_for_trust
        {
            let mut targets = Vec::new();
            let mut files = Vec::new();
            for destination in &manifest.decision.destinations {
                if cancel.is_cancelled() {
                    break;
                }
                let Some(target) = self
                    .capture
                    .db
                    .accepted_trusted_legacy_file(
                        self.root
                            .spec
                            .zone
                            .get("zoneName")
                            .and_then(Value::as_str)
                            .context("Missing selected source zone")?
                            .to_owned(),
                        manifest.decision.child.clone(),
                        destination.version_size.clone(),
                    )
                    .await?
                else {
                    continue;
                };
                if target.path != destination.path.to_path()
                    || crate::download::file::has_replacement_journal(
                        &config.directory,
                        &target.path,
                    )
                    .await?
                {
                    continue;
                }
                let root = config.directory.to_path_buf();
                let suffix = config.temp_suffix.to_string();
                let allow = config.legacy_preservation_allow_hardlinks;
                let candidate = target.clone();
                let retained = tokio::task::spawn_blocking(move || {
                    crate::download::trusted_legacy::retain(&root, &candidate, &suffix, allow)
                })
                .await?;
                let Ok(retained) = retained else { continue };
                if retained.target != target {
                    continue;
                }
                files.push(retained);
                targets.push(target);
            }
            if !targets.is_empty() {
                let root = config.directory.to_path_buf();
                let suffix = config.temp_suffix.to_string();
                let allow_hardlinks = config.legacy_preservation_allow_hardlinks;
                let cancel = cancel.clone();
                projection.admitted = self
                    .capture
                    .db
                    .settle_trusted_selection(
                        self.capture.owner.clone(),
                        self.root.id.clone(),
                        key.to_owned(),
                        manifest.decision.child.clone(),
                        targets,
                        fresh,
                        move || {
                            let check = || -> anyhow::Result<()> {
                                for retained in files {
                                    anyhow::ensure!(!cancel.is_cancelled(), "Selection cancelled");
                                    retained.validate(&root, &suffix, allow_hardlinks)?;
                                }
                                anyhow::ensure!(!cancel.is_cancelled(), "Selection cancelled");
                                Ok(())
                            };
                            check().map_err(|_error| {
                                crate::state::error::StateError::ProviderWorkConflict
                            })
                        },
                    )
                    .await?;
            }
        }
        if !projection.admitted && manifest.decision.outcome != SelectionOutcome::Excluded {
            tracing::debug!(reason = %projection.reason, "Selection destination admission deferred");
            if self.root.spec.format == 1 {
                self.held.store(true, Ordering::Relaxed);
            }
            tracing::warn!(reason = %projection.reason, "Selected work remains in the download ledger for retry");
            return Ok(false);
        }
        // Reuse applies one authorization rule for pending metadata, explicit
        // refresh, and recorded metadata options. Pre-existing markers remain
        // authoritative in the writer guard.
        let authorization = crate::state::db::provider_generations::ReuseAuthorization {
            current_metadata_flags: crate::download::pipeline::MetadataFlags::from(config).bits(),
            explicit_refresh_authorized: config.refresh_metadata
                && config.allows_existing_mutation(),
            scope: self.root.spec.scope.clone(),
            pass_key: key.to_owned(),
            child: manifest.decision.child.clone(),
            generation: Some(self.root.id.clone()),
        };
        for destination in &manifest.decision.destinations {
            if let Ok(fingerprint) = crate::download::file::fingerprint_downloaded_path(
                &config.directory,
                &destination.path.to_path(),
            )
            .await
            {
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
                        authorization.clone(),
                    )
                    .await?;
                if !reused {
                    tracing::debug!(
                        asset_id = %asset.state_id(),
                        path = %destination.path.to_path().display(),
                        "Exact selection publication reuse did not settle; retaining normal planning"
                    );
                }
            }
        }
        let mut retained_tasks = Vec::with_capacity(plan.tasks.len());
        for mut task in std::mem::take(&mut plan.tasks) {
            if self.fresh_confirmation
                && !manifest
                    .decision
                    .destinations
                    .iter()
                    .any(|d| d.version_size == task.version_size.as_str())
            {
                continue;
            }
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
    for context in std::iter::once(&context).chain(context.source_contexts.iter()) {
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
                context.scope.clone(),
                context.config_hash.clone(),
                root.id,
                (context.format == 2).then(|| context.profile.clone()),
            )
            .await?
            .is_some();
        if pending || retained {
            return Ok(Some(true));
        }
    }
    Ok(Some(false))
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
            album: if run.root.spec.format == 1 {
                pass.album
                    .clone()
                    .with_rank_capture(&run.root.id, &pass_key(pass))
            } else {
                pass.album.clone()
            },
            ..pass.clone()
        })
        .collect();
    let captured_config = Arc::new(DownloadConfig {
        selection_run: Some(run.clone()),
        ..config.as_ref().clone()
    });
    let mut retained = Box::pin(recover_retained(
        client,
        passes,
        config,
        controls,
        cancel.clone(),
        &run.root,
    ))
    .await?;
    for source in &context.source_contexts {
        if cancel.is_cancelled() || retained.provider_auth_errors > 0 {
            break;
        }
        let zone = source
            .zone
            .get("zoneName")
            .and_then(Value::as_str)
            .context("Missing source zone")?;
        let source_run = run.source_runs.get(zone).context("Missing source root")?;
        let mut single = source.clone();
        single.source_contexts.clear();
        let effective = Arc::new(DownloadConfig {
            selection_context: Some(Arc::new(single)),
            ..config.as_ref().clone()
        });
        let result = Box::pin(recover_retained(
            client,
            passes,
            &effective,
            controls,
            cancel.clone(),
            &source_run.root,
        ))
        .await?;
        super::models::merge_streaming_result(&mut retained, result);
    }
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
    if context.format == 2
        && result.checkpoint.selection_window_complete
        && !run.held.load(Ordering::Relaxed)
        && result.stats.sync_token_blocked_reason
            == Some(super::models::RECENT_LIMITED_FULL_ENUMERATION_REASON)
    {
        // A completed bounded selection is not a source cursor. Only the
        // preserved-boundary changes stream may supply its eventual token.
        result.sync_token = None;
        result.clear_full_query_token_block();
    }
    let query_checkpoint_ready = (result.sync_token.is_some()
        || result.checkpoint.selection_window_complete)
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
    let mut admission_receipts = Vec::new();
    let mut all_admissions_proven = covered;
    if covered {
        let receipt = context
            .capture
            .db
            .seal_observed_selection(
                context.capture.owner.clone(),
                run.root.id.clone(),
                query_checkpoint_ready,
                query_veto.clone(),
            )
            .await?;
        if let Some(receipt) = receipt {
            admission_receipts.push(receipt);
        } else {
            all_admissions_proven = false;
        }
    }
    for source in &context.source_contexts {
        let zone = source
            .zone
            .get("zoneName")
            .and_then(Value::as_str)
            .context("Missing source zone")?;
        let source_run = run
            .source_runs
            .get(zone)
            .context("Missing started source root")?;
        if covered {
            let receipt = source
                .capture
                .db
                .seal_observed_selection(
                    source.capture.owner.clone(),
                    source_run.root.id.clone(),
                    query_checkpoint_ready && !run.held.load(Ordering::Relaxed),
                    query_veto.clone(),
                )
                .await?;
            if let Some(receipt) = receipt {
                admission_receipts.push(receipt);
            } else {
                all_admissions_proven = false;
            }
        }
        source
            .capture
            .db
            .freeze_interrupted_selection(source.capture.owner.clone(), source.scope.clone())
            .await?;
        let failures = metadata_tail(config, &cancel, source, &source_run.root.id, None).await?;
        result.stats.exif_failures += failures;
        if failures > 0 {
            result.outcome = merge_download_outcomes(
                &result.outcome,
                &DownloadOutcome::PartialFailure {
                    failed_count: failures,
                },
            );
        }
        source
            .capture
            .db
            .complete_selection_metadata(source.capture.owner.clone(), source_run.root.id.clone())
            .await?;
    }
    context
        .capture
        .db
        .freeze_interrupted_selection(context.capture.owner.clone(), context.scope.clone())
        .await?;
    let metadata_failures = metadata_tail(config, &cancel, context, &run.root.id, None).await?;
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
    if context.format == 1
        && context
            .capture
            .db
            .unfinished_selection_debt(context.capture.owner.clone(), context.scope.clone())
            .await?
    {
        result.block_incremental_token(DEFERRED_REASON)
    }
    if all_admissions_proven && !run.held.load(Ordering::Relaxed) {
        result.checkpoint.observation = ObservationProof::inventory(admission_receipts);
    }
    result.checkpoint.project(&mut result.stats);
    Ok(result)
}

const METADATA_WRITER_BUDGET: usize = 500;

#[cfg(test)]
tokio::task_local! {
    // Scoped to one fixture future, so parallel tests retain production limits.
    pub(super) static TEST_METADATA_WRITER_BUDGET: usize;
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

/// Keep the existing 500-row writer budget while rotating past retained failed
/// or option-disabled physical writers. This cursor schedules retries only;
/// debt/progress remain in the exact publication and metadata receipts.
async fn metadata_tail(
    config: &DownloadConfig,
    cancel: &CancellationToken,
    context: &SelectionContext,
    generation: &str,
    captured: Option<Vec<crate::state::db::PendingMetadataRewrite>>,
) -> Result<usize> {
    if !config.refresh_metadata && context.format != 2 {
        return Ok(0);
    }
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
    let (current_reserve, budget) = metadata_writer_limits();
    let current = if let Some(captured) = captured {
        captured
    } else {
        context
            .capture
            .db
            .pending_selection_metadata(
                context.capture.owner.clone(),
                generation.to_owned(),
                current_reserve,
            )
            .await?
    };
    if !config.allows_existing_mutation() {
        // A later complete catalog cannot lift this operation's earlier refusal.
        // Keep exact requested work durable; do not advance retry scheduling here.
        let mut rows = current;
        if config.refresh_metadata {
            rows.extend(
                db.get_pending_metadata_rewrites_page_for_queue(
                    crate::state::db::MetadataRewriteQueue::Ordinary,
                    None,
                    offset,
                    budget.saturating_sub(rows.len()),
                )
                .await?,
            );
        }
        // The two bounded lanes can name the same physical request.
        let pending = rows
            .into_iter()
            .map(|row| {
                (
                    row.asset.library,
                    row.asset.id,
                    row.asset.version_size,
                    row.asset.local_path,
                    row.asset.checksum,
                    row.asset.metadata.metadata_hash.clone(),
                )
            })
            .collect::<std::collections::HashSet<_>>()
            .len();
        if pending > 0 {
            tracing::warn!(
                pending,
                "Requested metadata destinations in this bounded batch remain pending under the operation ownership restriction"
            );
        }
        return Ok(pending);
    }
    let current = if context.format == 2 && !current.is_empty() && !cancel.is_cancelled() {
        context
            .capture
            .db
            .defer_selection_metadata_round(
                context.capture.owner.clone(),
                generation.to_owned(),
                current,
            )
            .await?
    } else {
        current
    };
    let priority = crate::download::metadata_rewrite::run_pending_rows(
        db.as_ref(),
        flags,
        crate::download::metadata_rewrite::CaptureTimestampRepair::Preserve,
        config.temp_suffix.clone(),
        cancel,
        current,
        Some(&config.directory),
    )
    .await;
    if context.format == 2 && !config.refresh_metadata {
        return Ok(priority.failed);
    }
    let remaining = budget.saturating_sub(priority.fetched);
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
            (context.format == 2).then(|| context.profile.clone()),
        )
        .await?
    else {
        return Ok(crate::download::pipeline::StreamingResult {
            enumeration_complete: true,
            ..crate::download::pipeline::StreamingResult::default()
        });
    };
    anyhow::ensure!(
        (root.spec.profile == current.spec.profile
            || (root.spec.format == 2
                && current.spec.format == 2
                && crate::state::db::provider_generations::retained_profiles_compatible(
                    &root.spec.profile,
                    &current.spec.profile
                )))
            && root.spec.zone == current.spec.zone
            && root.spec.metadata_flags == current.spec.metadata_flags,
        "Retained selection configuration disagrees"
    );
    let metadata = context
        .capture
        .db
        .pending_selection_metadata(
            context.capture.owner.clone(),
            root.id.clone(),
            metadata_writer_limits().0,
        )
        .await?;
    let decisions = context
        .capture
        .db
        .retained_selection_decisions(context.capture.owner.clone(), root.id.clone())
        .await?;
    let run = context.replay_run(root.clone(), passes, true, true);
    for decision in &decisions {
        if !passes
            .iter()
            .any(|pass| pass_key(pass) == decision.decision.pass_key)
        {
            context
                .capture
                .db
                .defer_selection_confirmation(
                    context.capture.owner.clone(),
                    root.id.clone(),
                    decision.decision.pass_key.clone(),
                    decision.decision.child.clone(),
                )
                .await?;
            tracing::warn!(
                reason = "retained_pass_not_selected",
                "Previously selected work remains pending because its album is not selected"
            );
        }
    }
    let mut combined = crate::download::pipeline::StreamingResult {
        enumeration_complete: true,
        ..crate::download::pipeline::StreamingResult::default()
    };
    for pass in passes {
        if cancel.is_cancelled() || run.session_expired.load(Ordering::Relaxed) {
            break;
        }
        let key = pass_key(pass);
        let mut groups: BTreeMap<String, (Arc<SelectionRun>, Vec<Result<PhotoAsset>>)> =
            BTreeMap::new();
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
                let asset = asset.with_state_record_name(Arc::from(decision.state_id.clone()));
                let (owner, asset) = run
                    .prepare_replay_or_retain(decision, asset, &config.with_pass(pass))
                    .await?;
                groups
                    .entry(owner.root.id.clone())
                    .or_insert_with(|| (owner, Vec::new()))
                    .1
                    .push(Ok(asset));
            } else {
                run.defer_confirmation(&key, decision.decision.child.clone())
                    .await?;
            }
        }
        for (_, (run, assets)) in groups {
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
            if run.root.id != root.id {
                combined.exif_failures +=
                    metadata_tail(config, &cancel, context, &run.root.id, None).await?;
                context
                    .capture
                    .db
                    .complete_selection_metadata(context.capture.owner.clone(), run.root.id.clone())
                    .await?;
            }
            super::models::merge_streaming_result(&mut combined, result);
        }
    }
    combined.exif_failures +=
        metadata_tail(config, &cancel, context, &root.id, Some(metadata)).await?;
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
    let run = context.replay_run(root.clone(), passes, root.spec.format == 2, false);
    let metadata = context
        .capture
        .db
        .pending_selection_metadata(
            context.capture.owner.clone(),
            root.id.clone(),
            metadata_writer_limits().0,
        )
        .await?;
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
        if !context
            .profile
            .get("passes")
            .and_then(Value::as_array)
            .is_some_and(|items| {
                items.iter().any(|item| {
                    item.get("key").and_then(Value::as_str) == Some(pass_key(pass).as_str())
                })
            })
        {
            continue;
        }
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
            let mut groups: BTreeMap<String, (Arc<SelectionRun>, Vec<Result<PhotoAsset>>)> =
                BTreeMap::new();
            for decision in decisions {
                if let (Some(body), Some(master)) =
                    (&decision.decision.confirmation, &decision.decision.master)
                {
                    let asset =
                        current_asset(body, &decision.decision.child, master, &root.spec.zone)?
                            .with_state_record_name(Arc::from(decision.state_id.clone()));
                    let (owner, asset) = run
                        .prepare_replay_or_retain(&decision, asset, &config.with_pass(pass))
                        .await?;
                    groups
                        .entry(owner.root.id.clone())
                        .or_insert_with(|| (owner, Vec::new()))
                        .1
                        .push(Ok(asset));
                } else {
                    run.held.store(true, Ordering::Relaxed);
                }
            }
            for (_, (run, assets)) in groups {
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
                if run.root.id != root.id {
                    context
                        .capture
                        .db
                        .complete_selection_metadata(
                            context.capture.owner.clone(),
                            run.root.id.clone(),
                        )
                        .await?;
                }
                super::models::merge_streaming_result(&mut combined, result);
            }
        }
    }
    combined.exif_failures +=
        metadata_tail(config, &cancel, context, &run.root.id, Some(metadata)).await?;
    let mut result = crate::download::pipeline::build_download_result(
        client, passes, config, controls, combined, started, cancel,
    )
    .await?;
    context
        .capture
        .db
        .complete_selection_metadata(context.capture.owner.clone(), root.id.clone())
        .await?;
    if (root.spec.format == 1 && !root.checkpoint_ready) || run.held.load(Ordering::Relaxed) {
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
    if context.format == 1
        && context
            .capture
            .db
            .unfinished_selection_debt(context.capture.owner.clone(), context.scope.clone())
            .await?
    {
        result.block_incremental_token(DEFERRED_REASON)
    }
    Ok(result)
}

/// Deliver already-routed observations to the same immutable destination
/// owner used by baseline inventory. A quiet delta replays due work only.
async fn deliver_delta(
    client: &Client,
    passes: &[AlbumPass],
    config: &Arc<DownloadConfig>,
    controls: DownloadControls,
    cancel: CancellationToken,
    assets: Vec<(PhotoAsset, usize)>,
) -> Result<SyncResult> {
    let owner = config
        .selection_context
        .as_ref()
        .context("Missing download ledger context")?;
    let routed = assets.iter().all(|(asset, index)| {
        *index < passes.len()
            && std::iter::once(owner.as_ref())
                .chain(owner.source_contexts.iter())
                .any(|context| {
                    context.zone.get("zoneName").and_then(Value::as_str)
                        == Some(asset.source_zone().unwrap_or(config.library.as_ref()))
                })
    });
    let mut combined: Option<SyncResult> = None;
    for context in std::iter::once(owner.as_ref()).chain(owner.source_contexts.iter()) {
        let mut single = context.clone();
        single.source_contexts.clear();
        let mut effective = config.as_ref().clone();
        effective.selection_context = Some(Arc::new(single));
        // Preserve original pass indexes for exact-task URL refresh. Source
        // profiles choose eligible passes; another source cannot claim them.
        let zone = context
            .zone
            .get("zoneName")
            .and_then(Value::as_str)
            .context("Missing source zone")?;
        let selected = assets
            .iter()
            .filter(|(asset, _)| asset.source_zone().unwrap_or(config.library.as_ref()) == zone)
            .cloned()
            .collect();
        let mut result = deliver_delta_single(
            client,
            passes,
            &Arc::new(effective),
            controls,
            cancel.clone(),
            selected,
        )
        .await?;
        if let Some(previous) = &mut combined {
            previous.outcome = merge_download_outcomes(&previous.outcome, &result.outcome);
            previous.checkpoint.observation = previous
                .checkpoint
                .observation
                .take()
                .zip(result.checkpoint.observation.take())
                .and_then(|(a, b)| a.combine_sources(b));
            previous.accumulate(&result);
        } else {
            combined = Some(result);
        }
    }
    let mut result = combined.context("Missing selected ledger result")?;
    if !routed {
        result.checkpoint.observation = None;
    }
    Ok(result)
}

async fn deliver_delta_single(
    client: &Client,
    passes: &[AlbumPass],
    config: &Arc<DownloadConfig>,
    controls: DownloadControls,
    cancel: CancellationToken,
    assets: Vec<(PhotoAsset, usize)>,
) -> Result<SyncResult> {
    let context = config
        .selection_context
        .as_ref()
        .context("Missing download ledger context")?;
    if assets.is_empty()
        && let Some(root) = context.current().await?
    {
        let mut result = replay(client, passes, config, controls, cancel, root).await?;
        let spec = context.spec(config, "0".repeat(64));
        let receipt = context
            .capture
            .db
            .empty_observation_admission(context.capture.owner.clone(), spec)
            .await?;
        result.checkpoint.observation = Some(ObservationProof::delivery(receipt));
        return Ok(result);
    }
    let run = context.start(passes, config).await?;
    let started = Instant::now();
    let mut combined =
        recover_retained(client, passes, config, controls, cancel.clone(), &run.root).await?;
    for (index, pass) in passes.iter().enumerate() {
        if cancel.is_cancelled() || combined.provider_auth_errors > 0 {
            combined.enumeration_complete = false;
            break;
        }
        let selected: Vec<_> = assets
            .iter()
            .filter(|(_, selected)| *selected == index)
            .map(|(asset, _)| Ok::<_, anyhow::Error>(asset.clone()))
            .collect();
        if selected.is_empty() {
            continue;
        }
        let mut effective = config.with_pass(pass);
        effective.selection_run = Some(run.clone());
        effective.selection_pass = Some(pass_key(pass));
        let result = crate::download::pipeline::stream_and_download_from_stream(
            client,
            futures_util::stream::iter(selected),
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
    combined.exif_failures += metadata_tail(config, &cancel, context, &run.root.id, None).await?;
    let mut result = crate::download::pipeline::build_download_result(
        client,
        passes,
        config,
        controls,
        combined,
        started,
        cancel.clone(),
    )
    .await?;
    let accounted = !run.held.load(Ordering::Relaxed)
        && result.checkpoint.state_write_failures == 0
        && result.checkpoint.enumeration_errors == 0
        && !result.checkpoint.enumeration_incomplete
        && !cancel.is_cancelled();
    let admission = context
        .capture
        .db
        .seal_observed_selection(
            context.capture.owner.clone(),
            run.root.id.clone(),
            accounted,
            (!accounted).then(|| DEFERRED_REASON.to_owned()),
        )
        .await?;
    context
        .capture
        .db
        .complete_selection_metadata(context.capture.owner.clone(), run.root.id.clone())
        .await?;
    if !accounted {
        result.block_incremental_token(DEFERRED_REASON);
    }
    if accounted {
        result.checkpoint.observation = admission.map(ObservationProof::delivery);
    }
    Ok(result)
}

/// The prior full-inventory proof may replace only the duplicate recent
/// inventory. Every failed, unsafe, interrupted, changed, or non-format-2
/// case retains the ordinary inventory path.
#[derive(Clone, Copy)]
struct InventoryProofReuse {
    needs_inventory: bool,
    delta_completed: bool,
    inventory_rank_changed: bool,
    state_transition_failures: usize,
    identity_incomplete: bool,
    token_unsafe: bool,
    cancelled: bool,
    format: u8,
    proof_matches: bool,
}

fn can_reuse_inventory_proof(reuse: InventoryProofReuse) -> bool {
    reuse.needs_inventory
        && reuse.delta_completed
        && !reuse.inventory_rank_changed
        && reuse.state_transition_failures == 0
        && !reuse.identity_incomplete
        && !reuse.token_unsafe
        && !reuse.cancelled
        && reuse.format == 2
        && reuse.proof_matches
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
    assets: Vec<(PhotoAsset, usize)>,
) -> Result<SyncResult> {
    let context = config
        .selection_context
        .as_ref()
        .context("Missing selection context")?;
    if delta.missing_successor && context.format != 2 {
        // Legacy selection cannot deliver a partial source replay safely.
        return Err(crate::icloud::photos::error::MissingChangesSuccessor.into());
    }
    let recent = super::recent::active(config)
        .await
        .map_err(super::recent::RecentSelectionError)?;
    let needs_inventory = recent
        && super::recent::needs_inventory(
            passes,
            config,
            prior_cursor,
            delta.inventory_rank_changed,
        )
        .await
        .map_err(super::recent::RecentSelectionError)?;
    // The first full pass already admitted the current selected scope. Reuse
    // that typed proof only for a completed, unchanged delta from the saved
    // boundary; every other case keeps the ordinary inventory path and its
    // existing vetoes.
    let proof_matches = if needs_inventory
        && delta.sync_token.is_some()
        && !delta.inventory_rank_changed
        && delta.state_transition_failures == 0
        && !delta.identity_incomplete
        && delta.token_unsafe_reason.is_none()
        && !cancel.is_cancelled()
        && context.format == 2
    {
        match config.prior_inventory_proof.as_deref() {
            Some(proof) => Box::pin(context.inventory_proof_matches(config, proof)).await?,
            None => false,
        }
    } else {
        false
    };
    let reuse_inventory_proof = can_reuse_inventory_proof(InventoryProofReuse {
        needs_inventory,
        delta_completed: delta.sync_token.is_some(),
        inventory_rank_changed: delta.inventory_rank_changed,
        state_transition_failures: delta.state_transition_failures,
        identity_incomplete: delta.identity_incomplete,
        token_unsafe: delta.token_unsafe_reason.is_some(),
        cancelled: cancel.is_cancelled(),
        format: context.format,
        proof_matches,
    });
    let repeat_inventory = needs_inventory && !reuse_inventory_proof && !delta.missing_successor;
    if reuse_inventory_proof {
        tracing::debug!(
            "Reusing completed full-inventory proof for the recent bridge; skipping duplicate inventory"
        );
    }
    if recent {
        super::recent::record(passes, config, prior_cursor, false)
            .await
            .map_err(super::recent::RecentSelectionError)?;
    }
    let root = context.current().await?;
    let selection = if context.format == 2 && !repeat_inventory {
        deliver_delta(client, passes, config, controls, cancel.clone(), assets).await
    } else {
        match root {
            Some(root)
                if !repeat_inventory
                    && (root.sealed
                        || context
                            .capture
                            .db
                            .selection_waiting_for_retry(
                                context.capture.owner.clone(),
                                root.id.clone(),
                            )
                            .await?) =>
            {
                replay(client, passes, config, controls, cancel.clone(), root).await
            }
            _ => inventory(client, passes, config, controls, cancel.clone()).await,
        }
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
    if context.format == 2
        && delta.inventory_rank_changed
        && !repeat_inventory
        && !delta.missing_successor
    {
        let smart_passes: Vec<_> = passes
            .iter()
            .filter(|pass| pass.kind == PassKind::SmartFolder)
            .cloned()
            .collect();
        if !smart_passes.is_empty() && !cancel.is_cancelled() {
            // Zone deltas do not establish smart-folder membership. A sparse
            // Hidden/favorite edit can select an unchanged asset, so query only
            // the selected smart folders before acknowledging that delta.
            // Quiet cycles continue through durable retries without inventory.
            let mut smart_config = config.as_ref().clone();
            smart_config.selection_context = self::context(&smart_passes, config, controls)
                .await?
                .map(Arc::new);
            let smart_config = Arc::new(smart_config);
            let mut refreshed = inventory(
                client,
                &smart_passes,
                &smart_config,
                controls,
                cancel.clone(),
            )
            .await?;
            selected.outcome = merge_download_outcomes(&selected.outcome, &refreshed.outcome);
            selected.checkpoint.observation = selected
                .checkpoint
                .observation
                .take()
                .zip(refreshed.checkpoint.observation.take())
                .and_then(|(a, b)| a.with_smart(b));
            selected.accumulate(&refreshed);
        }
    }
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
        delta.inventory_rank_changed,
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
                passes, config, successor,
                true, // The guarded successor means selected work is durably accounted.
            )
            .await
            .map_err(super::recent::RecentSelectionError)?;
        }
    }
    selected.checkpoint.observation = selected
        .checkpoint
        .observation
        .take()
        .zip(selected.sync_token.as_deref())
        .and_then(|(proof, next)| proof.delta(prior_cursor, next));
    Ok(selected)
}

/// Acknowledge only an already published, operation-bound output. No writer runs.
pub(super) async fn finish_prepared(
    passes: &[AlbumPass],
    config: &DownloadConfig,
    controls: DownloadControls,
    cancel: &CancellationToken,
) -> Result<usize> {
    if !controls.run_mode.downloads_files() {
        return Ok(0);
    }
    if !config.allows_existing_mutation() {
        return prepared_remaining(passes, config).await;
    }
    let Some(context) = context_for_format(passes, config, controls, 1).await? else {
        return prepared_remaining(passes, config).await;
    };
    let db = &context.capture.db;
    let cursor_key = format!(
        "prepared_metadata_offset:{}",
        digest(context.scope.as_bytes())
    );
    let offset = db
        .get_metadata(&cursor_key)
        .await?
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(0);
    let (pending, total) = db
        .prepared_selection_page(
            context.capture.owner.clone(),
            context.scope.clone(),
            context.config_hash.clone(),
            offset,
        )
        .await?;
    if total == 0 {
        return Ok(0);
    }
    let fetched = pending.len();
    for pending in pending {
        if cancel.is_cancelled() {
            break;
        }
        let Ok(pending) = pending else { continue };
        let Some((checksum, size)) = pending
            .selection_receipt
            .as_ref()
            .and_then(|r| r.prepared.as_ref())
        else {
            continue;
        };
        let Some(path) = pending.asset.local_path.as_deref() else {
            continue;
        };
        let Ok(_guard) = crate::download::file::lock_download_destination(path, cancel).await
        else {
            continue;
        };
        if db.guard_primary_writer(path).await.is_err() {
            continue;
        }
        let Ok(Some(file)) =
            crate::download::file::retain_pending_file(&config.directory, path).await
        else {
            continue;
        };
        if file.fingerprint.size != *size
            || data_encoding::HEXLOWER.encode(&file.fingerprint.sha256) != *checksum
        {
            continue;
        }
        if file.validate_blocking().is_err() {
            continue;
        }
        let _finished = db
            .finish_metadata_rewrite_checked(
                &pending,
                crate::state::db::MetadataRewriteQueue::Ordinary,
                Some(checksum),
                pending.asset.local_checksum.as_deref(),
                crate::state::db::MetadataRewriteCompletion::Ordinary,
                Some(Box::new(move || {
                    file.validate_blocking().map_err(|_changed| {
                        crate::state::error::StateError::Invariant {
                            operation: "finish_prepared",
                            detail: "prepared destination changed before acknowledgement".into(),
                        }
                    })
                })),
            )
            .await;
    }
    let (_, remaining) = db
        .prepared_selection_page(context.capture.owner, context.scope, context.config_hash, 0)
        .await?;
    let removed = total.saturating_sub(remaining);
    let next = (if offset < total { offset } else { 0 })
        .saturating_add(fetched)
        .saturating_sub(removed);
    db.set_metadata(
        &cursor_key,
        &if next < remaining {
            next.to_string()
        } else {
            "0".into()
        },
    )
    .await?;
    Ok(remaining)
}

/// Reporting is scope-bound even when current settings prohibit acknowledgement.
pub(super) async fn prepared_remaining(
    passes: &[AlbumPass],
    config: &DownloadConfig,
) -> Result<usize> {
    let mut scopes = std::collections::HashSet::new();
    let mut remaining = 0;
    for pass in passes {
        let Some((capture, scope, _)) = pass.album.download_scope()? else {
            continue;
        };
        if !scopes.insert(scope.clone()) {
            continue;
        }
        let Some(store) = &config.state_db else {
            continue;
        };
        let concrete = Arc::clone(&capture.db) as Arc<dyn super::models::DownloadStore>;
        anyhow::ensure!(
            Arc::ptr_eq(store, &concrete),
            "Selection database context mismatch"
        );
        remaining += capture
            .db
            .prepared_selection_page(capture.owner, scope, String::new(), 0)
            .await?
            .1;
    }
    Ok(remaining)
}

#[cfg(test)]
mod tests {
    use super::{InventoryProofReuse, can_reuse_inventory_proof};

    fn reusable() -> InventoryProofReuse {
        InventoryProofReuse {
            needs_inventory: true,
            delta_completed: true,
            inventory_rank_changed: false,
            state_transition_failures: 0,
            identity_incomplete: false,
            token_unsafe: false,
            cancelled: false,
            format: 2,
            proof_matches: true,
        }
    }

    #[test]
    fn inventory_proof_reuse_requires_every_condition() {
        let base = reusable();
        assert!(can_reuse_inventory_proof(base));
        assert!(!can_reuse_inventory_proof(InventoryProofReuse {
            needs_inventory: false,
            ..base
        }));
        assert!(!can_reuse_inventory_proof(InventoryProofReuse {
            delta_completed: false,
            ..base
        }));
        assert!(!can_reuse_inventory_proof(InventoryProofReuse {
            inventory_rank_changed: true,
            ..base
        }));
        assert!(!can_reuse_inventory_proof(InventoryProofReuse {
            state_transition_failures: 1,
            ..base
        }));
        assert!(!can_reuse_inventory_proof(InventoryProofReuse {
            identity_incomplete: true,
            ..base
        }));
        assert!(!can_reuse_inventory_proof(InventoryProofReuse {
            token_unsafe: true,
            ..base
        }));
        assert!(!can_reuse_inventory_proof(InventoryProofReuse {
            cancelled: true,
            ..base
        }));
        assert!(!can_reuse_inventory_proof(InventoryProofReuse {
            format: 1,
            ..base
        }));
        assert!(!can_reuse_inventory_proof(InventoryProofReuse {
            proof_matches: false,
            ..base
        }));
    }
}
