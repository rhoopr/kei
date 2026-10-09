//! Responsibility-specific owners behind the download facade.

pub(super) mod cleanup;
pub(super) mod config;
pub(super) mod context;
pub(super) mod delta;
pub(super) mod dispatch;
pub(super) mod full;
pub(super) mod generation;
pub(super) mod incremental;
pub(super) mod maintenance;
pub(super) mod models;
pub(super) mod primary_layout;
pub(super) mod queue_projection;
pub(super) mod recent;
pub(super) mod reconciliation;
pub(super) mod recovery;
pub(super) mod selection;
pub(super) mod url_refresh;

#[cfg(test)]
mod test_support;

#[cfg(test)]
mod fixture_tests;
