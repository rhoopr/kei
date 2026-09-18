//! Incremental source-state transitions, relation changes, and hydration.

use std::borrow::Cow;
use std::sync::Arc;

use anyhow::Result;
use chrono::{DateTime, Utc};
use rustc_hash::{FxHashMap, FxHashSet};

use crate::icloud::photos::asset::ChangeEvent;
use crate::icloud::photos::session::is_session_error as is_provider_session_error;
use crate::icloud::photos::{PhotoAsset, ProviderRecordId, RecordLookupRequest, RecordResolution};
use crate::types::ChangeReason;

use super::config::DownloadConfig;
use super::context::{
    ClaimedLegacyMasterStates, DownloadContext, LegacyOwnerClaimMode,
    legacy_owner_claim_mode_for_configs,
};
use super::models::{
    ALBUM_DELTA_STATE_WRITE_FAILED_REASON, ALBUM_RELATION_HYDRATION_INCOMPLETE_REASON,
    ASSET_DELTA_HYDRATION_INCOMPLETE_REASON, ASSET_MASTER_MAPPING_STATE_WRITE_FAILED_REASON,
    DownloadStore, INCREMENTAL_DELETE_STATE_WRITE_FAILED_REASON,
    INCREMENTAL_HIDDEN_STATE_WRITE_FAILED_REASON, UNKNOWN_ALBUM_RELATION_ASSET_REASON,
    UNKNOWN_ALBUM_RELATION_CONTAINER_REASON,
};
use super::selection::IncrementalPassRouting;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IncrementalStateTransition {
    SoftDelete,
    HardDelete,
    Hidden,
}

impl IncrementalStateTransition {
    const fn label(self) -> &'static str {
        match self {
            Self::SoftDelete => "soft-delete",
            Self::HardDelete => "hard-delete",
            Self::Hidden => "hidden",
        }
    }

    const fn write_failed_reason(self) -> &'static str {
        match self {
            Self::SoftDelete | Self::HardDelete => INCREMENTAL_DELETE_STATE_WRITE_FAILED_REASON,
            Self::Hidden => INCREMENTAL_HIDDEN_STATE_WRITE_FAILED_REASON,
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum SourceStateUpdate {
    SoftDeleted { deleted_at: Option<DateTime<Utc>> },
    Hidden,
}

impl SourceStateUpdate {
    const fn transition(self) -> IncrementalStateTransition {
        match self {
            Self::SoftDeleted { .. } => IncrementalStateTransition::SoftDelete,
            Self::Hidden => IncrementalStateTransition::Hidden,
        }
    }
}

#[derive(Debug, Clone)]
struct SourceStateTransitionKey<'a> {
    record_name: Cow<'a, str>,
    record_type: Option<&'a str>,
    unresolved_identity: bool,
}

impl SourceStateTransitionKey<'_> {
    fn record_name(&self) -> &str {
        self.record_name.as_ref()
    }
}

fn record_incremental_state_transition_result(
    result: Result<usize, crate::state::error::StateError>,
    transition: IncrementalStateTransition,
    state_key: SourceStateTransitionKey<'_>,
    state_transition_failures: &mut usize,
    token_unsafe_reason: &mut Option<&'static str>,
) {
    match result {
        Ok(updated) if updated > 0 => {}
        Ok(_) => {
            tracing::debug!(
                record_name = state_key.record_name(),
                record_type = state_key.record_type,
                transition = transition.label(),
                "Incremental source-state transition was already absent from state DB"
            );
        }
        Err(e) => {
            *state_transition_failures += 1;
            token_unsafe_reason.get_or_insert(transition.write_failed_reason());
            tracing::warn!(
                record_name = state_key.record_name(),
                error = %e,
                transition = transition.label(),
                "Failed to record incremental source-state transition in state DB"
            );
        }
    }
}

fn source_state_transition_key(event: &ChangeEvent) -> SourceStateTransitionKey<'_> {
    if let Some(asset) = event.asset.as_ref() {
        return SourceStateTransitionKey {
            record_name: Cow::Borrowed(asset.state_id()),
            record_type: if asset.state_id() == asset.id() {
                Some("CPLMaster")
            } else {
                Some("CPLAsset")
            },
            unresolved_identity: false,
        };
    }

    if matches!(event.record_type.as_deref(), Some("CPLAsset")) {
        if let Some(master_record_name) = event.master_record_name.as_deref() {
            return SourceStateTransitionKey {
                record_name: Cow::Borrowed(master_record_name),
                record_type: Some("CPLMaster"),
                unresolved_identity: false,
            };
        }
        return SourceStateTransitionKey {
            record_name: Cow::Borrowed(&event.record_name),
            record_type: event.record_type.as_deref(),
            unresolved_identity: true,
        };
    }

    SourceStateTransitionKey {
        record_name: Cow::Borrowed(&event.record_name),
        record_type: event.record_type.as_deref(),
        unresolved_identity: event.record_type.is_none(),
    }
}

