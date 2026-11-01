//! Narrow, opt-in direct-zone eligibility for a compact source attestation.
//!
//! This does not change selection, path planning, metadata writes or retry
//! deadlines. Unsupported or historical roots do not obtain a capability.
use super::{SelectionContext, filter, hash_download_config, selection_policy};
use crate::commands::{AlbumPass, PassKind};
use crate::download::DownloadConfig;
use crate::icloud::photos::{PhotoAsset, current_asset, same_selected_facts};
use crate::state::db::provider_generations::ActiveGeneration;
use crate::state::db::provider_selection::SelectionPath;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

/// Created by the production generation owner from the actual base and pass
/// configuration; state accepts it only at the new-root insertion boundary.
pub(crate) struct NewRootPolicy {
    preimage: Vec<u8>,
    snapshot: Vec<u8>,
}
impl NewRootPolicy {
    pub(crate) fn matches_spec(&self, root: &ActiveGeneration) -> bool {
        let Ok(value) = serde_json::from_slice::<Value>(&self.preimage) else {
            return false;
        };
        data_encoding::HEXLOWER.encode(&Sha256::digest(&self.preimage)) == root.spec.config_hash
            && value.get("scope").and_then(Value::as_str) == Some(root.spec.scope.as_str())
            && value.get("zone") == Some(&root.spec.zone)
            && value.get("profile") == Some(&root.spec.profile)
    }
    pub(crate) fn preimage(&self) -> &[u8] {
        &self.preimage
    }
    pub(crate) fn snapshot(&self) -> &[u8] {
        &self.snapshot
    }
}

/// Pass-level current policy, minted from a real selection run. It does not
/// authorize a source receipt or create a new-root policy marker.
pub(crate) struct CurrentPassPolicy {
    owner: crate::state::db::account::AccountOwner,
    generation: String,
    pass: String,
    preimage: Vec<u8>,
    snapshot: Vec<u8>,
}
impl CurrentPassPolicy {
    pub(crate) fn matches(&self, root: &ActiveGeneration, pass: &str) -> bool {
        self.generation == root.id
            && self.pass == pass
            && NewRootPolicy {
                preimage: self.preimage.clone(),
                snapshot: Vec::new(),
            }
            .matches_spec(root)
    }
    pub(crate) fn matches_owner(&self, owner: &crate::state::db::account::AccountOwner) -> bool {
        &self.owner == owner
    }
    pub(crate) fn resolved_snapshot(&self) -> &[u8] {
        &self.snapshot
    }
    pub(crate) fn policy_preimage(&self) -> &[u8] {
        &self.preimage
    }
}

fn supports_current_pass(
    retry_only: bool,
    provisional: bool,
    indexing_only: bool,
    has_sparse_targets: bool,
) -> bool {
    !retry_only && !provisional && !indexing_only && !has_sparse_targets
}

/// Bind the pass witness to the actual authenticated selection run and source.
/// Callers must independently compare complete snapshot and preimage bytes
/// against each durable link, and verify the owner in the state transaction.
pub(super) fn mint_current_pass(
    context: &SelectionContext,
    run: &super::SelectionRun,
    pass: &AlbumPass,
    config: &DownloadConfig,
) -> Option<CurrentPassPolicy> {
    if !supports_current_pass(
        config.retry_only,
        config.provisional,
        config.indexing_inventory_only,
        !config.sparse_targets.is_empty(),
    ) || context.capture.owner != run.capture.owner
        || !std::sync::Arc::ptr_eq(&context.capture.db, &run.capture.db)
        || context.scope != run.root.spec.scope
        || context.zone != run.root.spec.zone
        || context.profile != run.root.spec.profile
        || context.format != run.root.spec.format
        || context.config_hash != run.root.spec.config_hash
        || run.capture.owner.authenticated_keys().is_err()
    {
        return None;
    }
    let sources = pass.album.selector_sources().ok()?;
    let [(capture, scope, zone)] = sources.as_slice() else {
        return None;
    };
    if capture.owner != run.capture.owner
        || !std::sync::Arc::ptr_eq(&capture.db, &run.capture.db)
        || scope != &context.scope
        || zone != &context.zone
    {
        return None;
    }
    let policy = prepare_new_root(context, pass, config)?;
    if !policy.matches_spec(&run.root) {
        return None;
    }
    Some(CurrentPassPolicy {
        owner: run.capture.owner.clone(),
        generation: run.root.id.clone(),
        pass: super::pass_key(pass),
        preimage: policy.preimage,
        snapshot: policy.snapshot,
    })
}

