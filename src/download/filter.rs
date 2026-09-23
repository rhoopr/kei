//! Asset filtering, version selection, and download task derivation.
//!
//! Ownership:
//! - `config`: shared sync/import path and filter settings.
//! - `eligibility`: content/date/filename filters and media classification.
//! - `versions`: RAW alignment, version/companion selection, and metadata keys.
//! - `metadata`: task metadata payloads and album/people enrichment.
//! - `expected_paths`: bare paths shared by sync and import.
//! - `collisions`: normalized claims and collision path resolution.
//! - `tasks`: composition of paths, collision results, and metadata into tasks.
//!
//! Production dependencies point from tasks to expected paths, collisions, and
//! metadata; expected paths use version selection. Configuration is shared.
//! Children do not depend on task construction. Path rendering stays in
//! `crate::download::paths`; filter policy stays here.
//!
//! Tests retain their names and assertions. Their paths change from
//! `download::filter::tests::<name>` to
//! `download::filter::<owner>::tests::<name>`. Shared fixtures are test-only.
//!
//! This facade retains the existing paths and effective visibility.

mod collisions;
mod config;
mod eligibility;
mod expected_paths;
mod metadata;
mod tasks;
mod versions;

#[cfg(test)]
mod test_support;

pub(super) use collisions::{NormalizedPath, PathPlanningMode, pre_ensure_asset_dir};
pub(super) use config::folder_structure_for_pass;
pub(crate) use config::{PathDerivationConfig, PathDerivationSource};
pub(crate) use eligibility::{FilterReason, determine_media_type, is_asset_filtered};
pub(super) use expected_paths::{
    DerivationContext, DerivedPath, MalformedTaskResource, derive_alternative_extra,
    derive_edited_extra, derive_expected_paths, derive_live_edited_extra, derive_mov_companion,
    derive_primary, malformed_no_task_resource,
};
pub(crate) use expected_paths::{ExpectedAssetPath, expected_paths_for};
pub(crate) use metadata::AssetGroupings;
pub(super) use metadata::MetadataPayload;
pub(super) use tasks::{DownloadTask, filter_asset_to_tasks};
pub(super) use versions::{VersionsView, extract_skip_candidates};
pub(crate) use versions::{metadata_capture, metadata_for_selected_version};