fn unpaired_cplasset_state_key(event: &ChangeEvent) -> Option<SourceStateTransitionKey<'_>> {
    if event.asset.is_none() && matches!(event.record_type.as_deref(), Some("CPLAsset")) {
        return Some(SourceStateTransitionKey {
            record_name: Cow::Borrowed(&event.record_name),
            record_type: Some("CPLAsset"),
            unresolved_identity: false,
        });
    }
    None
}

async fn cplasset_master_fallback_state_key<'a>(
    event: &'a ChangeEvent,
    config: &DownloadConfig,
    db: &dyn DownloadStore,
) -> std::result::Result<Option<SourceStateTransitionKey<'a>>, crate::state::error::StateError> {
    if !matches!(event.record_type.as_deref(), Some("CPLAsset")) {
        return Ok(None);
    }

    if let Some(master_record_name) = event.master_record_name.as_deref() {
        return Ok(Some(SourceStateTransitionKey {
            record_name: Cow::Borrowed(master_record_name),
            record_type: Some("CPLMaster"),
            unresolved_identity: false,
        }));
    }

    let Some(master_record_name) = db
        .get_master_record_name_for_asset(&config.library, &event.record_name)
        .await?
    else {
        return Ok(None);
    };

    tracing::debug!(
        asset_record_name = %event.record_name,
        master_record_name = %master_record_name,
        library = %config.library,
        "Resolved source-state CPLAsset event through asset/master mapping"
    );
    Ok(Some(SourceStateTransitionKey {
        record_name: Cow::Owned(master_record_name),
        record_type: Some("CPLMaster"),
        unresolved_identity: false,
    }))
}

async fn run_source_state_update(
    db: &dyn DownloadStore,
    config: &DownloadConfig,
    key: &SourceStateTransitionKey<'_>,
    update: SourceStateUpdate,
) -> Result<usize, crate::state::error::StateError> {
    match update {
        SourceStateUpdate::SoftDeleted { deleted_at } => {
            db.resolve_source_deleted_affected(&config.library, key.record_name(), deleted_at)
                .await
        }
        SourceStateUpdate::Hidden => {
            db.mark_hidden_at_source_affected(&config.library, key.record_name())
                .await
        }
    }
}

async fn apply_source_state_update<'a>(
    db: &dyn DownloadStore,
    config: &DownloadConfig,
    event: &'a ChangeEvent,
    update: SourceStateUpdate,
) -> (
    Result<usize, crate::state::error::StateError>,
    SourceStateTransitionKey<'a>,
) {
    let Some(asset_key) = unpaired_cplasset_state_key(event) else {
        let state_key = source_state_transition_key(event);
        let result = run_source_state_update(db, config, &state_key, update).await;
        return (result, state_key);
    };

    let result = run_source_state_update(db, config, &asset_key, update).await;
    match result {
        Ok(0) => match cplasset_master_fallback_state_key(event, config, db).await {
            Ok(Some(fallback_key)) => {
                let fallback_result =
                    run_source_state_update(db, config, &fallback_key, update).await;
                (fallback_result, fallback_key)
            }
            Ok(None) => (Ok(0), asset_key),
            Err(e) => (Err(e), asset_key),
        },
        other => (other, asset_key),
    }
}

async fn hard_delete_state_transition_key<'a>(
    event: &'a ChangeEvent,
    config: &DownloadConfig,
    db: &dyn DownloadStore,
) -> std::result::Result<SourceStateTransitionKey<'a>, crate::state::error::StateError> {
    let fallback = source_state_transition_key(event);
    if !fallback.unresolved_identity {
        return Ok(fallback);
    }

    let Some(master_record_name) = db
        .get_master_record_name_for_asset(&config.library, &event.record_name)
        .await?
    else {
        return Ok(fallback);
    };

    tracing::debug!(
        asset_record_name = %event.record_name,
        master_record_name = %master_record_name,
        library = %config.library,
        "Resolved hard-delete asset tombstone through asset/master mapping"
    );
    Ok(SourceStateTransitionKey {
        record_name: Cow::Owned(master_record_name),
        record_type: Some("CPLMaster"),
        unresolved_identity: false,
    })
}

/// Routing work left to the execution strategy after shared delta bookkeeping.
#[derive(Debug, PartialEq, Eq)]
#[must_use]
pub(super) enum IncrementalDeltaRouting {
    Album,
    Relation,
    Created,
    None,
}