/// Private-field capability minted only by this filter owner from the actual
/// current configuration and confirmed asset. It is not a completed route.
pub(crate) struct CurrentPolicyEligibility {
    generation: String,
    pass: String,
    child: String,
    master: String,
    confirmation_hash: [u8; 32],
    policy_preimage: Vec<u8>,
    resolved_snapshot: Vec<u8>,
}

impl CurrentPolicyEligibility {
    pub(crate) fn matches(
        &self,
        root: &ActiveGeneration,
        pass: &str,
        child: &str,
        master: &str,
        confirmation: &[u8],
    ) -> bool {
        self.generation == root.id
            && self.pass == pass
            && self.child == child
            && self.master == master
            && self.confirmation_hash == <[u8; 32]>::from(Sha256::digest(confirmation))
            && data_encoding::HEXLOWER.encode(&Sha256::digest(&self.policy_preimage))
                == root.spec.config_hash
    }
    pub(crate) fn policy_preimage(&self) -> &[u8] {
        &self.policy_preimage
    }
    pub(crate) fn resolved_snapshot(&self) -> &[u8] {
        &self.resolved_snapshot
    }
}

/// Canonical, versioned full input set for the existing path and eligibility
/// hash plus exact retry fields. Native path encoding is lossless. It is an
/// additional comparison, never ownership proof or a replacement for current
/// filter evaluation.
fn resolved_snapshot(config: &DownloadConfig, pass: &AlbumPass) -> Option<Vec<u8>> {
    let mut excludes: Vec<&str> = pass.exclude_ids.iter().map(String::as_str).collect();
    excludes.sort_unstable();
    let patterns: Vec<&str> = config
        .filename_exclude
        .iter()
        .map(glob::Pattern::as_str)
        .collect();
    let value = json!({
        "version":1,
        "directory":SelectionPath::from_path(&config.directory),
        "library":config.library.as_ref(),
        "folder_structure":config.folder_structure,
        "folder_structure_albums":config.folder_structure_albums.as_ref(),
        "folder_structure_smart_folders":config.folder_structure_smart_folders.as_ref(),
        "resolution":config.resolution as u8,
        "live_resolution":config.live_resolution as u8,
        "file_match_policy":config.file_match_policy as u8,
        "live_photo_mov_filename_policy":config.live_photo_mov_filename_policy as u8,
        "edited":config.edited,"edited_naming":format!("{:?}",config.edited_naming),
        "alternative":config.alternative,"raw_policy":config.raw_policy as u8,
        "keep_unicode_in_filenames":config.keep_unicode_in_filenames,
        "before":config.skip_created_before.map(crate::config::CreatedDateFilter::fingerprint),
        "after":config.skip_created_after.map(crate::config::CreatedDateFilter::fingerprint),
        "force_resolution":config.force_resolution,
        "media":{"photos":config.media.photos,"videos":config.media.videos,"live_photos":config.media.live_photos},
        "live_photo_mode":config.live_photo_mode as u8,"filename_exclude":patterns,
        "pass_excludes":excludes,"recent":config.recent,"recent_scope":format!("{:?}",config.recent_scope),
        "retry":{"max_retries":config.retry.max_retries,"base_delay_secs":config.retry.base_delay_secs,
                 "max_delay_secs":config.retry.max_delay_secs,"max_download_attempts":config.max_download_attempts},
        "temp_suffix":config.temp_suffix.as_ref(),
        "metadata_flags":crate::download::pipeline::MetadataFlags::from(config).bits(),
        "refresh_metadata":config.refresh_metadata,
        "legacy_preservation_allow_hardlinks":config.legacy_preservation_allow_hardlinks,
        "complete_path_selection":config.complete_path_selection,
        "capture_repair":format!("{:?}",config.capture_timestamp_repair),
        "repair_truncated":config.repair_truncated,
        "enum_hash":config.enum_config_hash.as_deref(),
    });
    serde_json::to_vec(&value).ok()
}

fn supports_managed_layout(
    naming: crate::types::EditedNaming,
    active: bool,
    preview: bool,
) -> bool {
    naming != crate::types::EditedNaming::Primary && !active && !preview
}

