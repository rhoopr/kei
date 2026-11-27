//! Which versions of an asset kei saves and the paths the path rules give
//! them.
//!
//! Ownership:
//! - `config`: path and filter settings shared by sync and import.
//! - `eligibility`: content, date and filename filters and media classification.
//! - `versions`: RAW alignment and version and companion selection.
//! - `metadata`: album and people groupings for metadata writers.
//! - `expected_paths`: the paths a set of path rules gives an asset.
//!
//! Path rendering stays in `crate::download::paths`; filter policy stays here.

mod config;
mod eligibility;
mod expected_paths;
#[allow(
    dead_code,
    unused_imports,
    reason = "metadata writers wait for the opt-in metadata pass; it removes this allow"
)]
mod metadata;
mod versions;

pub(crate) use config::PathDerivationConfig;
pub(crate) use eligibility::is_asset_filtered;
pub(super) use expected_paths::derive_expected_paths;