/// Shared facts for one zone delta. Callers retain their buffering and ordering:
/// streaming persists identity and applies source changes as events arrive;
/// collecting observes the whole delta before writes and applies relations before
/// routing. Hydration must not observe an event again or count it twice.
pub(super) struct IncrementalDeltaState<'a> {
    pub(super) summary: IncrementalDeltaSummary,
    pub(super) asset_to_master: FxHashMap<String, String>,
    planned_album_containers: FxHashMap<&'a str, &'a str>,
    ensured_planned_containers: FxHashSet<String>,
}

impl<'a> IncrementalDeltaState<'a> {
    pub(super) fn new(passes: &'a [crate::commands::AlbumPass]) -> Self {
        Self {
            summary: IncrementalDeltaSummary::default(),
            asset_to_master: FxHashMap::default(),
            planned_album_containers: passes
                .iter()
                .filter(|pass| pass.kind == crate::commands::PassKind::Album)
                .filter_map(|pass| {
                    pass.album
                        .container_id()
                        .map(|id| (id, pass.album.name.as_ref()))
                })
                .collect(),
            ensured_planned_containers: FxHashSet::default(),
        }
    }

    pub(super) fn observe_event(&mut self, event: &ChangeEvent) {
        self.summary.total_events += 1;
        if let Some(reason) = event.token_unsafe_reason {
            self.summary.token_unsafe_reason.get_or_insert(reason);
        }
    }

    pub(super) fn remember_asset_mapping(&mut self, event: &ChangeEvent) {
        if let Some(asset) = &event.asset {
            self.asset_to_master.insert(
                asset.asset_record_name().to_string(),
                asset.id().to_string(),
            );
        }
    }

    pub(super) async fn persist_asset_mapping(
        &mut self,
        event: &ChangeEvent,
        config: &DownloadConfig,
    ) {
        self.remember_asset_mapping(event);
        if let Some(asset) = &event.asset {
            self.summary
                .persist_asset_mapping_for_asset(asset, config)
                .await;
        }
    }

    fn routing_input(event: &ChangeEvent) -> IncrementalDeltaRouting {
        if event.album.is_some() {
            IncrementalDeltaRouting::Album
        } else if event.relation.is_some() {
            IncrementalDeltaRouting::Relation
        } else if event.token_unsafe_reason.is_some() || event.reason != ChangeReason::Created {
            IncrementalDeltaRouting::None
        } else {
            IncrementalDeltaRouting::Created
        }
    }

    pub(super) fn created_asset(event: &ChangeEvent) -> Option<&PhotoAsset> {
        if Self::routing_input(event) != IncrementalDeltaRouting::Created {
            return None;
        }
        event.asset.as_ref()
    }

    pub(super) async fn apply_event(
        &mut self,
        event: &ChangeEvent,
        config: &DownloadConfig,
    ) -> IncrementalDeltaRouting {
        let routing = Self::routing_input(event);
        match routing {
            IncrementalDeltaRouting::Created => self.summary.created_count += 1,
            IncrementalDeltaRouting::None if event.token_unsafe_reason.is_none() => {
                self.summary.apply_source_state_event(event, config).await;
            }
            _ => {}
        }
        routing
    }

    pub(super) async fn apply_album_event(&mut self, event: &ChangeEvent, config: &DownloadConfig) {
        apply_incremental_album_delta(event, config, &mut self.summary.token_unsafe_reason).await;
    }

    pub(super) async fn apply_relation_event(
        &mut self,
        event: &ChangeEvent,
        config: &DownloadConfig,
        routing: &IncrementalPassRouting,
    ) {
        apply_incremental_relation_delta(
            event,
            config,
            routing,
            &self.planned_album_containers,
            &mut self.ensured_planned_containers,
            &self.asset_to_master,
            &mut self.summary.token_unsafe_reason,
        )
        .await;
    }

    pub(super) fn record_completion(&mut self, token: Option<String>) {
        self.summary.sync_token = token;
    }
}

#[derive(Debug, Default)]
pub(super) struct IncrementalDeltaSummary {
    pub(super) sync_token: Option<String>,
    pub(super) token_unsafe_reason: Option<&'static str>,
    pub(super) created_count: u64,
    pub(super) soft_deleted_count: u64,
    pub(super) hard_deleted_count: u64,
    pub(super) hidden_count: u64,
    pub(super) total_events: u64,
    pub(super) state_transition_failures: usize,
    pub(super) auth_errors: usize,
    pub(super) first_auth_error: Option<anyhow::Error>,
}

impl IncrementalDeltaSummary {
    fn record_provider_error(&mut self, error: anyhow::Error) {
        if is_provider_session_error(&error) {
            self.auth_errors += 1;
            self.first_auth_error.get_or_insert(error);
        }
    }

    pub(super) async fn persist_asset_mapping_for_asset(
        &mut self,
        asset: &PhotoAsset,
        config: &DownloadConfig,
    ) {
        let library = asset.source_zone().unwrap_or(&config.library);
        self.persist_asset_master_mapping(asset.asset_record_name(), asset.id(), library, config)
            .await;
    }