/// Capture exact preimage and resolved values from the actual base/pass config
/// before a new root is inserted. Unsupported contexts do not get a marker.
pub(super) fn prepare_new_root(
    context: &SelectionContext,
    pass: &AlbumPass,
    config: &DownloadConfig,
) -> Option<NewRootPolicy> {
    if !supports_managed_layout(
        config.edited_naming,
        config.primary_layout_active,
        config.primary_layout_preview,
    ) || context.format != 2
        || context.zone.get("zoneName").and_then(Value::as_str) != Some(config.library.as_ref())
        || config.recent.is_some()
        || !config.exclude_asset_ids.is_empty()
        || !pass.exclude_ids.is_empty()
        || pass.kind != PassKind::Unfiled
        || context.profile.get("selector_binding").is_some()
    {
        return None;
    }
    let passes = context.profile.get("passes")?.as_array()?;
    if passes.len() != 1
        || context.profile.get("recent")? != &Value::Null
        || !context.profile.get("excludes")?.as_array()?.is_empty()
    {
        return None;
    }
    let expected = passes.first()?;
    let scope = expected.get("scope")?;
    if expected.get("key")?.as_str()? != super::pass_key(pass)
        || expected.get("kind")?.as_str()? != "Unfiled"
        || expected.get("scope")? != &pass.album.selection_scope()
        || !expected.get("excludes")?.as_array()?.is_empty()
        || scope.get("list_type")?.as_str()? != "CPLAssetAndMasterByAssetDateWithoutHiddenOrDeleted"
        || scope.get("query_filter")? != &Value::Null
        || scope.get("container_id")? != &Value::Null
        || !scope.get("cross_zone_sources")?.as_array()?.is_empty()
        || scope.get("zone")? != &context.zone
    {
        return None;
    }
    let anchored = SelectionPath::from_path(&std::path::absolute(&config.directory).ok()?);
    let mut fingerprint = selection_policy(
        config,
        &context.scope,
        &context.zone,
        None,
        hash_download_config(config),
        config.enum_config_hash.as_deref(),
        &anchored,
    );
    fingerprint
        .as_object_mut()?
        .insert("profile".into(), context.profile.clone());
    let preimage = serde_json::to_vec(&fingerprint).ok()?;
    if data_encoding::HEXLOWER.encode(&Sha256::digest(&preimage)) != context.config_hash {
        return None;
    }
    Some(NewRootPolicy {
        preimage,
        snapshot: resolved_snapshot(config, pass)?,
    })
}

/// Mint only after actual direct-zone routing and current confirmation. All
/// non-direct, recent-ranked, excluded, cross-zone, or stale inputs fall back.
pub(super) fn mint(
    context: &SelectionContext,
    root: &ActiveGeneration,
    pass: &AlbumPass,
    config: &DownloadConfig,
    observed: &PhotoAsset,
    confirmation: &[u8],
    stored_policy:&crate::state::db::provider_generations::compact_attestation::StoredPolicyPreimage,
) -> Option<CurrentPolicyEligibility> {
    if !supports_managed_layout(
        config.edited_naming,
        config.primary_layout_active,
        config.primary_layout_preview,
    ) || confirmation.len() > 16 * 1024 * 1024
        || root.spec.zone.get("zoneName").and_then(Value::as_str) != Some(config.library.as_ref())
        || root.spec.format != 2
        || context.config_hash != root.spec.config_hash
        || context.scope != root.spec.scope
        || context.zone != root.spec.zone
        || context.profile != root.spec.profile
        || config.recent.is_some()
        || !config.exclude_asset_ids.is_empty()
        || !pass.exclude_ids.is_empty()
        || pass.kind != PassKind::Unfiled
        || root.spec.profile.get("selector_binding").is_some()
        || observed.source_zone() != root.spec.zone.get("zoneName").and_then(Value::as_str)
    {
        return None;
    }
    let passes = root.spec.profile.get("passes")?.as_array()?;
    if passes.len() != 1 || root.spec.profile.get("recent")? != &Value::Null {
        return None;
    }
    let expected = passes.first()?;
    let scope = expected.get("scope")?;
    if scope.get("list_type")?.as_str()? != "CPLAssetAndMasterByAssetDateWithoutHiddenOrDeleted"
        || scope.get("query_filter")? != &Value::Null
        || scope.get("container_id")? != &Value::Null
        || !scope.get("cross_zone_sources")?.as_array()?.is_empty()
        || scope.get("zone")? != &root.spec.zone
    {
        return None;
    }
    let key = super::pass_key(pass);
    if expected.get("key")?.as_str()? != key
        || expected.get("kind")?.as_str()? != "Unfiled"
        || expected.get("scope")? != &pass.album.selection_scope()
        || !expected.get("excludes")?.as_array()?.is_empty()
        || !root.spec.profile.get("excludes")?.as_array()?.is_empty()
    {
        return None;
    }
    let child = observed.asset_record_name();
    let master = observed.id();
    let confirmed = current_asset(confirmation, child, master, &root.spec.zone).ok()?;
    if !same_selected_facts(observed, &confirmed) {
        return None;
    }
    let pass_config = config.with_pass(pass);
    if filter::is_asset_filtered(&confirmed, &pass_config).is_some()
        || filter::fallback_filename_excluded(&confirmed, &pass_config)
    {
        return None;
    }
    let anchored = SelectionPath::from_path(&std::path::absolute(&config.directory).ok()?);
    let mut fingerprint = selection_policy(
        config,
        &context.scope,
        &context.zone,
        None,
        hash_download_config(config),
        config.enum_config_hash.as_deref(),
        &anchored,
    );
    fingerprint
        .as_object_mut()?
        .insert("profile".into(), context.profile.clone());
    let policy_preimage = serde_json::to_vec(&fingerprint).ok()?;
    if data_encoding::HEXLOWER.encode(&Sha256::digest(&policy_preimage)) != root.spec.config_hash {
        return None;
    }
    let resolved_snapshot = resolved_snapshot(config, pass)?;
    if policy_preimage.len() > 16 * 1024 * 1024 || resolved_snapshot.len() > 16 * 1024 * 1024 {
        return None;
    }
    if !stored_policy.matches(root, &policy_preimage, &resolved_snapshot) {
        return None;
    }
    Some(CurrentPolicyEligibility {
        generation: root.id.clone(),
        pass: key,
        child: child.to_owned(),
        master: master.to_owned(),
        confirmation_hash: Sha256::digest(confirmation).into(),
        policy_preimage,
        resolved_snapshot,
    })
}

