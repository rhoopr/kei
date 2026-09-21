//! Download engine — streaming pipeline that starts downloading as soon as
//! the first API page returns, rather than enumerating the entire library
//! upfront. Uses a two-phase approach: (1) stream-and-download with bounded
//! concurrency, then (2) cleanup pass with fresh CDN URLs for any failures.

pub mod error;
pub mod file;
pub(crate) mod filter;
pub(crate) mod finalize;
#[cfg(feature = "xmp")]
pub(crate) mod heif;
pub(crate) mod limiter;
pub mod metadata;
pub(crate) mod metadata_rewrite;
pub mod paths;
pub(crate) mod pipeline;
pub(crate) mod planner;
pub(crate) mod recap;
mod retry;

pub(crate) use limiter::BandwidthLimiter;
pub(crate) use metadata_rewrite::CaptureTimestampRepair;

use filter::DownloadTask;

pub(crate) use filter::{AssetGroupings, determine_media_type};

mod orchestration;

pub(crate) use orchestration::config::{
    DOWNLOAD_CONFIG_HASH_KEY, DownloadConfig, compute_config_hash, hash_download_config,
    hash_legacy_download_config, sync_coverage_fingerprint_json,
};
use orchestration::context::{
    ClaimedLegacyMasterStates, DownloadContext, RecordedLocalFile, preload_download_context,
};
pub use orchestration::dispatch::download_photos_with_sync;
pub(crate) use orchestration::maintenance::drain_pending_metadata_rewrites;
use orchestration::models::PENDING_RETRY_UNMATCHED_REASON;
pub(super) use orchestration::models::PRODUCER_ENUMERATION_INCOMPLETE_REASON;
pub(crate) use orchestration::models::{
    DownloadControls, DownloadReporting, DownloadRunMode, DownloadStore, RecoveryAction,
    block_sync_token_for_unresolved_identity, sync_token_blocked_explanation,
    sync_token_blocked_source,
};
pub use orchestration::models::{
    DownloadOutcome, FullEnumerationReason, SkipBreakdown, SyncMode, SyncResult, SyncStats,
};
pub(crate) use orchestration::reconciliation::reconcile_catalog_paths;
use orchestration::selection::build_pass_configs_resolving_deferred_excludes;
use orchestration::url_refresh::{RetryTaskKey, UrlRetrySource, build_retry_download_tasks};

#[expect(
    unused_imports,
    reason = "retain the existing crate-level type paths in the facade"
)]
pub(crate) use orchestration::{models::PassKey, reconciliation::PathReconciliationResult};