    pub(super) async fn select_asset_state_identity(
        &mut self,
        asset: PhotoAsset,
        config: &DownloadConfig,
        download_ctx: &DownloadContext,
        claimed_legacy_master_states: &mut ClaimedLegacyMasterStates,
        claim_mode: LegacyOwnerClaimMode,
    ) -> Option<PhotoAsset> {
        let library = asset.source_zone().unwrap_or(&config.library);
        let selection = match claim_mode {
            LegacyOwnerClaimMode::ExistingOnly => {
                Ok(download_ctx.select_existing_asset_state_record_name(library, &asset))
            }
            LegacyOwnerClaimMode::ReadOnly => Ok(download_ctx.select_asset_state_record_name(
                library,
                &asset,
                claimed_legacy_master_states,
            )),
            LegacyOwnerClaimMode::Persist => {
                download_ctx
                    .select_asset_state_record_name_for_download(
                        config.state_db.as_deref(),
                        library,
                        &asset,
                        claimed_legacy_master_states,
                    )
                    .await
            }
        };
        let state_record_name = match selection {
            Ok(state_record_name) => state_record_name,
            Err(e) => {
                self.state_transition_failures += 1;
                self.token_unsafe_reason
                    .get_or_insert(ASSET_MASTER_MAPPING_STATE_WRITE_FAILED_REASON);
                tracing::warn!(
                    asset_id = %asset.id(),
                    asset_record_name = %asset.asset_record_name(),
                    library,
                    error = %e,
                    "Failed to claim legacy master state owner"
                );
                return None;
            }
        };
        Some(asset.with_state_record_name(state_record_name))
    }

    pub(super) async fn persist_asset_master_mapping(
        &mut self,
        asset_record_name: &str,
        master_record_name: &str,
        library: &str,
        config: &DownloadConfig,
    ) {
        let Some(db) = &config.state_db else {
            return;
        };
        if let Err(e) = db
            .upsert_asset_master_mapping(library, asset_record_name, master_record_name)
            .await
        {
            self.state_transition_failures += 1;
            self.token_unsafe_reason
                .get_or_insert(ASSET_MASTER_MAPPING_STATE_WRITE_FAILED_REASON);
            tracing::warn!(
                asset_id = master_record_name,
                asset_record_name,
                library,
                error = %e,
                "Failed to record asset/master mapping from incremental delta"
            );
        }
    }

    async fn apply_source_state_event(&mut self, event: &ChangeEvent, config: &DownloadConfig) {
        match event.reason {
            ChangeReason::Created => {}
            ChangeReason::SoftDeleted => {
                self.soft_deleted_count += 1;
                tracing::debug!(record_name = %event.record_name, record_type = ?event.record_type, "Skipping soft-deleted record");
                if let Some(db) = &config.state_db {
                    let deleted_at = event.asset.as_ref().and_then(|a| a.metadata().deleted_at);
                    let update = SourceStateUpdate::SoftDeleted { deleted_at };
                    let (result, state_key) =
                        apply_source_state_update(db.as_ref(), config, event, update).await;
                    record_incremental_state_transition_result(
                        result,
                        update.transition(),
                        state_key,
                        &mut self.state_transition_failures,
                        &mut self.token_unsafe_reason,
                    );
                }
            }
            ChangeReason::HardDeleted => {
                self.hard_deleted_count += 1;
                tracing::debug!(record_name = %event.record_name, record_type = ?event.record_type, "Skipping hard-deleted record");
                if let Some(db) = &config.state_db {
                    if event.record_type.is_none() && event.asset.is_none() {
                        let unresolved_tombstone_key = SourceStateTransitionKey {
                            record_name: Cow::Borrowed(&event.record_name),
                            record_type: None,
                            unresolved_identity: true,
                        };
                        match db
                            .resolve_master_family_source_deleted_affected(
                                &config.library,
                                unresolved_tombstone_key.record_name(),
                                None,
                            )
                            .await
                        {
                            Ok(updated) if updated > 0 => {
                                record_incremental_state_transition_result(
                                    Ok(updated),
                                    IncrementalStateTransition::HardDelete,
                                    unresolved_tombstone_key,
                                    &mut self.state_transition_failures,
                                    &mut self.token_unsafe_reason,
                                );
                                return;
                            }
                            Ok(_) => {}
                            Err(e) => {
                                record_incremental_state_transition_result(
                                    Err(e),
                                    IncrementalStateTransition::HardDelete,
                                    unresolved_tombstone_key,
                                    &mut self.state_transition_failures,
                                    &mut self.token_unsafe_reason,
                                );
                                return;
                            }
                        }
                    }

                    let state_key =
                        match hard_delete_state_transition_key(event, config, db.as_ref()).await {
                            Ok(state_key) => state_key,
                            Err(e) => {
                                self.state_transition_failures += 1;
                                self.token_unsafe_reason
                                    .get_or_insert(INCREMENTAL_DELETE_STATE_WRITE_FAILED_REASON);
                                tracing::warn!(
                                    record_name = %event.record_name,
                                    error = %e,
                                    "Failed to resolve hard-delete asset/master mapping"
                                );
                                return;
                            }
                        };
                    let result = if matches!(state_key.record_type, Some("CPLMaster"))
                        && !matches!(event.record_type.as_deref(), Some("CPLAsset") | None)
                    {
                        db.resolve_master_family_source_deleted_affected(
                            &config.library,
                            state_key.record_name(),
                            None,
                        )
                        .await
                    } else {
                        db.resolve_source_deleted_affected(
                            &config.library,
                            state_key.record_name(),
                            None,
                        )
                        .await
                    };
                    record_incremental_state_transition_result(
                        result,
                        IncrementalStateTransition::HardDelete,
                        state_key,
                        &mut self.state_transition_failures,
                        &mut self.token_unsafe_reason,
                    );
                }
            }
            ChangeReason::Hidden => {
                self.hidden_count += 1;
                tracing::debug!(record_name = %event.record_name, record_type = ?event.record_type, "Skipping hidden record");
                if let Some(db) = &config.state_db {
                    let update = SourceStateUpdate::Hidden;
                    let (result, state_key) =
                        apply_source_state_update(db.as_ref(), config, event, update).await;
                    record_incremental_state_transition_result(
                        result,
                        update.transition(),
                        state_key,
                        &mut self.state_transition_failures,
                        &mut self.token_unsafe_reason,
                    );
                }
            }
        }
    }