#[cfg(test)]
mod tests {
    use super::resolved_snapshot;
    use crate::config::CreatedDateFilter;
    use crate::download::orchestration::test_support::{test_config, unused_unfiled_changes_pass};
    use std::sync::Arc;

    #[test]
    fn resolved_policy_distinguishes_date_type_retry_and_glob_inputs() {
        let pass = unused_unfiled_changes_pass();
        let base = test_config();
        let first = resolved_snapshot(&base, &pass).unwrap();
        let mut changed = base.clone();
        changed.skip_created_before = Some(CreatedDateFilter::CaptureDate(
            chrono::NaiveDate::from_ymd_opt(2025, 1, 1).unwrap(),
        ));
        let capture_date = resolved_snapshot(&changed, &pass).unwrap();
        assert_ne!(first, capture_date);
        changed.skip_created_before = Some(CreatedDateFilter::Instant(
            chrono::DateTime::parse_from_rfc3339("2025-01-01T00:00:00Z")
                .unwrap()
                .with_timezone(&chrono::Utc),
        ));
        assert_ne!(capture_date, resolved_snapshot(&changed, &pass).unwrap());
        changed = base.clone();
        changed.retry.base_delay_secs = changed.retry.base_delay_secs.saturating_add(1);
        assert_ne!(first, resolved_snapshot(&changed, &pass).unwrap());
        changed = base.clone();
        changed.filename_exclude = Arc::from(vec![glob::Pattern::new("*.tmp").unwrap()]);
        assert_ne!(first, resolved_snapshot(&changed, &pass).unwrap());
    }
}

#[cfg(test)]
mod managed_layout_tests {
    use super::supports_managed_layout;
    use crate::types::EditedNaming;

    #[test]
    fn managed_primary_never_authorizes_compact_retry() {
        assert!(!supports_managed_layout(
            EditedNaming::Primary,
            false,
            false
        ));
        assert!(!supports_managed_layout(EditedNaming::Primary, true, false));
        assert!(!supports_managed_layout(EditedNaming::Primary, false, true));
        assert!(!supports_managed_layout(EditedNaming::Suffix, true, false));
        assert!(!supports_managed_layout(EditedNaming::Suffix, false, true));
        assert!(supports_managed_layout(EditedNaming::Suffix, false, false));
    }
}

#[cfg(test)]
mod current_pass_mode_tests {
    use super::supports_current_pass;
    #[test]
    fn specialized_modes_cannot_suppress_current_pass() {
        assert!(supports_current_pass(false, false, false, false));
        assert!(!supports_current_pass(true, false, false, false));
        assert!(!supports_current_pass(false, true, false, false));
        assert!(!supports_current_pass(false, false, true, false));
        assert!(!supports_current_pass(false, false, false, true));
    }
}