    pub(super) fn log_debug(&self) {
        tracing::debug!(
            created = self.created_count,
            soft_deleted = self.soft_deleted_count,
            hard_deleted = self.hard_deleted_count,
            hidden = self.hidden_count,
            "Incremental sync: {} change events",
            self.total_events,
        );
    }
}

async fn apply_incremental_album_delta(
    event: &ChangeEvent,
    config: &DownloadConfig,
    token_unsafe_reason: &mut Option<&'static str>,
) {
    let Some(album) = &event.album else {
        return;
    };
    let Some(db) = &config.state_db else {
        return;
    };
    let result = if album.is_deleted {
        if let Err(e) = db
            .mark_album_container_deleted(&config.library, &album.container_id)
            .await
        {
            Err(e)
        } else {
            db.invalidate_album_membership_snapshot(&config.library, &album.container_id)
                .await
        }
    } else {
        db.upsert_album_container(
            &config.library,
            &album.container_id,
            &album.album_name,
            "album",
        )
        .await
    };
    if let Err(e) = result {
        tracing::warn!(
            container_id = %album.container_id,
            error = %e,
            "Failed to apply album container delta"
        );
        token_unsafe_reason.get_or_insert(ALBUM_DELTA_STATE_WRITE_FAILED_REASON);
    }
}

async fn apply_incremental_relation_delta(
    event: &ChangeEvent,
    config: &DownloadConfig,
    routing: &IncrementalPassRouting,
    planned_album_containers: &FxHashMap<&str, &str>,
    ensured_planned_containers: &mut FxHashSet<String>,
    asset_to_master: &FxHashMap<String, String>,
    token_unsafe_reason: &mut Option<&'static str>,
) {
    let Some(relation) = &event.relation else {
        return;
    };
    let Some(db) = &config.state_db else {
        return;
    };

    let mut master_record_name = asset_to_master
        .get(relation.asset_record_name.as_ref())
        .cloned();
    if !relation.is_deleted && master_record_name.is_none() {
        match db
            .get_master_record_name_for_asset(&config.library, &relation.asset_record_name)
            .await
        {
            Ok(Some(master)) => {
                tracing::debug!(
                    container_id = %relation.container_id,
                    asset_record_name = %relation.asset_record_name,
                    master_record_name = %master,
                    library = %config.library,
                    "Resolved album relation asset through persisted asset/master mapping"
                );
                master_record_name = Some(master);
            }
            Ok(None) => {}
            Err(e) => {
                tracing::warn!(
                    container_id = %relation.container_id,
                    asset_record_name = %relation.asset_record_name,
                    library = %config.library,
                    error = %e,
                    "Failed to look up persisted asset/master mapping for album relation delta"
                );
                token_unsafe_reason.get_or_insert(ALBUM_RELATION_HYDRATION_INCOMPLETE_REASON);
            }
        }
    }

    if let Some(album_name) = planned_album_containers.get(relation.container_id.as_ref()) {
        let container_id = relation.container_id.as_ref();
        if !ensured_planned_containers.contains(container_id) {
            match db
                .upsert_album_container(
                    &config.library,
                    &relation.container_id,
                    album_name,
                    "album",
                )
                .await
            {
                Ok(()) => {
                    ensured_planned_containers.insert(container_id.to_string());
                }
                Err(e) => {
                    tracing::warn!(
                        container_id = %relation.container_id,
                        error = %e,
                        "Failed to upsert planned album container for relation delta"
                    );
                    token_unsafe_reason.get_or_insert(ALBUM_DELTA_STATE_WRITE_FAILED_REASON);
                }
            }
        }
    }

    let container_known = if relation.is_deleted {
        db.mark_album_membership_deleted(
            &config.library,
            &relation.container_id,
            &relation.asset_record_name,
        )
        .await
    } else {
        db.upsert_album_membership_delta(
            &config.library,
            &relation.container_id,
            &relation.asset_record_name,
            master_record_name.as_deref(),
            "icloud",
        )
        .await
    };

    match container_known {
        Ok(true) => {}
        Ok(false) => {
            tracing::warn!(
                container_id = %relation.container_id,
                asset_record_name = %relation.asset_record_name,
                "Album relation delta referenced an unknown album container"
            );
            if routing
                .album_passes_for_container(&relation.container_id)
                .is_some()
            {
                token_unsafe_reason.get_or_insert(UNKNOWN_ALBUM_RELATION_CONTAINER_REASON);
            }
        }
        Err(e) => {
            tracing::warn!(
                container_id = %relation.container_id,
                asset_record_name = %relation.asset_record_name,
                error = %e,
                "Failed to apply album relation delta"
            );
            token_unsafe_reason.get_or_insert(ALBUM_DELTA_STATE_WRITE_FAILED_REASON);
        }
    }

    if !relation.is_deleted
        && routing
            .album_passes_for_container(&relation.container_id)
            .is_some()
        && master_record_name.is_none()
    {
        tracing::warn!(
            container_id = %relation.container_id,
            asset_record_name = %relation.asset_record_name,
            "Selected album relation add referenced an asset not present in the delta page set"
        );
        token_unsafe_reason.get_or_insert(UNKNOWN_ALBUM_RELATION_ASSET_REASON);
    }
}

pub(super) async fn hydrate_unpaired_created_asset_deltas(
    events: &mut [ChangeEvent],
    pass: Option<&crate::commands::AlbumPass>,
    config: &DownloadConfig,
    summary: &mut IncrementalDeltaSummary,
) {
    let mut pending = Vec::new();
    let mut unresolved: FxHashMap<String, Vec<usize>> = FxHashMap::default();
    for (index, event) in events.iter().enumerate() {
        if event.reason != ChangeReason::Created
            || event.asset.is_some()
            || !matches!(event.record_type.as_deref(), Some("CPLAsset"))
        {
            continue;
        }

        let master_record_name = if let Some(master) = event.master_record_name.as_deref() {
            Some(master.to_string())
        } else if let Some(db) = &config.state_db {
            match db
                .get_master_record_name_for_asset(&config.library, &event.record_name)
                .await
            {
                Ok(master) => master,
                Err(e) => {
                    summary.state_transition_failures += 1;
                    summary
                        .token_unsafe_reason
                        .get_or_insert(ASSET_DELTA_HYDRATION_INCOMPLETE_REASON);
                    tracing::warn!(
                        asset_record_name = %event.record_name,
                        library = %config.library,
                        error = %e,
                        "Failed to resolve asset-only delta through persisted mapping"
                    );
                    continue;
                }
            }
        } else {
            None
        };

        match master_record_name {
            Some(master_record_name) => {
                pending.push((index, event.record_name.to_string(), master_record_name));
            }
            None => unresolved
                .entry(event.record_name.to_string())
                .or_default()
                .push(index),
        }
    }

    if pending.is_empty() && unresolved.is_empty() {
        return;
    }
    let Some(pass) = pass else {
        summary
            .token_unsafe_reason
            .get_or_insert(ASSET_DELTA_HYDRATION_INCOMPLETE_REASON);
        return;
    };

    if !unresolved.is_empty() {
        let requests: Vec<RecordLookupRequest> = unresolved
            .keys()
            .map(|record_name| {
                RecordLookupRequest::asset_only(ProviderRecordId::new(record_name.as_str()))
            })
            .collect();
        let identity_resolutions = pass.album.resolve_records(&requests).await;
        for (state_id, resolution) in identity_resolutions.results {
            let Some(indices) = unresolved.remove(state_id.as_str()) else {
                summary
                    .token_unsafe_reason
                    .get_or_insert(ASSET_DELTA_HYDRATION_INCOMPLETE_REASON);
                continue;
            };
            match resolution {
                RecordResolution::AssetPresent { master_record_name } => {
                    summary
                        .persist_asset_master_mapping(
                            state_id.as_str(),
                            master_record_name.as_str(),
                            &config.library,
                            config,
                        )
                        .await;
                    pending.extend(indices.into_iter().map(|index| {
                        (
                            index,
                            state_id.as_str().to_string(),
                            master_record_name.as_str().to_string(),
                        )
                    }));
                }
                RecordResolution::Deleted {
                    master_family: false,
                    ..
                } => {
                    tracing::debug!(
                        asset_record_name = state_id.as_str(),
                        library = %config.library,
                        "Asset-only delta disappeared before identity recovery"
                    );
                }
                RecordResolution::TransientFailure(error) => {
                    summary
                        .token_unsafe_reason
                        .get_or_insert(ASSET_DELTA_HYDRATION_INCOMPLETE_REASON);
                    tracing::warn!(
                        diagnostic = error.diagnostic(),
                        "Failed to recover master identity for asset-only delta"
                    );
                    summary.record_provider_error(error.into());
                }
                RecordResolution::Present(_)
                | RecordResolution::MasterPresent
                | RecordResolution::Deleted {
                    master_family: true,
                    ..
                }
                | RecordResolution::Unknown => {
                    summary
                        .token_unsafe_reason
                        .get_or_insert(ASSET_DELTA_HYDRATION_INCOMPLETE_REASON);
                    tracing::warn!(
                        asset_record_name = state_id.as_str(),
                        library = %config.library,
                        "Asset-only delta had no usable master identity"
                    );
                }
            }
        }
        if !unresolved.is_empty() {
            summary
                .token_unsafe_reason
                .get_or_insert(ASSET_DELTA_HYDRATION_INCOMPLETE_REASON);
        }
    }

    if pending.is_empty() || summary.auth_errors > 0 {
        return;
    }
    let requests: Vec<RecordLookupRequest> = pending
        .iter()
        .map(|(_, record_name, master)| {
            RecordLookupRequest::paired(
                ProviderRecordId::new(record_name.as_str()),
                ProviderRecordId::new(master.as_str()),
                ProviderRecordId::new(record_name.as_str()),
            )
        })
        .collect();
    let event_by_record_name: FxHashMap<String, (usize, String)> = pending
        .iter()
        .map(|(index, record_name, master)| (record_name.clone(), (*index, master.clone())))
        .collect();
    let resolutions = pass.album.resolve_records(&requests).await;
    for (state_id, resolution) in resolutions.results {
        let Some((index, master_record_name)) = event_by_record_name.get(state_id.as_str()) else {
            summary
                .token_unsafe_reason
                .get_or_insert(ASSET_DELTA_HYDRATION_INCOMPLETE_REASON);
            continue;
        };
        let Some(event) = events.get_mut(*index) else {
            summary
                .token_unsafe_reason
                .get_or_insert(ASSET_DELTA_HYDRATION_INCOMPLETE_REASON);
            continue;
        };
        match resolution {
            RecordResolution::Present(asset)
                if asset.asset_record_name() == event.record_name.as_ref() =>
            {
                summary
                    .persist_asset_mapping_for_asset(&asset, config)
                    .await;
                event.asset = Some(asset);
            }
            RecordResolution::Deleted {
                deleted_at,
                master_family,
            } => {
                if let Some(db) = &config.state_db {
                    let update = SourceStateUpdate::SoftDeleted { deleted_at };
                    let (result, state_key) = if master_family {
                        let state_key = SourceStateTransitionKey {
                            record_name: Cow::Borrowed(master_record_name),
                            record_type: Some("CPLMaster"),
                            unresolved_identity: false,
                        };
                        let result = db
                            .resolve_master_family_source_deleted_affected(
                                &config.library,
                                master_record_name,
                                deleted_at,
                            )
                            .await;
                        (result, state_key)
                    } else {
                        apply_source_state_update(db.as_ref(), config, event, update).await
                    };
                    record_incremental_state_transition_result(
                        result,
                        update.transition(),
                        state_key,
                        &mut summary.state_transition_failures,
                        &mut summary.token_unsafe_reason,
                    );
                }
            }
            RecordResolution::TransientFailure(error) => {
                summary
                    .token_unsafe_reason
                    .get_or_insert(ASSET_DELTA_HYDRATION_INCOMPLETE_REASON);
                tracing::warn!(
                    diagnostic = error.diagnostic(),
                    "Provider lookup could not hydrate an asset-only delta"
                );
                summary.record_provider_error(error.into());
            }
            RecordResolution::Present(_)
            | RecordResolution::AssetPresent { .. }
            | RecordResolution::MasterPresent
            | RecordResolution::Unknown => {
                summary
                    .token_unsafe_reason
                    .get_or_insert(ASSET_DELTA_HYDRATION_INCOMPLETE_REASON);
                tracing::warn!(
                    diagnostic = "incomplete_record_pair",
                    "Could not hydrate a complete asset-only delta"
                );
            }
        }
    }
    if !resolutions.complete {
        summary
            .token_unsafe_reason
            .get_or_insert(ASSET_DELTA_HYDRATION_INCOMPLETE_REASON);
    }
}

pub(super) struct IncrementalAssetHydrationContext<'a> {
    pub(super) asset_to_master: &'a mut FxHashMap<String, String>,
    pub(super) complete_delta_assets: &'a mut Vec<PhotoAsset>,
    pub(super) downloadable_assets: &'a mut Vec<(PhotoAsset, usize)>,
    pub(super) download_ctx: Option<&'a DownloadContext>,
    pub(super) claimed_legacy_master_states: &'a mut ClaimedLegacyMasterStates,
    pub(super) claim_mode: LegacyOwnerClaimMode,
    pub(super) pass_configs: &'a [Arc<DownloadConfig>],
}

pub(super) async fn hydrate_missing_selected_relation_assets(
    change_events: &[ChangeEvent],
    passes: &[crate::commands::AlbumPass],
    config: &DownloadConfig,
    routing: &IncrementalPassRouting,
    context: &mut IncrementalAssetHydrationContext<'_>,
    delta_summary: &mut IncrementalDeltaSummary,
) {
    let mut missing_by_hydrator: FxHashMap<usize, FxHashSet<String>> = FxHashMap::default();
    let mut pass_indices_by_asset: FxHashMap<String, FxHashSet<usize>> = FxHashMap::default();

    for event in change_events {
        let Some(relation) = &event.relation else {
            continue;
        };
        if relation.is_deleted {
            continue;
        }
        let Some(pass_indices) = routing.album_passes_for_container(&relation.container_id) else {
            continue;
        };
        let Some(pass_index) = pass_indices.first().copied() else {
            continue;
        };

        if context
            .asset_to_master
            .contains_key(relation.asset_record_name.as_ref())
        {
            continue;
        }

        pass_indices_by_asset
            .entry(relation.asset_record_name.to_string())
            .or_default()
            .extend(pass_indices.iter().copied());
        missing_by_hydrator
            .entry(pass_index)
            .or_default()
            .insert(relation.asset_record_name.to_string());
    }

    let mut hydrated_asset_record_names = FxHashSet::default();
    for (pass_index, mut missing) in missing_by_hydrator {
        missing.retain(|asset_record_name| {
            !context
                .asset_to_master
                .contains_key(asset_record_name.as_str())
                && !hydrated_asset_record_names.contains(asset_record_name.as_str())
        });
        if missing.is_empty() {
            continue;
        }
        let Some(pass) = passes.get(pass_index) else {
            continue;
        };

        let assets = match pass
            .album
            .hydrate_matching_assets_from_changes(&mut missing)
            .await
        {
            Ok(assets) => assets,
            Err(e) => {
                tracing::warn!(
                    pass_index,
                    error = %e,
                    "Failed to hydrate missing selected album relation assets"
                );
                delta_summary.record_provider_error(e);
                delta_summary
                    .token_unsafe_reason
                    .get_or_insert(ALBUM_RELATION_HYDRATION_INCOMPLETE_REASON);
                continue;
            }
        };

        for asset in assets {
            delta_summary
                .persist_asset_mapping_for_asset(&asset, config)
                .await;
            let asset_record_name = asset.asset_record_name().to_string();
            let pass_indices = pass_indices_by_asset.get(asset_record_name.as_str());
            let claim_mode = legacy_owner_claim_mode_for_configs(
                context.claim_mode,
                &asset,
                pass_indices
                    .into_iter()
                    .flat_map(|pass_indices| pass_indices.iter())
                    .filter_map(|pass_index| context.pass_configs.get(*pass_index))
                    .map(AsRef::as_ref),
            );
            let asset = if let Some(download_ctx) = context.download_ctx {
                let Some(asset) = delta_summary
                    .select_asset_state_identity(
                        asset,
                        config,
                        download_ctx,
                        context.claimed_legacy_master_states,
                        claim_mode,
                    )
                    .await
                else {
                    continue;
                };
                asset
            } else {
                asset
            };
            hydrated_asset_record_names.insert(asset_record_name.clone());
            context
                .asset_to_master
                .insert(asset_record_name.clone(), asset.id().to_string());
            context.complete_delta_assets.push(asset.clone());
            if let Some(pass_indices) = pass_indices {
                for pass_index in pass_indices {
                    context
                        .downloadable_assets
                        .push((asset.clone(), *pass_index));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests;
