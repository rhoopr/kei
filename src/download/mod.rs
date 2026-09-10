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

use pipeline::{
    AUTH_ERROR_THRESHOLD, MetadataFlags, PassConfig, PassResult, StreamRuntime, StreamingResult,
    build_download_outcome, format_duration, log_sync_summary, run_download_pass,
    state_confirmed_current_path_exists, stream_and_download_from_stream,
};

pub(crate) use filter::AssetGroupings;
pub(crate) use filter::determine_media_type;
use filter::{DownloadTask, FilterReason, is_asset_filtered};
#[cfg(test)]
use retry::take_matching_pending_retry_tasks;
use retry::{PendingRetryPlan, PendingRetryTarget, build_pending_retry_download_tasks};

use std::borrow::Cow;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use reqwest::Client;
use rustc_hash::{FxHashMap, FxHashSet};

use futures_util::Stream;
use futures_util::stream::{self, StreamExt};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tokio_util::sync::CancellationToken;

use crate::icloud::photos::asset::{ChangeEvent, MALFORMED_REQUIRED_ASSET_FIELDS_REASON};
use crate::icloud::photos::{
    PhotoAsset, ProviderRecordId, RecordLookupRequest, RecordResolution, SyncTokenError,
};
use crate::retry::RetryConfig;
use crate::state::{
    DownloadContextStateStore, DownloadStateStore, MembershipStore, MetadataRewriteStore,
    ReportStateStore, SyncTokenStore, TempFileOwnershipStore, VersionSizeKey,
};
use crate::types::{
    AssetVersionSize, ChangeReason, FileMatchPolicy, LivePhotoMode, LivePhotoMovFilenamePolicy,
    RawPolicy,
};

/// Outcome of a download pass.
#[derive(Debug)]
pub enum DownloadOutcome {
    /// All downloads completed successfully.
    Success,
    /// Session expired mid-sync; caller should re-authenticate and retry.
    SessionExpired { auth_error_count: usize },
    /// Some downloads failed (not session-related).
    PartialFailure { failed_count: usize },
}

/// How the sync should enumerate photos from iCloud.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncMode {
    /// Full enumeration via records/query (existing behavior).
    /// On completion, captures the syncToken for future incremental syncs.
    Full,
    /// Incremental delta sync via changes/zone with a stored syncToken.
    /// Falls back to Full if the token is invalid/expired.
    Incremental {
        /// The stored syncToken for the zone being synced.
        zone_sync_token: String,
    },
}

pub(crate) trait DownloadStore:
    DownloadContextStateStore
    + DownloadStateStore
    + MembershipStore
    + MetadataRewriteStore
    + ReportStateStore
    + SyncTokenStore
    + TempFileOwnershipStore
{
}

impl<T> DownloadStore for T where
    T: DownloadContextStateStore
        + DownloadStateStore
        + MembershipStore
        + MetadataRewriteStore
        + ReportStateStore
        + SyncTokenStore
        + TempFileOwnershipStore
{
}

/// Bounded reason vocabulary for full enumeration runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FullEnumerationReason {
    NoStoredToken,
    MetadataBackfill,
    AlbumRelationHydrationIncomplete,
    EnumConfigHashDrift,
    DownloadConfigHashDrift,
    #[allow(
        dead_code,
        reason = "serialized public reason retained for report compatibility after retry work moved off full enumeration"
    )]
    ExplicitRetryFailed,
    OtherStaticReason,
}

impl FullEnumerationReason {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::NoStoredToken => "no_stored_token",
            Self::MetadataBackfill => "metadata_backfill",
            Self::AlbumRelationHydrationIncomplete => ALBUM_RELATION_HYDRATION_INCOMPLETE_REASON,
            Self::EnumConfigHashDrift => "enum_config_hash_drift",
            Self::DownloadConfigHashDrift => "download_config_hash_drift",
            Self::ExplicitRetryFailed => "explicit_retry_failed",
            Self::OtherStaticReason => "other_static_reason",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IncrementalErrorClass {
    TokenFallback,
    TransientFailure,
    StaticFallback,
}

/// Classify incremental-enumeration failures before deciding whether to fall
/// back to a full records/query pass.
///
/// Token errors that CloudKit explicitly marks unsafe fall back to full
/// enumeration. Auth and transport transients bubble up because a full pass
/// would likely hit the same service condition. Other static/decode errors
/// fall back so malformed token responses do not strand the user.
fn classify_incremental_error(error: &anyhow::Error) -> IncrementalErrorClass {
    if error
        .downcast_ref::<SyncTokenError>()
        .is_some_and(SyncTokenError::should_fallback_to_full)
    {
        return IncrementalErrorClass::TokenFallback;
    }
    if error
        .downcast_ref::<crate::auth::error::AuthError>()
        .is_some()
        || error
            .downcast_ref::<reqwest::Error>()
            .is_some_and(is_transient_reqwest_error)
    {
        return IncrementalErrorClass::TransientFailure;
    }
    IncrementalErrorClass::StaticFallback
}

fn is_transient_reqwest_error(error: &reqwest::Error) -> bool {
    error
        .status()
        .is_some_and(|status| status == 429 || status.as_u16() >= 500)
        || error.is_timeout()
        || error.is_connect()
}

/// One-shot runtime behavior for a sync pass.
///
/// Kept outside [`DownloadConfig`] so path/filter/download decisions do not
/// grow presentation-only flags.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DownloadRunMode {
    Download,
    DryRun,
    PrintFilenames,
}

impl DownloadRunMode {
    pub(crate) fn is_dry_run(self) -> bool {
        matches!(self, Self::DryRun)
    }

    pub(crate) fn only_print_filenames(self) -> bool {
        matches!(self, Self::PrintFilenames)
    }

    pub(crate) fn downloads_files(self) -> bool {
        matches!(self, Self::Download)
    }
}

/// Presentation knobs for the download pipeline.
///
/// The core config owns what to download. This owns how progress and friendly
/// retry narration are shown while that work runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DownloadReporting {
    pub(crate) no_progress_bar: bool,
    pub(crate) personality_mode: crate::personality::Mode,
}

impl DownloadReporting {
    pub(crate) const fn new(
        no_progress_bar: bool,
        personality_mode: crate::personality::Mode,
    ) -> Self {
        Self {
            no_progress_bar,
            personality_mode,
        }
    }

    #[cfg(test)]
    pub(crate) const fn hidden() -> Self {
        Self::new(true, crate::personality::Mode::Off)
    }
}

/// Per-run behavior that does not affect download path or filter decisions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DownloadControls {
    pub(crate) run_mode: DownloadRunMode,
    pub(crate) reporting: DownloadReporting,
}

impl DownloadControls {
    pub(crate) const fn new(run_mode: DownloadRunMode, reporting: DownloadReporting) -> Self {
        Self {
            run_mode,
            reporting,
        }
    }

    #[cfg(test)]
    pub(crate) const fn download_hidden() -> Self {
        Self::new(DownloadRunMode::Download, DownloadReporting::hidden())
    }

    #[cfg(test)]
    pub(crate) const fn dry_run_hidden() -> Self {
        Self::new(DownloadRunMode::DryRun, DownloadReporting::hidden())
    }
}

/// Result of a sync cycle, including the optional new syncToken.
#[derive(Debug)]
pub struct SyncResult {
    /// The outcome of the download pass (success, session expired, partial failure).
    pub outcome: DownloadOutcome,
    /// The new zone-level syncToken, if one was captured during this sync.
    /// Store this for the next incremental sync.
    pub sync_token: Option<String>,
    /// Accumulated statistics from this sync run.
    pub stats: SyncStats,
    /// Whether this result came from a full records/query enumeration.
    pub(crate) full_enumeration_ran: bool,
}

/// Accumulated statistics from a sync run, used for JSON reports and notifications.
#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct SyncStats {
    pub assets_seen: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_total_at_start: Option<u64>,
    #[serde(skip_serializing_if = "is_false")]
    pub api_total_at_start_partial: bool,
    pub downloaded: usize,
    pub failed: usize,
    pub skipped: SkipBreakdown,
    pub bytes_downloaded: u64,
    pub disk_bytes_written: u64,
    pub exif_failures: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata_capture_revision: Option<i64>,
    pub metadata_capture_refreshed: usize,
    pub metadata_capture_failures: usize,
    pub metadata_capture_remaining: u64,
    /// True when this cycle durably reduced metadata-capture work.
    #[serde(skip)]
    pub(crate) metadata_capture_progressed: bool,
    pub state_write_failures: usize,
    pub enumeration_errors: usize,
    /// Best-effort count-probe failures observed before full enumeration.
    /// These are reported separately from producer enumeration errors because
    /// a naturally drained CloudKit stream with usable sync tokens can still
    /// be complete even when the count side-channel was flaky.
    pub count_probe_failures: usize,
    /// Pending DB rows pruned after a clean full enumeration proved they were
    /// not re-seen. State-only cleanup; media files are never deleted.
    pub stale_pending_pruned: u64,
    /// Number of count-only CloudKit pagination shortfall warnings observed.
    /// These are not hard enumeration failures and do not imply download
    /// failures.
    pub pagination_shortfall_warnings: usize,
    /// Sum of missing assets reported by diagnostic pagination shortfalls.
    pub pagination_shortfall_assets: u64,
    /// Count-backed tail owners opened to prove natural EOF.
    pub tail_probes: usize,
    /// Assets discovered beyond the provider's pre-enumeration count hint.
    pub count_undercount_assets: u64,
    /// True when the asset producer stopped before naturally exhausting the
    /// iCloud stream for a reason other than an external interrupt.
    pub enumeration_incomplete: bool,
    /// Number of cross-cycle inventory-drop warnings observed.
    pub inventory_drop_warnings: usize,
    /// Largest cross-cycle API inventory drop observed.
    pub inventory_drop_assets: u64,
    /// Drop percentage for the largest cross-cycle inventory warning.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inventory_drop_percent: Option<f64>,
    /// Previous API total for the largest cross-cycle inventory warning.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inventory_drop_previous_total: Option<u64>,
    /// Current API total for the largest cross-cycle inventory warning.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inventory_drop_current_total: Option<u64>,
    /// Library where the largest cross-cycle inventory warning occurred.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inventory_drop_library: Option<String>,
    /// Whether sync-token advancement was blocked for safety despite no
    /// download failure.
    pub sync_token_blocked: bool,
    /// Structured reason for `sync_token_blocked`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sync_token_blocked_reason: Option<&'static str>,
    /// High-level owner attribution for `sync_token_blocked_reason`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sync_token_blocked_source: Option<&'static str>,
    /// Human-readable explanation for why token advancement was blocked.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sync_token_blocked_explanation: Option<&'static str>,
    /// Bounded reason explaining why this run used full enumeration instead
    /// of incremental sync.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub full_enumeration_reason: Option<FullEnumerationReason>,
    /// Zone name where token advancement was blocked. Set by the cycle owner
    /// so report.json can identify the affected library directly.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sync_token_blocked_zone: Option<String>,
    /// Number of token receivers expected from full-enumeration passes.
    /// Emitted whenever token receiver telemetry was collected, even if
    /// `sync_token_blocked` is false.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sync_token_expected_receivers: Option<usize>,
    /// Number of passes that produced a non-blank sync token.
    /// Emitted whenever token receiver telemetry was collected, even if
    /// `sync_token_blocked` is false.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sync_token_receivers_with_token: Option<usize>,
    /// Number of passes that completed but produced no sync token.
    /// Emitted whenever token receiver telemetry was collected, even if
    /// `sync_token_blocked` is false.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sync_token_receivers_missing: Option<usize>,
    /// Number of passes that produced a blank sync token.
    /// Emitted whenever token receiver telemetry was collected, even if
    /// `sync_token_blocked` is false.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sync_token_receivers_blank: Option<usize>,
    /// Number of sync token channels that dropped before reporting.
    /// Emitted whenever token receiver telemetry was collected, even if
    /// `sync_token_blocked` is false.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sync_token_receivers_dropped: Option<usize>,
    /// Number of unique non-blank sync token values observed.
    /// Emitted whenever token receiver telemetry was collected, even if
    /// `sync_token_blocked` is false.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sync_token_unique_values: Option<usize>,
    /// Bounded pass-token recovery rounds attempted in this cycle.
    pub same_cycle_recovery_attempts: usize,
    /// Pass-token recovery rounds that completed checkpoint proof.
    pub same_cycle_recovery_successes: usize,
    #[serde(skip)]
    pub(crate) checkpoint_retry_passes: Vec<PassKey>,
    #[serde(skip)]
    pub(crate) checkpoint_revalidate_records: Vec<ProviderRecordId>,
    pub elapsed_secs: f64,
    pub interrupted: bool,
    /// Number of tasks that observed at least one HTTP 429 / 503 response
    /// during retry. A high ratio of rate_limited / assets_seen signals the
    /// sync is running against a back-pressured account; operators should
    /// either raise `[watch] interval` or lower `[download] threads`.
    pub rate_limited: usize,
    /// Photos downloaded this run (`MediaType::Photo` and
    /// `MediaType::LivePhotoImage`). Sums to `downloaded` together with
    /// `videos_downloaded` for any pure-asset run; multi-version downloads
    /// (a Live Photo's image + MOV) count both sides.
    pub photos_downloaded: usize,
    /// Videos downloaded this run (`MediaType::Video` and
    /// `MediaType::LivePhotoVideo`).
    pub videos_downloaded: usize,
    /// Per-cycle highlights for the friendly recap (biggest / oldest /
    /// newest-album). Empty when no downloads succeeded; consumers must
    /// guard on `is_empty()` before rendering. Skipped for serialisation
    /// because it carries `chrono::DateTime<Local>` and the JSON report
    /// contract is owned by the existing scalar fields above.
    #[serde(skip)]
    pub recap: recap::RunRecap,
}

const RATE_LIMIT_PRESSURE_PERCENT: u64 = 10;
const RATE_LIMIT_PERCENT_SCALE: u64 = 100;

impl SyncStats {
    /// Whether observed HTTP 429/503 retries crossed the operator-warning threshold.
    #[must_use]
    pub(crate) fn has_rate_limit_pressure(&self) -> bool {
        if self.rate_limited == 0 {
            return false;
        }
        if self.assets_seen == 0 {
            return true;
        }
        let observations = u64::try_from(self.rate_limited).unwrap_or(u64::MAX);
        observations.saturating_mul(RATE_LIMIT_PERCENT_SCALE) / self.assets_seen
            >= RATE_LIMIT_PRESSURE_PERCENT
    }

    /// Add `other` into `self`, field by field. Used by the per-cycle loop in
    /// `sync_loop::run_cycle` to fold each library's stats into a cycle-wide
    /// total.
    ///
    /// All numeric counters sum; boolean cycle facts OR; `skipped` delegates
    /// to [`SkipBreakdown::accumulate`].
    ///
    /// Adding a new field to `SyncStats` requires updating this method too --
    /// otherwise the new counter silently zeros out across multi-library
    /// syncs.
    pub fn accumulate(&mut self, other: &SyncStats) {
        self.assets_seen += other.assets_seen;
        let had_api_total = self.api_total_at_start.is_some();
        let other_has_api_total = other.api_total_at_start.is_some();
        self.api_total_at_start = match (self.api_total_at_start, other.api_total_at_start) {
            (Some(a), Some(b)) => Some(a.saturating_add(b)),
            (Some(a), None) => Some(a),
            (None, Some(b)) => Some(b),
            (None, None) => None,
        };
        self.api_total_at_start_partial = self.api_total_at_start_partial
            || other.api_total_at_start_partial
            || (had_api_total != other_has_api_total && self.api_total_at_start.is_some());
        self.downloaded += other.downloaded;
        self.failed += other.failed;
        self.skipped.accumulate(&other.skipped);
        self.bytes_downloaded += other.bytes_downloaded;
        self.disk_bytes_written += other.disk_bytes_written;
        self.exif_failures += other.exif_failures;
        self.metadata_capture_revision = self
            .metadata_capture_revision
            .max(other.metadata_capture_revision);
        self.metadata_capture_refreshed += other.metadata_capture_refreshed;
        self.metadata_capture_failures += other.metadata_capture_failures;
        self.metadata_capture_remaining += other.metadata_capture_remaining;
        self.metadata_capture_progressed |= other.metadata_capture_progressed;
        self.state_write_failures += other.state_write_failures;
        self.enumeration_errors += other.enumeration_errors;
        self.count_probe_failures += other.count_probe_failures;
        self.stale_pending_pruned += other.stale_pending_pruned;
        self.pagination_shortfall_warnings += other.pagination_shortfall_warnings;
        self.pagination_shortfall_assets += other.pagination_shortfall_assets;
        self.tail_probes += other.tail_probes;
        self.count_undercount_assets += other.count_undercount_assets;
        self.enumeration_incomplete = self.enumeration_incomplete || other.enumeration_incomplete;
        self.inventory_drop_warnings += other.inventory_drop_warnings;
        if other.inventory_drop_assets > self.inventory_drop_assets {
            self.inventory_drop_assets = other.inventory_drop_assets;
            self.inventory_drop_percent = other.inventory_drop_percent;
            self.inventory_drop_previous_total = other.inventory_drop_previous_total;
            self.inventory_drop_current_total = other.inventory_drop_current_total;
            self.inventory_drop_library = other.inventory_drop_library.clone();
        }
        self.sync_token_blocked = self.sync_token_blocked || other.sync_token_blocked;
        if self.sync_token_blocked_reason.is_none() {
            self.sync_token_blocked_reason = other.sync_token_blocked_reason;
        }
        if self.sync_token_blocked_source.is_none() {
            self.sync_token_blocked_source = other.sync_token_blocked_source;
        }
        if self.sync_token_blocked_explanation.is_none() {
            self.sync_token_blocked_explanation = other.sync_token_blocked_explanation;
        }
        if self.full_enumeration_reason.is_none() {
            self.full_enumeration_reason = other.full_enumeration_reason;
        }
        if self.sync_token_blocked_zone.is_none() {
            self.sync_token_blocked_zone = other.sync_token_blocked_zone.clone();
        }
        if self.sync_token_expected_receivers.is_none() {
            self.sync_token_expected_receivers = other.sync_token_expected_receivers;
        }
        if self.sync_token_receivers_with_token.is_none() {
            self.sync_token_receivers_with_token = other.sync_token_receivers_with_token;
        }
        if self.sync_token_receivers_missing.is_none() {
            self.sync_token_receivers_missing = other.sync_token_receivers_missing;
        }
        if self.sync_token_receivers_blank.is_none() {
            self.sync_token_receivers_blank = other.sync_token_receivers_blank;
        }
        if self.sync_token_receivers_dropped.is_none() {
            self.sync_token_receivers_dropped = other.sync_token_receivers_dropped;
        }
        if self.sync_token_unique_values.is_none() {
            self.sync_token_unique_values = other.sync_token_unique_values;
        }
        self.same_cycle_recovery_attempts += other.same_cycle_recovery_attempts;
        self.same_cycle_recovery_successes += other.same_cycle_recovery_successes;
        self.checkpoint_retry_passes
            .extend(other.checkpoint_retry_passes.iter().cloned());
        self.checkpoint_revalidate_records
            .extend(other.checkpoint_revalidate_records.iter().cloned());
        self.elapsed_secs += other.elapsed_secs;
        self.interrupted = self.interrupted || other.interrupted;
        self.rate_limited += other.rate_limited;
        self.photos_downloaded += other.photos_downloaded;
        self.videos_downloaded += other.videos_downloaded;
        self.recap.merge(other.recap.clone());
    }
}

const fn is_false(value: &bool) -> bool {
    !*value
}

const ALBUM_RELATION_HYDRATION_INCOMPLETE_REASON: &str = "album_relation_hydration_incomplete";
const DATE_BOUNDED_FULL_ENUMERATION_REASON: &str = "date_bounded_full_enumeration";
const RECENT_LIMITED_FULL_ENUMERATION_REASON: &str = "recent_limited_full_enumeration";
const UNPARSABLE_RELATION_DELTA_REASON: &str = "unparsable_relation_delta";
const UNKNOWN_ALBUM_RELATION_CONTAINER_REASON: &str = "unknown_album_relation_container";
const UNKNOWN_ALBUM_RELATION_ASSET_REASON: &str = "unknown_album_relation_asset";
const ALBUM_DELTA_STATE_WRITE_FAILED_REASON: &str = "album_delta_state_write_failed";
const ASSET_MASTER_MAPPING_STATE_WRITE_FAILED_REASON: &str =
    "asset_master_mapping_state_write_failed";
const ASSET_DELTA_HYDRATION_INCOMPLETE_REASON: &str = "asset_delta_hydration_incomplete";
const PROVIDER_METADATA_STATE_WRITE_FAILED_REASON: &str = "provider_metadata_state_write_failed";
const METADATA_CAPTURE_REPAIR_FAILED_REASON: &str = "metadata_capture_repair_failed";
const INCREMENTAL_DELETE_STATE_WRITE_FAILED_REASON: &str = "incremental_delete_state_write_failed";
const INCREMENTAL_DELETE_ZERO_ROWS_REASON: &str = "incremental_delete_no_matching_state";
const INCREMENTAL_HIDDEN_STATE_WRITE_FAILED_REASON: &str = "incremental_hidden_state_write_failed";
const INCREMENTAL_HIDDEN_ZERO_ROWS_REASON: &str = "incremental_hidden_no_matching_state";
const SMART_FOLDER_REFRESH_FAILED_REASON: &str = "smart_folder_refresh_failed";
const TARGETED_ALBUM_BACKFILL_FAILED_REASON: &str = "targeted_album_backfill_failed";
const PENDING_RETRY_UNMATCHED_REASON: &str = "pending_retry_unmatched";
const PAGINATION_SHORTFALL_REASON: &str = "pagination_shortfall";
const ICLOUD_ALBUM_COUNT_ERROR_REASON: &str = "icloud_album_count_error";
const INCREMENTAL_PREFLIGHT_URL_REFRESH_AFTER: Duration = Duration::from_secs(5 * 60);
const MAX_SAME_CYCLE_CHECKPOINT_RECOVERY_ROUNDS: usize = 1;
pub(super) const PRODUCER_ENUMERATION_INCOMPLETE_REASON: &str = "producer_enumeration_incomplete";

pub(crate) fn sync_token_blocked_source(reason: &str) -> &'static str {
    match reason {
        ALBUM_RELATION_HYDRATION_INCOMPLETE_REASON
        | ALBUM_DELTA_STATE_WRITE_FAILED_REASON
        | ASSET_DELTA_HYDRATION_INCOMPLETE_REASON
        | ASSET_MASTER_MAPPING_STATE_WRITE_FAILED_REASON
        | PROVIDER_METADATA_STATE_WRITE_FAILED_REASON
        | METADATA_CAPTURE_REPAIR_FAILED_REASON
        | DATE_BOUNDED_FULL_ENUMERATION_REASON
        | INCREMENTAL_DELETE_STATE_WRITE_FAILED_REASON
        | INCREMENTAL_HIDDEN_STATE_WRITE_FAILED_REASON
        | "kei_internal_token_receiver_dropped"
        | PRODUCER_ENUMERATION_INCOMPLETE_REASON
        | RECENT_LIMITED_FULL_ENUMERATION_REASON
        | SMART_FOLDER_REFRESH_FAILED_REASON
        | TARGETED_ALBUM_BACKFILL_FAILED_REASON
        | PENDING_RETRY_UNMATCHED_REASON => "kei",
        INCREMENTAL_DELETE_ZERO_ROWS_REASON
        | INCREMENTAL_HIDDEN_ZERO_ROWS_REASON
        | ICLOUD_ALBUM_COUNT_ERROR_REASON
        | PAGINATION_SHORTFALL_REASON
        | "icloud_blank_sync_token"
        | "icloud_sync_token_mismatch"
        | "icloud_sync_token_missing"
        | MALFORMED_REQUIRED_ASSET_FIELDS_REASON
        | UNPARSABLE_RELATION_DELTA_REASON
        | UNKNOWN_ALBUM_RELATION_ASSET_REASON
        | UNKNOWN_ALBUM_RELATION_CONTAINER_REASON => "icloud",
        _ => "unknown",
    }
}

pub(crate) fn sync_token_blocked_explanation(reason: &str) -> &'static str {
    match reason {
        PAGINATION_SHORTFALL_REASON => {
            "enumeration counts did not line up safely, so kei blocked token advancement"
        }
        ICLOUD_ALBUM_COUNT_ERROR_REASON => {
            "iCloud returned a missing or malformed album count response"
        }
        "icloud_sync_token_missing" => {
            "iCloud did not return a sync token for this full enumeration"
        }
        "icloud_blank_sync_token" => {
            "iCloud returned a blank sync token, which kei treated as unusable"
        }
        "icloud_sync_token_mismatch" => "iCloud returned conflicting sync tokens across passes",
        MALFORMED_REQUIRED_ASSET_FIELDS_REASON => {
            "iCloud returned a photo record without a usable required identity or capture date"
        }
        "kei_internal_token_receiver_dropped" => {
            "an internal token collection channel closed before completion"
        }
        PRODUCER_ENUMERATION_INCOMPLETE_REASON => {
            "kei stopped before iCloud enumeration reached the natural end of the stream"
        }
        RECENT_LIMITED_FULL_ENUMERATION_REASON => {
            "a count-limited recent sync is a bounded enumeration, so kei intentionally did not persist a full-enumeration sync token"
        }
        DATE_BOUNDED_FULL_ENUMERATION_REASON => {
            "a lower-date-bounded sync is a bounded enumeration, so kei intentionally did not persist a full-enumeration sync token"
        }
        ALBUM_RELATION_HYDRATION_INCOMPLETE_REASON => {
            "album membership state is not complete enough for incremental routing yet"
        }
        ASSET_DELTA_HYDRATION_INCOMPLETE_REASON => {
            "kei could not obtain a complete photo from an asset-only iCloud delta"
        }
        UNPARSABLE_RELATION_DELTA_REASON => {
            "iCloud returned an album relation delta kei could not parse safely"
        }
        UNKNOWN_ALBUM_RELATION_CONTAINER_REASON => {
            "an album relation referenced a container kei has not mapped yet"
        }
        UNKNOWN_ALBUM_RELATION_ASSET_REASON => {
            "an album relation referenced an asset kei cannot hydrate for album routing yet"
        }
        ALBUM_DELTA_STATE_WRITE_FAILED_REASON => "kei could not persist album delta state safely",
        ASSET_MASTER_MAPPING_STATE_WRITE_FAILED_REASON => {
            "kei could not persist asset-to-master mapping state safely"
        }
        PROVIDER_METADATA_STATE_WRITE_FAILED_REASON => {
            "kei could not persist changed provider metadata safely"
        }
        METADATA_CAPTURE_REPAIR_FAILED_REASON => {
            "kei could not complete automatic metadata-capture repair safely"
        }
        INCREMENTAL_DELETE_STATE_WRITE_FAILED_REASON => {
            "kei could not persist an incremental source-delete safely"
        }
        INCREMENTAL_DELETE_ZERO_ROWS_REASON => {
            "an incremental source-delete did not match local state, so kei blocked token advancement"
        }
        INCREMENTAL_HIDDEN_STATE_WRITE_FAILED_REASON => {
            "kei could not persist an incremental hidden-state change safely"
        }
        INCREMENTAL_HIDDEN_ZERO_ROWS_REASON => {
            "an incremental hidden-state change did not match local state, so kei blocked token advancement"
        }
        SMART_FOLDER_REFRESH_FAILED_REASON => {
            "a selected smart-folder refresh did not complete safely"
        }
        TARGETED_ALBUM_BACKFILL_FAILED_REASON => {
            "a targeted album backfill did not complete safely"
        }
        PENDING_RETRY_UNMATCHED_REASON => {
            "kei could not refresh every pending retry target; the rows remain durable for a later cycle"
        }
        "sync_token_unavailable" | "sync_token_missing" => {
            "no usable sync token was available at the end of the cycle"
        }
        _ => "the sync token was unavailable for an unspecified reason",
    }
}

/// Per-reason breakdown of skipped assets.
#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct SkipBreakdown {
    pub by_state: usize,
    pub on_disk: usize,
    pub by_media_type: usize,
    pub by_date_range: usize,
    pub by_live_photo: usize,
    pub by_filename: usize,
    pub by_excluded_album: usize,
    pub ampm_variant: usize,
    pub duplicates: usize,
    pub retry_exhausted: usize,
    pub retry_only: usize,
}

impl SkipBreakdown {
    pub fn total(&self) -> usize {
        self.by_state
            + self.on_disk
            + self.by_media_type
            + self.by_date_range
            + self.by_live_photo
            + self.by_filename
            + self.by_excluded_album
            + self.ampm_variant
            + self.duplicates
            + self.retry_exhausted
            + self.retry_only
    }

    /// Add `other` into `self` field-by-field. Mirrors
    /// [`SyncStats::accumulate`] for the nested skip breakdown.
    pub fn accumulate(&mut self, other: &SkipBreakdown) {
        self.by_state += other.by_state;
        self.on_disk += other.on_disk;
        self.by_media_type += other.by_media_type;
        self.by_date_range += other.by_date_range;
        self.by_live_photo += other.by_live_photo;
        self.by_filename += other.by_filename;
        self.by_excluded_album += other.by_excluded_album;
        self.ampm_variant += other.ampm_variant;
        self.duplicates += other.duplicates;
        self.retry_exhausted += other.retry_exhausted;
        self.retry_only += other.retry_only;
    }

    pub(crate) fn record_filter_reason(&mut self, reason: filter::FilterReason) {
        match reason {
            filter::FilterReason::MalformedAsset => self.by_filename += 1,
            filter::FilterReason::ExcludedAlbum => self.by_excluded_album += 1,
            filter::FilterReason::MediaType => self.by_media_type += 1,
            filter::FilterReason::LivePhoto => self.by_live_photo += 1,
            filter::FilterReason::DateRange => self.by_date_range += 1,
            filter::FilterReason::Filename => self.by_filename += 1,
        }
    }
}

/// Hash an `Option<NaiveDate>` with a tag byte for `None`/`Some` and the
/// "YYYY-MM-DD" Display representation for the date value.
fn hash_optional_date(hasher: &mut sha2::Sha256, date: Option<chrono::NaiveDate>) {
    use sha2::Digest;
    match date {
        None => hasher.update([0]),
        Some(d) => {
            hasher.update([1]);
            hasher.update(d.to_string().as_bytes());
        }
    }
}

/// Hash a byte slice with a trailing NUL separator. Pairs naturally with
/// other variable-length fields without ambiguity: `"a"` + `""` hashes
/// distinctly from `""` + `"a"`.
fn hash_bytes(hasher: &mut sha2::Sha256, bytes: &[u8]) {
    use sha2::Digest;
    hasher.update(bytes);
    hasher.update(b"\0");
}

/// Hash an `Option<u32>` with a tag byte for `None`/`Some` and the
/// little-endian bytes of the inner value.
fn hash_optional_u32(hasher: &mut sha2::Sha256, val: Option<u32>) {
    use sha2::Digest;
    match val {
        None => hasher.update([0]),
        Some(n) => {
            hasher.update([1]);
            hasher.update(n.to_le_bytes());
        }
    }
}

/// Finalize a SHA-256 hasher into a 16-char hex string (first 8 bytes).
fn finalize_hash(hasher: sha2::Sha256) -> String {
    use sha2::Digest;
    use std::fmt::Write;

    let hash = hasher.finalize();
    let mut hex = String::with_capacity(16);
    // First 8 bytes is plenty for collision avoidance in this context.
    #[allow(
        clippy::indexing_slicing,
        reason = "SHA-256 output is always 32 bytes; 8 is unconditionally in-bounds"
    )]
    for &b in &hash[..8] {
        let _ = Write::write_fmt(&mut hex, format_args!("{b:02x}"));
    }
    hex
}

/// Bump this when path derivation changes without a corresponding config
/// field changing. That forces existing state to revalidate on disk instead
/// of trusting paths derived under older code.
const LEGACY_PATH_DERIVATION_HASH_VERSION: u8 = 2;
const PATH_DERIVATION_HASH_VERSION: u8 = 3;
const ENUMERATION_SAFETY_HASH_VERSION: u8 = 2;
pub(crate) const DOWNLOAD_CONFIG_HASH_KEY: &str = "config_hash";

/// Fields shared between [`hash_download_config`] and [`compute_config_hash`]
/// that affect path resolution and asset eligibility.
#[derive(Debug)]
struct SharedHashFields<'a> {
    directory: &'a std::path::Path,
    folder_structure: &'a str,
    folder_structure_albums: &'a str,
    folder_structure_smart_folders: &'a str,
    resolution: crate::types::PhotoResolution,
    live_resolution: AssetVersionSize,
    file_match_policy: FileMatchPolicy,
    live_photo_mov_filename_policy: LivePhotoMovFilenamePolicy,
    edited: bool,
    alternative: bool,
    raw_policy: RawPolicy,
    keep_unicode_in_filenames: bool,
    skip_created_before: Option<crate::config::CreatedDateFilter>,
    skip_created_after: Option<crate::config::CreatedDateFilter>,
    force_resolution: bool,
    media: crate::config::MediaSelection,
    live_photo_mode: LivePhotoMode,
    filename_exclude: &'a [glob::Pattern],
}

/// Hash the shared config fields into the hasher. All enum values use
/// `repr(u8)` byte representations and dates use "YYYY-MM-DD" Display
/// format for stability across compiler/library upgrades.
fn hash_shared_fields(hasher: &mut sha2::Sha256, version: u8, f: &SharedHashFields<'_>) {
    use sha2::Digest;

    hasher.update([version]);
    hash_bytes(hasher, f.directory.as_os_str().as_encoded_bytes());
    hash_bytes(hasher, f.folder_structure.as_bytes());
    hash_bytes(hasher, f.folder_structure_albums.as_bytes());
    hash_bytes(hasher, f.folder_structure_smart_folders.as_bytes());
    hasher.update([f.resolution as u8]);
    hasher.update([f.live_resolution as u8]);
    hasher.update([f.file_match_policy as u8]);
    hasher.update([f.live_photo_mov_filename_policy as u8]);
    hasher.update([u8::from(f.edited)]);
    hasher.update([u8::from(f.alternative)]);
    hasher.update([f.raw_policy as u8]);
    hasher.update([u8::from(f.keep_unicode_in_filenames)]);
    // Eligibility fields stay in the local trust hash where changing them can
    // alter the selected versions. Current path hashing passes no date bounds
    // because the enumeration hash already owns date eligibility; the legacy
    // caller supplies them only to reproduce the v2 shape.
    //
    // Dates are truncated to day precision before hashing so that relative
    // intervals like "20d" (resolved to now-minus-20-days at parse time)
    // produce a stable hash across consecutive runs on the same day.
    hash_optional_created_date_filter(hasher, f.skip_created_before);
    hash_optional_created_date_filter(hasher, f.skip_created_after);
    hasher.update([u8::from(f.force_resolution)]);
    hasher.update([u8::from(f.media.photos)]);
    hasher.update([u8::from(f.media.videos)]);
    hasher.update([u8::from(f.media.live_photos)]);
    hasher.update([f.live_photo_mode as u8]);
    // filename_exclude patterns affect which assets are eligible
    let mut sorted_excludes: Vec<&str> = f
        .filename_exclude
        .iter()
        .map(glob::Pattern::as_str)
        .collect();
    sorted_excludes.sort_unstable();
    for pattern in &sorted_excludes {
        hash_bytes(hasher, pattern.as_bytes());
    }
}

fn hash_optional_created_date_filter(
    hasher: &mut sha2::Sha256,
    filter: Option<crate::config::CreatedDateFilter>,
) {
    use crate::config::CreatedDateFilter;
    use sha2::Digest;

    match filter {
        None => hash_optional_date(hasher, None),
        Some(CreatedDateFilter::Instant(boundary)) => {
            // Preserve the pre-capture-date hash shape for instant cutoffs.
            hash_optional_date(hasher, Some(boundary.date_naive()));
        }
        Some(CreatedDateFilter::CaptureDate(boundary)) => {
            // A distinct tag invalidates only filters whose semantics changed.
            hasher.update([2]);
            hasher.update(boundary.to_string().as_bytes());
        }
    }
}

fn selector_set_fingerprint_json(set: &BTreeSet<String>) -> serde_json::Value {
    let values: Vec<&str> = set.iter().map(String::as_str).collect();
    serde_json::json!(values)
}

fn album_selector_fingerprint_json(
    selector: &crate::selection::AlbumSelector,
) -> serde_json::Value {
    use crate::selection::AlbumSelector;
    match selector {
        AlbumSelector::None => serde_json::json!({"kind": "none"}),
        AlbumSelector::All { excluded } => {
            serde_json::json!({"kind": "all", "excluded": selector_set_fingerprint_json(excluded)})
        }
        AlbumSelector::Named { included, excluded } => serde_json::json!({
            "kind": "named",
            "included": selector_set_fingerprint_json(included),
            "excluded": selector_set_fingerprint_json(excluded),
        }),
    }
}

fn smart_folder_selector_fingerprint_json(
    selector: &crate::selection::SmartFolderSelector,
) -> serde_json::Value {
    use crate::selection::SmartFolderSelector;
    match selector {
        SmartFolderSelector::None => serde_json::json!({"kind": "none"}),
        SmartFolderSelector::All {
            include_sensitive,
            excluded,
        } => serde_json::json!({
            "kind": "all",
            "include_sensitive": include_sensitive,
            "excluded": selector_set_fingerprint_json(excluded),
        }),
        SmartFolderSelector::Named { included, excluded } => serde_json::json!({
            "kind": "named",
            "included": selector_set_fingerprint_json(included),
            "excluded": selector_set_fingerprint_json(excluded),
        }),
    }
}

fn library_selector_fingerprint_json(
    selector: &crate::selection::LibrarySelector,
) -> serde_json::Value {
    serde_json::json!({
        "primary": selector.primary,
        "shared_all": selector.shared_all,
        "named": selector_set_fingerprint_json(&selector.named),
        "excluded": selector_set_fingerprint_json(&selector.excluded),
    })
}

/// Build the canonical coverage fingerprint stored with scoped
/// `/changes/database` precheck tokens.
///
/// Keep this next to the download and enumeration hash owners because it is
/// the durable audit shape that combines selection, filter coverage, enum
/// safety, and path/download eligibility proof.
pub(crate) fn sync_coverage_fingerprint_json(
    config: &crate::config::Config,
    provider: &str,
    shape_version: i64,
    selected_zones: &[String],
    enum_config_hash: &str,
    download_config_hash: &str,
) -> anyhow::Result<String> {
    let skip_created_before = config
        .filters
        .skip_created_before
        .map(crate::config::CreatedDateFilter::fingerprint);
    let skip_created_after = config
        .filters
        .skip_created_after
        .map(crate::config::CreatedDateFilter::fingerprint);
    let mut filename_exclude: Vec<&str> = config
        .download
        .filename_exclude
        .iter()
        .map(glob::Pattern::as_str)
        .collect();
    filename_exclude.sort_unstable();
    let coverage = if let Some(count) = config.filters.recent {
        serde_json::json!({
            "kind": "bounded-recent-count",
            "count": count,
            "recent_scope": config.filters.recent_scope,
        })
    } else if skip_created_before.is_some() || skip_created_after.is_some() {
        serde_json::json!({
            "kind": "bounded-date-window",
            "skip_created_before": skip_created_before,
            "skip_created_after": skip_created_after,
        })
    } else {
        serde_json::json!({"kind": "complete"})
    };

    serde_json::to_string(&serde_json::json!({
        "provider": provider,
        "domain": config.auth.domain.as_str(),
        "shape_version": shape_version,
        "selected_zones": selected_zones,
        "selection": {
            "albums": album_selector_fingerprint_json(&config.filters.selection.albums),
            "albums_explicit": config.filters.selection.albums_explicit,
            "smart_folders": smart_folder_selector_fingerprint_json(&config.filters.selection.smart_folders),
            "smart_folders_explicit": config.filters.selection.smart_folders_explicit,
            "libraries": library_selector_fingerprint_json(&config.filters.selection.libraries),
            "unfiled": config.filters.selection.unfiled,
        },
        "filters": {
            "media": {
                "photos": config.filters.media.photos,
                "videos": config.filters.media.videos,
                "live_photos": config.filters.media.live_photos,
            },
            "filename_exclude": filename_exclude,
            "skip_created_before": skip_created_before,
            "skip_created_after": skip_created_after,
            "recent": config.filters.recent,
            "recent_scope": config.filters.recent_scope,
        },
        "coverage": coverage,
        "enum_config_hash": enum_config_hash,
        "download_config_hash": download_config_hash,
    }))
    .context("serialize sync coverage fingerprint")
}

/// Compute a deterministic hash of the config fields that affect path resolution.
///
/// When this hash changes between runs, we can't trust the state DB's download
/// records (the resolved paths may differ), so we fall back to the full pipeline
/// with filesystem existence checks.
///
/// Separate from [`compute_config_hash`]: path-only changes revalidate local
/// download state without clearing CloudKit zone tokens.
pub(crate) fn hash_download_config(config: &DownloadConfig) -> String {
    hash_download_config_with_date_bounds(config, PATH_DERIVATION_HASH_VERSION, false)
}

/// Reproduce the v2 mixed path-and-date hash for in-place migration.
pub(crate) fn hash_legacy_download_config(config: &DownloadConfig) -> String {
    hash_download_config_with_date_bounds(config, LEGACY_PATH_DERIVATION_HASH_VERSION, true)
}

fn hash_download_config_with_date_bounds(
    config: &DownloadConfig,
    version: u8,
    include_date_bounds: bool,
) -> String {
    use sha2::{Digest, Sha256};

    let (skip_created_before, skip_created_after) = if include_date_bounds {
        (config.skip_created_before, config.skip_created_after)
    } else {
        (None, None)
    };
    let mut hasher = Sha256::new();
    hash_shared_fields(
        &mut hasher,
        version,
        &SharedHashFields {
            directory: &config.directory,
            folder_structure: &config.folder_structure,
            folder_structure_albums: &config.folder_structure_albums,
            folder_structure_smart_folders: &config.folder_structure_smart_folders,
            resolution: config.resolution,
            live_resolution: config.live_resolution,
            file_match_policy: config.file_match_policy,
            live_photo_mov_filename_policy: config.live_photo_mov_filename_policy,
            edited: config.edited,
            alternative: config.alternative,
            raw_policy: config.raw_policy,
            keep_unicode_in_filenames: config.keep_unicode_in_filenames,
            skip_created_before,
            skip_created_after,
            force_resolution: config.force_resolution,
            media: config.media,
            live_photo_mode: config.live_photo_mode,
            filename_exclude: &config.filename_exclude,
        },
    );
    // `recent` affects which already-downloaded assets to trust/skip
    hash_optional_u32(&mut hasher, config.recent);
    if config.recent.is_some() {
        hasher.update(b"recent_scope:");
        hasher.update(match config.recent_scope {
            crate::cli::RecentScope::Global => b"global".as_slice(),
            crate::cli::RecentScope::PerFilter => b"per-filter".as_slice(),
        });
        hasher.update(b"\0");
    }
    finalize_hash(hasher)
}

/// Compute the config hash from the app-level `Config`.
///
/// Called before the sync-mode decision so stale sync tokens are cleared only
/// when an unsafe eligibility/config change cannot be routed incrementally.
///
/// This hash tracks only changes that make a stored CloudKit zone token unsafe.
/// Path-only fields stay in [`hash_download_config`] so a folder/template
/// change revalidates local files without discarding the CloudKit cursor.
/// Album, library, and smart-folder selection are also excluded here: the
/// incremental router can prove those cases from per-library tokens, trusted
/// album snapshots, and targeted smart-folder refreshes.
pub(crate) fn compute_config_hash(config: &crate::config::Config) -> String {
    use sha2::{Digest, Sha256};

    let live_resolution = config.photos.live_resolution.to_asset_version_size();
    let mut hasher = Sha256::new();
    hasher.update([ENUMERATION_SAFETY_HASH_VERSION]);
    hasher.update([config.photos.resolution as u8]);
    hasher.update([live_resolution as u8]);
    hasher.update([u8::from(config.photos.edited)]);
    hasher.update([u8::from(config.photos.alternative)]);
    hasher.update([config.photos.raw_policy as u8]);
    hash_optional_created_date_filter(&mut hasher, config.filters.skip_created_before);
    hash_optional_created_date_filter(&mut hasher, config.filters.skip_created_after);
    hasher.update([u8::from(config.photos.force_resolution)]);
    hasher.update([u8::from(config.filters.media.photos)]);
    hasher.update([u8::from(config.filters.media.videos)]);
    hasher.update([u8::from(config.filters.media.live_photos)]);
    hasher.update([config.photos.live_photo_mode as u8]);
    let mut sorted_excludes: Vec<&str> = config
        .download
        .filename_exclude
        .iter()
        .map(glob::Pattern::as_str)
        .collect();
    sorted_excludes.sort_unstable();
    for pattern in &sorted_excludes {
        hash_bytes(&mut hasher, pattern.as_bytes());
    }
    // Note: `recent` is intentionally excluded from this enum hash.
    // Changing --recent should not invalidate sync tokens because the
    // incremental path already applies the recent cap post-fetch.
    // `recent` IS included in hash_download_config (trust-state) so
    // changing it still triggers filesystem re-verification.

    // The unfiled selector is still unsafe to classify from the current state
    // alone: switching it on can make old, never-enumerated unfiled assets
    // newly eligible. Keep the full fallback for that unknown drift class.
    hasher.update(b"unfiled:");
    hasher.update([u8::from(config.filters.selection.unfiled)]);
    finalize_hash(hasher)
}

/// Subset of application config consumed by the download engine.
/// Decoupled from CLI parsing so the engine can be tested independently.
#[derive(Clone)]
pub(crate) struct DownloadConfig {
    /// Behind `Arc` so per-pass clones (`with_album_name`, `with_pass`,
    /// `with_exclude_ids`) refcount-bump instead of deep-cloning the
    /// PathBuf. Same pattern as `asset_groupings` and `exclude_asset_ids`.
    pub(crate) directory: Arc<Path>,
    /// Template for the unfiled (library-wide) pass. Also the source the
    /// per-pass clone in `with_pass` reads when the pass is `Unfiled`. After
    /// `with_pass` runs, this field holds the *expanded* per-pass template.
    pub(crate) folder_structure: String,
    /// Template for `PassKind::Album` passes (default `{album}`). Behind
    /// `Arc<str>` so per-pass clones refcount-bump instead of deep-cloning;
    /// the user-typed template never mutates after CLI parse.
    pub(crate) folder_structure_albums: Arc<str>,
    /// Template for `PassKind::SmartFolder` passes (default `{smart-folder}`).
    /// Behind `Arc<str>` for the same reason as `folder_structure_albums`.
    pub(crate) folder_structure_smart_folders: Arc<str>,
    pub(crate) resolution: crate::types::PhotoResolution,
    pub(crate) media: crate::config::MediaSelection,
    pub(crate) skip_created_before: Option<crate::config::CreatedDateFilter>,
    pub(crate) skip_created_after: Option<crate::config::CreatedDateFilter>,
    pub(crate) metadata: crate::config::MetadataConfig,
    pub(crate) refresh_metadata: bool,
    pub(crate) capture_timestamp_repair: CaptureTimestampRepair,
    pub(crate) repair_truncated: bool,
    pub(crate) concurrent_downloads: usize,
    pub(crate) recent: Option<u32>,
    pub(crate) recent_scope: crate::cli::RecentScope,
    pub(crate) retry: RetryConfig,
    pub(crate) live_photo_mode: LivePhotoMode,
    pub(crate) live_resolution: AssetVersionSize,
    pub(crate) live_photo_mov_filename_policy: LivePhotoMovFilenamePolicy,
    pub(crate) edited: bool,
    pub(crate) alternative: bool,
    pub(crate) raw_policy: RawPolicy,
    pub(crate) file_match_policy: FileMatchPolicy,
    pub(crate) force_resolution: bool,
    pub(crate) keep_unicode_in_filenames: bool,
    /// Compiled glob patterns for filename exclusion.
    ///
    /// Behind `Arc<[_]>` so per-pass clones share one allocation
    /// (significant with `-a all` over 100+ albums).
    pub(crate) filename_exclude: Arc<[glob::Pattern]>,
    /// Temp file suffix for partial downloads (e.g. `.kei-tmp`).
    pub(crate) temp_suffix: Arc<str>,
    /// State database for tracking download progress.
    pub(crate) state_db: Option<Arc<dyn DownloadStore>>,
    /// When true (retry-failed mode), only download assets already known to the
    /// state DB. Skip new assets discovered from iCloud that were never synced.
    pub(crate) retry_only: bool,
    /// Sync mode: full enumeration or incremental delta via syncToken.
    pub(crate) sync_mode: SyncMode,
    /// Hash of enumeration-affecting config. Full album snapshots persist this
    /// so later routing can prove a trusted snapshot still matches the plan.
    pub(crate) enum_config_hash: Option<Arc<str>>,
    /// Album name for `{album}` token in folder_structure. Set per-album when
    /// processing albums individually.
    pub(crate) album_name: Option<Arc<str>>,
    /// CloudKit zone name (e.g. "PrimarySync", "SharedSync-A1B2C3D4-...")
    /// scoping every asset processed under this config. Threaded into
    /// `AssetRecord.library` and every state-DB key so multi-library syncs
    /// don't collide on the (id, version_size) pair across zones.
    pub(crate) library: Arc<str>,
    /// Asset IDs to exclude (from `--exclude-album` without `--album`).
    pub(crate) exclude_asset_ids: Arc<FxHashSet<String>>,
    /// Maximum download attempts per asset before giving up (0 = unlimited).
    pub(crate) max_download_attempts: u32,
    /// Preloaded asset→album and asset→person indices, shared across clones.
    pub(crate) asset_groupings: Arc<AssetGroupings>,
    /// Shared token-bucket limiter applied across all concurrent download
    /// streams. `None` = no throughput cap.
    pub(crate) bandwidth_limiter: Option<BandwidthLimiter>,
}

impl DownloadConfig {
    /// Human-readable label for the active pass: the album's own name for
    /// album/smart-folder passes, "unfiled" for the unfiled pass (which uses
    /// `library.all()` whose `.name` is the empty string).
    pub(crate) fn pass_label(&self) -> &str {
        match self.album_name.as_deref() {
            Some("") | None => "unfiled",
            Some(name) => name,
        }
    }

    /// True when passes can produce divergent paths and need per-pass config
    /// expansion (`with_pass`) plus path-aware skip checks rather than the
    /// merged-stream optimisation + DB-only fast skip.
    ///
    /// Divergence sources: any of the three template fields
    /// (`folder_structure`, `folder_structure_albums`,
    /// `folder_structure_smart_folders`) carries a per-pass token
    /// (`{album}` / `{smart-folder}` / `{library}`), or the per-category
    /// templates differ from the base. Both cases mean a single merged
    /// stream + base config would route assets to the wrong on-disk path.
    ///
    /// Only meaningful on the *base* config. A per-pass config produced by
    /// `with_album_name` / `with_pass` has had per-pass tokens expanded out
    /// of `folder_structure`, but the per-category fields stay cloned from
    /// the base so this still reports the base verdict; per-pass code paths
    /// should check `album_name.is_some()` instead.
    pub(crate) fn requires_per_pass_paths(&self) -> bool {
        const PER_PASS_TOKENS: [&str; 3] = [
            paths::TOKEN_ALBUM,
            paths::TOKEN_SMART_FOLDER,
            paths::TOKEN_LIBRARY,
        ];
        let any_token = |s: &str| PER_PASS_TOKENS.iter().any(|t| s.contains(t));
        any_token(&self.folder_structure)
            || any_token(&self.folder_structure_albums)
            || any_token(&self.folder_structure_smart_folders)
            || self.folder_structure_albums.as_ref() != self.folder_structure.as_str()
            || self.folder_structure_smart_folders.as_ref() != self.folder_structure.as_str()
    }

    /// Clone this config for a single download pass: pick the per-category
    /// template (`folder_structure_albums` for `PassKind::Album`,
    /// `folder_structure_smart_folders` for `PassKind::SmartFolder`,
    /// `folder_structure` for `PassKind::Unfiled`), pre-expand the matching
    /// token (`{album}` / `{smart-folder}`), and pin the pass's exclude-ids
    /// set in one clone.
    ///
    /// The unfiled pass keeps the legacy `{album}` token so existing configs
    /// with `--folder-structure "{album}/..."` still produce the same
    /// on-disk tree.
    pub(crate) fn with_pass(&self, pass: &crate::commands::AlbumPass) -> Self {
        let folder_structure = filter::folder_structure_for_pass(
            &self.folder_structure,
            &self.folder_structure_albums,
            &self.folder_structure_smart_folders,
            &self.library,
            pass,
        );
        Self {
            album_name: Some(Arc::clone(&pass.album.name)),
            folder_structure,
            exclude_asset_ids: Arc::clone(&pass.exclude_ids),
            ..self.clone()
        }
    }

    /// Clone this config with a different `exclude_asset_ids` set. Used
    /// for the merged (non-`{album}`) full-sync path, where all passes
    /// share a single config but the exclude set is lifted off the plan.
    fn with_exclude_ids(&self, exclude_ids: Arc<FxHashSet<String>>) -> Self {
        Self {
            exclude_asset_ids: exclude_ids,
            ..self.clone()
        }
    }

    fn with_recent_scope(&self, recent_scope: crate::cli::RecentScope) -> Self {
        Self {
            recent_scope,
            ..self.clone()
        }
    }
}

impl std::fmt::Debug for DownloadConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut s = f.debug_struct("DownloadConfig");
        s.field("directory", &self.directory)
            .field("folder_structure", &self.folder_structure)
            .field("folder_structure_albums", &self.folder_structure_albums)
            .field(
                "folder_structure_smart_folders",
                &self.folder_structure_smart_folders,
            )
            .field("resolution", &self.resolution)
            .field("media", &self.media)
            .field("skip_created_before", &self.skip_created_before)
            .field("skip_created_after", &self.skip_created_after);
        s.field("metadata", &self.metadata)
            .field("refresh_metadata", &self.refresh_metadata)
            .field("capture_timestamp_repair", &self.capture_timestamp_repair)
            .field("repair_truncated", &self.repair_truncated)
            .field("concurrent_downloads", &self.concurrent_downloads)
            .field("recent", &self.recent)
            .field("recent_scope", &self.recent_scope)
            .field("retry", &self.retry)
            .field("live_photo_mode", &self.live_photo_mode)
            .field("live_resolution", &self.live_resolution)
            .field(
                "live_photo_mov_filename_policy",
                &self.live_photo_mov_filename_policy,
            )
            .field("edited", &self.edited)
            .field("alternative", &self.alternative)
            .field("raw_policy", &self.raw_policy)
            .field("file_match_policy", &self.file_match_policy)
            .field("force_resolution", &self.force_resolution)
            .field("keep_unicode_in_filenames", &self.keep_unicode_in_filenames)
            .field("filename_exclude", &self.filename_exclude)
            .field("temp_suffix", &self.temp_suffix)
            .field("state_db", &self.state_db.is_some())
            .field("retry_only", &self.retry_only)
            .field("sync_mode", &self.sync_mode)
            .field("enum_config_hash", &self.enum_config_hash)
            .field("album_name", &self.album_name)
            .field("exclude_asset_ids_count", &self.exclude_asset_ids.len())
            .field("max_download_attempts", &self.max_download_attempts)
            .field("bandwidth_limiter", &self.bandwidth_limiter)
            .finish()
    }
}

#[cfg(test)]
impl DownloadConfig {
    /// Default test config shared across download submodule tests.
    pub(super) fn test_default() -> Self {
        use rustc_hash::FxHashSet;
        Self {
            directory: Arc::from(Path::new("/nonexistent/download_filter_tests")),
            folder_structure: "{:%Y/%m/%d}".to_string(),
            folder_structure_albums: Arc::from(crate::config::DEFAULT_FOLDER_STRUCTURE_ALBUMS),
            folder_structure_smart_folders: Arc::from(
                crate::config::DEFAULT_FOLDER_STRUCTURE_SMART_FOLDERS,
            ),
            resolution: crate::types::PhotoResolution::Original,
            media: crate::config::MediaSelection::all(),
            skip_created_before: None,
            skip_created_after: None,
            metadata: crate::config::MetadataConfig::default(),
            refresh_metadata: false,
            capture_timestamp_repair: CaptureTimestampRepair::Preserve,
            repair_truncated: false,
            concurrent_downloads: 1,
            recent: None,
            recent_scope: crate::cli::RecentScope::Global,
            retry: crate::retry::RetryConfig::default(),
            live_photo_mode: LivePhotoMode::Both,
            live_resolution: AssetVersionSize::LiveOriginal,
            live_photo_mov_filename_policy: crate::types::LivePhotoMovFilenamePolicy::Suffix,
            edited: false,
            alternative: false,
            raw_policy: RawPolicy::AsIs,
            file_match_policy: FileMatchPolicy::NameSizeDedupWithSuffix,
            force_resolution: false,
            keep_unicode_in_filenames: false,
            filename_exclude: Arc::from(Vec::<glob::Pattern>::new()),
            temp_suffix: Arc::from(".kei-tmp"),
            state_db: None,
            retry_only: false,
            max_download_attempts: 10,
            sync_mode: SyncMode::Full,
            enum_config_hash: None,
            album_name: None,
            exclude_asset_ids: std::sync::Arc::new(FxHashSet::default()),
            asset_groupings: Arc::new(AssetGroupings::default()),
            bandwidth_limiter: None,
            library: Arc::from(crate::icloud::photos::PRIMARY_ZONE_NAME),
        }
    }
}

/// Look up an owned `String` id in a shared `Arc<str>` interner,
/// inserting a fresh `Arc` if absent. Returns the shared handle.
///
/// `Arc::from(String)` transfers the string's buffer into the `Arc<str>`
/// without copying the bytes, so the miss path allocates no more than
/// the baseline cost of a fresh handle.
fn intern_id(interner: &mut FxHashSet<Arc<str>>, s: String) -> Arc<str> {
    if let Some(existing) = interner.get(s.as_str()) {
        return Arc::clone(existing);
    }
    let a: Arc<str> = Arc::from(s);
    interner.insert(Arc::clone(&a));
    a
}

/// Pre-loaded download state for O(1) skip decisions.
///
/// Loaded once at sync start from the state database, this enables fast
/// in-memory lookups instead of per-asset DB queries. For 100K+ asset
/// libraries, this significantly reduces DB roundtrips.
///
/// Asset-id keys are `Arc<str>` rather than `Box<str>` so the same id
/// allocation is shared across every map here (and with the producer's
/// seen-ids / touched-ids sets). On a 100k-asset library this collapses
/// ~4-6 independent `Box<str>` allocations per asset into one.
/// `library -> asset_id -> set of version_size` strings. Used by
/// `DownloadContext::downloaded_ids` and `metadata_retry_markers`.
type LibraryAssetVersionSet = FxHashMap<Arc<str>, FxHashMap<Arc<str>, FxHashSet<Box<str>>>>;

/// `library -> asset_id`. Used by retry-only mode to decide whether an asset is
/// already known in the same CloudKit zone.
type LibraryAssetSet = FxHashMap<Arc<str>, FxHashSet<Arc<str>>>;

/// `library -> asset_id -> attempt count`. Used by max-attempt gating without
/// leaking failures across libraries that happen to share an id.
type LibraryAssetAttemptCounts = FxHashMap<Arc<str>, FxHashMap<Arc<str>, u32>>;

/// `library -> asset_id -> (version_size -> value)`. Used by
/// `DownloadContext::downloaded_checksums` and
/// `downloaded_metadata_hashes`.
type LibraryAssetVersionValueMap =
    FxHashMap<Arc<str>, FxHashMap<Arc<str>, FxHashMap<Box<str>, Box<str>>>>;

/// Durable local-file evidence for a state-backed skip or pending adoption.
#[derive(Debug)]
struct RecordedLocalFile {
    path: PathBuf,
    local_checksum: Option<Box<str>>,
    download_checksum: Option<Box<str>>,
}

/// `library -> asset_id -> (version_size -> recorded local-file evidence)`.
type LibraryAssetVersionFileMap =
    FxHashMap<Arc<str>, FxHashMap<Arc<str>, FxHashMap<Box<str>, RecordedLocalFile>>>;

/// `library -> master_record_name -> asset_record_names`. Full enumeration
/// uses this durable family history to keep a legacy master-keyed state row
/// attached to the same child when CloudKit changes record order or page
/// boundaries.
type LibraryMasterAssetSet = FxHashMap<Arc<str>, FxHashMap<Arc<str>, FxHashSet<Arc<str>>>>;

/// `library -> master_record_name -> asset_record_name`. The value is the
/// durable child that owns a legacy master-keyed state row.
type LibraryMasterAssetMap = FxHashMap<Arc<str>, FxHashMap<Arc<str>, Arc<str>>>;

type ClaimedLegacyMasterStates = FxHashSet<(Arc<str>, Arc<str>)>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LegacyOwnerClaimMode {
    ExistingOnly,
    ReadOnly,
    Persist,
}

/// Prevent a child excluded by every pass's album membership from claiming a
/// mapping-less legacy row. Other filters still select identity because they
/// apply to metadata refreshes even when no media task is planned.
fn legacy_owner_claim_mode_for_configs<'a>(
    requested: LegacyOwnerClaimMode,
    asset: &PhotoAsset,
    configs: impl IntoIterator<Item = &'a DownloadConfig>,
) -> LegacyOwnerClaimMode {
    if requested == LegacyOwnerClaimMode::ExistingOnly
        || configs.into_iter().all(|config| {
            matches!(
                is_asset_filtered(asset, config),
                Some(FilterReason::ExcludedAlbum)
            )
        })
    {
        LegacyOwnerClaimMode::ExistingOnly
    } else {
        requested
    }
}

const LEGACY_OWNER_CLAIM_MAX_ATTEMPTS: u32 = 3;
const LEGACY_OWNER_CLAIM_RETRY_DELAY: Duration = Duration::from_millis(50);

#[derive(Debug, Default)]
struct DownloadContext {
    /// Nested map: `library` -> `asset_id` -> set of `version_sizes` that
    /// are already downloaded. Three-level shape so multi-library syncs
    /// don't dedupe the same asset_id across zones (PR10 / schema v8).
    /// All key levels use borrowed `&str` lookups for zero-allocation probes.
    downloaded_ids: LibraryAssetVersionSet,
    /// Nested map: `library` -> `asset_id` -> (`version_size` -> checksum).
    /// Used to detect checksum changes (CloudKit asset updated) without DB queries.
    downloaded_checksums: LibraryAssetVersionValueMap,
    /// Nested map: `library` -> `asset_id` -> (`version_size` -> local file).
    /// Used to validate path-aware filesystem skips after state says the
    /// remote bytes are unchanged.
    downloaded_files: LibraryAssetVersionFileMap,
    /// Nested map: `library` -> `asset_id` -> (`version_size` -> metadata_hash).
    /// Used to detect metadata-only changes (favorite toggle, keywords, GPS
    /// edit, etc.) when file bytes are unchanged but CloudKit has newer
    /// metadata.
    #[cfg_attr(not(feature = "xmp"), allow(dead_code))]
    downloaded_metadata_hashes: LibraryAssetVersionValueMap,
    /// Nested map: `library` -> `asset_id` -> set of `version_sizes` with a
    /// non-null `metadata_write_failed_at` from a prior sync. These always
    /// route to the metadata-rewrite path regardless of whether the hash
    /// changed.
    #[cfg_attr(not(feature = "xmp"), allow(dead_code))]
    metadata_retry_markers: LibraryAssetVersionSet,
    /// Assets the provider reported deleted whose downloaded rows remain.
    /// Staleness checks skip these so they cannot report a change that
    /// `refresh_downloaded_asset_metadata` is unable to apply.
    soft_deleted_ids: LibraryAssetSet,
    /// Nested map: `library` -> `asset_id` -> set of `version_sizes` that
    /// are pending at sync start. Used to resolve failed/pending rows when
    /// the expected file is already on disk instead of promoting them back to
    /// failed after the producer skips the duplicate path.
    pending_ids: LibraryAssetVersionSet,
    /// Nested map: `library` -> `asset_id` -> (`version_size` -> filename)
    /// for pending rows. Pending on-disk adoption uses this to avoid adopting
    /// a same-name/same-size collision that belongs to a different asset.
    pending_filenames: LibraryAssetVersionValueMap,
    /// Provider checksums for pending rows. Identity stabilization uses these
    /// to retain a compatible legacy master-keyed retry without merging a
    /// different sibling into it.
    pending_checksums: LibraryAssetVersionValueMap,
    /// Current recorded local files for pending rows. A provider checksum
    /// change clears `downloaded_at`, so historical paths are excluded.
    pending_files: LibraryAssetVersionFileMap,
    /// Provider identity families recorded before this pipeline started.
    /// `None` disables legacy adoption after a mapping query failure. The
    /// snapshot intentionally excludes mappings discovered during the current
    /// run so a newly seen sibling cannot claim another child's legacy state.
    asset_master_mappings: Option<LibraryMasterAssetSet>,
    /// Persisted child owner for each adopted legacy master-keyed state row.
    /// `None` disables new legacy adoption after an owner query failure.
    legacy_master_state_owners: Option<LibraryMasterAssetMap>,
    /// Nested map: `library` -> set of asset IDs known to the state DB (any
    /// status). Used in retry-only mode to skip new assets that were never
    /// synced in the same CloudKit zone.
    known_ids: LibraryAssetSet,
    /// Per-library/asset maximum download attempt count (from failed assets).
    /// Used to skip assets that have exceeded `max_download_attempts`.
    attempt_counts: LibraryAssetAttemptCounts,
    /// True when at least one downloaded asset-version lacks a metadata hash.
    /// Cached because the producer checks this on hot on-disk skip paths.
    downloaded_without_metadata_hash: bool,
}

impl DownloadContext {
    /// Load the download context from the state database. All state queries
    /// are independent and run concurrently so sync start doesn't serialize
    /// on round-trip latency across them.
    async fn load<D>(db: &D, retry_only: bool) -> Self
    where
        D: DownloadContextStateStore + DownloadStateStore + MetadataRewriteStore + ?Sized,
    {
        let known_ids_fut = async {
            if retry_only {
                db.get_all_known_ids().await.unwrap_or_else(|e| {
                    tracing::warn!(error = %e, "Failed to load known IDs from state DB");
                    Default::default()
                })
            } else {
                Default::default()
            }
        };
        let (
            downloaded_records,
            soft_deleted,
            hashes,
            markers,
            pending,
            attempts,
            known_id_rows,
            mapping_rows,
            legacy_owner_rows,
        ) = tokio::join!(
            async {
                db.get_downloaded_file_records().await.unwrap_or_else(|e| {
                    tracing::warn!(error = %e, "Failed to load downloaded records from state DB");
                    Default::default()
                })
            },
            async {
                db.get_soft_deleted_downloaded_ids().await.unwrap_or_else(|e| {
                    tracing::warn!(error = %e, "Failed to load soft-deleted assets from state DB");
                    Default::default()
                })
            },
            async {
                db.get_downloaded_metadata_hashes()
                    .await
                    .unwrap_or_else(|e| {
                        tracing::warn!(error = %e, "Failed to load metadata hashes from state DB");
                        Default::default()
                    })
            },
            async {
                db.get_metadata_retry_markers().await.unwrap_or_else(|e| {
                    tracing::warn!(error = %e, "Failed to load metadata retry markers from state DB");
                    Default::default()
                })
            },
            async {
                db.get_pending().await.unwrap_or_else(|e| {
                    tracing::warn!(error = %e, "Failed to load pending assets from state DB");
                    Default::default()
                })
            },
            async {
                db.get_attempt_counts().await.unwrap_or_else(|e| {
                    tracing::warn!(error = %e, "Failed to load attempt counts from state DB");
                    Default::default()
                })
            },
            known_ids_fut,
            db.get_asset_master_mappings(),
            db.get_legacy_master_state_owners(),
        );

        // Shared interner so the same asset_id allocates exactly one
        // `Arc<str>` across every map below (and is cheaply cloneable
        // into each via Arc::clone). This collapses the former 4-6
        // independent `String -> Box<str>` conversions per id into one.
        //
        // FxHashSet<Arc<str>> over FxHashMap<String, Arc<str>> so the
        // interner doesn't keep a duplicate `String` alive for every
        // id; `Arc::from(String)` transfers the String's buffer into
        // the Arc without an extra copy.
        let mut interner: FxHashSet<Arc<str>> = FxHashSet::default();

        let mut downloaded_ids: LibraryAssetVersionSet = FxHashMap::default();
        let mut downloaded_checksums: LibraryAssetVersionValueMap = FxHashMap::default();
        let mut downloaded_files: LibraryAssetVersionFileMap = FxHashMap::default();
        for record in downloaded_records {
            let crate::state::DownloadedFileRecord {
                library,
                id,
                version_size,
                checksum,
                local_path,
                local_checksum,
                download_checksum,
            } = record;
            let lib = intern_id(&mut interner, library);
            let id = intern_id(&mut interner, id);
            let version_size: Box<str> = version_size.as_str().into();
            downloaded_ids
                .entry(Arc::clone(&lib))
                .or_default()
                .entry(Arc::clone(&id))
                .or_default()
                .insert(version_size.clone());
            downloaded_checksums
                .entry(Arc::clone(&lib))
                .or_default()
                .entry(Arc::clone(&id))
                .or_default()
                .insert(version_size.clone(), checksum.into_boxed_str());
            if let Some(path) = local_path {
                downloaded_files
                    .entry(lib)
                    .or_default()
                    .entry(id)
                    .or_default()
                    .insert(
                        version_size,
                        RecordedLocalFile {
                            path,
                            local_checksum: local_checksum.map(String::into_boxed_str),
                            download_checksum: download_checksum.map(String::into_boxed_str),
                        },
                    );
            }
        }

        let mut soft_deleted_ids: LibraryAssetSet = FxHashMap::default();
        for (library, asset_id) in soft_deleted {
            let lib = intern_id(&mut interner, library);
            let id = intern_id(&mut interner, asset_id);
            soft_deleted_ids.entry(lib).or_default().insert(id);
        }

        let mut downloaded_metadata_hashes: LibraryAssetVersionValueMap = FxHashMap::default();
        for ((library, asset_id, version_size), metadata_hash) in hashes {
            let lib = intern_id(&mut interner, library);
            let id = intern_id(&mut interner, asset_id);
            downloaded_metadata_hashes
                .entry(lib)
                .or_default()
                .entry(id)
                .or_default()
                .insert(
                    version_size.into_boxed_str(),
                    metadata_hash.into_boxed_str(),
                );
        }

        let mut metadata_retry_markers: LibraryAssetVersionSet = FxHashMap::default();
        for (library, asset_id, version_size) in markers {
            let lib = intern_id(&mut interner, library);
            let id = intern_id(&mut interner, asset_id);
            metadata_retry_markers
                .entry(lib)
                .or_default()
                .entry(id)
                .or_default()
                .insert(version_size.into_boxed_str());
        }

        let mut pending_ids: LibraryAssetVersionSet = FxHashMap::default();
        let mut pending_filenames: LibraryAssetVersionValueMap = FxHashMap::default();
        let mut pending_checksums: LibraryAssetVersionValueMap = FxHashMap::default();
        let mut pending_files: LibraryAssetVersionFileMap = FxHashMap::default();
        for record in pending {
            let crate::state::AssetRecord {
                library,
                id,
                version_size,
                checksum,
                filename,
                local_path,
                local_checksum,
                download_checksum,
                downloaded_at,
                ..
            } = record;
            let lib = intern_id(&mut interner, library.to_string());
            let id = intern_id(&mut interner, id.to_string());
            let version_size: Box<str> = version_size.as_str().into();
            pending_ids
                .entry(Arc::clone(&lib))
                .or_default()
                .entry(Arc::clone(&id))
                .or_default()
                .insert(version_size.clone());
            pending_filenames
                .entry(Arc::clone(&lib))
                .or_default()
                .entry(Arc::clone(&id))
                .or_default()
                .insert(version_size.clone(), filename);
            pending_checksums
                .entry(Arc::clone(&lib))
                .or_default()
                .entry(Arc::clone(&id))
                .or_default()
                .insert(version_size.clone(), checksum);
            if downloaded_at.is_some()
                && let Some(path) = local_path
            {
                pending_files
                    .entry(lib)
                    .or_default()
                    .entry(id)
                    .or_default()
                    .insert(
                        version_size,
                        RecordedLocalFile {
                            path,
                            local_checksum: local_checksum.map(String::into_boxed_str),
                            download_checksum: download_checksum.map(String::into_boxed_str),
                        },
                    );
            }
        }

        let asset_master_mappings = mapping_rows
            .map(|mapping_rows| {
                let mut mappings: LibraryMasterAssetSet = FxHashMap::default();
                for (library, asset_record_name, master_record_name) in mapping_rows {
                    let lib = intern_id(&mut interner, library);
                    let asset = intern_id(&mut interner, asset_record_name);
                    let master = intern_id(&mut interner, master_record_name);
                    mappings
                        .entry(lib)
                        .or_default()
                        .entry(master)
                        .or_default()
                        .insert(asset);
                }
                mappings
            })
            .map_err(|e| {
                tracing::warn!(error = %e, "Failed to load asset/master mappings from state DB");
            })
            .ok();

        let legacy_master_state_owners = legacy_owner_rows
            .map(|owner_rows| {
                let mut owners: LibraryMasterAssetMap = FxHashMap::default();
                for (library, master_record_name, asset_record_name) in owner_rows {
                    let lib = intern_id(&mut interner, library);
                    let master = intern_id(&mut interner, master_record_name);
                    let asset = intern_id(&mut interner, asset_record_name);
                    owners.entry(lib).or_default().insert(master, asset);
                }
                owners
            })
            .map_err(|e| {
                tracing::warn!(error = %e, "Failed to load legacy master state owners from state DB");
            })
            .ok();

        let mut known_ids: LibraryAssetSet = FxHashMap::default();
        for (library, asset_id) in known_id_rows {
            let lib = intern_id(&mut interner, library);
            let id = intern_id(&mut interner, asset_id);
            known_ids.entry(lib).or_default().insert(id);
        }

        let mut attempt_counts: LibraryAssetAttemptCounts = FxHashMap::default();
        for ((library, asset_id), count) in attempts {
            let lib = intern_id(&mut interner, library);
            let id = intern_id(&mut interner, asset_id);
            attempt_counts.entry(lib).or_default().insert(id, count);
        }
        let downloaded_without_metadata_hash = count_version_set_entries(&downloaded_ids)
            > count_value_map_entries(&downloaded_metadata_hashes);

        Self {
            downloaded_ids,
            downloaded_checksums,
            downloaded_files,
            downloaded_metadata_hashes,
            metadata_retry_markers,
            soft_deleted_ids,
            pending_ids,
            pending_filenames,
            pending_checksums,
            pending_files,
            asset_master_mappings,
            legacy_master_state_owners,
            known_ids,
            attempt_counts,
            downloaded_without_metadata_hash,
        }
    }

    fn is_known(&self, library: &str, asset_id: &str) -> bool {
        self.known_ids
            .get(library)
            .is_some_and(|ids| ids.contains(asset_id))
    }

    fn attempt_count(&self, library: &str, asset_id: &str) -> Option<u32> {
        self.attempt_counts
            .get(library)
            .and_then(|assets| assets.get(asset_id))
            .copied()
    }

    /// Whether a downloaded asset-version needs a metadata-only rewrite:
    /// the caller has already matched checksums (bytes unchanged) and now
    /// checks whether (a) the stored metadata_hash differs from the new
    /// one or (b) a persisted retry marker is set from a prior sync where
    /// the writer failed after bytes landed.
    #[cfg_attr(not(feature = "xmp"), allow(dead_code))]
    fn needs_metadata_rewrite(
        &self,
        library: &str,
        asset_id: &str,
        version_size: VersionSizeKey,
        new_metadata_hash: Option<&str>,
    ) -> bool {
        if self.is_soft_deleted(library, asset_id) {
            return false;
        }
        let vs_str = version_size.as_str();
        let has_retry_marker = self
            .metadata_retry_markers
            .get(library)
            .and_then(|m| m.get(asset_id))
            .is_some_and(|vsset| vsset.contains(vs_str));
        if has_retry_marker {
            return true;
        }
        let Some(new_hash) = new_metadata_hash else {
            return false;
        };
        match self
            .downloaded_metadata_hashes
            .get(library)
            .and_then(|m| m.get(asset_id))
            .and_then(|map| map.get(vs_str))
        {
            Some(stored) => stored.as_ref() != new_hash,
            None => true, // downloaded row has no stored hash yet -- refresh
        }
    }

    /// Whether the provider reported this asset deleted while its downloaded
    /// rows remain. `refresh_downloaded_asset_metadata` cannot match those
    /// rows, so treating them as stale would report a change that can never
    /// be applied.
    fn is_soft_deleted(&self, library: &str, asset_id: &str) -> bool {
        self.soft_deleted_ids
            .get(library)
            .is_some_and(|assets| assets.contains(asset_id))
    }

    /// Whether at least one downloaded version has provider metadata that
    /// differs from the complete asset returned by iCloud.
    fn has_provider_metadata_drift(
        &self,
        library: &str,
        asset_id: &str,
        new_metadata_hash: Option<&str>,
    ) -> bool {
        let Some(new_hash) = new_metadata_hash else {
            return false;
        };
        if self.is_soft_deleted(library, asset_id) {
            return false;
        }
        let Some(downloaded_versions) = self
            .downloaded_ids
            .get(library)
            .and_then(|assets| assets.get(asset_id))
        else {
            return false;
        };
        let stored_hashes = self
            .downloaded_metadata_hashes
            .get(library)
            .and_then(|assets| assets.get(asset_id));
        downloaded_versions.iter().any(|version_size| {
            stored_hashes
                .and_then(|hashes| hashes.get(version_size.as_ref()))
                .is_none_or(|stored| stored.as_ref() != new_hash)
        })
    }

    /// Whether any live downloaded version of this asset carries a metadata
    /// rewrite retry marker. Asset-level counterpart to the per-version
    /// `needs_metadata_rewrite`, for callers that decide once per asset.
    fn has_downloaded_metadata_retry_marker(&self, library: &str, asset_id: &str) -> bool {
        if self.is_soft_deleted(library, asset_id) {
            return false;
        }
        let Some(markers) = self
            .metadata_retry_markers
            .get(library)
            .and_then(|assets| assets.get(asset_id))
        else {
            return false;
        };
        self.downloaded_ids
            .get(library)
            .and_then(|assets| assets.get(asset_id))
            .is_some_and(|downloaded| {
                markers
                    .iter()
                    .any(|version_size| downloaded.contains(version_size.as_ref()))
            })
    }

    fn has_state_version(&self, library: &str, asset_id: &str) -> bool {
        self.downloaded_ids
            .get(library)
            .and_then(|assets| assets.get(asset_id))
            .is_some_and(|versions| !versions.is_empty())
            || self
                .pending_ids
                .get(library)
                .and_then(|assets| assets.get(asset_id))
                .is_some_and(|versions| !versions.is_empty())
    }

    fn has_matching_downloaded_checksum(
        &self,
        library: &str,
        asset_id: &str,
        asset: &PhotoAsset,
    ) -> bool {
        let Some(checksums) = self
            .downloaded_checksums
            .get(library)
            .and_then(|assets| assets.get(asset_id))
        else {
            return false;
        };
        asset.versions().iter().any(|(version_size, version)| {
            checksums
                .get(VersionSizeKey::from(*version_size).as_str())
                .is_some_and(|stored| stored.as_ref() == version.checksum.as_ref())
        })
    }

    fn has_matching_state_checksum(
        &self,
        library: &str,
        asset_id: &str,
        asset: &PhotoAsset,
    ) -> bool {
        self.has_matching_downloaded_checksum(library, asset_id, asset)
            || self
                .pending_checksums
                .get(library)
                .and_then(|assets| assets.get(asset_id))
                .is_some_and(|checksums| {
                    asset.versions().iter().any(|(version_size, version)| {
                        checksums
                            .get(VersionSizeKey::from(*version_size).as_str())
                            .is_some_and(|stored| stored.as_ref() == version.checksum.as_ref())
                    })
                })
    }

    /// Select the durable local state identity for a provider asset.
    ///
    /// New assets use their unique `CPLAsset.recordName`. A pre-v0.24 row can
    /// still be keyed by `CPLMaster.recordName`; retain that key only when its
    /// provider checksum matches and durable family history identifies this
    /// child as the sole sibling without its own state row.
    fn should_use_legacy_master_state(&self, library: &str, asset: &PhotoAsset) -> bool {
        let asset_record_name = asset.asset_record_name();
        let master_record_name = asset.id();
        if asset_record_name == master_record_name
            || self.has_state_version(library, asset_record_name)
        {
            return false;
        }
        if !self.has_matching_state_checksum(library, master_record_name, asset) {
            return false;
        }

        let Some(owners) = &self.legacy_master_state_owners else {
            return false;
        };
        if let Some(owner) = owners
            .get(library)
            .and_then(|masters| masters.get(master_record_name))
        {
            return owner.as_ref() == asset_record_name;
        }

        let Some(mappings) = &self.asset_master_mappings else {
            return false;
        };
        let Some(mapped_children) = mappings
            .get(library)
            .and_then(|masters| masters.get(master_record_name))
        else {
            // Databases predating durable asset/master mappings can still
            // retain a checksum-compatible master row for their first child.
            return true;
        };
        let mut children_without_state = mapped_children
            .iter()
            .filter(|child| !self.has_state_version(library, child));
        let sole_child = children_without_state.next();
        children_without_state.next().is_none()
            && sole_child.is_some_and(|child| child.as_ref() == asset_record_name)
    }

    fn select_asset_state_record_name(
        &self,
        library: &str,
        asset: &PhotoAsset,
        claimed_legacy_master_states: &mut ClaimedLegacyMasterStates,
    ) -> Arc<str> {
        let use_legacy_master = self.should_use_legacy_master_state(library, asset)
            && claimed_legacy_master_states.insert((Arc::from(library), Arc::from(asset.id())));
        if use_legacy_master {
            Arc::from(asset.id())
        } else {
            asset.asset_record_name_arc()
        }
    }

    fn select_existing_asset_state_record_name(
        &self,
        library: &str,
        asset: &PhotoAsset,
    ) -> Arc<str> {
        let has_persisted_owner = self
            .legacy_master_state_owners
            .as_ref()
            .and_then(|libraries| libraries.get(library))
            .and_then(|masters| masters.get(asset.id()))
            .is_some();
        if has_persisted_owner && self.should_use_legacy_master_state(library, asset) {
            Arc::from(asset.id())
        } else {
            asset.asset_record_name_arc()
        }
    }

    async fn select_asset_state_record_name_for_download(
        &self,
        db: Option<&dyn DownloadStore>,
        library: &str,
        asset: &PhotoAsset,
        claimed_legacy_master_states: &mut ClaimedLegacyMasterStates,
    ) -> std::result::Result<Arc<str>, crate::state::error::StateError> {
        let state_record_name =
            self.select_asset_state_record_name(library, asset, claimed_legacy_master_states);
        if state_record_name.as_ref() != asset.id()
            || self
                .legacy_master_state_owners
                .as_ref()
                .and_then(|libraries| libraries.get(library))
                .and_then(|masters| masters.get(asset.id()))
                .is_some()
        {
            return Ok(state_record_name);
        }

        let Some(db) = db else {
            return Ok(asset.asset_record_name_arc());
        };

        for attempt in 1..=LEGACY_OWNER_CLAIM_MAX_ATTEMPTS {
            match db
                .claim_legacy_master_state_owner(library, asset.id(), asset.asset_record_name())
                .await
            {
                Ok(true) => return Ok(state_record_name),
                Ok(false) => return Ok(asset.asset_record_name_arc()),
                Err(error) if attempt < LEGACY_OWNER_CLAIM_MAX_ATTEMPTS => {
                    tracing::debug!(
                        library,
                        master_record_name = asset.id(),
                        asset_record_name = asset.asset_record_name(),
                        attempt,
                        error = %error,
                        "Retrying legacy master state owner claim"
                    );
                    let delay =
                        LEGACY_OWNER_CLAIM_RETRY_DELAY.saturating_mul(1u32 << (attempt - 1));
                    tokio::time::sleep(delay).await;
                }
                Err(error) => return Err(error),
            }
        }

        Err(crate::state::error::StateError::Invariant {
            operation: "select_asset_state_record_name_for_download",
            detail: "legacy owner claim retry loop ran no attempts".into(),
        })
    }

    /// Check if an asset should be downloaded based on pre-loaded state.
    ///
    /// Returns:
    /// - `Some(true)` — definitely needs download (not in DB or checksum changed)
    /// - `Some(false)` — hard skip, DB confirms downloaded with matching checksum
    ///   (only when `trust_state` is true)
    /// - `None` — downloaded with matching checksum but needs filesystem check
    ///   to confirm file is still on disk (when `trust_state` is false)
    ///
    /// `trust_state=true` skips the filesystem stat: only `--only-print-filenames`
    /// uses it (no side effects, the user just wants to preview). The real-sync
    /// path uses `trust_state=false` — see PR #318 for why.
    ///
    /// Uses borrowed `&str` keys for zero-allocation lookups.
    fn should_download_fast(
        &self,
        library: &str,
        asset_id: &str,
        version_size: VersionSizeKey,
        checksum: &str,
        trust_state: bool,
    ) -> Option<bool> {
        if checksum.is_empty() {
            tracing::warn!(
                asset_id,
                version_size = %version_size.as_str(),
                "Empty remote checksum cannot be trusted for skip decisions"
            );
            return Some(true);
        }

        let version_size_str = version_size.as_str();

        // Borrowed `&str` keys at every level — no allocation per probe.
        let is_downloaded = self
            .downloaded_ids
            .get(library)
            .and_then(|m| m.get(asset_id))
            .is_some_and(|versions| versions.contains(version_size_str));

        if !is_downloaded {
            // Not in downloaded set — needs download
            return Some(true);
        }

        // Check if checksum changed (also zero-allocation lookup). Track
        // whether a stored checksum is present at all so we can audit the
        // "no stored checksum" path, which pre-v3 rows fall into.
        let stored_checksum = self
            .downloaded_checksums
            .get(library)
            .and_then(|m| m.get(asset_id))
            .and_then(|versions| versions.get(version_size_str));
        if let Some(stored) = stored_checksum {
            if stored.as_ref() != checksum {
                return Some(true);
            }
        } else {
            // Pre-v3 row with no stored local_checksum. Audit so operators can
            // correlate unexpected "skipped" counts with missing checksum
            // history (the row will gain a checksum on next download).
            tracing::debug!(
                asset_id = asset_id,
                version_size = %version_size_str,
                trust_state = trust_state,
                "no stored checksum for downloaded asset-version; skip decision uses trust_state only"
            );
        }

        if trust_state { Some(false) } else { None }
    }

    fn downloaded_file(
        &self,
        library: &str,
        asset_id: &str,
        version_size: VersionSizeKey,
    ) -> Option<&RecordedLocalFile> {
        self.downloaded_files
            .get(library)
            .and_then(|m| m.get(asset_id))
            .and_then(|versions| versions.get(version_size.as_str()))
    }

    fn pending_file(
        &self,
        library: &str,
        asset_id: &str,
        version_size: VersionSizeKey,
    ) -> Option<&RecordedLocalFile> {
        self.pending_files
            .get(library)
            .and_then(|m| m.get(asset_id))
            .and_then(|versions| versions.get(version_size.as_str()))
    }

    fn pending_file_matching_checksum(
        &self,
        library: &str,
        asset_id: &str,
        version_size: VersionSizeKey,
        provider_checksum: &str,
    ) -> Option<&RecordedLocalFile> {
        let stored_checksum = self
            .pending_checksums
            .get(library)
            .and_then(|assets| assets.get(asset_id))
            .and_then(|versions| versions.get(version_size.as_str()))?;
        if stored_checksum.as_ref() != provider_checksum {
            return None;
        }
        self.pending_file(library, asset_id, version_size)
    }

    fn has_downloaded_without_metadata_hash(&self) -> bool {
        self.downloaded_without_metadata_hash
    }
}

fn count_version_set_entries(map: &LibraryAssetVersionSet) -> usize {
    map.values()
        .map(|assets| {
            assets
                .values()
                .map(|versions| versions.len())
                .sum::<usize>()
        })
        .sum()
}

fn count_value_map_entries(map: &LibraryAssetVersionValueMap) -> usize {
    map.values()
        .map(|assets| {
            assets
                .values()
                .map(|versions| versions.len())
                .sum::<usize>()
        })
        .sum()
}

async fn preload_download_context(config: &DownloadConfig) -> Arc<DownloadContext> {
    let download_ctx = if let Some(db) = &config.state_db {
        tracing::debug!("Pre-loading download state from database");
        DownloadContext::load(db.as_ref(), config.retry_only).await
    } else {
        DownloadContext::default()
    };
    tracing::debug!(
        downloaded_ids = download_ctx.downloaded_ids.len(),
        "Download context loaded"
    );
    Arc::new(download_ctx)
}

/// Pre-compute one `Arc<DownloadConfig>` per pass. Each pass_index maps to
/// a derived config that pre-expands `{album}` and pins the pass's
/// exclude-asset-ids set. In `{album}` mode passes may legitimately differ
/// per entry; outside of it, passes share identical excludes but the per-
/// pass wrapper is harmless and keeps call sites uniform.
fn build_pass_configs(
    passes: &[crate::commands::AlbumPass],
    base: &DownloadConfig,
) -> Vec<Arc<DownloadConfig>> {
    passes
        .iter()
        .map(|pass| Arc::new(base.with_pass(pass)))
        .collect()
}

fn build_pass_configs_with_download_concurrency(
    passes: &[crate::commands::AlbumPass],
    base: &DownloadConfig,
    per_pass_download_concurrency: usize,
) -> Vec<Arc<DownloadConfig>> {
    passes
        .iter()
        .map(|pass| {
            let mut config = base.with_pass(pass);
            config.concurrent_downloads = per_pass_download_concurrency.max(1);
            Arc::new(config)
        })
        .collect()
}

#[cfg(test)]
fn incremental_requires_full_enumeration(passes: &[crate::commands::AlbumPass]) -> bool {
    passes
        .iter()
        .any(|pass| pass.kind != crate::commands::PassKind::Unfiled)
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum IncrementalRoutingDecision {
    Safe,
    TargetedAlbumBackfill {
        pass_indices: Vec<usize>,
        reason: FullEnumerationReason,
    },
    NeedsFull {
        reason: FullEnumerationReason,
    },
}

#[derive(Debug)]
struct IncrementalPassRouting {
    selected_album_passes: FxHashMap<String, Vec<usize>>,
    selected_container_ids: Vec<String>,
    unfiled_passes: Vec<usize>,
}

impl IncrementalPassRouting {
    fn from_passes(passes: &[crate::commands::AlbumPass]) -> Self {
        let mut selected_album_passes: FxHashMap<String, Vec<usize>> = FxHashMap::default();
        let mut unfiled_passes = Vec::new();

        for (index, pass) in passes.iter().enumerate() {
            match pass.kind {
                crate::commands::PassKind::Album => {
                    if let Some(container_id) = pass.album.container_id() {
                        selected_album_passes
                            .entry(container_id.to_string())
                            .or_default()
                            .push(index);
                    }
                }
                crate::commands::PassKind::Unfiled => unfiled_passes.push(index),
                crate::commands::PassKind::SmartFolder => {}
            }
        }

        let mut selected_container_ids: Vec<String> =
            selected_album_passes.keys().cloned().collect();
        selected_container_ids.sort_unstable();

        Self {
            selected_album_passes,
            selected_container_ids,
            unfiled_passes,
        }
    }

    fn has_selected_albums(&self) -> bool {
        !self.selected_album_passes.is_empty()
    }

    fn selected_container_refs(&self) -> Vec<&str> {
        self.selected_container_ids
            .iter()
            .map(String::as_str)
            .collect()
    }

    fn album_passes_for_container(&self, container_id: &str) -> Option<&[usize]> {
        self.selected_album_passes
            .get(container_id)
            .map(Vec::as_slice)
    }
}

async fn determine_incremental_routing_decision(
    passes: &[crate::commands::AlbumPass],
    config: &DownloadConfig,
    controls: DownloadControls,
) -> IncrementalRoutingDecision {
    let has_unmapped_album_pass = passes.iter().any(|pass| {
        pass.kind == crate::commands::PassKind::Album && pass.album.container_id().is_none()
    });
    if has_unmapped_album_pass {
        return IncrementalRoutingDecision::NeedsFull {
            reason: FullEnumerationReason::AlbumRelationHydrationIncomplete,
        };
    }

    let routing = IncrementalPassRouting::from_passes(passes);
    if !routing.has_selected_albums() {
        return IncrementalRoutingDecision::Safe;
    }

    let Some(db) = &config.state_db else {
        return IncrementalRoutingDecision::NeedsFull {
            reason: FullEnumerationReason::AlbumRelationHydrationIncomplete,
        };
    };

    let mut backfill_pass_indices = Vec::new();
    for (index, pass) in passes.iter().enumerate() {
        if pass.kind != crate::commands::PassKind::Album {
            continue;
        }
        let Some(container_id) = pass.album.container_id() else {
            continue;
        };
        match db
            .selected_album_containers_have_complete_snapshots(&config.library, &[container_id])
            .await
        {
            Ok(true) => {}
            Ok(false) => backfill_pass_indices.push(index),
            Err(e) => {
                tracing::warn!(
                    container_id,
                    error = %e,
                    "Failed to verify album membership snapshot for incremental routing"
                );
                return IncrementalRoutingDecision::NeedsFull {
                    reason: FullEnumerationReason::OtherStaticReason,
                };
            }
        }
    }

    if backfill_pass_indices.is_empty() {
        IncrementalRoutingDecision::Safe
    } else if should_record_album_snapshots(passes, config, controls) {
        IncrementalRoutingDecision::TargetedAlbumBackfill {
            pass_indices: backfill_pass_indices,
            reason: FullEnumerationReason::AlbumRelationHydrationIncomplete,
        }
    } else {
        IncrementalRoutingDecision::NeedsFull {
            reason: FullEnumerationReason::AlbumRelationHydrationIncomplete,
        }
    }
}

async fn route_incremental_asset_to_passes(
    asset: &PhotoAsset,
    routing: &IncrementalPassRouting,
    selected_container_ids: &[&str],
    config: &DownloadConfig,
) -> Result<Vec<usize>> {
    if !routing.has_selected_albums() {
        return Ok(routing.unfiled_passes.clone());
    }

    let db = config
        .state_db
        .as_ref()
        .context("Album-aware incremental routing requires a state database")?;
    let memberships = db
        .get_live_selected_album_memberships_for_asset(
            &config.library,
            asset.asset_record_name(),
            selected_container_ids,
        )
        .await
        .with_context(|| {
            format!(
                "Could not look up album memberships for asset {}",
                asset.asset_record_name()
            )
        })?;

    let mut pass_indices = FxHashSet::default();
    for membership in &memberships {
        if let Some(indices) = routing.album_passes_for_container(&membership.container_id) {
            pass_indices.extend(indices.iter().copied());
        }
    }
    if memberships.is_empty() {
        pass_indices.extend(routing.unfiled_passes.iter().copied());
    }

    let mut pass_indices: Vec<usize> = pass_indices.into_iter().collect();
    pass_indices.sort_unstable();
    Ok(pass_indices)
}

fn split_incremental_and_smart_folder_passes(
    passes: &[crate::commands::AlbumPass],
) -> (
    Vec<crate::commands::AlbumPass>,
    Vec<crate::commands::AlbumPass>,
) {
    passes
        .iter()
        .cloned()
        .partition(|pass| pass.kind != crate::commands::PassKind::SmartFolder)
}

fn merge_download_outcomes(left: &DownloadOutcome, right: &DownloadOutcome) -> DownloadOutcome {
    let mut auth_error_count = 0usize;
    let mut failed_count = 0usize;
    for outcome in [left, right] {
        match outcome {
            DownloadOutcome::Success => {}
            DownloadOutcome::SessionExpired {
                auth_error_count: n,
            } => {
                auth_error_count += *n;
            }
            DownloadOutcome::PartialFailure { failed_count: n } => {
                failed_count += *n;
            }
        }
    }

    if auth_error_count > 0 {
        DownloadOutcome::SessionExpired { auth_error_count }
    } else if failed_count > 0 {
        DownloadOutcome::PartialFailure { failed_count }
    } else {
        DownloadOutcome::Success
    }
}

async fn collect_pass_asset_ids(pass: &crate::commands::AlbumPass) -> Result<FxHashSet<String>> {
    let count = pass
        .album
        .len()
        .await
        .with_context(|| format!("Could not count assets in album `{}`", pass.album.name))?;
    let (stream, _token_rx) = pass.album.photo_stream_with_token(None, Some(count), 1);
    tokio::pin!(stream);
    let mut ids = FxHashSet::default();
    while let Some(item) = stream.next().await {
        let asset = item?;
        ids.insert(asset.asset_record_name().to_string());
    }
    Ok(ids)
}

async fn build_pass_configs_resolving_deferred_excludes(
    passes: &[crate::commands::AlbumPass],
    base: &DownloadConfig,
) -> Result<Vec<Arc<DownloadConfig>>> {
    let mut pass_configs = build_pass_configs(passes, base);
    let Some(unfiled_index) = deferred_unfiled_index(passes) else {
        return Ok(pass_configs);
    };

    let per_album: Vec<Result<FxHashSet<String>>> = stream::iter(
        passes
            .iter()
            .filter(|pass| pass.kind == crate::commands::PassKind::Album),
    )
    .map(collect_pass_asset_ids)
    .buffer_unordered(base.concurrent_downloads.max(1))
    .collect()
    .await;

    let mut exclude_ids = FxHashSet::default();
    for ids in per_album {
        exclude_ids.extend(ids?);
    }

    if let (Some(pass), Some(slot)) = (
        passes.get(unfiled_index),
        pass_configs.get_mut(unfiled_index),
    ) {
        let mut unfiled_config = base.with_pass(pass);
        unfiled_config.exclude_asset_ids = Arc::new(exclude_ids);
        *slot = Arc::new(unfiled_config);
    }
    Ok(pass_configs)
}

#[derive(Debug)]
struct PerPassStreamingResult {
    pass_index: usize,
    kind: crate::commands::PassKind,
    label: String,
    count: u64,
    elapsed: std::time::Duration,
    token_rx: tokio::sync::oneshot::Receiver<Option<String>>,
    result: StreamingResult,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PassKey {
    pub(crate) index: usize,
    pub(crate) kind: crate::commands::PassKind,
    pub(crate) label: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RecoveryAction {
    ContinueTail,
    RetryPasses(Vec<PassKey>),
    RevalidateRecords(Vec<ProviderRecordId>),
    ReplayFromPriorToken,
    ReconcileInventory(FullEnumerationReason),
    Reauthenticate,
    Stop,
}

impl RecoveryAction {
    pub(crate) const fn as_str(&self) -> &'static str {
        match self {
            Self::ContinueTail => "continue_tail",
            Self::RetryPasses(_) => "retry_passes",
            Self::RevalidateRecords(_) => "revalidate_records",
            Self::ReplayFromPriorToken => "replay_from_prior_token",
            Self::ReconcileInventory(_) => "reconcile_inventory",
            Self::Reauthenticate => "reauthenticate",
            Self::Stop => "stop",
        }
    }

    fn retry_passes(&self) -> &[PassKey] {
        match self {
            Self::RetryPasses(passes) => passes,
            _ => &[],
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PassTokenResult {
    Present(String),
    Missing,
    Blank,
    ReceiverDropped,
    EnumerationIncomplete,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PassTokenObservation {
    pass: PassKey,
    result: PassTokenResult,
}

struct PassTokenReceiver {
    pass: PassKey,
    receiver: tokio::sync::oneshot::Receiver<Option<String>>,
    enumeration_complete: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TokenGap {
    Missing,
    Blank,
    ReceiverDropped,
    EnumerationIncomplete,
    Mismatch,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ZoneTokenEvidence {
    Complete {
        token: String,
    },
    Recoverable {
        passes: Vec<PassKey>,
        reason: TokenGap,
    },
    Incomplete {
        reason: TokenGap,
    },
}

fn classify_zone_token_evidence(observations: &[PassTokenObservation]) -> ZoneTokenEvidence {
    let passes_without_tokens = || {
        observations
            .iter()
            .filter(|observation| !matches!(observation.result, PassTokenResult::Present(_)))
            .map(|observation| observation.pass.clone())
            .collect()
    };

    for (result, reason) in [
        (
            PassTokenResult::EnumerationIncomplete,
            TokenGap::EnumerationIncomplete,
        ),
        (PassTokenResult::ReceiverDropped, TokenGap::ReceiverDropped),
        (PassTokenResult::Blank, TokenGap::Blank),
        (PassTokenResult::Missing, TokenGap::Missing),
    ] {
        if observations.iter().any(|observation| {
            std::mem::discriminant(&observation.result) == std::mem::discriminant(&result)
        }) {
            return ZoneTokenEvidence::Recoverable {
                passes: passes_without_tokens(),
                reason,
            };
        }
    }

    let tokens: Vec<&str> = observations
        .iter()
        .filter_map(|observation| match &observation.result {
            PassTokenResult::Present(token) => Some(token.as_str()),
            _ => None,
        })
        .collect();
    let Some(first) = tokens.first() else {
        return ZoneTokenEvidence::Incomplete {
            reason: TokenGap::Missing,
        };
    };
    if tokens.iter().all(|token| token == first) {
        ZoneTokenEvidence::Complete {
            token: (*first).to_string(),
        }
    } else {
        ZoneTokenEvidence::Recoverable {
            passes: observations
                .iter()
                .map(|observation| observation.pass.clone())
                .collect(),
            reason: TokenGap::Mismatch,
        }
    }
}

type DownloadPhotoStream = Pin<Box<dyn Stream<Item = anyhow::Result<PhotoAsset>> + Send + 'static>>;

const DEFERRED_UNFILED_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);
const DEFERRED_UNFILED_HEARTBEAT_ASSETS: u64 = 1_000;

#[derive(Debug)]
struct DeferredUnfiledHeartbeat {
    library: Arc<str>,
    expected_assets: Option<u64>,
    started: Instant,
    last_log: Instant,
    last_logged_assets: u64,
    assets_enumerated: u64,
}

impl DeferredUnfiledHeartbeat {
    fn start(library: Arc<str>, expected_assets: Option<u64>) -> Self {
        let now = Instant::now();
        tracing::info!(
            library = %library,
            pass_type = "unfiled",
            expected_assets = ?expected_assets,
            assets_enumerated = 0_u64,
            "Deferred unfiled enumeration started"
        );
        Self {
            library,
            expected_assets,
            started: now,
            last_log: now,
            last_logged_assets: 0,
            assets_enumerated: 0,
        }
    }

    fn record_asset(&mut self) {
        self.assets_enumerated = self.assets_enumerated.saturating_add(1);
        let now = Instant::now();
        let asset_delta = self
            .assets_enumerated
            .saturating_sub(self.last_logged_assets);
        if asset_delta < DEFERRED_UNFILED_HEARTBEAT_ASSETS
            && now.duration_since(self.last_log) < DEFERRED_UNFILED_HEARTBEAT_INTERVAL
        {
            return;
        }

        self.last_log = now;
        self.last_logged_assets = self.assets_enumerated;
        tracing::info!(
            library = %self.library,
            pass_type = "unfiled",
            assets_enumerated = self.assets_enumerated,
            expected_assets = ?self.expected_assets,
            elapsed = %format_duration(self.started.elapsed()),
            "Deferred unfiled enumeration progress"
        );
    }

    fn complete(&self) {
        tracing::info!(
            library = %self.library,
            pass_type = "unfiled",
            assets_enumerated = self.assets_enumerated,
            expected_assets = ?self.expected_assets,
            elapsed = %format_duration(self.started.elapsed()),
            "Deferred unfiled enumeration complete"
        );
    }
}

fn track_deferred_unfiled_heartbeat(
    stream: DownloadPhotoStream,
    library: Arc<str>,
    expected_assets: Option<u64>,
) -> DownloadPhotoStream {
    let heartbeat = DeferredUnfiledHeartbeat::start(library, expected_assets);
    Box::pin(stream::unfold(
        (stream, heartbeat),
        |(mut stream, mut heartbeat)| async move {
            match stream.next().await {
                Some(item) => {
                    if item.is_ok() {
                        heartbeat.record_asset();
                    }
                    Some((item, (stream, heartbeat)))
                }
                None => {
                    heartbeat.complete();
                    None
                }
            }
        },
    ))
}

fn open_photo_stream_for_controls(
    album: &crate::icloud::photos::PhotoAlbum,
    limit: Option<u32>,
    total_count: Option<u64>,
    fast_concurrency: usize,
    download_concurrency: usize,
    controls: DownloadControls,
    treat_empty_tail_as_error: bool,
) -> (
    DownloadPhotoStream,
    tokio::sync::oneshot::Receiver<Option<String>>,
) {
    if controls.run_mode.is_dry_run() || controls.run_mode.only_print_filenames() {
        album.photo_stream_with_token_policy(
            limit,
            total_count,
            fast_concurrency,
            treat_empty_tail_as_error,
        )
    } else {
        album.photo_stream_with_token_for_download_policy(
            limit,
            total_count,
            download_concurrency,
            treat_empty_tail_as_error,
        )
    }
}

struct RecentFrontier {
    asset_ids: Arc<FxHashSet<String>>,
    oldest_created: Option<DateTime<Utc>>,
}

struct FullPassStreamOptions {
    pass_index: usize,
    controls: DownloadControls,
    count: u64,
    kind: crate::commands::PassKind,
    shutdown_token: CancellationToken,
    download_ctx: Option<Arc<DownloadContext>>,
    album_snapshot: Option<AlbumSnapshotRecorder>,
}

#[derive(Clone)]
struct AlbumSnapshotRecorder {
    db: Arc<dyn DownloadStore>,
    library: Arc<str>,
    container_id: Arc<str>,
    generation: i64,
    write_failed: Arc<AtomicBool>,
}

impl AlbumSnapshotRecorder {
    async fn start_for_pass(
        db: Option<Arc<dyn DownloadStore>>,
        pass: &crate::commands::AlbumPass,
        enum_config_hash: Option<&str>,
    ) -> Result<Option<Self>> {
        if pass.kind != crate::commands::PassKind::Album {
            return Ok(None);
        }
        let Some(db) = db else {
            return Ok(None);
        };
        let Some(container_id) = pass.album.container_id() else {
            tracing::debug!(
                album = %pass.album.name,
                library = %pass.album.zone_name(),
                "Album pass has no container ID; skipping membership snapshot"
            );
            return Ok(None);
        };
        let library = pass.album.zone_name();
        if let Err(e) = db
            .upsert_album_container(library, container_id, &pass.album.name, "album")
            .await
        {
            tracing::warn!(
                album = %pass.album.name,
                library,
                container_id,
                error = %e,
                "Failed to upsert album container; skipping membership snapshot"
            );
            return Err(e.into());
        }
        let generation = match db
            .start_album_membership_snapshot(library, container_id, enum_config_hash)
            .await
        {
            Ok(generation) => generation,
            Err(e) => {
                tracing::warn!(
                    album = %pass.album.name,
                    library,
                    container_id,
                    error = %e,
                    "Failed to start album membership snapshot"
                );
                return Err(e.into());
            }
        };
        Ok(Some(Self {
            db,
            library: Arc::from(library),
            container_id: Arc::from(container_id),
            generation,
            write_failed: Arc::new(AtomicBool::new(false)),
        }))
    }

    async fn record_asset(&self, asset: &PhotoAsset) {
        if let Err(e) = self
            .db
            .add_album_membership_to_snapshot(
                &self.library,
                &self.container_id,
                self.generation,
                asset.asset_record_name(),
                Some(asset.id()),
                "icloud",
            )
            .await
        {
            self.write_failed.store(true, Ordering::Relaxed);
            tracing::warn!(
                asset_id = %asset.id(),
                asset_record_name = %asset.asset_record_name(),
                library = %self.library,
                container_id = %self.container_id,
                generation = self.generation,
                error = %e,
                "Failed to record album membership snapshot row"
            );
        }
    }

    async fn complete_if_clean(&self, result: &mut StreamingResult) {
        if self.write_failed.load(Ordering::Relaxed) {
            result.state_write_failures += 1;
            return;
        }
        if result.enumeration_errors > 0 || !result.enumeration_complete {
            tracing::debug!(
                library = %self.library,
                container_id = %self.container_id,
                generation = self.generation,
                write_failed = self.write_failed.load(Ordering::Relaxed),
                enumeration_errors = result.enumeration_errors,
                enumeration_complete = result.enumeration_complete,
                "Leaving album membership snapshot incomplete"
            );
            return;
        }
        if let Err(e) = self
            .db
            .complete_album_membership_snapshot(&self.library, &self.container_id, self.generation)
            .await
        {
            result.state_write_failures += 1;
            tracing::warn!(
                library = %self.library,
                container_id = %self.container_id,
                generation = self.generation,
                error = %e,
                "Failed to complete album membership snapshot"
            );
        }
    }
}

fn should_record_album_snapshots(
    passes: &[crate::commands::AlbumPass],
    config: &DownloadConfig,
    controls: DownloadControls,
) -> bool {
    !controls.run_mode.is_dry_run()
        && !controls.run_mode.only_print_filenames()
        && config.state_db.is_some()
        && config.recent.is_none()
        && config.skip_created_before.is_none()
        && passes.iter().any(|pass| {
            pass.kind == crate::commands::PassKind::Album && pass.album.container_id().is_some()
        })
}

fn deferred_unfiled_index(passes: &[crate::commands::AlbumPass]) -> Option<usize> {
    let has_album_pass = passes
        .iter()
        .any(|pass| pass.kind == crate::commands::PassKind::Album);
    if !has_album_pass {
        return None;
    }
    passes.iter().position(|pass| {
        pass.kind == crate::commands::PassKind::Unfiled && pass.exclude_ids.is_empty()
    })
}

fn should_use_scope_recent_frontier(passes: &[crate::commands::AlbumPass]) -> bool {
    passes
        .iter()
        .any(|pass| pass.kind != crate::commands::PassKind::Unfiled || !pass.exclude_ids.is_empty())
}

fn should_use_global_recent_frontier(
    passes: &[crate::commands::AlbumPass],
    config: &DownloadConfig,
) -> bool {
    config.recent.is_some()
        && config.recent_scope == crate::cli::RecentScope::Global
        && should_use_scope_recent_frontier(passes)
}

async fn build_recent_frontier(
    passes: &[crate::commands::AlbumPass],
    config: &DownloadConfig,
    controls: DownloadControls,
    shutdown_token: CancellationToken,
) -> Result<Option<RecentFrontier>> {
    let Some(recent) = config.recent else {
        return Ok(None);
    };
    if !should_use_global_recent_frontier(passes, config) {
        return Ok(None);
    }

    let Some(frontier_source) = passes
        .iter()
        .find(|pass| pass.kind == crate::commands::PassKind::Unfiled)
        .map(|pass| pass.album.clone_as_library_wide())
        .or_else(|| {
            passes
                .first()
                .map(|pass| pass.album.clone_as_library_wide())
        })
    else {
        return Ok(None);
    };

    let (stream, _token_rx) = open_photo_stream_for_controls(
        &frontier_source,
        Some(recent),
        None,
        config.concurrent_downloads,
        config.concurrent_downloads,
        controls,
        false,
    );
    tokio::pin!(stream);

    let mut asset_ids = FxHashSet::default();
    let mut oldest_created: Option<DateTime<Utc>> = None;
    while let Some(item) = stream.next().await {
        if shutdown_token.is_cancelled() {
            break;
        }
        let asset = item?;
        let created = asset.created();
        if enumeration_created_lower_bound(config)
            .map(|boundary| created < boundary)
            .unwrap_or(false)
        {
            break;
        }
        oldest_created = Some(oldest_created.map_or(created, |oldest| oldest.min(created)));
        asset_ids.insert(asset.asset_record_name().to_string());
    }
    Ok(Some(RecentFrontier {
        asset_ids: Arc::new(asset_ids),
        oldest_created,
    }))
}

/// Convert a capture-local date lower bound into a conservative UTC
/// enumeration bound.  Capture offsets can move the local midnight by almost
/// a day, so stopping at the unadjusted UTC midnight could omit valid assets.
fn enumeration_created_lower_bound(config: &DownloadConfig) -> Option<DateTime<Utc>> {
    config
        .skip_created_before
        .map(crate::config::CreatedDateFilter::conservative_utc_lower_bound)
}

fn stream_created_lower_bound(
    config: &DownloadConfig,
    frontier: Option<&RecentFrontier>,
) -> Option<DateTime<Utc>> {
    frontier
        .and_then(|frontier| frontier.oldest_created)
        .into_iter()
        .chain(enumeration_created_lower_bound(config))
        .max()
}

fn filter_stream_to_enumeration_bounds(
    stream: DownloadPhotoStream,
    config: &DownloadConfig,
    frontier: Option<&RecentFrontier>,
    bounds_truncated: Arc<AtomicBool>,
) -> DownloadPhotoStream {
    let asset_ids = frontier.map(|frontier| Arc::clone(&frontier.asset_ids));
    let lower_created_bound = stream_created_lower_bound(config, frontier);
    Box::pin(
        stream
            .take_while(move |item| {
                std::future::ready(match item {
                    Ok(asset) => {
                        let in_bounds = lower_created_bound
                            .map(|boundary| asset.created() >= boundary)
                            .unwrap_or(true);
                        if !in_bounds {
                            bounds_truncated.store(true, Ordering::Relaxed);
                        }
                        in_bounds
                    }
                    Err(_) => true,
                })
            })
            .filter_map(move |item| {
                std::future::ready(match item {
                    Ok(asset)
                        if asset_ids
                            .as_ref()
                            .map(|ids| ids.contains(asset.asset_record_name()))
                            .unwrap_or(true) =>
                    {
                        Some(Ok(asset))
                    }
                    Ok(_) => None,
                    Err(e) => Some(Err(e)),
                })
            }),
    )
}

fn scope_frontier_limit(
    config: &DownloadConfig,
    recent_frontier: Option<&RecentFrontier>,
) -> Option<u32> {
    recent_frontier.map_or(config.recent, |_| None)
}

async fn run_full_pass_stream<S>(
    download_client: Client,
    stream: S,
    token_rx: tokio::sync::oneshot::Receiver<Option<String>>,
    pass_config: Arc<DownloadConfig>,
    options: FullPassStreamOptions,
) -> Result<PerPassStreamingResult>
where
    S: futures_util::Stream<Item = anyhow::Result<PhotoAsset>> + Send + 'static,
{
    // Per-album bar: the bar represents only this album's progress,
    // not the cumulative grand total. When the divider is active
    // (multi-pass friendly), the bar plus divider together give the
    // user per-album awareness; the divider's done lines accumulate
    // in scrollback so completed albums don't disappear.
    let pass_start = Instant::now();
    let progress = crate::personality::progress::for_passes(
        options.controls.reporting.no_progress_bar,
        options.controls.run_mode.only_print_filenames(),
        options.count,
        options.controls.reporting.personality_mode,
    );
    let pass_pb = progress.bar;
    let pass_bytes = progress.bytes;

    let snapshot = options.album_snapshot.clone();
    let stream: DownloadPhotoStream = match snapshot {
        Some(recorder) => Box::pin(stream.then(move |item| {
            let recorder = recorder.clone();
            async move {
                if let Ok(asset) = &item {
                    recorder.record_asset(asset).await;
                }
                item
            }
        })),
        None => Box::pin(stream),
    };

    let mut result = stream_and_download_from_stream(
        &download_client,
        stream,
        &pass_config,
        options.controls,
        options.count,
        options.shutdown_token,
        StreamRuntime::with_context(
            Some(pass_pb.clone()),
            Some(std::sync::Arc::clone(&pass_bytes)),
            options.download_ctx,
        )
        .deferring_metadata_drain(),
    )
    .await?;
    if let Some(snapshot) = &options.album_snapshot {
        snapshot.complete_if_clean(&mut result).await;
    }

    let elapsed = pass_start.elapsed();
    pass_pb.finish_and_clear();
    Ok(PerPassStreamingResult {
        pass_index: options.pass_index,
        kind: options.kind,
        label: pass_config.pass_label().to_string(),
        count: options.count,
        elapsed,
        token_rx,
        result,
    })
}

fn merge_streaming_result(combined: &mut StreamingResult, result: StreamingResult) {
    combined.downloaded += result.downloaded;
    combined.exif_failures += result.exif_failures;
    combined.failed.extend(result.failed);
    combined.auth_errors += result.auth_errors;
    combined.state_write_failures += result.state_write_failures;
    combined.enumeration_errors += result.enumeration_errors;
    combined.assets_seen += result.assets_seen;
    combined.skip_summary += result.skip_summary;
    // AND-fold across passes so a single pass aborting (e.g.
    // producer-channel close, panic) leaves the marker set.
    combined.enumeration_complete = combined.enumeration_complete && result.enumeration_complete;
}

fn merge_token_recovery_result(combined: &mut StreamingResult, result: StreamingResult) {
    // A token-recovery pass replays an already observed pass through the real
    // planner and state pipeline. Preserve the original inventory/progress
    // counts rather than double-counting every replayed asset, but retain all
    // transfer, durability, and error outcomes produced by the repair.
    combined.downloaded += result.downloaded;
    combined.exif_failures += result.exif_failures;
    combined.failed.extend(result.failed);
    combined.auth_errors += result.auth_errors;
    combined.state_write_failures += result.state_write_failures;
    combined.enumeration_errors += result.enumeration_errors;
    combined.bytes_downloaded += result.bytes_downloaded;
    combined.disk_bytes_written += result.disk_bytes_written;
    combined.rate_limit_observations += result.rate_limit_observations;
    combined.url_expired_abort = combined.url_expired_abort || result.url_expired_abort;
    combined.photos_downloaded += result.photos_downloaded;
    combined.videos_downloaded += result.videos_downloaded;
    combined.recap.merge(result.recap);
    combined.enumeration_complete = combined.enumeration_complete && result.enumeration_complete;
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct RetryTaskKey {
    asset_id: Arc<str>,
    version_size: VersionSizeKey,
    download_path: std::path::PathBuf,
}

#[derive(Debug, Clone)]
struct UrlRetrySource {
    asset_record_name: Arc<str>,
    pass_index: usize,
}

impl From<&DownloadTask> for RetryTaskKey {
    fn from(task: &DownloadTask) -> Self {
        Self {
            asset_id: Arc::clone(&task.asset_id),
            version_size: task.version_size,
            download_path: task.download_path.clone(),
        }
    }
}

fn retry_state_ids_by_asset_record(tasks: &[DownloadTask]) -> FxHashMap<Arc<str>, Arc<str>> {
    tasks
        .iter()
        .map(|task| {
            (
                Arc::clone(&task.asset_record_name),
                Arc::clone(&task.asset_id),
            )
        })
        .collect()
}

fn retry_hydrator_pass_index(
    passes: &[crate::commands::AlbumPass],
    pass_indices: &FxHashSet<usize>,
) -> Option<usize> {
    pass_indices
        .iter()
        .copied()
        .filter(|pass_index| {
            passes
                .get(*pass_index)
                .is_some_and(|pass| pass.kind != crate::commands::PassKind::Unfiled)
        })
        .min()
        .or_else(|| (!passes.is_empty()).then_some(0))
}

fn take_matching_retry_tasks<I>(
    tasks: I,
    pending_keys: &mut FxHashSet<RetryTaskKey>,
    out: &mut Vec<DownloadTask>,
) where
    I: IntoIterator<Item = DownloadTask>,
{
    for task in tasks {
        let key = RetryTaskKey::from(&task);
        if pending_keys.remove(&key) {
            out.push(task);
            if pending_keys.is_empty() {
                break;
            }
        }
    }
}

#[derive(Debug, Default)]
pub(crate) struct PathReconciliationResult {
    pub(crate) complete: bool,
    pub(crate) stats: SyncStats,
}

async fn requeue_missing_catalog_file(
    db: &dyn DownloadStore,
    task: &DownloadTask,
    stats: &mut SyncStats,
) {
    if let Err(error) = db
        .mark_failed(
            &task.library,
            &task.asset_id,
            task.version_size.as_str(),
            crate::commands::reconcile::FILE_MISSING_REASON,
        )
        .await
    {
        stats.state_write_failures += 1;
        tracing::warn!(asset_id = %task.asset_id, %error, "Path reconciliation could not requeue a missing catalog file");
    }
}

pub(crate) async fn reconcile_catalog_paths(
    passes: &[crate::commands::AlbumPass],
    config: Arc<DownloadConfig>,
    shutdown_token: CancellationToken,
) -> Result<PathReconciliationResult> {
    let Some(db) = &config.state_db else {
        return Ok(PathReconciliationResult::default());
    };
    let Some(provider_pass) = passes.first() else {
        return Ok(PathReconciliationResult::default());
    };

    let mut records = Vec::new();
    let mut offset = 0u64;
    const PAGE_SIZE: u32 = 1_000;
    loop {
        let page = db.get_downloaded_page(offset, PAGE_SIZE).await?;
        let page_len = page.len();
        records.extend(page.into_iter().filter(|record| {
            record.library.as_ref() == config.library.as_ref() && !record.metadata.is_deleted
        }));
        if page_len < PAGE_SIZE as usize {
            break;
        }
        offset = offset.saturating_add(u64::from(PAGE_SIZE));
    }
    if records.is_empty() {
        return Ok(PathReconciliationResult {
            complete: true,
            stats: SyncStats::default(),
        });
    }

    let mut targets: FxHashSet<PendingRetryTarget> = records
        .iter()
        .map(PendingRetryTarget::from_record)
        .collect();
    let mut requests = Vec::new();
    let mut seen_requests = FxHashSet::default();
    let mut master_by_state_id = FxHashMap::default();
    for record in &records {
        let mapped_master = db
            .get_master_record_name_for_asset(&config.library, &record.id)
            .await?;
        let master = mapped_master
            .as_deref()
            .unwrap_or(record.id.as_ref())
            .to_owned();
        let asset_record_names = if mapped_master.is_some() {
            vec![record.id.to_string()]
        } else {
            db.get_asset_record_names_for_master(&config.library, &master)
                .await?
        };
        master_by_state_id.insert(record.id.to_string(), master.clone());
        for asset_record_name in asset_record_names {
            let key = (
                record.id.to_string(),
                master.clone(),
                asset_record_name.clone(),
            );
            if seen_requests.insert(key.clone()) {
                requests.push(RecordLookupRequest::paired(
                    ProviderRecordId::new(key.0),
                    ProviderRecordId::new(key.1),
                    ProviderRecordId::new(key.2),
                ));
            }
        }
    }

    let pass_configs: Vec<Arc<DownloadConfig>> = passes
        .iter()
        .map(|pass| Arc::new(config.with_pass(pass)))
        .collect();
    let records_by_target: FxHashMap<PendingRetryTarget, &crate::state::AssetRecord> = records
        .iter()
        .map(|record| (PendingRetryTarget::from_record(record), record))
        .collect();
    let mut albums_by_asset: FxHashMap<String, FxHashSet<String>> = FxHashMap::default();
    for (asset_id, album_name) in db.get_all_asset_albums(&config.library).await? {
        albums_by_asset
            .entry(asset_id)
            .or_default()
            .insert(album_name);
    }
    let album_container_ids: Vec<String> = passes
        .iter()
        .filter(|pass| pass.kind == crate::commands::PassKind::Album)
        .filter_map(|pass| pass.album.container_id().map(ToOwned::to_owned))
        .collect();
    let album_pass_count = passes
        .iter()
        .filter(|pass| pass.kind == crate::commands::PassKind::Album)
        .count();
    let album_container_refs: Vec<&str> = album_container_ids.iter().map(String::as_str).collect();
    let album_membership_complete = album_container_ids.len() == album_pass_count
        && (album_container_refs.is_empty()
            || db
                .selected_album_containers_have_complete_snapshots(
                    &config.library,
                    &album_container_refs,
                )
                .await?);
    let selection_complete = album_membership_complete
        && !passes
            .iter()
            .any(|pass| pass.kind == crate::commands::PassKind::SmartFolder);
    let batch = provider_pass.album.resolve_records(&requests).await;
    let mut task_planner = planner::TaskPlanner::new();
    let mut tasks = Vec::new();
    let mut task_keys = FxHashSet::default();
    for (state_id, resolution) in batch.results {
        if shutdown_token.is_cancelled() {
            break;
        }
        match resolution {
            RecordResolution::Present(asset) => {
                targets.retain(|target| target.asset_id.as_ref() != state_id.as_str());
                let known_albums = albums_by_asset.get(state_id.as_str());
                for (pass, pass_config) in passes.iter().zip(&pass_configs) {
                    let selected = match pass.kind {
                        crate::commands::PassKind::Album => known_albums
                            .is_some_and(|albums| albums.contains(pass.album.name.as_ref())),
                        crate::commands::PassKind::Unfiled => {
                            known_albums.is_none_or(FxHashSet::is_empty)
                        }
                        crate::commands::PassKind::SmartFolder => false,
                    };
                    if !selected {
                        continue;
                    }
                    let plan = task_planner.plan_asset(&asset, pass_config).await;
                    if plan.filter_reason.is_some() {
                        continue;
                    }
                    for task in plan.tasks {
                        let target = PendingRetryTarget::from_task(&task);
                        if let Some(record) = records_by_target.get(&target)
                            && record.checksum.as_ref() == task.checksum.as_ref()
                            && record.size_bytes == task.size
                            && let Some(path) = record.local_path.clone()
                        {
                            let recorded_file = RecordedLocalFile {
                                path,
                                local_checksum: record
                                    .local_checksum
                                    .clone()
                                    .map(String::into_boxed_str),
                                download_checksum: record
                                    .download_checksum
                                    .clone()
                                    .map(String::into_boxed_str),
                            };
                            if pipeline::recorded_current_path_exists(
                                pass_config,
                                &asset,
                                task.version_size,
                                &mut task_planner,
                                &recorded_file,
                            )
                            .await
                            .is_some()
                            {
                                continue;
                            }
                        }
                        let key = RetryTaskKey::from(&task);
                        if task_keys.insert(key) {
                            tasks.push(task);
                        }
                    }
                }
            }
            RecordResolution::Deleted {
                deleted_at,
                master_family,
            } => {
                let id = state_id.as_str();
                if master_family {
                    let master = master_by_state_id.get(id).map(String::as_str).unwrap_or(id);
                    db.resolve_master_family_source_deleted_affected(
                        &config.library,
                        master,
                        deleted_at,
                    )
                    .await?;
                } else {
                    db.resolve_source_deleted_affected(&config.library, id, deleted_at)
                        .await?;
                }
                targets.retain(|target| target.asset_id.as_ref() != id);
            }
            RecordResolution::AssetPresent { .. }
            | RecordResolution::MasterPresent
            | RecordResolution::Unknown
            | RecordResolution::TransientFailure(_) => {}
        }
    }

    let mut stats = SyncStats::default();
    let mut deferred_to_pending_retry = false;
    for task in tasks {
        let target = PendingRetryTarget::from_task(&task);
        let Some(record) = records_by_target.get(&target) else {
            tracing::debug!(
                version_size = task.version_size.as_str(),
                "Path reconciliation left newly selected version to normal download"
            );
            continue;
        };
        let Some(source_path) = record.local_path.as_deref() else {
            deferred_to_pending_retry = true;
            requeue_missing_catalog_file(db.as_ref(), &task, &mut stats).await;
            tracing::debug!(asset_id = %task.asset_id, "Path reconciliation deferred catalog row without a local file to targeted retry");
            continue;
        };
        match tokio::fs::try_exists(source_path).await {
            Ok(true) => {}
            Ok(false) => {
                deferred_to_pending_retry = true;
                requeue_missing_catalog_file(db.as_ref(), &task, &mut stats).await;
                tracing::debug!(asset_id = %task.asset_id, path = %source_path.display(), "Path reconciliation deferred missing local file to targeted retry");
                continue;
            }
            Err(error) => {
                stats.failed += 1;
                tracing::warn!(asset_id = %task.asset_id, path = %source_path.display(), %error, "Path reconciliation could not inspect the catalog file");
                continue;
            }
        }
        match file::copy_local_file_no_replace(
            source_path,
            &task.download_path,
            &config.temp_suffix,
        )
        .await
        {
            Ok(Some(local_checksum)) => {
                if let Err(error) = db
                    .mark_downloaded(
                        &task.library,
                        &task.asset_id,
                        task.version_size.as_str(),
                        &task.download_path,
                        &local_checksum,
                        None,
                    )
                    .await
                {
                    stats.state_write_failures += 1;
                    tracing::warn!(asset_id = %task.asset_id, %error, "Failed to persist reconciled local path");
                } else {
                    stats.downloaded += 1;
                    if matches!(
                        task.media_type,
                        crate::state::MediaType::Photo | crate::state::MediaType::LivePhotoImage
                    ) {
                        stats.photos_downloaded += 1;
                    } else {
                        stats.videos_downloaded += 1;
                    }
                    stats.disk_bytes_written =
                        stats.disk_bytes_written.saturating_add(record.size_bytes);
                }
            }
            Ok(None) => {
                stats.failed += 1;
                tracing::warn!(asset_id = %task.asset_id, path = %task.download_path.display(), "Path reconciliation found conflicting destination bytes");
            }
            Err(error) => {
                stats.failed += 1;
                tracing::warn!(asset_id = %task.asset_id, %error, "Failed to copy existing media into reconciled path");
            }
        }
    }
    stats.interrupted = shutdown_token.is_cancelled();
    let complete = batch.complete
        && selection_complete
        && targets.is_empty()
        && !deferred_to_pending_retry
        && stats.failed == 0
        && stats.state_write_failures == 0
        && !stats.interrupted;
    Ok(PathReconciliationResult { complete, stats })
}

/// Re-enumerate iCloud and rebuild only the failed tasks with fresh CDN URLs.
///
/// The first pass may fail because signed content URLs expired before the
/// worker reached them. Retrying the complete library after that is both slow
/// and risky: newly-issued URLs for early tasks can age again while unrelated
/// albums are planned. Limit cleanup to the exact asset/version/path tuples
/// that failed so the retry pass starts consuming refreshed URLs quickly.
async fn build_retry_download_tasks(
    passes: &[crate::commands::AlbumPass],
    config: &DownloadConfig,
    failed_tasks: &[DownloadTask],
    shutdown_token: CancellationToken,
) -> Result<Vec<DownloadTask>> {
    if failed_tasks.is_empty() {
        return Ok(Vec::new());
    }

    let mut pending_keys: FxHashSet<RetryTaskKey> =
        failed_tasks.iter().map(RetryTaskKey::from).collect();
    let retry_state_ids = retry_state_ids_by_asset_record(failed_tasks);
    let requested_count = pending_keys.len();
    let pass_configs = build_pass_configs_resolving_deferred_excludes(passes, config).await?;
    let mut tasks: Vec<DownloadTask> = Vec::with_capacity(requested_count);
    let mut task_planner = planner::TaskPlanner::new();

    for (pass_index, pass) in passes.iter().enumerate() {
        if pending_keys.is_empty() || shutdown_token.is_cancelled() {
            break;
        }

        let assets = pass.album.photos(config.recent).await?;
        #[allow(
            clippy::indexing_slicing,
            reason = "pass_index comes from enumerate() over `passes`; pass_configs is \
                      built 1:1 from the same slice"
        )]
        let pass_config = &pass_configs[pass_index];

        for asset in &assets {
            if pending_keys.is_empty() || shutdown_token.is_cancelled() {
                break;
            }
            let Some(state_id) = retry_state_ids.get(asset.asset_record_name()) else {
                continue;
            };
            let asset = asset.clone().with_state_record_name(Arc::clone(state_id));
            let plan = task_planner.plan_asset(&asset, pass_config).await;
            if plan.filter_reason.is_some() {
                continue;
            }
            take_matching_retry_tasks(plan.tasks, &mut pending_keys, &mut tasks);
        }
    }

    if !pending_keys.is_empty() {
        tracing::warn!(
            requested = requested_count,
            refreshed = tasks.len(),
            missing = pending_keys.len(),
            "Cleanup pass could not refresh every failed task; unmatched failures remain pending"
        );
    }

    Ok(tasks)
}

/// Hydrate current incremental asset records and rebuild only the task tuples
/// that actually failed with expired CDN URLs.
///
/// Replaying the same token-bounded `/changes/zone` delta can return the same
/// signed URLs that just aged out. Hydration scans the current zone state
/// without that old sync token so the retry pass starts from newly-issued URLs
/// instead of immediately retrying the stale batch.
async fn build_incremental_expired_url_retry_tasks(
    passes: &[crate::commands::AlbumPass],
    pass_configs: &[Arc<DownloadConfig>],
    retry_sources: &FxHashMap<RetryTaskKey, UrlRetrySource>,
    failed_tasks: &[DownloadTask],
    shutdown_token: CancellationToken,
) -> Result<Vec<DownloadTask>> {
    if failed_tasks.is_empty() {
        return Ok(Vec::new());
    }

    let mut pending_keys: FxHashSet<RetryTaskKey> =
        failed_tasks.iter().map(RetryTaskKey::from).collect();
    let retry_state_ids = retry_state_ids_by_asset_record(failed_tasks);
    let requested_count = pending_keys.len();
    let mut pass_indices_by_asset: FxHashMap<String, FxHashSet<usize>> = FxHashMap::default();

    for task in failed_tasks {
        let key = RetryTaskKey::from(task);
        let Some(source) = retry_sources.get(&key) else {
            tracing::warn!(
                asset_id = %task.asset_id,
                version_size = %task.version_size.as_str(),
                path = %task.download_path.display(),
                "Could not map expired incremental URL back to its source asset"
            );
            continue;
        };
        pass_indices_by_asset
            .entry(source.asset_record_name.to_string())
            .or_default()
            .insert(source.pass_index);
    }

    let mut tasks = Vec::with_capacity(requested_count);
    let mut task_planner = planner::TaskPlanner::new();
    if !pending_keys.is_empty()
        && !pass_indices_by_asset.is_empty()
        && !shutdown_token.is_cancelled()
    {
        let mut missing_by_hydrator: FxHashMap<usize, FxHashSet<String>> = FxHashMap::default();
        for (asset_record_name, pass_indices) in &pass_indices_by_asset {
            let Some(pass_index) = retry_hydrator_pass_index(passes, pass_indices) else {
                continue;
            };
            missing_by_hydrator
                .entry(pass_index)
                .or_default()
                .insert(asset_record_name.clone());
        }

        let mut hydrated_asset_record_names = FxHashSet::default();
        for (pass_index, mut missing) in missing_by_hydrator {
            missing.retain(|asset_record_name| {
                !hydrated_asset_record_names.contains(asset_record_name.as_str())
            });
            if missing.is_empty() || pending_keys.is_empty() || shutdown_token.is_cancelled() {
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
                        "Failed to hydrate expired incremental retry assets"
                    );
                    continue;
                }
            };

            for asset in assets {
                let asset_record_name = asset.asset_record_name().to_string();
                hydrated_asset_record_names.insert(asset_record_name.clone());
                let Some(pass_indices) = pass_indices_by_asset.remove(asset_record_name.as_str())
                else {
                    continue;
                };
                let Some(state_id) = retry_state_ids.get(asset_record_name.as_str()) else {
                    continue;
                };
                let asset = asset.with_state_record_name(Arc::clone(state_id));

                for pass_index in pass_indices {
                    let Some(pass_config) = pass_configs.get(pass_index) else {
                        continue;
                    };
                    let plan = task_planner.plan_asset(&asset, pass_config).await;
                    if plan.filter_reason.is_some() {
                        continue;
                    }
                    take_matching_retry_tasks(plan.tasks, &mut pending_keys, &mut tasks);
                    if pending_keys.is_empty() || shutdown_token.is_cancelled() {
                        break;
                    }
                }
            }
        }
    }

    if !pending_keys.is_empty() {
        tracing::warn!(
            requested = requested_count,
            refreshed = tasks.len(),
            missing = pending_keys.len(),
            "Incremental expired-URL retry could not refresh every failed task"
        );
    }

    Ok(tasks)
}

fn merge_expired_url_retry_result(
    pass_result: &mut PassResult,
    expired_retry_candidates: Vec<DownloadTask>,
    refreshed_keys: FxHashSet<RetryTaskKey>,
    retry_result: PassResult,
) {
    let mut still_failed: Vec<DownloadTask> = expired_retry_candidates
        .into_iter()
        .filter(|task| !refreshed_keys.contains(&RetryTaskKey::from(task)))
        .collect();
    let unrefreshed_expired_failures = still_failed.len();
    still_failed.extend(retry_result.failed);
    pass_result.failed = still_failed;
    pass_result.downloaded += retry_result.downloaded;
    pass_result
        .downloaded_tasks
        .extend(retry_result.downloaded_tasks);
    pass_result.auth_errors += retry_result.auth_errors;
    pass_result.exif_failures += retry_result.exif_failures;
    pass_result.state_write_failures += retry_result.state_write_failures;
    pass_result.bytes_downloaded += retry_result.bytes_downloaded;
    pass_result.disk_bytes_written += retry_result.disk_bytes_written;
    pass_result.rate_limit_observations += retry_result.rate_limit_observations;
    pass_result.photos_downloaded += retry_result.photos_downloaded;
    pass_result.videos_downloaded += retry_result.videos_downloaded;
    pass_result.recap.merge(retry_result.recap);
    pass_result.url_expired_abort =
        retry_result.url_expired_abort || unrefreshed_expired_failures > 0;
}

async fn refresh_stale_incremental_tasks_before_download(
    passes: &[crate::commands::AlbumPass],
    pass_configs: &[Arc<DownloadConfig>],
    retry_sources: &FxHashMap<RetryTaskKey, UrlRetrySource>,
    tasks: Vec<DownloadTask>,
    urls_obtained_at: Option<Instant>,
    refresh_after: Duration,
    shutdown_token: CancellationToken,
) -> Vec<DownloadTask> {
    if tasks.is_empty() || shutdown_token.is_cancelled() {
        return tasks;
    }
    let Some(urls_obtained_at) = urls_obtained_at else {
        return tasks;
    };
    let url_age = urls_obtained_at.elapsed();
    if url_age < refresh_after {
        return tasks;
    }

    let requested = tasks.len();
    tracing::info!(
        requested,
        url_age_secs = url_age.as_secs_f64(),
        threshold_secs = refresh_after.as_secs_f64(),
        "Refreshing incremental download URLs before starting downloads"
    );

    let refreshed_tasks = match build_incremental_expired_url_retry_tasks(
        passes,
        pass_configs,
        retry_sources,
        &tasks,
        shutdown_token,
    )
    .await
    {
        Ok(tasks) => tasks,
        Err(e) => {
            tracing::warn!(
                requested,
                error = %e,
                "Could not refresh incremental download URLs before starting downloads; using original URLs"
            );
            return tasks;
        }
    };

    if refreshed_tasks.is_empty() {
        tracing::warn!(
            requested,
            "Pre-download incremental URL refresh returned no tasks; using original URLs"
        );
        return tasks;
    }

    let refreshed_count = refreshed_tasks.len();
    let mut refreshed_by_key: FxHashMap<RetryTaskKey, DownloadTask> = refreshed_tasks
        .into_iter()
        .map(|task| (RetryTaskKey::from(&task), task))
        .collect();
    let mut ordered_tasks = Vec::with_capacity(requested);
    let mut unrefreshed = 0usize;
    for task in tasks {
        let key = RetryTaskKey::from(&task);
        if let Some(refreshed_task) = refreshed_by_key.remove(&key) {
            ordered_tasks.push(refreshed_task);
        } else {
            unrefreshed += 1;
            ordered_tasks.push(task);
        }
    }

    if unrefreshed > 0 {
        tracing::warn!(
            requested,
            refreshed = refreshed_count,
            unrefreshed,
            "Pre-download incremental URL refresh could not refresh every task; preserving original URLs for the rest"
        );
    } else {
        tracing::info!(
            requested,
            refreshed = refreshed_count,
            "Pre-download incremental URL refresh completed"
        );
    }
    ordered_tasks
}

/// Download photos with syncToken support.
///
/// In `SyncMode::Full`: runs the existing full enumeration via
/// `photo_stream_with_token`, captures the syncToken after the stream is
/// consumed, and delegates download logic to the existing pipeline.
///
/// In `SyncMode::Incremental`: uses `changes_stream` for delta sync,
/// filters `ChangeEvent`s to downloadable assets, and feeds them through
/// the existing download pipeline. Falls back to `SyncMode::Full` if the
/// token is invalid or expired.
/// Minimum untouched age before cleanup removes a durably owned temporary
/// file. A second kei process can share the download root while using another
/// state database, so recent writes remain protected even when this database
/// has older completed-sync evidence.
const PART_FILE_RECENT_GRACE_SECS: i64 = 10 * 60;

#[derive(Debug, Default)]
struct OwnedTempCleanup {
    removed: usize,
    retire: Vec<PathBuf>,
}

/// Remove only stale exact paths from kei's durable temporary-file ownership
/// ledger. No suffix match can authorize deletion, and the removal stays
/// relative to a verified directory or file handle.
fn remove_owned_orphan_parts(
    root: &Path,
    owned: &[crate::state::OwnedTempFile],
    cutoff_secs: i64,
    now_secs: i64,
    recent_grace_secs: i64,
) -> OwnedTempCleanup {
    remove_owned_orphan_parts_with(
        root,
        owned,
        cutoff_secs,
        now_secs,
        recent_grace_secs,
        |_| {},
    )
}

fn remove_owned_orphan_parts_with<F>(
    root: &Path,
    owned: &[crate::state::OwnedTempFile],
    cutoff_secs: i64,
    now_secs: i64,
    recent_grace_secs: i64,
    mut before_remove: F,
) -> OwnedTempCleanup
where
    F: FnMut(&Path),
{
    use crate::fs_util::ConfinedFileOpen;

    let mut cleanup = OwnedTempCleanup::default();
    for record in owned {
        debug_assert!(record.claimed_at < cutoff_secs);
        let candidate = match crate::fs_util::open_confined_regular_file(root, &record.path) {
            Ok(candidate) => candidate,
            Err(error) => {
                tracing::warn!(
                    path = %record.path.display(),
                    error = %error,
                    "Could not safely inspect owned temporary file during cleanup"
                );
                continue;
            }
        };
        match candidate {
            ConfinedFileOpen::OutsideRoot => continue,
            ConfinedFileOpen::Retire => {
                cleanup.retire.push(record.path.clone());
            }
            ConfinedFileOpen::Regular(file) => {
                let mtime_secs = file.modified_secs();
                // A path modified after the completed-sync cutoff no longer
                // matches the stale claim. Retire the claim without touching
                // the file so it can never authorize a later deletion.
                if mtime_secs >= cutoff_secs {
                    cleanup.retire.push(record.path.clone());
                    continue;
                }
                let is_recently_touched = recent_grace_secs > 0
                    && mtime_secs > now_secs.saturating_sub(recent_grace_secs);
                if is_recently_touched {
                    continue;
                }
                before_remove(&record.path);
                match file.remove() {
                    Ok(()) => {
                        cleanup.removed += 1;
                        cleanup.retire.push(record.path.clone());
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        cleanup.retire.push(record.path.clone());
                    }
                    Err(error) => tracing::warn!(
                        path = %record.path.display(),
                        error = %error,
                        "Failed to remove owned orphan temporary file"
                    ),
                }
            }
        }
    }
    cleanup
}

/// Remove orphaned `.part` files from the download directory.
///
/// Loads stale exact-path ownership evidence from SQLite, rejects symlinked
/// paths, and removes only owned files older than the last completed sync.
/// CONTRACT: TEMP_FILE_DELETE_REQUIRES_DURABLE_OWNERSHIP
async fn cleanup_orphan_part_files(config: &DownloadConfig) {
    let Some(db) = &config.state_db else { return };
    let cutoff = match db.get_summary().await {
        Ok(summary) => match summary.last_sync_completed {
            Some(ts) => ts,
            None => return, // No prior sync — nothing is orphaned
        },
        Err(e) => {
            tracing::debug!(error = %e, "Could not query last sync time for .part cleanup");
            return;
        }
    };

    let cutoff_secs = cutoff.timestamp();
    let owned = match db.get_owned_temp_files_before(cutoff_secs).await {
        Ok(owned) if owned.is_empty() => return,
        Ok(owned) => owned,
        Err(error) => {
            tracing::warn!(error = %error, "Could not load temporary-file ownership for cleanup");
            return;
        }
    };
    let dir = match crate::fs_util::absolute_lexical(&config.directory) {
        Ok(dir) => dir,
        Err(error) => {
            tracing::warn!(
                path = %config.directory.display(),
                error = %error,
                "Could not resolve download root for owned temporary-file cleanup"
            );
            return;
        }
    };
    let now_secs = chrono::Utc::now().timestamp();

    let cleanup = tokio::task::spawn_blocking(move || {
        remove_owned_orphan_parts(
            &dir,
            &owned,
            cutoff_secs,
            now_secs,
            PART_FILE_RECENT_GRACE_SECS,
        )
    })
    .await
    .unwrap_or_default();

    if let Err(error) = db.retire_temp_files(&cleanup.retire).await {
        tracing::warn!(error = %error, "Could not retire temporary-file ownership after cleanup");
    }

    if cleanup.removed > 0 {
        tracing::info!(
            count = cleanup.removed,
            "Cleaned up owned orphan temporary files"
        );
    }
}

/// Drain pending metadata-rewrite markers in bounded batches so a
/// `--refresh-metadata` migration finishes in its own run. Returns the number
/// of markers left failing when the drain stops.
pub(crate) async fn drain_pending_metadata_rewrites(
    db: &dyn DownloadStore,
    metadata: &crate::config::MetadataConfig,
    capture_timestamp_repair: CaptureTimestampRepair,
    library_scope: &[&str],
    temp_suffix: Arc<str>,
    shutdown_token: &CancellationToken,
) -> usize {
    let flags = MetadataFlags::from(metadata);
    if !flags.has_any_write() {
        return 0;
    }
    let mut offset = 0usize;
    let mut failed = 0usize;
    loop {
        let pass = metadata_rewrite::run_pending_page(
            db,
            flags,
            capture_timestamp_repair,
            Arc::clone(&temp_suffix),
            shutdown_token,
            Some(library_scope),
            offset,
        )
        .await;
        failed = failed.saturating_add(pass.failed);
        if pass.fetched == 0 {
            return failed;
        }
        tracing::debug!(
            applied = pass.applied,
            retired = pass.retired_from_selected_queue,
            failed = pass.failed,
            "Metadata rewrite batch completed"
        );
        if shutdown_token.is_cancelled() {
            return failed.max(1);
        }
        offset = offset.saturating_add(
            pass.fetched
                .saturating_sub(pass.retired_from_selected_queue),
        );
    }
}

async fn has_metadata_backfill_work(config: &DownloadConfig) -> bool {
    let Some(db) = &config.state_db else {
        return false;
    };
    match db.has_downloaded_without_metadata_hash().await {
        Ok(needs_backfill) => needs_backfill,
        Err(e) => {
            tracing::warn!(
                error = %e,
                "Failed to check metadata backfill state before incremental sync"
            );
            false
        }
    }
}

const METADATA_CAPTURE_BATCH: usize = 500;

#[derive(Default)]
struct MetadataCaptureRepair {
    stats: SyncStats,
    failures: usize,
}

fn metadata_capture_candidate_matches(
    asset: &PhotoAsset,
    candidate: &crate::state::MetadataCaptureCandidate,
) -> bool {
    candidate.versions.iter().any(|evidence| {
        asset.versions().iter().any(|(version_size, version)| {
            VersionSizeKey::from(*version_size) == evidence.version_size
                && version.size == evidence.size_bytes
                && version.checksum.as_ref() == evidence.checksum
        })
    })
}

async fn record_metadata_capture_failure(
    db: &dyn DownloadStore,
    library: &str,
    error: &str,
    repair: &mut MetadataCaptureRepair,
) {
    repair.failures = repair.failures.saturating_add(1);
    repair.stats.metadata_capture_failures =
        repair.stats.metadata_capture_failures.saturating_add(1);
    if let Err(state_error) = db
        .record_metadata_capture_failure(library, crate::state::METADATA_CAPTURE_REVISION, error)
        .await
    {
        repair.stats.state_write_failures = repair.stats.state_write_failures.saturating_add(1);
        tracing::warn!(error = %state_error, "Failed to persist metadata-capture repair failure");
    }
}

async fn refresh_metadata_capture_candidate(
    db: &dyn DownloadStore,
    config: &DownloadConfig,
    candidate: &crate::state::MetadataCaptureCandidate,
    asset: PhotoAsset,
    repair: &mut MetadataCaptureRepair,
) {
    if let Err(error) = db
        .upsert_asset_master_mapping(&candidate.library, asset.asset_record_name(), asset.id())
        .await
    {
        let message = error.to_string();
        record_metadata_capture_failure(db, &candidate.library, &message, repair).await;
        return;
    }
    let mark_for_rewrite = MetadataFlags::from(config).has_any_write();
    match db
        .refresh_downloaded_asset_metadata(
            &candidate.library,
            &candidate.asset_id,
            asset.metadata(),
            mark_for_rewrite,
            false,
            crate::state::METADATA_CAPTURE_REVISION,
        )
        .await
    {
        Ok(updated) if updated > 0 => {
            repair.stats.metadata_capture_refreshed =
                repair.stats.metadata_capture_refreshed.saturating_add(1);
        }
        Ok(_) => {
            record_metadata_capture_failure(
                db,
                &candidate.library,
                "provider metadata matched no live downloaded catalogue row",
                repair,
            )
            .await;
        }
        Err(error) => {
            let message = error.to_string();
            record_metadata_capture_failure(db, &candidate.library, &message, repair).await;
        }
    }
}

async fn run_metadata_capture_repair(
    passes: &[crate::commands::AlbumPass],
    config: &DownloadConfig,
    controls: DownloadControls,
    shutdown_token: &CancellationToken,
) -> MetadataCaptureRepair {
    // CONTRACT: METADATA_CAPTURE_REVISION_REPAIR_IS_DURABLE
    let mut repair = MetadataCaptureRepair::default();
    repair.stats.metadata_capture_revision = Some(crate::state::METADATA_CAPTURE_REVISION);
    if !controls.run_mode.downloads_files() || config.refresh_metadata {
        return repair;
    }
    let Some(db) = &config.state_db else {
        return repair;
    };
    let library = config.library.as_ref();
    let initial = match db
        .begin_metadata_capture_revision(library, crate::state::METADATA_CAPTURE_REVISION)
        .await
    {
        Ok(status) => status,
        Err(error) => {
            repair.failures = 1;
            repair.stats.metadata_capture_failures = 1;
            repair.stats.state_write_failures = 1;
            tracing::warn!(error = %error, library, "Could not initialize metadata-capture repair");
            block_sync_token_for_incremental_delta(
                &mut repair.stats,
                METADATA_CAPTURE_REPAIR_FAILED_REASON,
            );
            return repair;
        }
    };
    repair.stats.metadata_capture_remaining = initial.remaining_assets;
    if initial.remaining_assets == 0 {
        return repair;
    }
    let Some(provider_pass) = passes.first() else {
        record_metadata_capture_failure(
            db.as_ref(),
            library,
            "selected library has no provider pass for metadata hydration",
            &mut repair,
        )
        .await;
        block_sync_token_for_incremental_delta(
            &mut repair.stats,
            METADATA_CAPTURE_REPAIR_FAILED_REASON,
        );
        return repair;
    };
    let candidates = match db
        .get_metadata_capture_candidates(
            library,
            crate::state::METADATA_CAPTURE_REVISION,
            METADATA_CAPTURE_BATCH,
        )
        .await
    {
        Ok(candidates) => candidates,
        Err(error) => {
            let message = error.to_string();
            record_metadata_capture_failure(db.as_ref(), library, &message, &mut repair).await;
            block_sync_token_for_incremental_delta(
                &mut repair.stats,
                METADATA_CAPTURE_REPAIR_FAILED_REASON,
            );
            return repair;
        }
    };
    let mut candidates_by_id: FxHashMap<String, crate::state::MetadataCaptureCandidate> =
        candidates
            .into_iter()
            .map(|candidate| (candidate.asset_id.clone(), candidate))
            .collect();
    let requests: Vec<RecordLookupRequest> = candidates_by_id
        .values()
        .map(|candidate| match &candidate.asset_record_name {
            Some(asset_record_name) => RecordLookupRequest::paired(
                ProviderRecordId::new(candidate.asset_id.as_str()),
                ProviderRecordId::new(candidate.master_record_name.as_str()),
                ProviderRecordId::new(asset_record_name.as_str()),
            ),
            None => RecordLookupRequest::master_only(
                ProviderRecordId::new(candidate.asset_id.as_str()),
                ProviderRecordId::new(candidate.master_record_name.as_str()),
            ),
        })
        .collect();
    let resolutions = provider_pass.album.resolve_records(&requests).await;
    repair.stats.rate_limited = repair
        .stats
        .rate_limited
        .saturating_add(resolutions.rate_limit_observations);
    let mut legacy_masters = FxHashSet::default();
    for (state_id, resolution) in resolutions.results {
        if shutdown_token.is_cancelled() {
            break;
        }
        let Some(candidate) = candidates_by_id.remove(state_id.as_str()) else {
            continue;
        };
        match resolution {
            RecordResolution::Present(asset) => {
                refresh_metadata_capture_candidate(
                    db.as_ref(),
                    config,
                    &candidate,
                    asset,
                    &mut repair,
                )
                .await;
            }
            RecordResolution::MasterPresent => {
                legacy_masters.insert(candidate.master_record_name.clone());
                candidates_by_id.insert(candidate.asset_id.clone(), candidate);
            }
            RecordResolution::Deleted {
                deleted_at,
                master_family,
            } => {
                let result = if master_family {
                    db.resolve_master_family_source_deleted_affected(
                        &candidate.library,
                        &candidate.master_record_name,
                        deleted_at,
                    )
                    .await
                } else {
                    db.resolve_source_deleted_affected(
                        &candidate.library,
                        &candidate.asset_id,
                        deleted_at,
                    )
                    .await
                };
                match result {
                    Ok(_) => {}
                    Err(error) => {
                        let message = error.to_string();
                        record_metadata_capture_failure(
                            db.as_ref(),
                            &candidate.library,
                            &message,
                            &mut repair,
                        )
                        .await;
                    }
                }
            }
            RecordResolution::AssetPresent { .. } | RecordResolution::Unknown => {
                record_metadata_capture_failure(
                    db.as_ref(),
                    &candidate.library,
                    "provider lookup did not return a complete photo record",
                    &mut repair,
                )
                .await;
            }
            RecordResolution::TransientFailure(error) => {
                let message = error.to_string();
                record_metadata_capture_failure(
                    db.as_ref(),
                    &candidate.library,
                    &message,
                    &mut repair,
                )
                .await;
            }
        }
    }

    if !legacy_masters.is_empty() && !shutdown_token.is_cancelled() {
        match provider_pass
            .album
            .hydrate_matching_master_assets_from_changes(&legacy_masters, shutdown_token)
            .await
        {
            Ok(assets) => {
                let mut assets_by_master: FxHashMap<String, Vec<PhotoAsset>> = FxHashMap::default();
                for asset in assets {
                    assets_by_master
                        .entry(asset.id().to_owned())
                        .or_default()
                        .push(asset);
                }
                let legacy_ids: Vec<String> = candidates_by_id
                    .iter()
                    .filter(|(_, candidate)| legacy_masters.contains(&candidate.master_record_name))
                    .map(|(state_id, _)| state_id.clone())
                    .collect();
                for state_id in legacy_ids {
                    let Some(candidate) = candidates_by_id.remove(&state_id) else {
                        continue;
                    };
                    let mut matches = assets_by_master
                        .remove(&candidate.master_record_name)
                        .unwrap_or_default()
                        .into_iter()
                        .filter(|asset| metadata_capture_candidate_matches(asset, &candidate));
                    let Some(asset) = matches.next() else {
                        record_metadata_capture_failure(
                            db.as_ref(),
                            &candidate.library,
                            "no current provider child matched durable catalogue evidence",
                            &mut repair,
                        )
                        .await;
                        continue;
                    };
                    if matches.next().is_some() {
                        record_metadata_capture_failure(
                            db.as_ref(),
                            &candidate.library,
                            "multiple provider children matched durable catalogue evidence",
                            &mut repair,
                        )
                        .await;
                        continue;
                    }
                    match db
                        .claim_legacy_master_state_owner(
                            &candidate.library,
                            &candidate.master_record_name,
                            asset.asset_record_name(),
                        )
                        .await
                    {
                        Ok(true) => {
                            let asset = asset.with_state_record_name(Arc::from(state_id));
                            refresh_metadata_capture_candidate(
                                db.as_ref(),
                                config,
                                &candidate,
                                asset,
                                &mut repair,
                            )
                            .await;
                        }
                        Ok(false) => {
                            record_metadata_capture_failure(
                                db.as_ref(),
                                &candidate.library,
                                "a different provider child owns the legacy catalogue row",
                                &mut repair,
                            )
                            .await;
                        }
                        Err(error) => {
                            let message = error.to_string();
                            record_metadata_capture_failure(
                                db.as_ref(),
                                &candidate.library,
                                &message,
                                &mut repair,
                            )
                            .await;
                        }
                    }
                }
            }
            Err(error) => {
                let message = error.to_string();
                let unresolved: Vec<String> = candidates_by_id
                    .iter()
                    .filter(|(_, candidate)| legacy_masters.contains(&candidate.master_record_name))
                    .map(|(state_id, _)| state_id.clone())
                    .collect();
                for state_id in unresolved {
                    if let Some(candidate) = candidates_by_id.remove(&state_id) {
                        record_metadata_capture_failure(
                            db.as_ref(),
                            &candidate.library,
                            &message,
                            &mut repair,
                        )
                        .await;
                    }
                }
            }
        }
    }

    if shutdown_token.is_cancelled() {
        repair.stats.interrupted = true;
    } else {
        for (_, candidate) in candidates_by_id {
            record_metadata_capture_failure(
                db.as_ref(),
                &candidate.library,
                "provider lookup omitted the requested catalogue identity",
                &mut repair,
            )
            .await;
        }
    }
    match db
        .complete_metadata_capture_revision(library, crate::state::METADATA_CAPTURE_REVISION)
        .await
    {
        Ok(status) => {
            repair.stats.metadata_capture_progressed =
                status.remaining_assets < initial.remaining_assets;
            repair.stats.metadata_capture_remaining = status.remaining_assets;
        }
        Err(error) => {
            repair.stats.state_write_failures = repair.stats.state_write_failures.saturating_add(1);
            repair.failures = repair.failures.saturating_add(1);
            repair.stats.metadata_capture_failures =
                repair.stats.metadata_capture_failures.saturating_add(1);
            tracing::warn!(error = %error, library, "Could not finalize metadata-capture repair state");
        }
    }
    if repair.failures > 0 || repair.stats.interrupted {
        block_sync_token_for_incremental_delta(
            &mut repair.stats,
            METADATA_CAPTURE_REPAIR_FAILED_REASON,
        );
    }
    repair
}

fn set_full_enumeration_reason(result: &mut SyncResult, reason: FullEnumerationReason) {
    if result.full_enumeration_ran && result.stats.full_enumeration_reason.is_none() {
        result.stats.full_enumeration_reason = Some(reason);
    }
}

fn block_sync_token_for_incremental_delta(stats: &mut SyncStats, reason: &'static str) {
    if stats.sync_token_blocked_reason.is_none() {
        stats.sync_token_blocked_reason = Some(reason);
        stats.sync_token_blocked_source = Some(sync_token_blocked_source(reason));
        stats.sync_token_blocked_explanation = Some(sync_token_blocked_explanation(reason));
    }
    stats.sync_token_blocked = true;
}

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

async fn backfill_asset_master_mappings_from_album_history(db: &dyn DownloadStore) {
    match db
        .backfill_asset_master_mappings_from_album_memberships()
        .await
    {
        Ok(0) => {}
        Ok(inserted) => {
            tracing::info!(
                inserted,
                "Backfilled asset/master mappings from album membership history"
            );
        }
        Err(e) => {
            tracing::warn!(
                error = %e,
                "Failed to backfill asset/master mappings from album membership history"
            );
        }
    }
}

fn clear_full_query_token_block_stats(stats: &mut SyncStats) {
    stats.sync_token_blocked = false;
    stats.sync_token_blocked_reason = None;
    stats.sync_token_blocked_source = None;
    stats.sync_token_blocked_explanation = None;
    stats.sync_token_blocked_zone = None;
    stats.sync_token_expected_receivers = None;
    stats.sync_token_receivers_with_token = None;
    stats.sync_token_receivers_missing = None;
    stats.sync_token_receivers_blank = None;
    stats.sync_token_receivers_dropped = None;
    stats.sync_token_unique_values = None;
}

async fn download_photos_full_with_reason(
    download_client: &Client,
    passes: &[crate::commands::AlbumPass],
    config: &Arc<DownloadConfig>,
    controls: DownloadControls,
    shutdown_token: CancellationToken,
    reason: FullEnumerationReason,
) -> Result<SyncResult> {
    let mut result =
        download_photos_full_with_token(download_client, passes, config, controls, shutdown_token)
            .await?;
    set_full_enumeration_reason(&mut result, reason);
    Ok(result)
}

async fn targeted_backfill_snapshots_complete(
    passes: &[crate::commands::AlbumPass],
    config: &DownloadConfig,
) -> bool {
    let Some(db) = &config.state_db else {
        return false;
    };
    let container_ids: Vec<String> = passes
        .iter()
        .filter_map(|pass| pass.album.container_id().map(ToOwned::to_owned))
        .collect();
    let container_refs: Vec<&str> = container_ids.iter().map(String::as_str).collect();
    match db
        .selected_album_containers_have_complete_snapshots(&config.library, &container_refs)
        .await
    {
        Ok(complete) => complete,
        Err(e) => {
            tracing::warn!(
                error = %e,
                "Failed to verify targeted album backfill snapshots"
            );
            false
        }
    }
}

async fn download_photos_incremental_with_targeted_album_backfill(
    download_client: &Client,
    passes: &[crate::commands::AlbumPass],
    config: &Arc<DownloadConfig>,
    zone_sync_token: &str,
    controls: DownloadControls,
    shutdown_token: CancellationToken,
    backfill_pass_indices: &[usize],
    reason: FullEnumerationReason,
) -> Result<SyncResult> {
    let backfill_passes: Vec<crate::commands::AlbumPass> = backfill_pass_indices
        .iter()
        .filter_map(|index| passes.get(*index).cloned())
        .collect();
    let backfill_result = download_photos_full_with_reason(
        download_client,
        &backfill_passes,
        config,
        controls,
        shutdown_token.clone(),
        reason,
    )
    .await?;
    let backfill_failed = !matches!(backfill_result.outcome, DownloadOutcome::Success)
        || backfill_result.stats.interrupted
        || backfill_result.stats.enumeration_errors > 0
        || shutdown_token.is_cancelled()
        || !targeted_backfill_snapshots_complete(&backfill_passes, config).await;

    let SyncResult {
        outcome: backfill_outcome,
        sync_token: _,
        stats: mut combined_stats,
        full_enumeration_ran: backfill_full_enumeration_ran,
    } = backfill_result;

    if backfill_failed {
        block_sync_token_for_incremental_delta(
            &mut combined_stats,
            TARGETED_ALBUM_BACKFILL_FAILED_REASON,
        );
        return Ok(SyncResult {
            outcome: backfill_outcome,
            sync_token: None,
            stats: combined_stats,
            full_enumeration_ran: backfill_full_enumeration_ran,
        });
    }

    // Full album queries may report their own query sync-token telemetry, but
    // targeted backfill does not use that token. The zone token may advance
    // only after the following /changes/zone pass completes safely.
    clear_full_query_token_block_stats(&mut combined_stats);

    let incremental_result = if passes
        .iter()
        .any(|pass| pass.kind == crate::commands::PassKind::SmartFolder)
    {
        download_photos_incremental_with_smart_folder_refresh(
            download_client,
            passes,
            config,
            zone_sync_token,
            controls,
            shutdown_token,
        )
        .await?
    } else {
        download_photos_incremental(
            download_client,
            passes,
            config,
            zone_sync_token,
            controls,
            shutdown_token,
        )
        .await?
    };

    let SyncResult {
        outcome: incremental_outcome,
        sync_token,
        stats: incremental_stats,
        full_enumeration_ran: incremental_full_enumeration_ran,
    } = incremental_result;
    combined_stats.accumulate(&incremental_stats);
    let outcome = merge_download_outcomes(&backfill_outcome, &incremental_outcome);
    let sync_token = (!combined_stats.sync_token_blocked)
        .then_some(sync_token)
        .flatten();

    Ok(SyncResult {
        outcome,
        sync_token,
        stats: combined_stats,
        full_enumeration_ran: backfill_full_enumeration_ran || incremental_full_enumeration_ran,
    })
}

async fn download_photos_incremental_with_smart_folder_refresh(
    download_client: &Client,
    passes: &[crate::commands::AlbumPass],
    config: &Arc<DownloadConfig>,
    zone_sync_token: &str,
    controls: DownloadControls,
    shutdown_token: CancellationToken,
) -> Result<SyncResult> {
    let (incremental_passes, smart_folder_passes) =
        split_incremental_and_smart_folder_passes(passes);

    if incremental_passes.is_empty() {
        return download_photos_full_with_token(
            download_client,
            &smart_folder_passes,
            config,
            controls,
            shutdown_token,
        )
        .await;
    }

    let incremental_result = download_photos_incremental(
        download_client,
        &incremental_passes,
        config,
        zone_sync_token,
        controls,
        shutdown_token.clone(),
    )
    .await?;

    if smart_folder_passes.is_empty() {
        return Ok(incremental_result);
    }

    let smart_folder_config =
        if config.recent.is_some() && config.recent_scope == crate::cli::RecentScope::Global {
            Arc::new(config.with_recent_scope(crate::cli::RecentScope::PerFilter))
        } else {
            Arc::clone(config)
        };

    let mut smart_folder_result = download_photos_full_with_token_policy(
        download_client,
        &smart_folder_passes,
        &smart_folder_config,
        controls,
        shutdown_token,
        false,
    )
    .await?;

    let smart_folder_refresh_failed =
        !matches!(smart_folder_result.outcome, DownloadOutcome::Success)
            || smart_folder_result.stats.interrupted
            || smart_folder_result.stats.enumeration_errors > 0;
    if !smart_folder_refresh_failed {
        // The token that matters for this mixed incremental cycle is the
        // `/changes/zone` token captured below. A selected smart-folder
        // records/query refresh can complete cleanly while still lacking a
        // full-enumeration query token, especially under bounded modes. Keep
        // refresh failures conservative, but do not let query-token telemetry
        // veto the safe incremental zone checkpoint.
        clear_full_query_token_block_stats(&mut smart_folder_result.stats);
    }

    let SyncResult {
        outcome: incremental_outcome,
        sync_token: incremental_sync_token,
        stats: mut combined_stats,
        full_enumeration_ran: incremental_full_enumeration_ran,
    } = incremental_result;
    combined_stats.accumulate(&smart_folder_result.stats);

    if smart_folder_refresh_failed {
        block_sync_token_for_incremental_delta(
            &mut combined_stats,
            SMART_FOLDER_REFRESH_FAILED_REASON,
        );
    }

    let sync_token = (!combined_stats.sync_token_blocked)
        .then_some(incremental_sync_token)
        .flatten();
    let outcome = merge_download_outcomes(&incremental_outcome, &smart_folder_result.outcome);

    Ok(SyncResult {
        outcome,
        sync_token,
        stats: combined_stats,
        full_enumeration_ran: incremental_full_enumeration_ran
            || smart_folder_result.full_enumeration_ran,
    })
}

async fn run_targeted_recovery_pass(
    download_client: &Client,
    passes: &[crate::commands::AlbumPass],
    config: &Arc<DownloadConfig>,
    controls: DownloadControls,
    shutdown_token: CancellationToken,
) -> Result<SyncResult> {
    let started = Instant::now();
    let plan = build_pending_retry_download_tasks(passes, config, shutdown_token.clone()).await?;
    let PendingRetryPlan {
        tasks,
        retry_sources,
        pass_configs,
        unmatched_targets,
        requested,
    } = plan;
    let unmatched = unmatched_targets.len();

    if requested == 0 {
        return Ok(pending_retry_no_download_result(
            &started,
            &shutdown_token,
            0,
            0,
        ));
    }

    tracing::info!(
        requested,
        refreshed = tasks.len(),
        unmatched,
        "Retrying pending assets with targeted enumeration"
    );

    if controls.run_mode.only_print_filenames() {
        #[allow(
            clippy::print_stdout,
            reason = "--only-print-filenames writes target paths to stdout so callers can pipe to xargs/etc"
        )]
        for task in &tasks {
            println!("{}", task.download_path.display());
        }
        return Ok(pending_retry_no_download_result(
            &started,
            &shutdown_token,
            unmatched,
            0,
        ));
    }

    if controls.run_mode.is_dry_run() {
        return Ok(pending_retry_no_download_result(
            &started,
            &shutdown_token,
            unmatched,
            tasks.len(),
        ));
    }

    if tasks.is_empty() {
        return Ok(pending_retry_no_download_result(
            &started,
            &shutdown_token,
            unmatched,
            0,
        ));
    }

    let pass_config = PassConfig {
        client: download_client,
        retry_config: &config.retry,
        metadata: MetadataFlags::from(config.as_ref()),
        mark_capture_repair_after_download: matches!(
            config.capture_timestamp_repair,
            CaptureTimestampRepair::ReplaceWithCaptureLocal
        ),
        concurrency: config.concurrent_downloads,
        reporting: controls.reporting,
        temp_suffix: Arc::clone(&config.temp_suffix),
        shutdown_token: shutdown_token.clone(),
        state_db: config.state_db.clone(),
        rate_limit_counter: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        bandwidth_limiter: config.bandwidth_limiter.clone(),
        library: Arc::clone(&config.library),
    };
    let planned_tasks = tasks.clone();
    let mut pass_result = run_download_pass(pass_config, tasks).await;
    if pass_result.url_expired_abort
        && pass_result.auth_errors < AUTH_ERROR_THRESHOLD
        && !shutdown_token.is_cancelled()
    {
        let downloaded_keys: FxHashSet<RetryTaskKey> = pass_result
            .downloaded_tasks
            .iter()
            .map(RetryTaskKey::from)
            .collect();
        let expired_retry_candidates: Vec<DownloadTask> = planned_tasks
            .iter()
            .filter(|task| !downloaded_keys.contains(&RetryTaskKey::from(*task)))
            .cloned()
            .collect();
        let retry_tasks = match build_incremental_expired_url_retry_tasks(
            passes,
            &pass_configs,
            &retry_sources,
            &expired_retry_candidates,
            shutdown_token.clone(),
        )
        .await
        {
            Ok(tasks) => tasks,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "Could not refresh expired pending-retry download URLs; leaving failures for replay"
                );
                Vec::new()
            }
        };
        if !retry_tasks.is_empty() {
            let retry_task_count = retry_tasks.len();
            tracing::info!(
                count = retry_task_count,
                "Refreshing expired pending-retry download URLs and retrying failed tasks"
            );
            let refreshed_keys: FxHashSet<RetryTaskKey> =
                retry_tasks.iter().map(RetryTaskKey::from).collect();
            let retry_pass_config = PassConfig {
                client: download_client,
                retry_config: &config.retry,
                metadata: MetadataFlags::from(config.as_ref()),
                mark_capture_repair_after_download: matches!(
                    config.capture_timestamp_repair,
                    CaptureTimestampRepair::ReplaceWithCaptureLocal
                ),
                concurrency: config.concurrent_downloads,
                reporting: controls.reporting,
                temp_suffix: Arc::clone(&config.temp_suffix),
                shutdown_token: shutdown_token.clone(),
                state_db: config.state_db.clone(),
                rate_limit_counter: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                bandwidth_limiter: config.bandwidth_limiter.clone(),
                library: Arc::clone(&config.library),
            };
            let retry_result = run_download_pass(retry_pass_config, retry_tasks).await;
            merge_expired_url_retry_result(
                &mut pass_result,
                expired_retry_candidates,
                refreshed_keys,
                retry_result,
            );
        } else if !expired_retry_candidates.is_empty() {
            pass_result.failed = expired_retry_candidates;
        }
    }
    let failed = pass_result.failed.len();
    if failed > 0 {
        for task in &pass_result.failed {
            tracing::error!(asset_id = %task.asset_id, path = %task.download_path.display(), "Targeted retry failed");
        }
    }
    let remaining_unmatched = unmatched;
    let checkpoint_revalidate_records = unmatched_targets
        .iter()
        .map(|target| ProviderRecordId::new(target.asset_id.to_string()))
        .collect();

    let stats = SyncStats {
        downloaded: pass_result.downloaded,
        failed: failed.saturating_add(remaining_unmatched),
        bytes_downloaded: pass_result.bytes_downloaded,
        disk_bytes_written: pass_result.disk_bytes_written,
        exif_failures: pass_result.exif_failures,
        state_write_failures: pass_result.state_write_failures,
        elapsed_secs: started.elapsed().as_secs_f64(),
        interrupted: shutdown_token.is_cancelled()
            || pass_result.auth_errors >= AUTH_ERROR_THRESHOLD,
        rate_limited: pass_result.rate_limit_observations,
        photos_downloaded: pass_result.photos_downloaded,
        videos_downloaded: pass_result.videos_downloaded,
        recap: pass_result.recap.clone(),
        checkpoint_revalidate_records,
        ..SyncStats::default()
    };
    if pass_result.auth_errors >= AUTH_ERROR_THRESHOLD {
        return Ok(SyncResult {
            outcome: DownloadOutcome::SessionExpired {
                auth_error_count: pass_result.auth_errors,
            },
            sync_token: None,
            stats,
            full_enumeration_ran: false,
        });
    }

    let failed_count =
        failed + remaining_unmatched + pass_result.exif_failures + pass_result.state_write_failures;
    Ok(SyncResult {
        outcome: if failed_count > 0 {
            DownloadOutcome::PartialFailure { failed_count }
        } else {
            DownloadOutcome::Success
        },
        sync_token: None,
        stats,
        full_enumeration_ran: false,
    })
}

fn pending_retry_no_download_result(
    started: &Instant,
    shutdown_token: &CancellationToken,
    unmatched: usize,
    downloaded: usize,
) -> SyncResult {
    let remaining_unmatched = unmatched;
    let stats = SyncStats {
        downloaded,
        failed: remaining_unmatched,
        elapsed_secs: started.elapsed().as_secs_f64(),
        interrupted: shutdown_token.is_cancelled(),
        ..SyncStats::default()
    };
    SyncResult {
        outcome: if remaining_unmatched > 0 {
            DownloadOutcome::PartialFailure {
                failed_count: remaining_unmatched,
            }
        } else {
            DownloadOutcome::Success
        },
        sync_token: None,
        stats,
        full_enumeration_ran: false,
    }
}

async fn append_targeted_recovery_to_sync_result(
    download_client: &Client,
    passes: &[crate::commands::AlbumPass],
    config: &Arc<DownloadConfig>,
    controls: DownloadControls,
    shutdown_token: CancellationToken,
    sync_result: SyncResult,
) -> Result<SyncResult> {
    if !matches!(sync_result.outcome, DownloadOutcome::Success)
        || sync_result.stats.interrupted
        || shutdown_token.is_cancelled()
    {
        return Ok(sync_result);
    }

    let retry_result = match run_targeted_recovery_pass(
        download_client,
        passes,
        config,
        controls,
        shutdown_token.clone(),
    )
    .await
    {
        Ok(result) => result,
        Err(e) => {
            let stats = SyncStats {
                failed: 1,
                elapsed_secs: 0.0,
                ..SyncStats::default()
            };
            tracing::warn!(
                error = %e,
                diagnostic = PENDING_RETRY_UNMATCHED_REASON,
                "Targeted recovery failed before downloads; retaining durable state for a later cycle"
            );
            SyncResult {
                outcome: DownloadOutcome::PartialFailure { failed_count: 1 },
                sync_token: None,
                stats,
                full_enumeration_ran: false,
            }
        }
    };

    let SyncResult {
        outcome: source_outcome,
        sync_token: source_sync_token,
        stats: mut combined_stats,
        full_enumeration_ran: source_full_enumeration_ran,
    } = sync_result;
    let outcome = merge_download_outcomes(&source_outcome, &retry_result.outcome);
    combined_stats.accumulate(&retry_result.stats);
    let sync_token = if !combined_stats.sync_token_blocked
        && !combined_stats.interrupted
        && !controls.run_mode.is_dry_run()
        && !controls.run_mode.only_print_filenames()
    {
        source_sync_token
    } else {
        None
    };

    Ok(SyncResult {
        outcome,
        sync_token,
        stats: combined_stats,
        full_enumeration_ran: source_full_enumeration_ran || retry_result.full_enumeration_ran,
    })
}

pub async fn download_photos_with_sync(
    download_client: &Client,
    passes: &[crate::commands::AlbumPass],
    config: Arc<DownloadConfig>,
    controls: DownloadControls,
    shutdown_token: CancellationToken,
) -> Result<SyncResult> {
    let sync_started_at = chrono::Utc::now().timestamp();
    cleanup_orphan_part_files(&config).await;
    if matches!(config.sync_mode, SyncMode::Incremental { .. })
        && let Some(db) = &config.state_db
    {
        backfill_asset_master_mappings_from_album_history(db.as_ref()).await;
    }
    let metadata_capture_repair =
        run_metadata_capture_repair(passes, &config, controls, &shutdown_token).await;

    // Give every non-downloaded asset a fresh start this sync:
    // failed -> pending (with attempts reset), and stale attempt counts on
    // pending assets cleared so the per-sync cap starts from zero.
    if let Some(db) = &config.state_db {
        match db.prune_source_deleted_retries(Some(&config.library)).await {
            Ok(0) => {}
            Ok(count) => {
                tracing::info!(
                    count,
                    "Pruned source-deleted pending/failed assets from retry queue"
                );
            }
            Err(e) => {
                tracing::warn!(error = %e, "Failed to prune source-deleted retry assets");
            }
        }
        let error_retention = if config.repair_truncated {
            crate::state::RetryErrorRetention::Preserve(
                crate::commands::reconcile::FILE_TRUNCATED_REASON,
            )
        } else {
            crate::state::RetryErrorRetention::Clear
        };
        match db
            .prepare_for_retry(Some(&config.library), error_retention)
            .await
        {
            Ok((failed, stale, _)) => {
                if failed > 0 {
                    tracing::debug!(count = failed, "Reset failed assets for retry");
                }
                if stale > 0 {
                    tracing::debug!(
                        count = stale,
                        "Cleared stale attempt counts on pending assets"
                    );
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "Failed to reset assets for retry");
            }
        }
    }

    let result = match &config.sync_mode {
        SyncMode::Full => {
            download_photos_full_with_token(
                download_client,
                passes,
                &config,
                controls,
                shutdown_token.clone(),
            )
            .await
        }
        SyncMode::Incremental { .. } if has_metadata_backfill_work(&config).await => {
            let reason = FullEnumerationReason::MetadataBackfill;
            tracing::info!(
                full_enumeration_reason = reason.as_str(),
                "Metadata backfill requires full enumeration, skipping incremental sync"
            );
            download_photos_full_with_reason(
                download_client,
                passes,
                &config,
                controls,
                shutdown_token.clone(),
                reason,
            )
            .await
        }
        SyncMode::Incremental { zone_sync_token } => {
            match determine_incremental_routing_decision(passes, &config, controls).await {
                IncrementalRoutingDecision::NeedsFull { reason } => {
                    tracing::debug!(
                        full_enumeration_reason = reason.as_str(),
                        "Selected passes are not safe for incremental routing, skipping incremental"
                    );
                    download_photos_full_with_reason(
                        download_client,
                        passes,
                        &config,
                        controls,
                        shutdown_token.clone(),
                        reason,
                    )
                    .await
                }
                IncrementalRoutingDecision::TargetedAlbumBackfill {
                    pass_indices,
                    reason,
                } => {
                    tracing::debug!(
                        full_enumeration_reason = reason.as_str(),
                        backfill_passes = pass_indices.len(),
                        "Backfilling missing album snapshots before incremental routing"
                    );
                    download_photos_incremental_with_targeted_album_backfill(
                        download_client,
                        passes,
                        &config,
                        zone_sync_token,
                        controls,
                        shutdown_token.clone(),
                        &pass_indices,
                        reason,
                    )
                    .await
                }
                IncrementalRoutingDecision::Safe => {
                    let token = zone_sync_token.clone();
                    let has_smart_folder_pass = passes
                        .iter()
                        .any(|pass| pass.kind == crate::commands::PassKind::SmartFolder);
                    let incremental_result = if has_smart_folder_pass {
                        download_photos_incremental_with_smart_folder_refresh(
                            download_client,
                            passes,
                            &config,
                            &token,
                            controls,
                            shutdown_token.clone(),
                        )
                        .await
                    } else {
                        download_photos_incremental(
                            download_client,
                            passes,
                            &config,
                            &token,
                            controls,
                            shutdown_token.clone(),
                        )
                        .await
                    };
                    match incremental_result {
                        Ok(result) => Ok(result),
                        Err(e) => match classify_incremental_error(&e) {
                            IncrementalErrorClass::TokenFallback
                            | IncrementalErrorClass::StaticFallback => {
                                let reason = FullEnumerationReason::OtherStaticReason;
                                tracing::warn!(
                                    error = %e,
                                    full_enumeration_reason = reason.as_str(),
                                    "Incremental sync failed, falling back to full enumeration"
                                );
                                download_photos_full_with_reason(
                                    download_client,
                                    passes,
                                    &config,
                                    controls,
                                    shutdown_token.clone(),
                                    reason,
                                )
                                .await
                            }
                            IncrementalErrorClass::TransientFailure => Err(e),
                        },
                    }
                }
            }
        }
    }?;

    let mut result = append_targeted_recovery_to_sync_result(
        download_client,
        passes,
        &config,
        controls,
        shutdown_token.clone(),
        result,
    )
    .await?;

    result.stats.accumulate(&metadata_capture_repair.stats);
    if metadata_capture_repair.failures > 0 {
        result.outcome = merge_download_outcomes(
            &result.outcome,
            &DownloadOutcome::PartialFailure {
                failed_count: metadata_capture_repair.failures,
            },
        );
    }
    if metadata_capture_repair.stats.sync_token_blocked {
        result.sync_token = None;
    }

    // Pending is transient — anything still pending after a complete sync either
    // wasn't enumerated or failed silently. Skip on interrupt where pending is expected.
    if let Some(db) = &config.state_db
        && !shutdown_token.is_cancelled()
    {
        match db.promote_pending_to_failed(sync_started_at).await {
            Ok(promoted) if promoted > 0 => {
                tracing::warn!(
                    count = promoted,
                    "Promoted unresolved pending assets to failed"
                );
            }
            Err(e) => {
                tracing::warn!(error = %e, "Failed to promote pending assets");
            }
            _ => {}
        }
    }

    Ok(result)
}

/// Fold per-pass `album.len()` results into a `(counts, error_count)` tuple,
/// logging a `warn!` for each failure. Errors are mapped to a count of 0 so
/// downstream progress and concurrency math still has a value. The returned
/// error count is diagnostic only: callers must not turn a failed count
/// side-channel into a semantic completeness bound.
fn fold_pass_count_results(
    results: Vec<anyhow::Result<u64>>,
    passes: &[crate::commands::AlbumPass],
) -> (Vec<u64>, usize) {
    let mut errors: usize = 0;
    let counts: Vec<u64> = results
        .into_iter()
        .zip(passes)
        .map(|(result, pass)| match result {
            Ok(n) => n,
            Err(e) => {
                errors += 1;
                tracing::warn!(
                    album = %pass.album,
                    error = %e,
                    "Failed to query album length; treating count as a display-only \
                     zero and relying on the record stream for completeness"
                );
                0
            }
        })
        .collect();
    (counts, errors)
}

#[derive(Debug)]
struct PassCountPlan {
    display_counts: Vec<u64>,
    stream_total_counts: Vec<Option<u64>>,
    exact_total: Option<u64>,
    len_errors: usize,
}

fn capped_exact_total(counts: &[u64], recent: Option<u32>) -> u64 {
    let total = counts.iter().sum::<u64>();
    total.min(recent.map(u64::from).unwrap_or(u64::MAX))
}

fn display_total_for_recent_scope(counts: &[u64], config: &DownloadConfig) -> u64 {
    match (config.recent, config.recent_scope) {
        (Some(recent), crate::cli::RecentScope::Global) => capped_exact_total(counts, Some(recent)),
        _ => counts.iter().sum(),
    }
}

fn should_skip_pass_count_fetch(config: &DownloadConfig) -> bool {
    // Recent-limited and lower-date-bounded runs deliberately enumerate only a
    // prefix of each newest-first pass. The Hyperion count endpoint reports the
    // full pass size, not how many complete assets will be yielded inside that
    // bounded prefix, so using it as an exact undercount bound false-fires on
    // live accounts with sparse windows. These bounded full syncs suppress the
    // sync token below because they are not complete zone enumerations.
    config.recent.is_some() || config.skip_created_before.is_some()
}

async fn build_pass_count_plan(
    passes: &[crate::commands::AlbumPass],
    config: &DownloadConfig,
    _controls: DownloadControls,
) -> PassCountPlan {
    if should_skip_pass_count_fetch(config) {
        let display_count = config.recent.map(u64::from).unwrap_or(0);
        return PassCountPlan {
            display_counts: vec![display_count; passes.len()],
            stream_total_counts: vec![None; passes.len()],
            exact_total: None,
            len_errors: 0,
        };
    }

    // Album counts share CloudKit's `/internal/records/query/batch`
    // endpoint, so the same-library pass set can fetch all counts with one
    // HTTP call. This matters for default multi-pass syncs and especially
    // `-a all`, where the old per-pass count probe scaled linearly before
    // the first byte of the first download.
    // Capture per-pass `len()` errors separately from stream errors. A failed
    // count endpoint is not itself proof that records/query was incomplete,
    // so token safety below is decided by natural stream completion and
    // sync-token usability instead of a fabricated count bound.
    let pass_albums: Vec<&crate::icloud::photos::PhotoAlbum> =
        passes.iter().map(|pass| &pass.album).collect();
    let pass_count_results = crate::icloud::photos::PhotoAlbum::len_many(&pass_albums).await;
    let (display_counts, len_errors) = fold_pass_count_results(pass_count_results, passes);
    let stream_total_counts = if len_errors > 0 {
        vec![None; passes.len()]
    } else {
        display_counts.iter().copied().map(Some).collect()
    };
    let exact_total = (len_errors == 0).then(|| capped_exact_total(&display_counts, config.recent));

    PassCountPlan {
        display_counts,
        stream_total_counts,
        exact_total,
        len_errors,
    }
}

/// Classification of how the producer-observed asset count compared with the
/// pre-enumeration API total.
///
/// A positive shortfall means the count side-channel claimed there were assets
/// that the producer stream never observed. Duplicate asset IDs can explain
/// some provider count drift because the producer intentionally counts unique
/// assets after duplicate suppression. Any remaining gap is diagnostic-only:
/// the records/query stream, token capture, and write outcomes decide whether
/// the sync token can advance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PaginationShortfall {
    Match,
    DuplicateCompensated { shortfall: u64 },
    Shortfall { shortfall: u64 },
}

/// Pure classifier for the pagination-undercount gate. `total` is the
/// pre-enumeration API count (post `--recent` cap and known filters);
/// `unique_seen` is the producer's `assets_seen` count after duplicate asset
/// IDs have been suppressed. Caller is responsible for the `total > 0` guard
/// and any dry-run / print-only suppression.
fn classify_pagination_shortfall(
    total: u64,
    unique_seen: u64,
    duplicate_asset_ids: u64,
) -> PaginationShortfall {
    if unique_seen >= total {
        return PaginationShortfall::Match;
    }

    let raw_seen = unique_seen.saturating_add(duplicate_asset_ids);
    if raw_seen >= total {
        return PaginationShortfall::DuplicateCompensated {
            shortfall: total - unique_seen,
        };
    }

    let shortfall = total - raw_seen;
    PaginationShortfall::Shortfall { shortfall }
}

/// Resolve the zone sync token from every full-enumeration pass that reported
/// one. All passes for a zone must agree before the token can advance; picking
/// the first completed pass would hide snapshot drift between album-scoped
/// enumerations.
/// Full enumeration with syncToken capture.
///
/// Uses `photo_stream_with_token` to capture the zone-level syncToken
/// while running the standard streaming download pipeline. The token
/// is returned alongside the download outcome.
async fn download_photos_full_with_token(
    download_client: &Client,
    passes: &[crate::commands::AlbumPass],
    config: &Arc<DownloadConfig>,
    controls: DownloadControls,
    shutdown_token: CancellationToken,
) -> Result<SyncResult> {
    download_photos_full_with_token_policy(
        download_client,
        passes,
        config,
        controls,
        shutdown_token,
        true,
    )
    .await
}

async fn download_photos_full_with_token_policy(
    download_client: &Client,
    passes: &[crate::commands::AlbumPass],
    config: &Arc<DownloadConfig>,
    controls: DownloadControls,
    shutdown_token: CancellationToken,
    repair_token_gaps: bool,
) -> Result<SyncResult> {
    let started = Instant::now();
    let record_album_snapshots = should_record_album_snapshots(passes, config, controls);
    let needs_per_pass = config.requires_per_pass_paths() || record_album_snapshots;

    // Mark every unique zone as in-progress so an interrupted full
    // enumeration leaves a trail the next startup can surface to the
    // operator. Clears once the enumeration returns normally.
    let mut enum_zones: Vec<String> = passes
        .iter()
        .map(|p| p.album.zone_name().to_string())
        .collect();
    enum_zones.sort();
    enum_zones.dedup();
    if let Some(db) = &config.state_db {
        for zone in &enum_zones {
            if let Err(e) = db.begin_enum_progress(zone).await {
                tracing::debug!(error = %e, zone, "Failed to mark enumeration start");
            }
        }
    }

    let pass_count_plan = build_pass_count_plan(passes, config, controls).await;
    let pass_counts = pass_count_plan.display_counts;
    let pass_stream_counts = pass_count_plan.stream_total_counts;
    let mut pagination_counts = pass_counts.clone();
    let mut exact_total = pass_count_plan.exact_total;
    let mut pagination_count_deduction = 0u64;
    let len_errors = pass_count_plan.len_errors;
    let display_total = display_total_for_recent_scope(&pass_counts, config);
    let deferred_unfiled = deferred_unfiled_index(passes);
    let recent_frontier =
        build_recent_frontier(passes, config, controls, shutdown_token.clone()).await?;
    let bounds_truncated = Arc::new(AtomicBool::new(false));
    let strict_empty_tail_errors = config.recent.is_none()
        && config.skip_created_before.is_none()
        && !controls.run_mode.only_print_filenames()
        && !controls.run_mode.is_dry_run();

    // Pass-specific path mode still needs one derived config per pass so
    // `{album}` / `{smart-folder}` / `{library}` expand correctly, but the
    // CloudKit streams are independent. Run those pass streams concurrently
    // instead of serializing round trips across albums. Download workers are
    // divided across active pass pipelines so real downloads do not multiply
    // the user-selected `[download].threads` by the number of albums.
    let (mut streaming_result, token_receivers) = if needs_per_pass {
        let mut combined_result = StreamingResult {
            // Enumeration is "complete" only when every pass finished
            // its stream cleanly. Start optimistic; flip to false on the
            // first pass that ended early (shutdown, channel-close, or
            // panic) so the marker stays set and the next startup logs
            // the interruption.
            enumeration_complete: !passes.is_empty(),
            ..StreamingResult::default()
        };
        let pass_parallelism = passes.len().min(config.concurrent_downloads).max(1);
        let per_pass_download_concurrency = config
            .concurrent_downloads
            .div_ceil(pass_parallelism)
            .max(1);
        let pass_configs = build_pass_configs_with_download_concurrency(
            passes,
            config,
            per_pass_download_concurrency,
        );
        let shared_download_ctx = if controls.run_mode.is_dry_run() {
            None
        } else {
            Some(preload_download_context(config).await)
        };

        // Build per-pass labels for the album divider. Friendly multi-pass
        // syncs print a done line (✓) above the bar after each album
        // completes. Single-pass and off-mode syncs skip the divider.
        let pass_labels: Vec<(&str, u64)> = passes
            .iter()
            .zip(&pass_counts)
            .map(|(pass, &count)| {
                let label: &str = pass.album.name.as_ref();
                let label = if label.is_empty() { "unfiled" } else { label };
                (label, count)
            })
            .collect();
        let divider = crate::personality::album_divider::AlbumDivider::new(
            controls.reporting.personality_mode,
            &pass_labels,
        );

        let deferred_ids = deferred_unfiled
            .map(|_| Arc::new(std::sync::Mutex::new(FxHashSet::<String>::default())));

        let non_unfiled_results = stream::iter(
            passes
                .iter()
                .enumerate()
                .zip(&pass_counts)
                .zip(&pass_stream_counts)
                .zip(pass_configs.iter().cloned())
                .filter(|((((index, _pass), _count), _total_count), _config)| {
                    Some(*index) != deferred_unfiled
                }),
        )
        .map(
            |((((pass_index, pass), &count), total_count), pass_config)| {
                let shutdown_token = shutdown_token.clone();
                let download_client = download_client.clone();
                let deferred_ids = deferred_ids.clone();
                let recent_frontier = recent_frontier.as_ref();
                let download_ctx = shared_download_ctx.clone();
                let bounds_truncated = Arc::clone(&bounds_truncated);
                async move {
                    let album_snapshot = if record_album_snapshots {
                        AlbumSnapshotRecorder::start_for_pass(
                            config.state_db.clone(),
                            pass,
                            config.enum_config_hash.as_deref(),
                        )
                        .await?
                    } else {
                        None
                    };
                    let (stream, token_rx) = open_photo_stream_for_controls(
                        &pass.album,
                        scope_frontier_limit(config, recent_frontier),
                        *total_count,
                        config.concurrent_downloads,
                        pass_config.concurrent_downloads,
                        controls,
                        strict_empty_tail_errors,
                    );
                    let stream = filter_stream_to_enumeration_bounds(
                        stream,
                        config,
                        recent_frontier,
                        Arc::clone(&bounds_truncated),
                    );

                    if pass.kind == crate::commands::PassKind::Album
                        && let Some(deferred_ids) = deferred_ids
                    {
                        let stream = stream.map(move |item| {
                            if let Ok(asset) = &item
                                && let Ok(mut ids) = deferred_ids.lock()
                            {
                                ids.insert(asset.asset_record_name().to_string());
                            }
                            item
                        });
                        return run_full_pass_stream(
                            download_client,
                            stream,
                            token_rx,
                            pass_config,
                            FullPassStreamOptions {
                                pass_index,
                                controls,
                                count,
                                kind: pass.kind,
                                shutdown_token,
                                download_ctx: download_ctx.clone(),
                                album_snapshot,
                            },
                        )
                        .await;
                    }

                    run_full_pass_stream(
                        download_client,
                        stream,
                        token_rx,
                        pass_config,
                        FullPassStreamOptions {
                            pass_index,
                            controls,
                            count,
                            kind: pass.kind,
                            shutdown_token,
                            download_ctx,
                            album_snapshot,
                        },
                    )
                    .await
                }
            },
        )
        .buffer_unordered(pass_parallelism)
        .collect::<Vec<Result<PerPassStreamingResult>>>();

        let pass_results = non_unfiled_results.await;

        let mut token_receivers = Vec::with_capacity(passes.len());
        let mut deferred_exclusions_complete = true;
        for pass_result in pass_results {
            let PerPassStreamingResult {
                pass_index,
                kind,
                label,
                count,
                elapsed,
                token_rx,
                result,
            } = pass_result?;

            if deferred_unfiled.is_some()
                && kind == crate::commands::PassKind::Album
                && (result.enumeration_errors > 0 || !result.enumeration_complete)
            {
                deferred_exclusions_complete = false;
            }
            if deferred_unfiled.is_some()
                && kind == crate::commands::PassKind::Album
                && count > 0
                && result.assets_seen == 0
                && result.enumeration_errors == 0
                && result.enumeration_complete
            {
                // A deferred-unfiled run has one library-wide stream that can
                // cover assets counted by album-side passes. If an album pass
                // contributes a count but no records to this cycle, keeping
                // that count in the summed pagination total compares
                // pass-count bookkeeping against producer-observed records and
                // creates a false shortfall.
                pagination_count_deduction = pagination_count_deduction.saturating_add(count);
            }

            token_receivers.push(PassTokenReceiver {
                pass: PassKey {
                    index: pass_index,
                    kind,
                    label: label.clone(),
                },
                receiver: token_rx,
                enumeration_complete: result.enumeration_complete && result.enumeration_errors == 0,
            });
            let downloaded_u64 = u64::try_from(result.downloaded).unwrap_or(u64::MAX);
            divider.mark_done(&label, downloaded_u64, count, elapsed);

            merge_streaming_result(&mut combined_result, result);
        }

        if let Some(index) = deferred_unfiled {
            if deferred_exclusions_complete {
                let excluded_ids = deferred_ids
                    .as_ref()
                    .and_then(|ids| ids.lock().ok().map(|guard| guard.clone()))
                    .unwrap_or_default();
                if let (Some(pass), Some(pass_config)) =
                    (passes.get(index), pass_configs.get(index).cloned())
                {
                    let stream_total_count = pass_stream_counts.get(index).copied().flatten();
                    let (stream, token_rx) = open_photo_stream_for_controls(
                        &pass.album,
                        scope_frontier_limit(config, recent_frontier.as_ref()),
                        stream_total_count,
                        config.concurrent_downloads,
                        pass_config.concurrent_downloads,
                        controls,
                        strict_empty_tail_errors,
                    );
                    let library = Arc::<str>::from(pass.album.zone_name());
                    let stream = filter_stream_to_enumeration_bounds(
                        stream,
                        config,
                        recent_frontier.as_ref(),
                        Arc::clone(&bounds_truncated),
                    );
                    let stream =
                        track_deferred_unfiled_heartbeat(stream, library, stream_total_count)
                            .filter(move |item| {
                                let keep = item.as_ref().map_or(true, |asset| {
                                    !excluded_ids.contains(asset.asset_record_name())
                                });
                                std::future::ready(keep)
                            });
                    let pass_result = run_full_pass_stream(
                        download_client.clone(),
                        stream,
                        token_rx,
                        pass_config,
                        FullPassStreamOptions {
                            pass_index: index,
                            controls,
                            count: pass_counts.get(index).copied().unwrap_or(0),
                            kind: crate::commands::PassKind::Unfiled,
                            shutdown_token: shutdown_token.clone(),
                            download_ctx: shared_download_ctx.clone(),
                            album_snapshot: None,
                        },
                    )
                    .await?;
                    let filtered_count = pass_result.result.assets_seen;
                    if let Some(slot) = pagination_counts.get_mut(index) {
                        *slot = filtered_count;
                    }
                    let downloaded_u64 =
                        u64::try_from(pass_result.result.downloaded).unwrap_or(u64::MAX);
                    divider.mark_done(
                        &pass_result.label,
                        downloaded_u64,
                        pass_result.count,
                        pass_result.elapsed,
                    );
                    token_receivers.push(PassTokenReceiver {
                        pass: PassKey {
                            index: pass_result.pass_index,
                            kind: pass_result.kind,
                            label: pass_result.label.clone(),
                        },
                        receiver: pass_result.token_rx,
                        enumeration_complete: pass_result.result.enumeration_complete
                            && pass_result.result.enumeration_errors == 0,
                    });
                    merge_streaming_result(&mut combined_result, pass_result.result);
                }
            } else {
                combined_result.enumeration_complete = false;
            }
        }
        divider.finish();

        (combined_result, token_receivers)
    } else {
        let merged_exclude_ids = passes
            .first()
            .map(|p| Arc::clone(&p.exclude_ids))
            .unwrap_or_else(|| Arc::new(FxHashSet::default()));
        let merged_config = if Arc::ptr_eq(&merged_exclude_ids, &config.exclude_asset_ids) {
            Arc::clone(config)
        } else {
            Arc::new(config.with_exclude_ids(merged_exclude_ids))
        };
        let mut token_receivers = Vec::with_capacity(passes.len());
        let streams: Vec<_> = passes
            .iter()
            .enumerate()
            .zip(&pass_stream_counts)
            .map(|((pass_index, pass), total_count)| {
                let (stream, token_rx) = open_photo_stream_for_controls(
                    &pass.album,
                    scope_frontier_limit(config, recent_frontier.as_ref()),
                    *total_count,
                    config.concurrent_downloads,
                    config.concurrent_downloads,
                    controls,
                    strict_empty_tail_errors,
                );
                let label = if pass.album.name.is_empty() {
                    "unfiled".to_string()
                } else {
                    pass.album.name.to_string()
                };
                token_receivers.push(PassTokenReceiver {
                    pass: PassKey {
                        index: pass_index,
                        kind: pass.kind,
                        label,
                    },
                    receiver: token_rx,
                    enumeration_complete: true,
                });
                filter_stream_to_enumeration_bounds(
                    stream,
                    config,
                    recent_frontier.as_ref(),
                    Arc::clone(&bounds_truncated),
                )
            })
            .collect();

        let combined = stream::select_all(streams);
        // Merged-stream branch already runs as a single call, so it creates
        // one bar internally; no shared-bar plumbing needed.
        let result = stream_and_download_from_stream(
            download_client,
            combined,
            &merged_config,
            controls,
            display_total,
            shutdown_token.clone(),
            StreamRuntime::new(None, None).deferring_metadata_drain(),
        )
        .await?;

        (result, token_receivers)
    };

    // Count-probe failures stay diagnostic unless the primary records/query
    // stream also proves unsafe. Do not fold them into `enumeration_errors`
    // or force `enumeration_complete = false`; otherwise a flaky count
    // endpoint can trap users in repeated full enumeration after CloudKit
    // delivered a naturally drained stream and a usable token.
    if exact_total.is_some() {
        exact_total = Some(
            capped_exact_total(&pagination_counts, config.recent)
                .saturating_sub(pagination_count_deduction),
        );
    }
    let api_total_at_start = if len_errors == 0
        && config.recent.is_none()
        && config.skip_created_before.is_none()
        && !controls.run_mode.only_print_filenames()
        && !controls.run_mode.is_dry_run()
    {
        exact_total
    } else {
        None
    };

    // Check if enumeration saw significantly fewer assets than the API reported.
    // The count side-channel can include assets outside the stream's effective
    // scope, so a mismatch is diagnostic-only. Token advancement is still gated
    // below by the records/query stream completing naturally, the returned
    // syncToken being usable and unanimous, and the download/state outcome
    // proving all streamed work was handled.
    let mut pagination_shortfall_assets = 0u64;
    let mut pagination_shortfall_warnings = 0usize;
    let count_lookup_failed = len_errors > 0;
    if !count_lookup_failed
        && !controls.run_mode.only_print_filenames()
        && !controls.run_mode.is_dry_run()
        && let Some(total) = exact_total.filter(|total| *total > 0)
    {
        let duplicate_asset_ids =
            u64::try_from(streaming_result.skip_summary.duplicates).unwrap_or(u64::MAX);
        let decision =
            classify_pagination_shortfall(total, streaming_result.assets_seen, duplicate_asset_ids);
        match decision {
            PaginationShortfall::Match => {}
            PaginationShortfall::DuplicateCompensated { shortfall } => {
                tracing::warn!(
                    expected = total,
                    seen = streaming_result.assets_seen,
                    shortfall,
                    duplicate_asset_ids,
                    "Enumeration count shortfall was explained by duplicate asset IDs; \
                         continuing sync token capture"
                );
            }
            PaginationShortfall::Shortfall { shortfall } => {
                pagination_shortfall_assets = shortfall;
                pagination_shortfall_warnings = 1;
                tracing::warn!(
                    expected = total,
                    seen = streaming_result.assets_seen,
                    duplicate_asset_ids,
                    shortfall,
                    "Enumeration saw fewer assets than the count side-channel reported; \
                         recording diagnostic and continuing sync token capture"
                );
            }
        }
    }

    // Collect the sync token from every album's token receiver and require
    // agreement before advancing. In practice, all passes for a zone should
    // report the same token; disagreement means the full enumeration did not
    // observe one coherent snapshot.
    // Don't advance the token for read-only operations or when the producer
    // stream was incomplete (would permanently skip missed assets).
    // Bounded runs are eligible to report a token only when the producer can
    // prove the bound did not actually truncate the stream. A count-limited
    // stream suppresses its token when it hits the cap, and lower-date-bound /
    // global-recent filtering marks `bounds_truncated` when it stops before
    // natural EOF.
    let token_attempt_allowed =
        !controls.run_mode.only_print_filenames() && !controls.run_mode.is_dry_run();
    let mut token_block_reason: Option<&'static str> = None;
    let mut token_expected_receivers: Option<usize> = None;
    let mut token_receivers_with_token: Option<usize> = None;
    let mut token_receivers_missing: Option<usize> = None;
    let mut token_receivers_blank: Option<usize> = None;
    let mut token_receivers_dropped: Option<usize> = None;
    let mut token_unique_values: Option<usize> = None;
    let mut same_cycle_recovery_attempts = 0usize;
    let mut same_cycle_recovery_successes = 0usize;
    let mut checkpoint_retry_passes = Vec::new();
    let sync_token = if token_attempt_allowed {
        let expected_token_count = token_receivers.len();
        token_expected_receivers = Some(expected_token_count);
        let mut observations = Vec::with_capacity(expected_token_count);
        for receiver in token_receivers {
            let result = if !receiver.enumeration_complete {
                let _ = receiver.receiver.await;
                PassTokenResult::EnumerationIncomplete
            } else {
                match receiver.receiver.await {
                    Ok(Some(token)) if token.trim().is_empty() => PassTokenResult::Blank,
                    Ok(Some(token)) => PassTokenResult::Present(token.trim().to_string()),
                    Ok(None) => PassTokenResult::Missing,
                    Err(_) => PassTokenResult::ReceiverDropped,
                }
            };
            observations.push(PassTokenObservation {
                pass: receiver.pass,
                result,
            });
        }
        let tokens: Vec<&str> = observations
            .iter()
            .filter_map(|observation| match &observation.result {
                PassTokenResult::Present(token) => Some(token.as_str()),
                _ => None,
            })
            .collect();
        let missing_tokens = observations
            .iter()
            .filter(|observation| matches!(observation.result, PassTokenResult::Missing))
            .count();
        let blank_tokens = observations
            .iter()
            .filter(|observation| matches!(observation.result, PassTokenResult::Blank))
            .count();
        let dropped_receivers = observations
            .iter()
            .filter(|observation| matches!(observation.result, PassTokenResult::ReceiverDropped))
            .count();
        let unique_token_count = tokens.iter().copied().collect::<FxHashSet<_>>().len();
        token_receivers_with_token = Some(tokens.len());
        token_receivers_missing = Some(missing_tokens);
        token_receivers_blank = Some(blank_tokens);
        token_receivers_dropped = Some(dropped_receivers);
        token_unique_values = Some(unique_token_count);
        let bounded_stream_truncated = bounds_truncated.load(Ordering::Relaxed);
        let repair_entire_enumeration = (!streaming_result.enumeration_complete
            || streaming_result.enumeration_errors > 0)
            && !bounded_stream_truncated;
        let initial_evidence = if bounded_stream_truncated {
            ZoneTokenEvidence::Incomplete {
                reason: TokenGap::EnumerationIncomplete,
            }
        } else if repair_entire_enumeration {
            ZoneTokenEvidence::Recoverable {
                passes: observations
                    .iter()
                    .map(|observation| observation.pass.clone())
                    .collect(),
                reason: TokenGap::EnumerationIncomplete,
            }
        } else {
            classify_zone_token_evidence(&observations)
        };
        match initial_evidence {
            ZoneTokenEvidence::Complete { token } => Some(token),
            ZoneTokenEvidence::Recoverable { passes, reason } if !repair_token_gaps => {
                checkpoint_retry_passes = passes;
                token_block_reason = Some(match reason {
                    TokenGap::ReceiverDropped => "kei_internal_token_receiver_dropped",
                    TokenGap::Blank => "icloud_blank_sync_token",
                    TokenGap::Mismatch => "icloud_sync_token_mismatch",
                    TokenGap::Missing | TokenGap::EnumerationIncomplete => {
                        "icloud_sync_token_missing"
                    }
                });
                None
            }
            ZoneTokenEvidence::Recoverable {
                reason,
                passes: recovery_passes,
            } => {
                let recovery_action = RecoveryAction::RetryPasses(recovery_passes);
                tracing::warn!(
                    reason = ?reason,
                    passes = ?recovery_action.retry_passes(),
                    "Provider checkpoint preserved: pass proof is incomplete; retrying only affected passes now"
                );
                same_cycle_recovery_attempts = MAX_SAME_CYCLE_CHECKPOINT_RECOVERY_ROUNDS;
                let recovery_configs =
                    match build_pass_configs_resolving_deferred_excludes(passes, config).await {
                        Ok(configs) => configs,
                        Err(error) => {
                            tracing::warn!(
                                error = %error,
                                "Same-cycle pass recovery could not rebuild pass exclusions"
                            );
                            Vec::new()
                        }
                    };
                let mut recovery_enumeration_complete = true;
                let mut recovery_enumeration_errors = 0usize;
                let expected_recovery_passes = recovery_action.retry_passes().len();
                let mut completed_recovery_passes = 0usize;
                for pass_key in recovery_action.retry_passes().iter().cloned() {
                    let Some(pass) = passes.get(pass_key.index) else {
                        recovery_enumeration_complete = false;
                        continue;
                    };
                    let Some(pass_config) = recovery_configs.get(pass_key.index).cloned() else {
                        recovery_enumeration_complete = false;
                        continue;
                    };
                    let total_count = pass_stream_counts.get(pass_key.index).copied().flatten();
                    let (stream, token_rx) = open_photo_stream_for_controls(
                        &pass.album,
                        scope_frontier_limit(config, recent_frontier.as_ref()),
                        total_count,
                        config.concurrent_downloads,
                        pass_config.concurrent_downloads,
                        controls,
                        strict_empty_tail_errors,
                    );
                    let stream = filter_stream_to_enumeration_bounds(
                        stream,
                        config,
                        recent_frontier.as_ref(),
                        Arc::clone(&bounds_truncated),
                    );
                    let recovered = run_full_pass_stream(
                        download_client.clone(),
                        stream,
                        token_rx,
                        pass_config,
                        FullPassStreamOptions {
                            pass_index: pass_key.index,
                            controls,
                            count: pass_counts.get(pass_key.index).copied().unwrap_or(0),
                            kind: pass.kind,
                            shutdown_token: shutdown_token.clone(),
                            download_ctx: None,
                            album_snapshot: None,
                        },
                    )
                    .await;
                    let recovered = match recovered {
                        Ok(recovered) => recovered,
                        Err(error) => {
                            recovery_enumeration_complete = false;
                            recovery_enumeration_errors =
                                recovery_enumeration_errors.saturating_add(1);
                            if let Some(observation) = observations
                                .iter_mut()
                                .find(|observation| observation.pass.index == pass_key.index)
                            {
                                observation.result = PassTokenResult::EnumerationIncomplete;
                            }
                            tracing::warn!(
                                pass = ?pass_key,
                                error = %error,
                                "Same-cycle pass recovery did not complete"
                            );
                            continue;
                        }
                    };
                    completed_recovery_passes += 1;
                    let recovered_result = if !recovered.result.enumeration_complete
                        || recovered.result.enumeration_errors > 0
                    {
                        let _ = recovered.token_rx.await;
                        PassTokenResult::EnumerationIncomplete
                    } else {
                        match recovered.token_rx.await {
                            Ok(Some(token)) if token.trim().is_empty() => PassTokenResult::Blank,
                            Ok(Some(token)) => PassTokenResult::Present(token.trim().to_string()),
                            Ok(None) => PassTokenResult::Missing,
                            Err(_) => PassTokenResult::ReceiverDropped,
                        }
                    };
                    recovery_enumeration_complete =
                        recovery_enumeration_complete && recovered.result.enumeration_complete;
                    recovery_enumeration_errors = recovery_enumeration_errors
                        .saturating_add(recovered.result.enumeration_errors);
                    if let Some(observation) = observations
                        .iter_mut()
                        .find(|observation| observation.pass.index == pass_key.index)
                    {
                        observation.result = recovered_result;
                    }
                    merge_token_recovery_result(&mut streaming_result, recovered.result);
                }

                if completed_recovery_passes != expected_recovery_passes {
                    recovery_enumeration_complete = false;
                    recovery_enumeration_errors = recovery_enumeration_errors.saturating_add(
                        expected_recovery_passes.saturating_sub(completed_recovery_passes),
                    );
                }

                if repair_entire_enumeration {
                    streaming_result.enumeration_complete = recovery_enumeration_complete;
                    streaming_result.enumeration_errors = recovery_enumeration_errors;
                }

                let final_tokens: Vec<&str> = observations
                    .iter()
                    .filter_map(|observation| match &observation.result {
                        PassTokenResult::Present(token) => Some(token.as_str()),
                        _ => None,
                    })
                    .collect();
                token_receivers_with_token = Some(final_tokens.len());
                token_receivers_missing = Some(
                    observations
                        .iter()
                        .filter(|observation| {
                            matches!(observation.result, PassTokenResult::Missing)
                        })
                        .count(),
                );
                token_receivers_blank = Some(
                    observations
                        .iter()
                        .filter(|observation| matches!(observation.result, PassTokenResult::Blank))
                        .count(),
                );
                token_receivers_dropped = Some(
                    observations
                        .iter()
                        .filter(|observation| {
                            matches!(observation.result, PassTokenResult::ReceiverDropped)
                        })
                        .count(),
                );
                token_unique_values =
                    Some(final_tokens.iter().copied().collect::<FxHashSet<_>>().len());

                match classify_zone_token_evidence(&observations) {
                    ZoneTokenEvidence::Complete { token }
                        if streaming_result.enumeration_complete
                            && streaming_result.enumeration_errors == 0 =>
                    {
                        same_cycle_recovery_successes = 1;
                        tracing::info!(
                            passes = ?observations.iter().map(|observation| &observation.pass).collect::<Vec<_>>(),
                            "Provider checkpoint proof repaired in the current cycle"
                        );
                        Some(token)
                    }
                    ZoneTokenEvidence::Complete { .. } => {
                        token_block_reason = Some("icloud_sync_token_missing");
                        None
                    }
                    ZoneTokenEvidence::Recoverable { passes, reason } => {
                        checkpoint_retry_passes = passes;
                        token_block_reason = Some(match reason {
                            TokenGap::ReceiverDropped => "kei_internal_token_receiver_dropped",
                            TokenGap::Blank => "icloud_blank_sync_token",
                            TokenGap::Mismatch => "icloud_sync_token_mismatch",
                            TokenGap::Missing | TokenGap::EnumerationIncomplete => {
                                "icloud_sync_token_missing"
                            }
                        });
                        None
                    }
                    ZoneTokenEvidence::Incomplete { reason } => {
                        checkpoint_retry_passes = observations
                            .iter()
                            .map(|observation| observation.pass.clone())
                            .collect();
                        token_block_reason = Some(match reason {
                            TokenGap::ReceiverDropped => "kei_internal_token_receiver_dropped",
                            TokenGap::Blank => "icloud_blank_sync_token",
                            TokenGap::Mismatch => "icloud_sync_token_mismatch",
                            TokenGap::Missing | TokenGap::EnumerationIncomplete => {
                                "icloud_sync_token_missing"
                            }
                        });
                        None
                    }
                }
            }
            ZoneTokenEvidence::Incomplete { reason } => {
                checkpoint_retry_passes = observations
                    .iter()
                    .map(|observation| observation.pass.clone())
                    .collect();
                token_block_reason = Some(match reason {
                    TokenGap::ReceiverDropped => "kei_internal_token_receiver_dropped",
                    TokenGap::Blank => "icloud_blank_sync_token",
                    TokenGap::Mismatch => "icloud_sync_token_mismatch",
                    TokenGap::Missing | TokenGap::EnumerationIncomplete => {
                        "icloud_sync_token_missing"
                    }
                });
                None
            }
        }
    } else {
        None
    };
    let token_eligible = token_attempt_allowed
        && streaming_result.enumeration_complete
        && streaming_result.enumeration_errors == 0;

    // Capture the enumeration-complete signal before
    // `build_download_outcome` consumes `streaming_result`. The marker
    // gate below uses this signal directly so a partial-failure run
    // whose enumeration phase finished still clears the marker.
    let enumeration_complete = streaming_result.enumeration_complete;
    let enumeration_errors = streaming_result.enumeration_errors;
    let tail_probes = if config.recent.is_none() && config.skip_created_before.is_none() {
        pass_stream_counts
            .iter()
            .filter(|count| count.is_some())
            .count()
    } else {
        0
    };
    let count_undercount_assets = exact_total
        .map(|count| streaming_result.assets_seen.saturating_sub(count))
        .unwrap_or(0);

    // Both enumeration branches defer their rewrite queue to here, so a full
    // enumeration drains exactly once. Per-pass pipelines run concurrently and
    // separate drains can otherwise write the same file, where the later write
    // wins whether or not it holds the newer metadata.
    let metadata_flags = MetadataFlags::from(config.as_ref());
    if controls.run_mode.downloads_files()
        && !config.refresh_metadata
        && metadata_flags.has_any_write()
        && let Some(db) = &config.state_db
    {
        streaming_result.exif_failures += metadata_rewrite::run_pending(
            db.as_ref(),
            metadata_flags,
            Arc::clone(&config.temp_suffix),
            &shutdown_token,
        )
        .await
        .failed;
    }

    // Build the outcome using the same logic as download_photos
    let (outcome, mut stats) = build_download_outcome(
        download_client,
        passes,
        config,
        controls,
        streaming_result,
        started,
        shutdown_token,
    )
    .await?;
    stats.pagination_shortfall_warnings = pagination_shortfall_warnings;
    stats.pagination_shortfall_assets = pagination_shortfall_assets;
    stats.tail_probes = tail_probes;
    stats.count_undercount_assets = count_undercount_assets;
    stats.count_probe_failures = len_errors;
    stats.api_total_at_start = api_total_at_start;
    stats.same_cycle_recovery_attempts = same_cycle_recovery_attempts;
    stats.same_cycle_recovery_successes = same_cycle_recovery_successes;
    stats.checkpoint_retry_passes = checkpoint_retry_passes;
    if token_attempt_allowed {
        stats.sync_token_expected_receivers = token_expected_receivers;
        stats.sync_token_receivers_with_token = token_receivers_with_token;
        stats.sync_token_receivers_missing = token_receivers_missing;
        stats.sync_token_receivers_blank = token_receivers_blank;
        stats.sync_token_receivers_dropped = token_receivers_dropped;
        stats.sync_token_unique_values = token_unique_values;
    }
    let bounded_run = config.recent.is_some() || config.skip_created_before.is_some();
    let bounded_limit_truncated = bounded_run
        && (bounds_truncated.load(Ordering::Relaxed)
            || (token_eligible
                && sync_token.is_none()
                && matches!(
                    token_block_reason,
                    Some("icloud_sync_token_missing")
                        | Some("kei_internal_token_receiver_dropped")
                        | Some("sync_token_unavailable")
                        | None
                )));
    if count_lookup_failed && token_eligible && sync_token.is_some() {
        if !stats.sync_token_blocked {
            tracing::warn!(
                count_probe_failures = len_errors,
                "Count probes failed, but records/query completed naturally with a usable \
                 sync token; recording diagnostic and allowing token advancement"
            );
        }
    } else if bounded_limit_truncated
        && !controls.run_mode.only_print_filenames()
        && !controls.run_mode.is_dry_run()
        && enumeration_errors == 0
    {
        let bounded_reason = if config.recent.is_some() {
            RECENT_LIMITED_FULL_ENUMERATION_REASON
        } else {
            DATE_BOUNDED_FULL_ENUMERATION_REASON
        };
        stats.sync_token_blocked = true;
        stats.sync_token_blocked_reason = Some(bounded_reason);
        stats.sync_token_blocked_source = Some(sync_token_blocked_source(bounded_reason));
        stats.sync_token_blocked_explanation = Some(sync_token_blocked_explanation(bounded_reason));
    } else if token_eligible && sync_token.is_none() {
        let reason = token_block_reason.unwrap_or("sync_token_unavailable");
        stats.sync_token_blocked = true;
        stats.sync_token_blocked_reason = Some(reason);
        stats.sync_token_blocked_source = Some(sync_token_blocked_source(reason));
        stats.sync_token_blocked_explanation = Some(sync_token_blocked_explanation(reason));
    } else if count_lookup_failed && !stats.sync_token_blocked {
        stats.sync_token_blocked = true;
        stats.sync_token_blocked_reason = Some(ICLOUD_ALBUM_COUNT_ERROR_REASON);
        stats.sync_token_blocked_source =
            Some(sync_token_blocked_source(ICLOUD_ALBUM_COUNT_ERROR_REASON));
        stats.sync_token_blocked_explanation = Some(sync_token_blocked_explanation(
            ICLOUD_ALBUM_COUNT_ERROR_REASON,
        ));
    } else if token_attempt_allowed
        && sync_token.is_none()
        && let Some(reason) = token_block_reason
    {
        stats.sync_token_blocked = true;
        stats.sync_token_blocked_reason = Some(reason);
        stats.sync_token_blocked_source = Some(sync_token_blocked_source(reason));
        stats.sync_token_blocked_explanation = Some(sync_token_blocked_explanation(reason));
    }

    // Clear enumeration-in-progress markers when the producer reached the
    // natural end of the API stream. The gate ignores download-side
    // failures so a partial-failure cycle whose enumeration finished
    // doesn't leave the marker set forever. Shutdown still suppresses the
    // clear because the producer's cancellation path leaves
    // `enumeration_complete = false`.
    if enumeration_complete && let Some(db) = &config.state_db {
        for zone in &enum_zones {
            if let Err(e) = db.end_enum_progress(zone).await {
                tracing::debug!(error = %e, zone, "Failed to clear enumeration marker");
            }
        }
    }

    Ok(SyncResult {
        outcome,
        sync_token,
        stats,
        full_enumeration_ran: true,
    })
}

#[derive(Debug, Default)]
struct IncrementalDeltaSummary {
    sync_token: Option<String>,
    token_unsafe_reason: Option<&'static str>,
    created_count: u64,
    soft_deleted_count: u64,
    hard_deleted_count: u64,
    hidden_count: u64,
    total_events: u64,
    state_transition_failures: usize,
}

impl IncrementalDeltaSummary {
    fn observe_event(&mut self, event: &ChangeEvent) {
        self.total_events += 1;
        if let Some(reason) = event.token_unsafe_reason {
            self.token_unsafe_reason.get_or_insert(reason);
        }
    }

    fn remember_asset_mapping(
        event: &ChangeEvent,
        asset_to_master: &mut FxHashMap<String, String>,
    ) {
        if let Some(asset) = &event.asset {
            asset_to_master.insert(
                asset.asset_record_name().to_string(),
                asset.id().to_string(),
            );
        }
    }

    async fn persist_asset_mapping(&mut self, event: &ChangeEvent, config: &DownloadConfig) {
        let Some(asset) = &event.asset else {
            return;
        };
        self.persist_asset_mapping_for_asset(asset, config).await;
    }

    async fn persist_asset_mapping_for_asset(
        &mut self,
        asset: &PhotoAsset,
        config: &DownloadConfig,
    ) {
        let library = asset.source_zone().unwrap_or(&config.library);
        self.persist_asset_master_mapping(asset.asset_record_name(), asset.id(), library, config)
            .await;
    }

    async fn select_asset_state_identity(
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

    async fn persist_asset_master_mapping(
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

    fn record_created(&mut self) {
        self.created_count += 1;
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

    fn log_debug(&self) {
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

fn single_unfiled_streaming_pass<'a>(
    passes: &'a [crate::commands::AlbumPass],
    config: &DownloadConfig,
    routing: &IncrementalPassRouting,
) -> Option<&'a crate::commands::AlbumPass> {
    // Keep relation-sensitive cases on the collecting path: selected albums
    // need all relation deltas applied before routing created assets, and
    // `--recent` currently caps after the full delta is known. The unfiled-only
    // path can stream created assets immediately because album relation deltas
    // update state for future cycles but do not change this pass's routing.
    if config.recent.is_some()
        || routing.has_selected_albums()
        || routing.unfiled_passes.len() != 1
        || passes.len() != 1
    {
        return None;
    }

    let index = *routing.unfiled_passes.first()?;
    passes
        .get(index)
        .filter(|pass| pass.kind == crate::commands::PassKind::Unfiled)
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

async fn hydrate_unpaired_created_asset_deltas(
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
                        asset_record_name = state_id.as_str(),
                        library = %config.library,
                        error = %error,
                        "Failed to recover master identity for asset-only delta"
                    );
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

    if pending.is_empty() {
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
            RecordResolution::Present(_)
            | RecordResolution::AssetPresent { .. }
            | RecordResolution::MasterPresent
            | RecordResolution::Unknown
            | RecordResolution::TransientFailure(_) => {
                summary
                    .token_unsafe_reason
                    .get_or_insert(ASSET_DELTA_HYDRATION_INCOMPLETE_REASON);
                tracing::warn!(
                    asset_record_name = %event.record_name,
                    master_record_name,
                    library = %config.library,
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

struct IncrementalAssetHydrationContext<'a> {
    asset_to_master: &'a mut FxHashMap<String, String>,
    complete_delta_assets: &'a mut Vec<PhotoAsset>,
    downloadable_assets: &'a mut Vec<(PhotoAsset, usize)>,
    download_ctx: Option<&'a DownloadContext>,
    claimed_legacy_master_states: &'a mut ClaimedLegacyMasterStates,
    claim_mode: LegacyOwnerClaimMode,
    pass_configs: &'a [Arc<DownloadConfig>],
}

async fn hydrate_missing_selected_relation_assets(
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

fn stream_incremental_assets_for_single_unfiled_pass(
    pass: crate::commands::AlbumPass,
    config: Arc<DownloadConfig>,
    zone_sync_token: String,
    run_mode: DownloadRunMode,
    shutdown_token: CancellationToken,
) -> (
    ReceiverStream<Result<PhotoAsset>>,
    tokio::task::JoinHandle<Result<IncrementalDeltaSummary>>,
) {
    let capacity = config.concurrent_downloads.saturating_mul(2).max(1);
    let (asset_tx, asset_rx) = mpsc::channel::<Result<PhotoAsset>>(capacity);
    let (mut change_stream, token_rx) = pass.album.changes_stream(&zone_sync_token);
    let handle = tokio::spawn(async move {
        let mut summary = IncrementalDeltaSummary::default();
        let routing = IncrementalPassRouting::from_passes(std::slice::from_ref(&pass));
        let planned_album_containers: FxHashMap<&str, &str> = FxHashMap::default();
        let mut ensured_planned_containers: FxHashSet<String> = FxHashSet::default();
        let mut asset_to_master: FxHashMap<String, String> = FxHashMap::default();
        let mut album_events = Vec::new();
        let mut relation_events = Vec::new();
        let mut unpaired_asset_events = Vec::new();

        while let Some(result) = change_stream.next().await {
            if shutdown_token.is_cancelled() {
                break;
            }
            let event = result?;
            summary.observe_event(&event);
            IncrementalDeltaSummary::remember_asset_mapping(&event, &mut asset_to_master);
            summary.persist_asset_mapping(&event, &config).await;

            if event.album.is_some() {
                album_events.push(event);
                continue;
            }
            if event.relation.is_some() {
                relation_events.push(event);
                continue;
            }
            if event.token_unsafe_reason.is_some() {
                continue;
            }

            match event.reason {
                ChangeReason::Created => {
                    summary.record_created();
                    if let Some(asset) = event.asset {
                        if asset_tx.send(Ok(asset)).await.is_err() {
                            return Ok(summary);
                        }
                    } else if matches!(event.record_type.as_deref(), Some("CPLAsset")) {
                        unpaired_asset_events.push(event);
                    }
                }
                ChangeReason::SoftDeleted | ChangeReason::HardDeleted | ChangeReason::Hidden => {
                    summary.apply_source_state_event(&event, &config).await;
                }
            }
        }

        hydrate_unpaired_created_asset_deltas(
            &mut unpaired_asset_events,
            Some(&pass),
            &config,
            &mut summary,
        )
        .await;
        let download_ctx = if run_mode.downloads_files() && !unpaired_asset_events.is_empty() {
            Some(preload_download_context(&config).await)
        } else {
            None
        };
        let mut claimed_legacy_master_states = ClaimedLegacyMasterStates::default();
        for event in unpaired_asset_events {
            if let Some(mut asset) = event.asset {
                if let Some(download_ctx) = download_ctx.as_deref() {
                    let claim_mode = legacy_owner_claim_mode_for_configs(
                        LegacyOwnerClaimMode::Persist,
                        &asset,
                        std::iter::once(config.as_ref()),
                    );
                    let Some(selected_asset) = summary
                        .select_asset_state_identity(
                            asset,
                            &config,
                            download_ctx,
                            &mut claimed_legacy_master_states,
                            claim_mode,
                        )
                        .await
                    else {
                        continue;
                    };
                    asset = selected_asset;
                    apply_changed_provider_metadata(&config, &asset, download_ctx, &mut summary)
                        .await;
                }
                if asset_tx.send(Ok(asset)).await.is_err() {
                    return Ok(summary);
                }
            }
        }

        for event in &album_events {
            apply_incremental_album_delta(event, &config, &mut summary.token_unsafe_reason).await;
        }
        for event in &relation_events {
            apply_incremental_relation_delta(
                event,
                &config,
                &routing,
                &planned_album_containers,
                &mut ensured_planned_containers,
                &asset_to_master,
                &mut summary.token_unsafe_reason,
            )
            .await;
        }

        if let Ok(token) = token_rx.await {
            summary.sync_token = Some(token);
        }
        Ok(summary)
    });

    (ReceiverStream::new(asset_rx), handle)
}

async fn download_photos_incremental_streaming(
    download_client: &Client,
    pass: &crate::commands::AlbumPass,
    pass_config: Arc<DownloadConfig>,
    zone_sync_token: &str,
    controls: DownloadControls,
    started: Instant,
    shutdown_token: CancellationToken,
) -> Result<SyncResult> {
    let (asset_stream, delta_handle) = stream_incremental_assets_for_single_unfiled_pass(
        pass.clone(),
        Arc::clone(&pass_config),
        zone_sync_token.to_string(),
        controls.run_mode,
        shutdown_token.clone(),
    );
    let streaming_result = match stream_and_download_from_stream(
        download_client,
        asset_stream,
        &pass_config,
        controls,
        0,
        shutdown_token.clone(),
        StreamRuntime::new(None, None),
    )
    .await
    {
        Ok(result) => result,
        Err(e) => {
            delta_handle.abort();
            return Err(e);
        }
    };
    let delta_summary = delta_handle
        .await
        .context("incremental changes producer task panicked")??;

    delta_summary.log_debug();

    let (mut outcome, mut stats) = build_download_outcome(
        download_client,
        std::slice::from_ref(pass),
        &pass_config,
        controls,
        streaming_result,
        started,
        shutdown_token.clone(),
    )
    .await?;

    stats.state_write_failures += delta_summary.state_transition_failures;
    stats.interrupted = stats.interrupted || shutdown_token.is_cancelled();
    if let Some(reason) = delta_summary.token_unsafe_reason {
        block_sync_token_for_incremental_delta(&mut stats, reason);
    }
    if delta_summary.state_transition_failures > 0 {
        outcome = merge_download_outcomes(
            &outcome,
            &DownloadOutcome::PartialFailure {
                failed_count: delta_summary.state_transition_failures,
            },
        );
    }

    let sync_token = if controls.run_mode.only_print_filenames() || controls.run_mode.is_dry_run() {
        None
    } else {
        (!stats.sync_token_blocked)
            .then_some(delta_summary.sync_token)
            .flatten()
    };

    Ok(SyncResult {
        outcome,
        sync_token,
        stats,
        full_enumeration_ran: false,
    })
}

/// Incremental delta sync via `changes_stream`.
///
/// Fetches `ChangeEvent`s since the given sync token, filters to
/// downloadable assets, and feeds them through the download pipeline.
async fn download_photos_incremental(
    download_client: &Client,
    passes: &[crate::commands::AlbumPass],
    config: &Arc<DownloadConfig>,
    zone_sync_token: &str,
    controls: DownloadControls,
    shutdown_token: CancellationToken,
) -> Result<SyncResult> {
    let routing = IncrementalPassRouting::from_passes(passes);
    if let Some(pass) = single_unfiled_streaming_pass(passes, config, &routing) {
        let pass_config = Arc::new(config.with_pass(pass));
        return download_photos_incremental_streaming(
            download_client,
            pass,
            pass_config,
            zone_sync_token,
            controls,
            Instant::now(),
            shutdown_token,
        )
        .await;
    }

    download_photos_incremental_collecting(
        download_client,
        passes,
        config,
        zone_sync_token,
        controls,
        shutdown_token,
    )
    .await
}

async fn download_photos_incremental_collecting(
    download_client: &Client,
    passes: &[crate::commands::AlbumPass],
    config: &Arc<DownloadConfig>,
    zone_sync_token: &str,
    controls: DownloadControls,
    shutdown_token: CancellationToken,
) -> Result<SyncResult> {
    download_photos_incremental_collecting_inner(
        download_client,
        passes,
        config,
        zone_sync_token,
        controls,
        shutdown_token,
        INCREMENTAL_PREFLIGHT_URL_REFRESH_AFTER,
    )
    .await
}

async fn apply_changed_provider_metadata(
    config: &DownloadConfig,
    asset: &PhotoAsset,
    download_ctx: &DownloadContext,
    summary: &mut IncrementalDeltaSummary,
) {
    let Some(db) = &config.state_db else {
        return;
    };
    let library = asset.source_zone().unwrap_or(&config.library);
    if !download_ctx.has_provider_metadata_drift(
        library,
        asset.state_id(),
        asset.metadata().metadata_hash.as_deref(),
    ) {
        return;
    }
    let mark_for_rewrite = MetadataFlags::from(config).has_any_write();
    let refresh = db
        .refresh_downloaded_asset_metadata(
            library,
            asset.state_id(),
            asset.metadata(),
            mark_for_rewrite,
            false,
            crate::state::METADATA_CAPTURE_REVISION,
        )
        .await;
    match refresh {
        Ok(updated) if updated > 0 => {}
        Ok(_) => {
            summary.state_transition_failures += 1;
            summary
                .token_unsafe_reason
                .get_or_insert(PROVIDER_METADATA_STATE_WRITE_FAILED_REASON);
            tracing::warn!(
                asset_id = %asset.id(),
                library,
                "Changed provider metadata matched no downloaded state row"
            );
        }
        Err(e) => {
            summary.state_transition_failures += 1;
            summary
                .token_unsafe_reason
                .get_or_insert(PROVIDER_METADATA_STATE_WRITE_FAILED_REASON);
            tracing::warn!(
                asset_id = %asset.id(),
                library,
                error = %e,
                "Failed to persist changed provider metadata"
            );
        }
    }
}

async fn run_collecting_metadata_rewrite_batch(
    config: &DownloadConfig,
    run_mode: DownloadRunMode,
    shutdown_token: &CancellationToken,
) -> usize {
    if !run_mode.downloads_files() {
        return 0;
    }
    let Some(db) = &config.state_db else {
        return 0;
    };
    let metadata = MetadataFlags::from(config);
    if !metadata.has_any_write() {
        return 0;
    }
    metadata_rewrite::run_pending(
        db.as_ref(),
        metadata,
        Arc::clone(&config.temp_suffix),
        shutdown_token,
    )
    .await
    .failed
}

async fn download_photos_incremental_collecting_inner(
    download_client: &Client,
    passes: &[crate::commands::AlbumPass],
    config: &Arc<DownloadConfig>,
    zone_sync_token: &str,
    controls: DownloadControls,
    shutdown_token: CancellationToken,
    preflight_url_refresh_after: Duration,
) -> Result<SyncResult> {
    let started = Instant::now();
    let legacy_owner_claim_mode = if controls.run_mode.downloads_files() {
        LegacyOwnerClaimMode::Persist
    } else {
        LegacyOwnerClaimMode::ReadOnly
    };
    let pass_configs = build_pass_configs(passes, config);

    // Each asset is paired with its source pass index so both `{album}`
    // expansion and per-pass exclusion (notably, the unfiled pass's set
    // that prevents assets already in some user album from downloading
    // twice) can be applied downstream.
    let mut downloadable_assets: Vec<(PhotoAsset, usize)> = Vec::new();
    let mut change_events = Vec::new();
    let mut first_download_url_obtained_at: Option<Instant> = None;
    let mut delta_summary = IncrementalDeltaSummary::default();
    let routing = IncrementalPassRouting::from_passes(passes);
    let selected_container_ids = routing.selected_container_refs();

    // `changes_stream` is zone-scoped, not album-scoped. Query it once and
    // fan created assets out through the selected passes locally; querying
    // once per pass repeats the same `/changes/zone` pages on every watch
    // cycle with work.
    if let Some(pass) = passes.first() {
        let phase_started = Instant::now();
        let (change_stream, token_rx) = pass.album.changes_stream(zone_sync_token);
        tokio::pin!(change_stream);

        while let Some(result) = change_stream.next().await {
            if shutdown_token.is_cancelled() {
                break;
            }
            let event = result?;
            if first_download_url_obtained_at.is_none() && event.asset.is_some() {
                first_download_url_obtained_at = Some(Instant::now());
            }
            delta_summary.observe_event(&event);
            change_events.push(event);
        }

        if let Ok(token) = token_rx.await {
            delta_summary.sync_token = Some(token);
        }
        tracing::debug!(
            phase_elapsed = %format_duration(phase_started.elapsed()),
            elapsed = %format_duration(started.elapsed()),
            events = change_events.len(),
            "Incremental changes read complete"
        );
    }

    let phase_started = Instant::now();
    let mut asset_to_master: FxHashMap<String, String> = FxHashMap::default();
    let mut download_ctx = if change_events
        .iter()
        .any(|event| event.reason == ChangeReason::Created)
    {
        Some(preload_download_context(config).await)
    } else {
        None
    };
    for event in &change_events {
        IncrementalDeltaSummary::remember_asset_mapping(event, &mut asset_to_master);
        delta_summary.persist_asset_mapping(event, config).await;
    }
    hydrate_unpaired_created_asset_deltas(
        &mut change_events,
        passes.first(),
        config,
        &mut delta_summary,
    )
    .await;
    let mut claimed_legacy_master_states = ClaimedLegacyMasterStates::default();
    if let Some(download_ctx) = download_ctx.as_deref() {
        for event in &mut change_events {
            if event.reason != ChangeReason::Created {
                continue;
            }
            let Some(asset) = event.asset.take() else {
                continue;
            };
            let claim_mode = legacy_owner_claim_mode_for_configs(
                legacy_owner_claim_mode,
                &asset,
                pass_configs.iter().map(AsRef::as_ref),
            );
            event.asset = delta_summary
                .select_asset_state_identity(
                    asset,
                    config,
                    download_ctx,
                    &mut claimed_legacy_master_states,
                    claim_mode,
                )
                .await;
        }
    }
    let mut complete_delta_assets: Vec<PhotoAsset> = change_events
        .iter()
        .filter(|event| {
            event.reason == ChangeReason::Created
                && event.album.is_none()
                && event.relation.is_none()
                && event.token_unsafe_reason.is_none()
        })
        .filter_map(|event| event.asset.clone())
        .collect();
    for event in &change_events {
        IncrementalDeltaSummary::remember_asset_mapping(event, &mut asset_to_master);
    }
    let planned_album_containers: FxHashMap<&str, &str> = passes
        .iter()
        .filter(|pass| pass.kind == crate::commands::PassKind::Album)
        .filter_map(|pass| {
            pass.album
                .container_id()
                .map(|container_id| (container_id, pass.album.name.as_ref()))
        })
        .collect();
    let mut ensured_planned_containers: FxHashSet<String> = FxHashSet::default();

    for event in &change_events {
        apply_incremental_album_delta(event, config, &mut delta_summary.token_unsafe_reason).await;
    }
    tracing::debug!(
        phase_elapsed = %format_duration(phase_started.elapsed()),
        elapsed = %format_duration(started.elapsed()),
        "Incremental album state phase complete"
    );

    let phase_started = Instant::now();
    let downloadable_before_relation_hydration = downloadable_assets.len();
    {
        let mut hydration_context = IncrementalAssetHydrationContext {
            asset_to_master: &mut asset_to_master,
            complete_delta_assets: &mut complete_delta_assets,
            downloadable_assets: &mut downloadable_assets,
            download_ctx: download_ctx.as_deref(),
            claimed_legacy_master_states: &mut claimed_legacy_master_states,
            claim_mode: legacy_owner_claim_mode,
            pass_configs: &pass_configs,
        };
        hydrate_missing_selected_relation_assets(
            &change_events,
            passes,
            config,
            &routing,
            &mut hydration_context,
            &mut delta_summary,
        )
        .await;
    }
    if first_download_url_obtained_at.is_none()
        && downloadable_assets.len() > downloadable_before_relation_hydration
    {
        first_download_url_obtained_at = Some(Instant::now());
    }
    let hydrated_assets = downloadable_assets
        .len()
        .saturating_sub(downloadable_before_relation_hydration);
    tracing::debug!(
        phase_elapsed = %format_duration(phase_started.elapsed()),
        elapsed = %format_duration(started.elapsed()),
        hydrated_assets,
        "Incremental relation hydration phase complete"
    );

    if controls.run_mode.downloads_files() && !complete_delta_assets.is_empty() {
        let context = match download_ctx.take() {
            Some(context) => context,
            None => preload_download_context(config).await,
        };
        let mut refreshed_assets = FxHashSet::default();
        for asset in &complete_delta_assets {
            if !refreshed_assets.insert(asset.state_id_arc()) {
                continue;
            }
            apply_changed_provider_metadata(config, asset, &context, &mut delta_summary).await;
        }
        download_ctx = Some(context);
    }

    let phase_started = Instant::now();
    for event in &change_events {
        apply_incremental_relation_delta(
            event,
            config,
            &routing,
            &planned_album_containers,
            &mut ensured_planned_containers,
            &asset_to_master,
            &mut delta_summary.token_unsafe_reason,
        )
        .await;
    }

    for event in &change_events {
        if event.album.is_some() || event.relation.is_some() || event.token_unsafe_reason.is_some()
        {
            continue;
        }
        match event.reason {
            ChangeReason::Created => {
                delta_summary.record_created();
                if let Some(asset) = &event.asset {
                    match route_incremental_asset_to_passes(
                        asset,
                        &routing,
                        &selected_container_ids,
                        config,
                    )
                    .await
                    {
                        Ok(pass_indices) => {
                            for pass_index in pass_indices {
                                downloadable_assets.push((asset.clone(), pass_index));
                            }
                        }
                        Err(e) => {
                            tracing::warn!(
                                asset_record_name = %asset.asset_record_name(),
                                asset_id = %asset.id(),
                                error = %e,
                                "Failed to route incremental asset through album membership state"
                            );
                            delta_summary
                                .token_unsafe_reason
                                .get_or_insert(ALBUM_RELATION_HYDRATION_INCOMPLETE_REASON);
                        }
                    }
                }
            }
            ChangeReason::SoftDeleted | ChangeReason::HardDeleted | ChangeReason::Hidden => {
                delta_summary.apply_source_state_event(event, config).await;
            }
        }
    }
    tracing::debug!(
        phase_elapsed = %format_duration(phase_started.elapsed()),
        elapsed = %format_duration(started.elapsed()),
        downloadable_assets = downloadable_assets.len(),
        "Incremental routing phase complete"
    );

    delta_summary.log_debug();

    if downloadable_assets.is_empty() {
        let rewrite_failures =
            run_collecting_metadata_rewrite_batch(config, controls.run_mode, &shutdown_token).await;
        let mut stats = SyncStats {
            elapsed_secs: started.elapsed().as_secs_f64(),
            state_write_failures: delta_summary.state_transition_failures,
            exif_failures: rewrite_failures,
            interrupted: shutdown_token.is_cancelled(),
            ..SyncStats::default()
        };
        if let Some(reason) = delta_summary.token_unsafe_reason {
            block_sync_token_for_incremental_delta(&mut stats, reason);
        }
        tracing::info!("No new photos to download from incremental sync");
        tracing::info!(elapsed = %format_duration(started.elapsed()), "  completed");
        let sync_token = if controls.run_mode.only_print_filenames() {
            None
        } else {
            (!stats.sync_token_blocked)
                .then_some(delta_summary.sync_token)
                .flatten()
        };
        return Ok(SyncResult {
            outcome: if delta_summary.state_transition_failures > 0 || rewrite_failures > 0 {
                DownloadOutcome::PartialFailure {
                    failed_count: delta_summary
                        .state_transition_failures
                        .saturating_add(rewrite_failures),
                }
            } else {
                DownloadOutcome::Success
            },
            sync_token,
            stats,
            full_enumeration_ran: false,
        });
    }

    let download_ctx = match download_ctx {
        Some(download_ctx) => download_ctx,
        None => preload_download_context(config).await,
    };
    let mut planning_state_write_failures = 0usize;

    // Respect --recent: cap the number of assets to download
    if let Some(recent) = config.recent {
        let limit = recent as usize;
        if downloadable_assets.len() > limit {
            tracing::debug!(
                total = downloadable_assets.len(),
                limit,
                "Capping incremental assets to --recent limit"
            );
            downloadable_assets.truncate(limit);
        }
    }

    tracing::debug!(
        count = downloadable_assets.len(),
        "Assets to download from incremental sync"
    );

    // Convert assets to download tasks via path-aware on-disk verification.
    // Each pass (concrete album or unfiled) gets its own derived config so
    // that both album-specific path expansion and per-pass exclude sets are
    // applied. Configs are cached per pass index to avoid redundant
    // allocations when many assets flow through the same pass.
    let mut tasks: Vec<DownloadTask> = Vec::new();
    let mut task_planner = planner::TaskPlanner::new();
    let mut skip_breakdown = SkipBreakdown::default();
    let mut enumeration_errors = 0usize;
    // Incremental routing already decides whether a changed asset belongs to
    // a selected album pass or the unfiled pass from trusted membership
    // state. Re-enumerating every album here to rebuild unfiled excludes
    // delays consumption of the fresh signed download URLs from
    // `changes/zone`, which can make those URLs expire before downloads
    // start on large album sets.
    let phase_started = Instant::now();
    let mut retry_sources: FxHashMap<RetryTaskKey, UrlRetrySource> = FxHashMap::default();

    for (asset, pass_index) in &downloadable_assets {
        #[allow(
            clippy::indexing_slicing,
            reason = "pass_index was assigned by the producer from the same `passes` slice \
                      that pass_configs was built from; indices are valid"
        )]
        let effective_config = &pass_configs[*pass_index];

        let mut plan = task_planner.plan_asset(asset, effective_config).await;
        if let Some(reason) = plan.filter_reason {
            skip_breakdown.record_filter_reason(reason);
            continue;
        }
        if let Some(resource) = &plan.malformed_resource {
            enumeration_errors += 1;
            tracing::error!(
                asset_id = %asset.id(),
                field = %resource.field,
                reason = %resource.reason,
                "Malformed CloudKit resource prevented incremental download planning"
            );
            continue;
        }

        let mut state_skipped = 0usize;
        let planned_tasks = std::mem::take(&mut plan.tasks);
        plan.tasks.reserve(planned_tasks.len());
        for task in planned_tasks {
            let keep = match download_ctx.should_download_fast(
                &task.library,
                &task.asset_id,
                task.version_size,
                &task.checksum,
                false,
            ) {
                Some(false) => {
                    state_skipped = state_skipped.saturating_add(1);
                    false
                }
                None => {
                    let exists = state_confirmed_current_path_exists(
                        &download_ctx,
                        effective_config,
                        asset,
                        &task,
                        &mut task_planner,
                    )
                    .await
                    .is_some();
                    if exists {
                        state_skipped = state_skipped.saturating_add(1);
                    }
                    !exists
                }
                Some(true) => true,
            };
            if keep {
                plan.tasks.push(task);
            }
        }
        skip_breakdown.by_state = skip_breakdown.by_state.saturating_add(state_skipped);

        for task in &plan.tasks {
            retry_sources.insert(
                RetryTaskKey::from(task),
                UrlRetrySource {
                    asset_record_name: asset.asset_record_name_arc(),
                    pass_index: *pass_index,
                },
            );
        }

        // Upsert state records so mark_downloaded/mark_failed can find them.
        // Without this, the UPDATE in mark_downloaded matches 0 rows and the
        // file ends up on disk but untracked in the state DB.
        if let Some(db) = &config.state_db {
            for task in &plan.tasks {
                if let Err(e) =
                    planner::upsert_seen_for_task(db.as_ref(), effective_config, asset, task).await
                {
                    planning_state_write_failures = planning_state_write_failures.saturating_add(1);
                    tracing::warn!(
                        asset_id = %task.asset_id,
                        error = %e,
                        "Failed to record asset in state DB"
                    );
                }
            }
            // Record this asset's membership in the current album so
            // consumers (EXIF keywords, XMP sidecars, Immich albums) can
            // reconstruct the logical album graph from the state DB.
            if let Err(e) =
                planner::record_album_membership_if_named(db.as_ref(), effective_config, asset)
                    .await
                && let Some(album_name) = effective_config.album_name.as_deref()
            {
                planning_state_write_failures = planning_state_write_failures.saturating_add(1);
                tracing::warn!(
                    asset_id = %asset.id(),
                    album = %album_name,
                    library = %effective_config.library,
                    error = %e,
                    "Failed to record album membership after retries"
                );
            }
        }

        if plan.tasks.is_empty() && state_skipped == 0 {
            skip_breakdown.on_disk += 1;
        }
        tasks.extend(plan.tasks);
    }
    tracing::debug!(
        phase_elapsed = %format_duration(phase_started.elapsed()),
        elapsed = %format_duration(started.elapsed()),
        planned_tasks = tasks.len(),
        "Incremental task planning phase complete"
    );

    if skip_breakdown.by_state > 0 {
        tracing::debug!(
            skipped = skip_breakdown.by_state,
            "Skipped already-downloaded assets (state DB)"
        );
    }

    if tasks.is_empty() {
        let rewrite_failures =
            run_collecting_metadata_rewrite_batch(config, controls.run_mode, &shutdown_token).await;
        let mut stats = SyncStats {
            skipped: skip_breakdown,
            enumeration_errors,
            state_write_failures: delta_summary
                .state_transition_failures
                .saturating_add(planning_state_write_failures),
            exif_failures: rewrite_failures,
            elapsed_secs: started.elapsed().as_secs_f64(),
            interrupted: shutdown_token.is_cancelled(),
            ..SyncStats::default()
        };
        if let Some(reason) = delta_summary.token_unsafe_reason {
            block_sync_token_for_incremental_delta(&mut stats, reason);
        }
        tracing::info!("All incremental assets already downloaded or filtered");
        tracing::info!(elapsed = %format_duration(started.elapsed()), "  completed");
        let failed_count = enumeration_errors
            .saturating_add(delta_summary.state_transition_failures)
            .saturating_add(planning_state_write_failures)
            .saturating_add(rewrite_failures);
        let outcome = if failed_count > 0 {
            DownloadOutcome::PartialFailure { failed_count }
        } else {
            DownloadOutcome::Success
        };
        let sync_token = if controls.run_mode.only_print_filenames() {
            None
        } else {
            (enumeration_errors == 0 && !stats.sync_token_blocked)
                .then_some(delta_summary.sync_token)
                .flatten()
        };
        return Ok(SyncResult {
            outcome,
            sync_token,
            stats,
            full_enumeration_ran: false,
        });
    }

    if controls.run_mode.only_print_filenames() {
        #[allow(
            clippy::print_stdout,
            reason = "--only-print-filenames writes target paths to stdout so callers can pipe to xargs/etc"
        )]
        for task in &tasks {
            println!("{}", task.download_path.display());
        }
        let mut stats = SyncStats {
            skipped: skip_breakdown,
            enumeration_errors,
            state_write_failures: delta_summary
                .state_transition_failures
                .saturating_add(planning_state_write_failures),
            elapsed_secs: started.elapsed().as_secs_f64(),
            ..SyncStats::default()
        };
        if let Some(reason) = delta_summary.token_unsafe_reason {
            block_sync_token_for_incremental_delta(&mut stats, reason);
        }
        // Don't advance the sync token — this is a read-only operation.
        return Ok(SyncResult {
            outcome: if enumeration_errors > 0 || delta_summary.state_transition_failures > 0 {
                DownloadOutcome::PartialFailure {
                    failed_count: enumeration_errors + delta_summary.state_transition_failures,
                }
            } else {
                DownloadOutcome::Success
            },
            sync_token: None,
            stats,
            full_enumeration_ran: false,
        });
    }

    if controls.run_mode.downloads_files() {
        tasks = refresh_stale_incremental_tasks_before_download(
            passes,
            &pass_configs,
            &retry_sources,
            tasks,
            first_download_url_obtained_at,
            preflight_url_refresh_after,
            shutdown_token.clone(),
        )
        .await;
    }

    let task_count = tasks.len();
    tracing::info!(
        count = task_count,
        url_age_secs = ?first_download_url_obtained_at.map(|instant| instant.elapsed().as_secs_f64()),
        "Downloading files from incremental sync"
    );

    // Run the download pass on the collected tasks
    let pass_config = PassConfig {
        client: download_client,
        retry_config: &config.retry,
        metadata: MetadataFlags::from(config.as_ref()),
        mark_capture_repair_after_download: matches!(
            config.capture_timestamp_repair,
            CaptureTimestampRepair::ReplaceWithCaptureLocal
        ),
        concurrency: config.concurrent_downloads,
        reporting: controls.reporting,
        temp_suffix: Arc::clone(&config.temp_suffix),
        shutdown_token: shutdown_token.clone(),
        state_db: config.state_db.clone(),
        rate_limit_counter: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        bandwidth_limiter: config.bandwidth_limiter.clone(),
        library: Arc::clone(&config.library),
    };
    let planned_tasks = tasks.clone();
    let mut pass_result = run_download_pass(pass_config, tasks).await;

    if pass_result.url_expired_abort
        && pass_result.auth_errors < AUTH_ERROR_THRESHOLD
        && !shutdown_token.is_cancelled()
    {
        let downloaded_keys: FxHashSet<RetryTaskKey> = pass_result
            .downloaded_tasks
            .iter()
            .map(RetryTaskKey::from)
            .collect();
        let expired_retry_candidates: Vec<DownloadTask> = planned_tasks
            .iter()
            .filter(|task| !downloaded_keys.contains(&RetryTaskKey::from(*task)))
            .cloned()
            .collect();
        let retry_tasks = match build_incremental_expired_url_retry_tasks(
            passes,
            &pass_configs,
            &retry_sources,
            &expired_retry_candidates,
            shutdown_token.clone(),
        )
        .await
        {
            Ok(tasks) => tasks,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "Could not refresh expired incremental download URLs; leaving failures for replay"
                );
                Vec::new()
            }
        };
        if !retry_tasks.is_empty() {
            let retry_task_count = retry_tasks.len();
            tracing::info!(
                count = retry_task_count,
                "Refreshing expired incremental download URLs and retrying failed tasks"
            );
            let refreshed_keys: FxHashSet<RetryTaskKey> =
                retry_tasks.iter().map(RetryTaskKey::from).collect();
            let retry_pass_config = PassConfig {
                client: download_client,
                retry_config: &config.retry,
                metadata: MetadataFlags::from(config.as_ref()),
                mark_capture_repair_after_download: matches!(
                    config.capture_timestamp_repair,
                    CaptureTimestampRepair::ReplaceWithCaptureLocal
                ),
                concurrency: config.concurrent_downloads,
                reporting: controls.reporting,
                temp_suffix: Arc::clone(&config.temp_suffix),
                shutdown_token: shutdown_token.clone(),
                state_db: config.state_db.clone(),
                rate_limit_counter: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                bandwidth_limiter: config.bandwidth_limiter.clone(),
                library: Arc::clone(&config.library),
            };
            let retry_result = run_download_pass(retry_pass_config, retry_tasks).await;
            merge_expired_url_retry_result(
                &mut pass_result,
                expired_retry_candidates,
                refreshed_keys,
                retry_result,
            );
        } else if !expired_retry_candidates.is_empty() {
            pass_result.failed = expired_retry_candidates;
        }
    }

    pass_result.exif_failures = pass_result.exif_failures.saturating_add(
        run_collecting_metadata_rewrite_batch(config, controls.run_mode, &shutdown_token).await,
    );

    let failed = pass_result.failed.len();
    let succeeded = pass_result.downloaded;

    // Log failed downloads before the summary
    if failed > 0 {
        for task in &pass_result.failed {
            tracing::error!(asset_id = %task.asset_id, path = %task.download_path.display(), "Download failed");
        }
    }

    let mut stats = SyncStats {
        assets_seen: 0, // incremental doesn't have total library count
        downloaded: succeeded,
        failed,
        skipped: skip_breakdown,
        bytes_downloaded: pass_result.bytes_downloaded,
        disk_bytes_written: pass_result.disk_bytes_written,
        exif_failures: pass_result.exif_failures,
        state_write_failures: pass_result
            .state_write_failures
            .saturating_add(delta_summary.state_transition_failures)
            .saturating_add(planning_state_write_failures),
        enumeration_errors,
        pagination_shortfall_warnings: 0,
        pagination_shortfall_assets: 0,
        sync_token_blocked: false,
        sync_token_blocked_reason: None,
        elapsed_secs: started.elapsed().as_secs_f64(),
        interrupted: shutdown_token.is_cancelled()
            || pass_result.auth_errors >= AUTH_ERROR_THRESHOLD,
        rate_limited: pass_result.rate_limit_observations,
        photos_downloaded: pass_result.photos_downloaded,
        videos_downloaded: pass_result.videos_downloaded,
        recap: pass_result.recap.clone(),
        ..SyncStats::default()
    };
    if let Some(reason) = delta_summary.token_unsafe_reason {
        block_sync_token_for_incremental_delta(&mut stats, reason);
    }
    log_sync_summary(
        "\u{2500}\u{2500} Incremental Sync Summary \u{2500}\u{2500}",
        &stats,
    );

    if pass_result.auth_errors >= AUTH_ERROR_THRESHOLD {
        return Ok(SyncResult {
            outcome: DownloadOutcome::SessionExpired {
                auth_error_count: pass_result.auth_errors,
            },
            sync_token: (!stats.sync_token_blocked)
                .then_some(delta_summary.sync_token)
                .flatten(),
            stats,
            full_enumeration_ran: false,
        });
    }

    let outcome = if failed > 0
        || pass_result.exif_failures > 0
        || pass_result.state_write_failures > 0
        || delta_summary.state_transition_failures > 0
        || planning_state_write_failures > 0
        || enumeration_errors > 0
    {
        DownloadOutcome::PartialFailure {
            failed_count: failed
                + pass_result.exif_failures
                + pass_result.state_write_failures
                + delta_summary.state_transition_failures
                + planning_state_write_failures
                + enumeration_errors,
        }
    } else {
        DownloadOutcome::Success
    };

    Ok(SyncResult {
        outcome,
        sync_token: (enumeration_errors == 0 && !stats.sync_token_blocked)
            .then_some(delta_summary.sync_token)
            .flatten(),
        stats,
        full_enumeration_ran: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::{AlbumPass, PassKind};
    use crate::icloud::photos::{PhotoAlbum, PhotoAlbumConfig, PhotosSession};
    use crate::state::SqliteStateDb;
    use crate::test_helpers::{
        DynamicRecentPhotosSession, MockPhotosFlow, TestAssetRecord, TracingCapture,
        mock_photo_query_page, mock_photo_records_for_zone_with_filename,
        mock_photo_records_for_zone_with_filename_and_asset_date,
    };
    use chrono::TimeZone;
    use serde_json::{Value, json};
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use tempfile::TempDir;
    use tokio::time::Duration;

    fn reqwest_status_error(status: u16) -> anyhow::Error {
        let response = http::Response::builder()
            .status(status)
            .body(Vec::<u8>::new())
            .expect("response");
        reqwest::Response::from(response)
            .error_for_status()
            .expect_err("status should be an error")
            .into()
    }

    fn classify_incremental_error_for(error: SyncTokenError) -> IncrementalErrorClass {
        let error = anyhow::Error::new(error);
        classify_incremental_error(&error)
    }

    #[test]
    fn classify_incremental_error_detects_token_fallback_errors() {
        assert_eq!(
            classify_incremental_error_for(SyncTokenError::InvalidToken {
                reason: "expired".into(),
            }),
            IncrementalErrorClass::TokenFallback
        );
        assert_eq!(
            classify_incremental_error_for(SyncTokenError::ZoneNotFound {
                zone_name: "PrimarySync".into(),
            }),
            IncrementalErrorClass::TokenFallback
        );
    }

    #[test]
    fn classify_incremental_error_detects_transient_errors() {
        let auth_error: anyhow::Error = crate::auth::error::AuthError::ApiError {
            code: 503,
            message: "unavailable".into(),
        }
        .into();
        assert_eq!(
            classify_incremental_error(&auth_error),
            IncrementalErrorClass::TransientFailure
        );

        let reqwest_429 = reqwest_status_error(429);
        assert_eq!(
            classify_incremental_error(&reqwest_429),
            IncrementalErrorClass::TransientFailure
        );

        let reqwest_503 = reqwest_status_error(503);
        assert_eq!(
            classify_incremental_error(&reqwest_503),
            IncrementalErrorClass::TransientFailure
        );
    }

    #[test]
    fn classify_incremental_error_treats_static_and_generic_errors_as_fallback() {
        assert_eq!(
            classify_incremental_error_for(SyncTokenError::UnexpectedZoneError {
                zone_name: "PrimarySync".into(),
                error_code: "TRY_AGAIN_LATER".into(),
            }),
            IncrementalErrorClass::StaticFallback
        );

        let reqwest_400 = reqwest_status_error(400);
        assert_eq!(
            classify_incremental_error(&reqwest_400),
            IncrementalErrorClass::StaticFallback
        );

        let generic = anyhow::anyhow!("decode failed");
        assert_eq!(
            classify_incremental_error(&generic),
            IncrementalErrorClass::StaticFallback
        );
    }

    #[test]
    fn classify_incremental_error_detects_context_wrapped_errors() {
        let token = anyhow::Error::new(SyncTokenError::InvalidToken {
            reason: "expired".into(),
        })
        .context("changes/zone");
        assert_eq!(
            classify_incremental_error(&token),
            IncrementalErrorClass::TokenFallback
        );

        let transient = reqwest_status_error(503).context("changes/zone");
        assert_eq!(
            classify_incremental_error(&transient),
            IncrementalErrorClass::TransientFailure
        );
    }

    fn test_config() -> DownloadConfig {
        DownloadConfig::test_default()
    }

    fn retry_test_task(asset_id: &str, version_size: VersionSizeKey, path: &str) -> DownloadTask {
        DownloadTask {
            url: format!("https://p01.icloud-content.com/{asset_id}").into(),
            download_path: Path::new("/tmp/codex/kei/retry-tests").join(path),
            publication: file::FinalPublication::NoReplace,
            checksum: "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=".into(),
            asset_id: Arc::from(asset_id),
            asset_record_name: Arc::from(asset_id),
            library: Arc::from("PrimarySync"),
            metadata: Arc::new(filter::MetadataPayload::default()),
            size: 1024,
            created_local: chrono::Local::now().fixed_offset(),
            version_size,
            media_type: crate::state::MediaType::Photo,
        }
    }

    #[test]
    fn cleanup_retry_filter_keeps_only_exact_failed_task_keys() {
        let failed = retry_test_task("ASSET_A", VersionSizeKey::Original, "a.jpg");
        let matching_refresh = DownloadTask {
            url: "https://p01.icloud-content.com/fresh-a".into(),
            ..failed.clone()
        };
        let wrong_version = retry_test_task("ASSET_A", VersionSizeKey::Medium, "a.jpg");
        let wrong_path = retry_test_task("ASSET_A", VersionSizeKey::Original, "elsewhere/a.jpg");
        let unrelated = retry_test_task("ASSET_B", VersionSizeKey::Original, "b.jpg");
        let mut pending_keys: FxHashSet<RetryTaskKey> =
            std::iter::once(RetryTaskKey::from(&failed)).collect();
        let mut out = Vec::new();

        take_matching_retry_tasks(
            vec![
                wrong_version,
                wrong_path,
                unrelated,
                matching_refresh.clone(),
            ],
            &mut pending_keys,
            &mut out,
        );

        assert!(pending_keys.is_empty());
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].asset_id.as_ref(), "ASSET_A");
        assert_eq!(out[0].version_size, VersionSizeKey::Original);
        assert_eq!(out[0].download_path, matching_refresh.download_path);
        assert_eq!(
            out[0].url.as_ref(),
            "https://p01.icloud-content.com/fresh-a"
        );
    }

    #[test]
    fn pending_retry_filter_matches_asset_version_without_path() {
        let original = retry_test_task("ASSET_A", VersionSizeKey::Original, "old/a.jpg");
        let refreshed_elsewhere =
            retry_test_task("ASSET_A", VersionSizeKey::Original, "new/location/a.jpg");
        let wrong_version = retry_test_task("ASSET_A", VersionSizeKey::Medium, "new/a.jpg");
        let unrelated = retry_test_task("ASSET_B", VersionSizeKey::Original, "new/b.jpg");
        let mut pending_targets: FxHashSet<PendingRetryTarget> =
            std::iter::once(PendingRetryTarget {
                library: Arc::from("PrimarySync"),
                asset_id: Arc::clone(&original.asset_id),
                version_size: original.version_size,
            })
            .collect();
        let mut out = Vec::new();

        take_matching_pending_retry_tasks(
            vec![wrong_version, unrelated, refreshed_elsewhere.clone()],
            &mut pending_targets,
            &mut out,
        );

        assert!(pending_targets.is_empty());
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].asset_id.as_ref(), "ASSET_A");
        assert_eq!(out[0].version_size, VersionSizeKey::Original);
        assert_eq!(out[0].download_path, refreshed_elsewhere.download_path);
    }

    fn changes_album(name: &str, session: impl PhotosSession + 'static) -> PhotoAlbum {
        changes_album_with_container(name, None, session)
    }

    fn changes_album_with_container(
        name: &str,
        container_id: Option<&str>,
        session: impl PhotosSession + 'static,
    ) -> PhotoAlbum {
        PhotoAlbum::new(
            PhotoAlbumConfig {
                params: Arc::new(HashMap::new()),
                service_endpoint: Arc::from("https://example.com"),
                name: Arc::from(name),
                list_type: Arc::from("CPLAssetAndMasterByAssetDateWithoutHiddenOrDeleted"),
                obj_type: Arc::from("CPLAssetByAssetDateWithoutHiddenOrDeleted"),
                query_filter: None,
                page_size: 100,
                zone_id: Arc::new(json!({"zoneName": "PrimarySync"})),
                retry_config: RetryConfig::default(),
                container_id: container_id.map(Arc::from),
                cross_zone_sources: Vec::new(),
            },
            Box::new(session),
        )
    }

    fn unused_unfiled_changes_pass() -> AlbumPass {
        AlbumPass {
            kind: PassKind::Unfiled,
            album: changes_album(
                "",
                changes_zone_session(Arc::new(AtomicUsize::new(0)), Vec::new()),
            ),
            exclude_ids: Arc::new(FxHashSet::default()),
        }
    }

    #[derive(Clone)]
    struct CountingChangesZoneSession {
        changes_zone_calls: Arc<AtomicUsize>,
        count_query_calls: Arc<AtomicUsize>,
        records_query_calls: Arc<AtomicUsize>,
        records: Vec<Value>,
        query_page: Value,
        asset_count: u64,
    }

    fn changes_zone_session(
        changes_zone_calls: Arc<AtomicUsize>,
        records: Vec<Value>,
    ) -> CountingChangesZoneSession {
        changes_zone_session_with_query_page(changes_zone_calls, records, json!({"records": []}), 0)
    }

    fn changes_zone_session_with_query_page(
        changes_zone_calls: Arc<AtomicUsize>,
        records: Vec<Value>,
        query_page: Value,
        asset_count: u64,
    ) -> CountingChangesZoneSession {
        CountingChangesZoneSession {
            changes_zone_calls,
            count_query_calls: Arc::new(AtomicUsize::new(0)),
            records_query_calls: Arc::new(AtomicUsize::new(0)),
            records,
            query_page,
            asset_count,
        }
    }

    impl CountingChangesZoneSession {
        fn count_query_count(&self) -> usize {
            self.count_query_calls.load(Ordering::SeqCst)
        }

        fn records_query_count(&self) -> usize {
            self.records_query_calls.load(Ordering::SeqCst)
        }
    }

    #[async_trait::async_trait]
    impl PhotosSession for CountingChangesZoneSession {
        async fn post(
            &self,
            url: &str,
            _body: String,
            _headers: &[(&str, &str)],
        ) -> anyhow::Result<Value> {
            if url.contains("/changes/zone?") {
                self.changes_zone_calls.fetch_add(1, Ordering::SeqCst);
                return Ok(json!({
                    "zones": [{
                        "zoneID": {"zoneName": "PrimarySync", "ownerRecordName": "_defaultOwner"},
                        "syncToken": "zone-token-next",
                        "moreComing": false,
                        "records": self.records.clone(),
                    }]
                }));
            }

            if url.contains("/internal/records/query/batch") {
                self.count_query_calls.fetch_add(1, Ordering::SeqCst);
                return Ok(json!({
                    "batch": [{"records": [{"fields": {"itemCount": {"value": self.asset_count}}}]}]
                }));
            }

            if url.contains("/records/lookup?") || url.contains("/records/query?") {
                self.records_query_calls.fetch_add(1, Ordering::SeqCst);
                return Ok(self.query_page.clone());
            }

            Ok(json!({"records": []}))
        }

        fn clone_box(&self) -> Box<dyn PhotosSession> {
            Box::new(self.clone())
        }
    }

    #[derive(Clone)]
    struct CountingQuerySession {
        count_query_calls: Arc<AtomicUsize>,
        records_query_calls: Arc<AtomicUsize>,
        page: Value,
        asset_count: u64,
        fail_records_query: bool,
    }

    impl CountingQuerySession {
        fn new(page: Value, asset_count: u64) -> Self {
            Self {
                count_query_calls: Arc::new(AtomicUsize::new(0)),
                records_query_calls: Arc::new(AtomicUsize::new(0)),
                page,
                asset_count,
                fail_records_query: false,
            }
        }

        fn failing(page: Value, asset_count: u64) -> Self {
            Self {
                fail_records_query: true,
                ..Self::new(page, asset_count)
            }
        }

        fn count_query_count(&self) -> usize {
            self.count_query_calls.load(Ordering::SeqCst)
        }

        fn records_query_count(&self) -> usize {
            self.records_query_calls.load(Ordering::SeqCst)
        }
    }

    #[async_trait::async_trait]
    impl PhotosSession for CountingQuerySession {
        async fn post(
            &self,
            url: &str,
            _body: String,
            _headers: &[(&str, &str)],
        ) -> anyhow::Result<Value> {
            if url.contains("/internal/records/query/batch") {
                self.count_query_calls.fetch_add(1, Ordering::SeqCst);
                return Ok(json!({
                    "batch": [{"records": [{"fields": {"itemCount": {"value": self.asset_count}}}]}]
                }));
            }

            if url.contains("/records/query?") {
                let call = self.records_query_calls.fetch_add(1, Ordering::SeqCst);
                if self.fail_records_query {
                    anyhow::bail!("smart folder refresh failed");
                }
                if call == 0 {
                    return Ok(self.page.clone());
                }
                return Ok(json!({
                    "records": [],
                    "syncToken": self.page.get("syncToken").cloned().unwrap_or(Value::Null)
                }));
            }

            Ok(json!({"records": []}))
        }

        fn clone_box(&self) -> Box<dyn PhotosSession> {
            Box::new(self.clone())
        }
    }

    #[derive(Clone)]
    struct BackfillAndChangesSession {
        changes_zone_calls: Arc<AtomicUsize>,
    }

    #[derive(Clone)]
    struct RelationHydrationSession {
        incremental_calls: Arc<AtomicUsize>,
        hydrate_calls: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl PhotosSession for RelationHydrationSession {
        async fn post(
            &self,
            url: &str,
            body: String,
            _headers: &[(&str, &str)],
        ) -> anyhow::Result<Value> {
            if !url.contains("/changes/zone?") {
                return Ok(json!({"records": []}));
            }

            let request: Value = serde_json::from_str(&body)?;
            let sync_token = request["zones"]
                .as_array()
                .and_then(|zones| zones.first())
                .and_then(|zone| zone.get("syncToken"))
                .and_then(Value::as_str);
            let records = if sync_token == Some("zone-token-prev") {
                self.incremental_calls.fetch_add(1, Ordering::SeqCst);
                vec![relation_delta_record(
                    "container-vacation",
                    "asset-MASTER_HYDRATED",
                )]
            } else {
                self.hydrate_calls.fetch_add(1, Ordering::SeqCst);
                incremental_photo_records("MASTER_HYDRATED")
            };

            Ok(json!({
                "zones": [{
                    "zoneID": {"zoneName": "PrimarySync", "ownerRecordName": "_defaultOwner"},
                    "syncToken": "zone-token-next",
                    "moreComing": false,
                    "records": records,
                }]
            }))
        }

        fn clone_box(&self) -> Box<dyn PhotosSession> {
            Box::new(self.clone())
        }
    }

    #[async_trait::async_trait]
    impl PhotosSession for BackfillAndChangesSession {
        async fn post(
            &self,
            url: &str,
            _body: String,
            _headers: &[(&str, &str)],
        ) -> anyhow::Result<Value> {
            if url.contains("/internal/records/query/batch") {
                return Ok(json!({
                    "batch": [{"records": [{"fields": {"itemCount": {"value": 0}}}]}]
                }));
            }

            if url.contains("/changes/zone?") {
                self.changes_zone_calls.fetch_add(1, Ordering::SeqCst);
                return Ok(json!({
                    "zones": [{
                        "zoneID": {"zoneName": "PrimarySync", "ownerRecordName": "_defaultOwner"},
                        "syncToken": "zone-token-next",
                        "moreComing": false,
                        "records": [],
                    }]
                }));
            }

            Ok(json!({"records": [], "syncToken": "album-token"}))
        }

        fn clone_box(&self) -> Box<dyn PhotosSession> {
            Box::new(self.clone())
        }
    }

    fn smart_folder_unfiled_passes(
        changes_calls: Arc<AtomicUsize>,
        unfiled_records: Vec<Value>,
        smart_session: CountingQuerySession,
    ) -> Vec<AlbumPass> {
        vec![
            AlbumPass {
                kind: PassKind::SmartFolder,
                album: changes_album("Favorites", smart_session),
                exclude_ids: Arc::new(FxHashSet::default()),
            },
            AlbumPass {
                kind: PassKind::Unfiled,
                album: changes_album("", changes_zone_session(changes_calls, unfiled_records)),
                exclude_ids: Arc::new(FxHashSet::default()),
            },
        ]
    }

    fn incremental_test_config(dir: &TempDir) -> DownloadConfig {
        let mut config = test_config();
        config.directory = Arc::from(dir.path());
        config.sync_mode = SyncMode::Incremental {
            zone_sync_token: "zone-token-prev".to_string(),
        };
        config
    }

    async fn run_print_incremental_sync(
        passes: &[AlbumPass],
        config: DownloadConfig,
    ) -> SyncResult {
        download_photos_with_sync(
            &Client::new(),
            passes,
            Arc::new(config),
            DownloadControls::new(DownloadRunMode::PrintFilenames, DownloadReporting::hidden()),
            CancellationToken::new(),
        )
        .await
        .expect("print-only incremental sync should succeed")
    }

    fn incremental_photo_records(record_name: &str) -> Vec<Value> {
        vec![
            json!({
                "recordName": record_name,
                "recordType": "CPLMaster",
                "fields": {
                    "filenameEnc": {"value": "changed.jpg", "type": "STRING"},
                    "resOriginalRes": {
                        "value": {
                            "downloadURL": "https://p01.icloud-content.com/changed.jpg",
                            "size": 1024,
                            "fileChecksum": "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA="
                        }
                    },
                    "resOriginalWidth": {"value": 100, "type": "INT64"},
                    "resOriginalHeight": {"value": 100, "type": "INT64"},
                    "resOriginalFileType": {"value": "public.jpeg"},
                    "itemType": {"value": "public.jpeg"},
                    "adjustmentRenderType": {"value": 0, "type": "INT64"}
                },
                "recordChangeTag": "ct1"
            }),
            json!({
                "recordName": format!("asset-{record_name}"),
                "recordType": "CPLAsset",
                "fields": {
                    "masterRef": {
                        "value": {
                            "recordName": record_name,
                            "zoneID": {"zoneName": "PrimarySync"}
                        },
                        "type": "REFERENCE"
                    },
                    "assetDate": {"value": 1700000000000i64, "type": "TIMESTAMP"},
                    "addedDate": {"value": 1700000000000i64, "type": "TIMESTAMP"}
                },
                "recordChangeTag": "ct2"
            }),
        ]
    }

    fn incremental_photo_records_with_favorite(record_name: &str, is_favorite: bool) -> Vec<Value> {
        let mut records = incremental_photo_records(record_name);
        records[1]["fields"]["isFavorite"] =
            json!({"value": i64::from(is_favorite), "type": "INT64"});
        records
    }

    async fn seed_downloaded_metadata_asset(
        db: &SqliteStateDb,
        config: &DownloadConfig,
        pass: &AlbumPass,
        asset: &PhotoAsset,
    ) -> PathBuf {
        let pass_config = config.with_pass(pass);
        let expected = filter::expected_paths_for(asset, &pass_config)
            .into_iter()
            .next()
            .expect("asset should have an expected path");
        tokio::fs::create_dir_all(expected.path.parent().expect("path has parent"))
            .await
            .expect("create expected parent");
        tokio::fs::write(
            &expected.path,
            vec![0u8; usize::try_from(expected.size).expect("test asset size fits usize")],
        )
        .await
        .expect("seed existing media");
        let filename = expected
            .path
            .file_name()
            .and_then(|name| name.to_str())
            .expect("path has UTF-8 filename");
        let record = TestAssetRecord::new(asset.state_id())
            .library(asset.source_zone().unwrap_or(&pass_config.library))
            .checksum(&expected.checksum)
            .filename(filename)
            .created_at(asset.created())
            .size(expected.size)
            .version_size(expected.version_size)
            .metadata(asset.metadata().clone())
            .build();
        db.upsert_seen(&record).await.expect("seed state row");
        db.mark_downloaded(
            asset.source_zone().unwrap_or(&pass_config.library),
            asset.state_id(),
            expected.version_size.as_str(),
            &expected.path,
            "seeded-local-sha256",
            None,
        )
        .await
        .expect("mark seeded media downloaded");
        expected.path
    }

    async fn relation_removed_metadata_edit_fixture(
        db: &Arc<SqliteStateDb>,
        dir: &TempDir,
    ) -> (Vec<AlbumPass>, DownloadConfig, PathBuf) {
        seed_complete_album_snapshot(
            db,
            "container-vacation",
            "Vacation",
            &[("asset-ROUTED_METADATA", "ROUTED_METADATA")],
        )
        .await;
        let stored_records = incremental_photo_records_with_favorite("ROUTED_METADATA", false);
        let stored_asset = PhotoAsset::new(stored_records[0].clone(), stored_records[1].clone());
        let changed_records = incremental_photo_records_with_favorite("ROUTED_METADATA", true);
        let mut delta_records = vec![relation_delete_record(
            "container-vacation",
            "asset-ROUTED_METADATA",
        )];
        delta_records.extend(changed_records);
        let pass = AlbumPass {
            kind: PassKind::Album,
            album: changes_album_with_container(
                "Vacation",
                Some("container-vacation"),
                changes_zone_session(Arc::new(AtomicUsize::new(0)), delta_records),
            ),
            exclude_ids: Arc::new(FxHashSet::default()),
        };
        let mut config = test_config();
        config.directory = Arc::from(dir.path());
        config.folder_structure_albums = Arc::from("{album}");
        config.state_db = Some(Arc::clone(db) as Arc<dyn DownloadStore>);
        let media_path = seed_downloaded_metadata_asset(db, &config, &pass, &stored_asset).await;

        (vec![pass], config, media_path)
    }

    fn incremental_photo_records_with_url(
        record_name: &str,
        filename: &str,
        download_url: &str,
        size: u64,
    ) -> Vec<Value> {
        let mut records =
            mock_photo_records_for_zone_with_filename(record_name, "PrimarySync", filename);
        records[0]["fields"]["resOriginalRes"]["value"]["downloadURL"] = json!(download_url);
        records[0]["fields"]["resOriginalRes"]["value"]["size"] = json!(size);
        records
    }

    #[tokio::test]
    async fn incremental_multi_page_unfiled_streams_through_bounded_pipeline() {
        let session = MockPhotosFlow::new()
            .changes_zone_page(
                incremental_photo_records_with_url(
                    "PAGE_ONE",
                    "page1.jpg",
                    "https://p01.icloud-content.com/page1.jpg",
                    1024,
                ),
                "zone-token-page-1",
                true,
            )
            .changes_zone_page(
                incremental_photo_records_with_url(
                    "PAGE_TWO",
                    "page2.jpg",
                    "https://p01.icloud-content.com/page2.jpg",
                    1024,
                ),
                "zone-token-after",
                false,
            )
            .build();
        let passes = vec![AlbumPass {
            kind: PassKind::Unfiled,
            album: mock_album("Library", session),
            exclude_ids: Arc::new(FxHashSet::default()),
        }];

        let dir = TempDir::new().expect("temp dir");
        let db = Arc::new(SqliteStateDb::open_in_memory().expect("state db"));
        let mut config = test_config();
        config.directory = Arc::from(dir.path());
        config.state_db = Some(db);
        config.concurrent_downloads = 1;

        let result = download_photos_incremental(
            &Client::new(),
            &passes,
            &Arc::new(config),
            "zone-token-before",
            DownloadControls::dry_run_hidden(),
            CancellationToken::new(),
        )
        .await
        .expect("multi-page incremental sync should succeed");

        assert!(
            matches!(result.outcome, DownloadOutcome::Success),
            "result: {result:?}"
        );
        assert_eq!(result.sync_token, None);
        assert_eq!(result.stats.downloaded, 2);
    }

    fn album_delta_record(container_id: &str, album_name: &str) -> Value {
        json!({
            "recordName": container_id,
            "recordType": "CPLAlbum",
            "fields": {
                "albumName": {"value": album_name}
            }
        })
    }

    fn deleted_album_delta_record(container_id: &str) -> Value {
        json!({
            "recordName": container_id,
            "recordType": "CPLAlbum",
            "fields": {},
            "deleted": true,
        })
    }

    fn relation_delta_record(container_id: &str, asset_record_name: &str) -> Value {
        json!({
            "recordName": format!("{asset_record_name}-IN-{container_id}"),
            "recordType": "CPLContainerRelation",
            "fields": {
                "containerId": {"value": container_id},
                "itemId": {"value": asset_record_name}
            }
        })
    }

    fn relation_delete_record(container_id: &str, asset_record_name: &str) -> Value {
        json!({
            "recordName": format!("{asset_record_name}-IN-{container_id}"),
            "recordType": "CPLContainerRelation",
            "deleted": true
        })
    }

    async fn seed_complete_album_snapshot(
        db: &SqliteStateDb,
        container_id: &str,
        album_name: &str,
        memberships: &[(&str, &str)],
    ) {
        db.upsert_album_container("PrimarySync", container_id, album_name, "album")
            .await
            .unwrap();
        let generation = db
            .start_album_membership_snapshot("PrimarySync", container_id, Some("hash-test"))
            .await
            .unwrap();
        for (asset_record_name, master_record_name) in memberships {
            db.add_album_membership_to_snapshot(
                "PrimarySync",
                container_id,
                generation,
                asset_record_name,
                Some(master_record_name),
                "icloud",
            )
            .await
            .unwrap();
        }
        db.complete_album_membership_snapshot("PrimarySync", container_id, generation)
            .await
            .unwrap();
    }

    fn unparsable_relation_delete_record() -> Value {
        json!({
            "recordName": "not-a-relation-delta",
            "recordType": "CPLContainerRelation",
            "deleted": true
        })
    }

    #[derive(Clone)]
    struct ConcurrentRecordsSession {
        in_flight_records_queries: Arc<AtomicUsize>,
        max_in_flight_records_queries: Arc<AtomicUsize>,
        records_delay: Duration,
    }

    impl ConcurrentRecordsSession {
        fn new(records_delay: Duration) -> Self {
            Self {
                in_flight_records_queries: Arc::new(AtomicUsize::new(0)),
                max_in_flight_records_queries: Arc::new(AtomicUsize::new(0)),
                records_delay,
            }
        }

        fn max_in_flight(&self) -> usize {
            self.max_in_flight_records_queries.load(Ordering::SeqCst)
        }

        fn note_records_query_start(&self) {
            let current = self
                .in_flight_records_queries
                .fetch_add(1, Ordering::SeqCst)
                + 1;
            let mut observed = self.max_in_flight_records_queries.load(Ordering::SeqCst);
            while current > observed {
                match self.max_in_flight_records_queries.compare_exchange(
                    observed,
                    current,
                    Ordering::SeqCst,
                    Ordering::SeqCst,
                ) {
                    Ok(_) => break,
                    Err(next) => observed = next,
                }
            }
        }
    }

    #[async_trait::async_trait]
    impl PhotosSession for ConcurrentRecordsSession {
        async fn post(
            &self,
            url: &str,
            _body: String,
            _headers: &[(&str, &str)],
        ) -> anyhow::Result<Value> {
            if url.contains("/internal/records/query/batch") {
                return Ok(json!({
                    "batch": [{"records": [{"fields": {"itemCount": {"value": 0}}}]}]
                }));
            }

            if url.contains("/records/query?") {
                self.note_records_query_start();
                tokio::time::sleep(self.records_delay).await;
                self.in_flight_records_queries
                    .fetch_sub(1, Ordering::SeqCst);
                return Ok(json!({
                    "records": [],
                    "syncToken": "zone-token"
                }));
            }

            Ok(json!({"records": []}))
        }

        fn clone_box(&self) -> Box<dyn PhotosSession> {
            Box::new(self.clone())
        }
    }

    fn probe_album(name: &str, session: ConcurrentRecordsSession) -> PhotoAlbum {
        PhotoAlbum::new(
            PhotoAlbumConfig {
                params: Arc::new(HashMap::new()),
                service_endpoint: Arc::from("https://example.com"),
                name: Arc::from(name),
                list_type: Arc::from("CPLAssetAndMasterByAssetDateWithoutHiddenOrDeleted"),
                obj_type: Arc::from("CPLAssetByAssetDateWithoutHiddenOrDeleted"),
                query_filter: None,
                page_size: 100,
                zone_id: Arc::new(json!({"zoneName": "PrimarySync"})),
                retry_config: RetryConfig::default(),
                container_id: None,
                cross_zone_sources: Vec::new(),
            },
            Box::new(session),
        )
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum DeferredOrderKind {
        Album,
        Unfiled,
    }

    #[derive(Clone)]
    struct DeferredOrderSession {
        kind: DeferredOrderKind,
        album_done: Arc<AtomicBool>,
        unfiled_started_too_early: Arc<AtomicBool>,
        record_name: Arc<str>,
    }

    impl DeferredOrderSession {
        fn new_pair() -> (Self, Self) {
            let album_done = Arc::new(AtomicBool::new(false));
            let unfiled_started_too_early = Arc::new(AtomicBool::new(false));
            (
                Self {
                    kind: DeferredOrderKind::Album,
                    album_done: Arc::clone(&album_done),
                    unfiled_started_too_early: Arc::clone(&unfiled_started_too_early),
                    record_name: Arc::from("ORDER_ALBUM"),
                },
                Self {
                    kind: DeferredOrderKind::Unfiled,
                    album_done,
                    unfiled_started_too_early,
                    record_name: Arc::from("ORDER_UNFILED"),
                },
            )
        }

        fn unfiled_started_too_early(&self) -> bool {
            self.unfiled_started_too_early.load(Ordering::SeqCst)
        }
    }

    #[async_trait::async_trait]
    impl PhotosSession for DeferredOrderSession {
        async fn post(
            &self,
            url: &str,
            _body: String,
            _headers: &[(&str, &str)],
        ) -> anyhow::Result<Value> {
            if url.contains("/internal/records/query/batch") {
                return Ok(json!({
                    "batch": [{"records": [{"fields": {"itemCount": {"value": 1}}}]}]
                }));
            }

            if url.contains("/records/query?") {
                match self.kind {
                    DeferredOrderKind::Album => {
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        self.album_done.store(true, Ordering::SeqCst);
                    }
                    DeferredOrderKind::Unfiled => {
                        if !self.album_done.load(Ordering::SeqCst) {
                            self.unfiled_started_too_early.store(true, Ordering::SeqCst);
                        }
                    }
                }
                return Ok(json!({
                    "records": mock_photo_records_for_zone_with_filename(
                        &self.record_name,
                        "PrimarySync",
                        &format!("{}.jpg", self.record_name),
                    ),
                    "syncToken": "zone-token",
                }));
            }

            Ok(json!({"records": []}))
        }

        fn clone_box(&self) -> Box<dyn PhotosSession> {
            Box::new(self.clone())
        }
    }

    fn mock_album(name: &str, session: crate::test_helpers::MockPhotosSession) -> PhotoAlbum {
        album_with_session("PrimarySync", name, Box::new(session))
    }

    fn mock_album_with_container(
        name: &str,
        container_id: &str,
        session: crate::test_helpers::MockPhotosSession,
    ) -> PhotoAlbum {
        album_with_session_and_container("PrimarySync", name, Some(container_id), Box::new(session))
    }

    #[derive(Clone, Debug)]
    struct SharedChangesZoneSession {
        responses: Arc<std::sync::Mutex<std::collections::VecDeque<Value>>>,
    }

    impl SharedChangesZoneSession {
        fn new(responses: Vec<Value>) -> Self {
            Self {
                responses: Arc::new(std::sync::Mutex::new(responses.into())),
            }
        }
    }

    #[async_trait::async_trait]
    impl PhotosSession for SharedChangesZoneSession {
        async fn post(
            &self,
            _url: &str,
            _body: String,
            _headers: &[(&str, &str)],
        ) -> anyhow::Result<Value> {
            self.responses
                .lock()
                .expect("poisoned")
                .pop_front()
                .ok_or_else(|| anyhow::anyhow!("unexpected extra changes/zone call"))
        }

        fn clone_box(&self) -> Box<dyn PhotosSession> {
            Box::new(self.clone())
        }
    }

    #[derive(Clone, Debug)]
    struct RecoveringPassTokenSession {
        query_calls: Arc<AtomicUsize>,
        records: Arc<Vec<Value>>,
    }

    #[derive(Clone, Debug)]
    struct RecoveringIncompletePassSession {
        query_calls: Arc<AtomicUsize>,
        records: Arc<Vec<Value>>,
    }

    #[async_trait::async_trait]
    impl PhotosSession for RecoveringIncompletePassSession {
        async fn post(
            &self,
            url: &str,
            _body: String,
            _headers: &[(&str, &str)],
        ) -> anyhow::Result<Value> {
            if url.contains("/internal/records/query/batch") {
                return Ok(json!({
                    "batch": [{"records": [{"fields": {"itemCount": {"value": 1}}}]}]
                }));
            }
            if !url.contains("/records/query?") {
                return Ok(json!({"records": []}));
            }

            let call = self.query_calls.fetch_add(1, Ordering::SeqCst);
            if call == 0 {
                return Ok(json!({"records": "malformed"}));
            }
            if call == 1 {
                return Ok(json!({
                    "records": self.records.as_ref().clone(),
                    "syncToken": "zone-token-recovered"
                }));
            }
            Ok(json!({"records": [], "syncToken": "zone-token-recovered"}))
        }

        fn clone_box(&self) -> Box<dyn PhotosSession> {
            Box::new(self.clone())
        }
    }

    #[async_trait::async_trait]
    impl PhotosSession for RecoveringPassTokenSession {
        async fn post(
            &self,
            url: &str,
            _body: String,
            _headers: &[(&str, &str)],
        ) -> anyhow::Result<Value> {
            if url.contains("/internal/records/query/batch") {
                return Ok(json!({
                    "batch": [{"records": [{"fields": {"itemCount": {"value": 1}}}]}]
                }));
            }
            if !url.contains("/records/query?") {
                return Ok(json!({"records": []}));
            }

            let call = self.query_calls.fetch_add(1, Ordering::SeqCst);
            let recovery_round = call >= 6;
            let first_page = call.is_multiple_of(6);
            Ok(match (first_page, recovery_round) {
                (true, false) => json!({"records": self.records.as_ref().clone()}),
                (false, false) => json!({"records": []}),
                (true, true) => json!({
                    "records": self.records.as_ref().clone(),
                    "syncToken": "zone-token-recovered"
                }),
                (false, true) => {
                    json!({"records": [], "syncToken": "zone-token-recovered"})
                }
            })
        }

        fn clone_box(&self) -> Box<dyn PhotosSession> {
            Box::new(self.clone())
        }
    }

    #[derive(Clone, Debug)]
    struct SplitChangesZoneSession {
        delta_responses: Arc<std::sync::Mutex<std::collections::VecDeque<Value>>>,
        hydrate_responses: Arc<std::sync::Mutex<std::collections::VecDeque<Value>>>,
    }

    impl SplitChangesZoneSession {
        fn new(delta_responses: Vec<Value>, hydrate_responses: Vec<Value>) -> Self {
            Self {
                delta_responses: Arc::new(std::sync::Mutex::new(delta_responses.into())),
                hydrate_responses: Arc::new(std::sync::Mutex::new(hydrate_responses.into())),
            }
        }
    }

    #[async_trait::async_trait]
    impl PhotosSession for SplitChangesZoneSession {
        async fn post(
            &self,
            _url: &str,
            body: String,
            _headers: &[(&str, &str)],
        ) -> anyhow::Result<Value> {
            let responses = if body.contains("\"syncToken\"") {
                &self.delta_responses
            } else {
                &self.hydrate_responses
            };
            responses
                .lock()
                .expect("poisoned")
                .pop_front()
                .ok_or_else(|| anyhow::anyhow!("unexpected extra changes/zone call"))
        }

        fn clone_box(&self) -> Box<dyn PhotosSession> {
            Box::new(self.clone())
        }
    }

    #[derive(Clone, Debug)]
    struct AssetOnlyExpiredUrlSession {
        delta_records: Arc<Vec<Value>>,
        lookup_records: Arc<Vec<Value>>,
        hydration_records: Arc<Vec<Value>>,
    }

    #[async_trait::async_trait]
    impl PhotosSession for AssetOnlyExpiredUrlSession {
        async fn post(
            &self,
            url: &str,
            body: String,
            _headers: &[(&str, &str)],
        ) -> anyhow::Result<Value> {
            if url.contains("/records/lookup?") {
                return Ok(json!({"records": self.lookup_records.as_ref().clone()}));
            }
            if url.contains("/changes/zone?") {
                let request: Value = serde_json::from_str(&body)?;
                let has_sync_token = request["zones"]
                    .as_array()
                    .and_then(|zones| zones.first())
                    .and_then(|zone| zone.get("syncToken"))
                    .is_some();
                let records = if has_sync_token {
                    self.delta_records.as_ref().clone()
                } else {
                    self.hydration_records.as_ref().clone()
                };
                return Ok(changes_zone_response(records, "zone-token-next"));
            }
            Ok(json!({"records": []}))
        }

        fn clone_box(&self) -> Box<dyn PhotosSession> {
            Box::new(self.clone())
        }
    }

    #[derive(Clone, Debug)]
    struct PendingRetryExpiredUrlSession {
        query_calls: Arc<AtomicUsize>,
        expired_records: Arc<Vec<Value>>,
        fresh_records: Arc<Vec<Value>>,
    }

    impl PendingRetryExpiredUrlSession {
        fn new(expired_records: Vec<Value>, fresh_records: Vec<Value>) -> Self {
            Self {
                query_calls: Arc::new(AtomicUsize::new(0)),
                expired_records: Arc::new(expired_records),
                fresh_records: Arc::new(fresh_records),
            }
        }
    }

    #[async_trait::async_trait]
    impl PhotosSession for PendingRetryExpiredUrlSession {
        async fn post(
            &self,
            url: &str,
            body: String,
            _headers: &[(&str, &str)],
        ) -> anyhow::Result<Value> {
            if url.contains("/changes/zone?") {
                let request: Value = serde_json::from_str(&body)?;
                let has_sync_token = request["zones"]
                    .as_array()
                    .and_then(|zones| zones.first())
                    .and_then(|zone| zone.get("syncToken"))
                    .and_then(Value::as_str)
                    .is_some();
                let records = if has_sync_token {
                    Vec::new()
                } else {
                    self.fresh_records.as_ref().clone()
                };
                return Ok(changes_zone_response(records, "zone-token-next"));
            }

            if url.contains("/records/lookup?") || url.contains("/records/query?") {
                let call = self.query_calls.fetch_add(1, Ordering::SeqCst);
                return Ok(if call == 0 {
                    json!({
                        "records": self.expired_records.as_ref().clone(),
                        "syncToken": "ignored-query-token"
                    })
                } else {
                    json!({"records": [], "syncToken": "ignored-query-token"})
                });
            }

            Ok(json!({"records": []}))
        }

        fn clone_box(&self) -> Box<dyn PhotosSession> {
            Box::new(self.clone())
        }
    }

    #[derive(Clone, Debug)]
    struct PendingLookupSession {
        records: Arc<Vec<Value>>,
    }

    #[async_trait::async_trait]
    impl PhotosSession for PendingLookupSession {
        async fn post(
            &self,
            url: &str,
            _body: String,
            _headers: &[(&str, &str)],
        ) -> anyhow::Result<Value> {
            if url.contains("/changes/zone?") {
                return Ok(changes_zone_response(Vec::new(), "zone-token-next"));
            }
            if url.contains("/records/lookup?") {
                return Ok(json!({"records": self.records.as_ref().clone()}));
            }
            Ok(json!({"records": [], "syncToken": "ignored-query-token"}))
        }

        fn clone_box(&self) -> Box<dyn PhotosSession> {
            Box::new(self.clone())
        }
    }

    #[derive(Clone, Debug)]
    struct BoundedPolicyLookupSession {
        records: Arc<Vec<Value>>,
    }

    #[async_trait::async_trait]
    impl PhotosSession for BoundedPolicyLookupSession {
        async fn post(
            &self,
            url: &str,
            _body: String,
            _headers: &[(&str, &str)],
        ) -> anyhow::Result<Value> {
            if url.contains("/records/lookup?") {
                return Ok(json!({"records": self.records.as_ref().clone()}));
            }
            if url.contains("/changes/zone?") {
                return Ok(json!({
                    "zones": [{
                        "zoneID": {
                            "zoneName": "PrimarySync",
                            "ownerRecordName": "_defaultOwner"
                        },
                        "moreComing": false,
                        "records": []
                    }]
                }));
            }
            Ok(json!({"records": []}))
        }

        fn clone_box(&self) -> Box<dyn PhotosSession> {
            Box::new(self.clone())
        }
    }

    #[derive(Clone, Debug)]
    struct LegacyPendingHydrationSession {
        lookup_records: Arc<Vec<Value>>,
        hydration_records: Arc<Vec<Value>>,
        hydration_error: Option<Arc<str>>,
    }

    #[async_trait::async_trait]
    impl PhotosSession for LegacyPendingHydrationSession {
        async fn post(
            &self,
            url: &str,
            body: String,
            _headers: &[(&str, &str)],
        ) -> anyhow::Result<Value> {
            if url.contains("/records/lookup?") {
                return Ok(json!({"records": self.lookup_records.as_ref().clone()}));
            }
            if url.contains("/changes/zone?") {
                let request: Value = serde_json::from_str(&body)?;
                let has_sync_token = request["zones"]
                    .as_array()
                    .and_then(|zones| zones.first())
                    .and_then(|zone| zone.get("syncToken"))
                    .is_some();
                let records = if has_sync_token {
                    Vec::new()
                } else {
                    if let Some(error) = &self.hydration_error {
                        anyhow::bail!(error.to_string());
                    }
                    self.hydration_records.as_ref().clone()
                };
                return Ok(changes_zone_response(records, "zone-token-next"));
            }
            Ok(json!({"records": [], "syncToken": "ignored-query-token"}))
        }

        fn clone_box(&self) -> Box<dyn PhotosSession> {
            Box::new(self.clone())
        }
    }

    fn changes_zone_response(records: Vec<Value>, sync_token: &str) -> Value {
        json!({
            "zones": [{
                "zoneID": {"zoneName": "PrimarySync", "ownerRecordName": "_defaultOwner"},
                "syncToken": sync_token,
                "moreComing": false,
                "records": records,
            }]
        })
    }

    fn album_with_session(zone: &str, name: &str, session: Box<dyn PhotosSession>) -> PhotoAlbum {
        album_with_session_and_container(zone, name, None, session)
    }

    fn album_with_session_and_container(
        zone: &str,
        name: &str,
        container_id: Option<&str>,
        session: Box<dyn PhotosSession>,
    ) -> PhotoAlbum {
        album_with_session_and_retry_config(
            zone,
            name,
            container_id,
            RetryConfig::default(),
            session,
        )
    }

    fn album_with_session_and_retry_config(
        zone: &str,
        name: &str,
        container_id: Option<&str>,
        retry_config: RetryConfig,
        session: Box<dyn PhotosSession>,
    ) -> PhotoAlbum {
        PhotoAlbum::new(
            PhotoAlbumConfig {
                params: Arc::new(HashMap::new()),
                service_endpoint: Arc::from("https://example.com"),
                name: Arc::from(name),
                list_type: Arc::from("CPLAssetAndMasterByAssetDateWithoutHiddenOrDeleted"),
                obj_type: Arc::from("CPLAssetByAssetDateWithoutHiddenOrDeleted"),
                query_filter: None,
                page_size: 100,
                zone_id: Arc::new(json!({"zoneName": zone})),
                retry_config,
                container_id: container_id.map(Arc::from),
                cross_zone_sources: Vec::new(),
            },
            session,
        )
    }

    #[derive(Clone)]
    struct RecentScopeAsset {
        id: String,
        asset_date: i64,
    }

    #[derive(Clone)]
    struct RecentScopeSession {
        all_assets: Arc<Vec<RecentScopeAsset>>,
        album_assets: Arc<Vec<RecentScopeAsset>>,
        all_offsets: Arc<std::sync::Mutex<Vec<u64>>>,
        album_offsets: Arc<std::sync::Mutex<Vec<u64>>>,
    }

    impl RecentScopeSession {
        fn new(all_assets: Vec<RecentScopeAsset>, album_assets: Vec<RecentScopeAsset>) -> Self {
            Self {
                all_assets: Arc::new(all_assets),
                album_assets: Arc::new(album_assets),
                all_offsets: Arc::new(std::sync::Mutex::new(Vec::new())),
                album_offsets: Arc::new(std::sync::Mutex::new(Vec::new())),
            }
        }

        fn album_offsets(&self) -> Vec<u64> {
            self.album_offsets
                .lock()
                .expect("album offsets lock")
                .clone()
        }

        fn page_records(
            assets: &[RecentScopeAsset],
            offset: u64,
            results_limit: u64,
        ) -> Vec<Value> {
            let start = usize::try_from(offset).unwrap_or(usize::MAX);
            let page_assets = usize::try_from(results_limit / 2).unwrap_or(usize::MAX);
            let end = start.saturating_add(page_assets).min(assets.len());
            let mut records = Vec::with_capacity(end.saturating_sub(start) * 2);
            for asset in assets.get(start..end).unwrap_or_default() {
                records.extend(mock_photo_records_for_zone_with_filename_and_asset_date(
                    &asset.id,
                    "PrimarySync",
                    &format!("{}.jpg", asset.id),
                    asset.asset_date,
                ));
            }
            records
        }
    }

    #[async_trait::async_trait]
    impl PhotosSession for RecentScopeSession {
        async fn post(
            &self,
            url: &str,
            body: String,
            _headers: &[(&str, &str)],
        ) -> anyhow::Result<Value> {
            if url.contains("/internal/records/query/batch") {
                return Ok(json!({
                    "batch": [{"records": [{"fields": {"itemCount": {"value": self.all_assets.len() as u64}}}]}]
                }));
            }
            if !url.contains("/records/query?") {
                return Ok(json!({"records": []}));
            }

            let request: Value = serde_json::from_str(&body)?;
            let record_type = request["query"]["recordType"].as_str().unwrap_or_default();
            let offset = request["query"]["filterBy"]
                .as_array()
                .and_then(|filters| {
                    filters.iter().find_map(|filter| {
                        (filter["fieldName"] == "startRank")
                            .then(|| filter["fieldValue"]["value"].as_u64())
                            .flatten()
                    })
                })
                .unwrap_or(0);
            let results_limit = request["resultsLimit"].as_u64().unwrap_or(0);

            let assets = if record_type == "CPLAssetAndMasterByAssetDateWithoutHiddenOrDeleted" {
                self.all_offsets
                    .lock()
                    .expect("all offsets lock")
                    .push(offset);
                self.all_assets.as_ref()
            } else {
                self.album_offsets
                    .lock()
                    .expect("album offsets lock")
                    .push(offset);
                self.album_assets.as_ref()
            };
            let records = Self::page_records(assets, offset, results_limit);
            Ok(json!({"records": records, "syncToken": "zone-token"}))
        }

        fn clone_box(&self) -> Box<dyn PhotosSession> {
            Box::new(self.clone())
        }
    }

    fn recent_scope_album(name: &str, session: RecentScopeSession) -> PhotoAlbum {
        PhotoAlbum::new(
            PhotoAlbumConfig {
                params: Arc::new(HashMap::new()),
                service_endpoint: Arc::from("https://example.com"),
                name: Arc::from(name),
                list_type: Arc::from("CPLContainerRelationLiveByAssetDate"),
                obj_type: Arc::from("CPLContainerRelationNotDeletedByAssetDate:test"),
                query_filter: None,
                page_size: 2,
                zone_id: Arc::new(json!({"zoneName": "PrimarySync"})),
                retry_config: RetryConfig::default(),
                container_id: Some(Arc::from("test")),
                cross_zone_sources: Vec::new(),
            },
            Box::new(session),
        )
    }

    async fn seed_existing_file_for_asset(
        base_config: &mut DownloadConfig,
        pass: &AlbumPass,
        asset: &PhotoAsset,
    ) {
        base_config.file_match_policy = FileMatchPolicy::NameId7;
        let pass_config = base_config.with_pass(pass);
        let expected_path = filter::expected_paths_for(asset, &pass_config)
            .into_iter()
            .next()
            .expect("mock asset should have an expected path");
        tokio::fs::create_dir_all(expected_path.path.parent().expect("path has parent"))
            .await
            .expect("create parent dir");
        tokio::fs::write(&expected_path.path, vec![0u8; 1024])
            .await
            .expect("seed existing file");
        seed_downloaded_state_for_expected_path(base_config, &pass_config, asset, &expected_path)
            .await;
    }

    async fn seed_downloaded_state_for_expected_path(
        base_config: &mut DownloadConfig,
        pass_config: &DownloadConfig,
        asset: &PhotoAsset,
        expected_path: &filter::ExpectedAssetPath,
    ) {
        let db = match &base_config.state_db {
            Some(db) => Arc::clone(db),
            None => {
                let db: Arc<dyn DownloadStore> =
                    Arc::new(SqliteStateDb::open_in_memory().expect("open state db"));
                base_config.state_db = Some(Arc::clone(&db));
                db
            }
        };
        let library = asset.source_zone().unwrap_or(&pass_config.library);
        let filename = expected_path
            .path
            .file_name()
            .and_then(|name| name.to_str())
            .expect("expected path has filename");
        let record = TestAssetRecord::new(asset.state_id())
            .library(library)
            .checksum(&expected_path.checksum)
            .filename(filename)
            .created_at(asset.created())
            .size(expected_path.size)
            .version_size(expected_path.version_size)
            .build();
        db.upsert_seen(&record).await.expect("seed state row");
        db.mark_downloaded(
            library,
            asset.state_id(),
            expected_path.version_size.as_str(),
            &expected_path.path,
            "seeded-local-sha256",
            None,
        )
        .await
        .expect("mark seeded file downloaded");
        assert!(
            !db.should_download(
                library,
                asset.state_id(),
                expected_path.version_size.as_str(),
                &expected_path.checksum,
                &expected_path.path,
            )
            .await
            .expect("seeded state should be readable"),
            "seeded downloaded state must make the expected file a safe skip"
        );
    }

    async fn seed_existing_recent_files(
        base_config: &mut DownloadConfig,
        pass: &AlbumPass,
        zone: &str,
        ids: &[String],
        filename_prefix: &str,
    ) {
        for (index, id) in ids.iter().enumerate() {
            let records = mock_photo_records_for_zone_with_filename(
                id,
                zone,
                &format!("{filename_prefix}-{index:04}.jpg"),
            );
            let asset = PhotoAsset::new(records[0].clone(), records[1].clone());
            seed_existing_file_for_asset(base_config, pass, &asset).await;
        }
    }

    fn recent_ids(prefix: &str, count: u64) -> Vec<String> {
        (0..count).map(|i| format!("{prefix}-{i:04}")).collect()
    }

    fn recent_scope_assets(prefix: &str, count: u64, base_date: i64) -> Vec<RecentScopeAsset> {
        (0..count)
            .map(|i| RecentScopeAsset {
                id: format!("{prefix}-{i:04}"),
                asset_date: base_date - i64::try_from(i).expect("test index fits i64") * 1_000,
            })
            .collect()
    }

    fn recent_scope_photo_asset(asset: &RecentScopeAsset) -> PhotoAsset {
        let records = mock_photo_records_for_zone_with_filename_and_asset_date(
            &asset.id,
            "PrimarySync",
            &format!("{}.jpg", asset.id),
            asset.asset_date,
        );
        PhotoAsset::new(records[0].clone(), records[1].clone())
    }

    fn unique_ids_in_order(ids: Vec<String>) -> Vec<String> {
        let mut seen = FxHashSet::default();
        ids.into_iter()
            .filter(|id| seen.insert(id.clone()))
            .collect()
    }

    fn mock_photo_records_with_filename(record_name: &str, filename: &str) -> Vec<Value> {
        vec![
            mock_master_record_with_filename(record_name, filename),
            mock_asset_record_for(&format!("asset-{record_name}"), record_name),
        ]
    }

    fn mock_master_record_with_filename(record_name: &str, filename: &str) -> Value {
        json!({
            "recordName": record_name,
            "recordType": "CPLMaster",
            "fields": {
                "filenameEnc": {"value": filename, "type": "STRING"},
                "resOriginalRes": {
                    "value": {
                        "downloadURL": "https://p01.icloud-content.com/photo.jpg",
                        "size": 1024,
                        "fileChecksum": "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA="
                    }
                },
                "resOriginalWidth": {"value": 100, "type": "INT64"},
                "resOriginalHeight": {"value": 100, "type": "INT64"},
                "resOriginalFileType": {"value": "public.jpeg"},
                "itemType": {"value": "public.jpeg"},
                "adjustmentRenderType": {"value": 0, "type": "INT64"}
            },
            "recordChangeTag": "ct1"
        })
    }

    fn mock_asset_record_for(asset_record_name: &str, master_record_name: &str) -> Value {
        json!({
            "recordName": asset_record_name,
            "recordType": "CPLAsset",
            "fields": {
                "masterRef": {
                    "value": {
                        "recordName": master_record_name,
                        "zoneID": {"zoneName": "PrimarySync"}
                    },
                    "type": "REFERENCE"
                },
                "assetDate": {"value": 1700000000000i64, "type": "TIMESTAMP"},
                "addedDate": {"value": 1700000000000i64, "type": "TIMESTAMP"}
            },
            "recordChangeTag": "ct2"
        })
    }

    #[tokio::test]
    async fn full_sync_per_pass_streams_overlap_when_paths_are_pass_specific() {
        let session = ConcurrentRecordsSession::new(Duration::from_millis(100));
        let passes = vec![
            AlbumPass {
                kind: PassKind::Album,
                album: probe_album("album_a", session.clone()),
                exclude_ids: Arc::new(FxHashSet::default()),
            },
            AlbumPass {
                kind: PassKind::Album,
                album: probe_album("album_b", session.clone()),
                exclude_ids: Arc::new(FxHashSet::default()),
            },
        ];

        let mut config = test_config();
        let dir = TempDir::new().expect("temp dir");
        config.directory = Arc::from(dir.path());
        config.concurrent_downloads = 2;

        let result = download_photos_full_with_token(
            &Client::new(),
            &passes,
            &Arc::new(config),
            DownloadControls::dry_run_hidden(),
            CancellationToken::new(),
        )
        .await
        .expect("dry-run full sync should succeed");

        assert!(
            matches!(result.outcome, DownloadOutcome::Success),
            "empty dry-run enumeration should succeed"
        );
        assert_eq!(result.stats.enumeration_errors, 0);
        assert!(
            session.max_in_flight() >= 2,
            "album records/query streams should overlap; max in-flight was {}",
            session.max_in_flight()
        );
    }

    #[tokio::test]
    async fn full_sync_threads_one_keeps_pass_streams_serial() {
        let session = ConcurrentRecordsSession::new(Duration::from_millis(25));
        let passes = vec![
            AlbumPass {
                kind: PassKind::Album,
                album: probe_album("album_a", session.clone()),
                exclude_ids: Arc::new(FxHashSet::default()),
            },
            AlbumPass {
                kind: PassKind::Album,
                album: probe_album("album_b", session.clone()),
                exclude_ids: Arc::new(FxHashSet::default()),
            },
        ];

        let mut config = test_config();
        let dir = TempDir::new().expect("temp dir");
        config.directory = Arc::from(dir.path());
        config.concurrent_downloads = 1;

        let result = download_photos_full_with_token(
            &Client::new(),
            &passes,
            &Arc::new(config),
            DownloadControls::dry_run_hidden(),
            CancellationToken::new(),
        )
        .await
        .expect("dry-run full sync should succeed");

        assert!(
            matches!(result.outcome, DownloadOutcome::Success),
            "result: {result:?}"
        );
        assert_eq!(
            session.max_in_flight(),
            1,
            "threads=1 should prevent multi-pass enumeration from consuming multiple cores at once"
        );
    }

    #[tokio::test]
    async fn full_sync_deferred_unfiled_excludes_album_members() {
        let album_session = MockPhotosFlow::new()
            .album_count(1)
            .query_photo_page("MASTER_1", Some("zone-token"))
            .build();
        let unfiled_session = MockPhotosFlow::new()
            .album_count(1)
            .query_photo_page("MASTER_1", Some("zone-token"))
            .build();
        let passes = vec![
            AlbumPass {
                kind: PassKind::Album,
                album: mock_album("Vacation", album_session),
                exclude_ids: Arc::new(FxHashSet::default()),
            },
            AlbumPass {
                kind: PassKind::Unfiled,
                album: mock_album("", unfiled_session),
                exclude_ids: Arc::new(FxHashSet::default()),
            },
        ];

        let mut config = test_config();
        let dir = TempDir::new().expect("temp dir");
        config.directory = Arc::from(dir.path());
        config.concurrent_downloads = 2;

        let result = download_photos_full_with_token(
            &Client::new(),
            &passes,
            &Arc::new(config),
            DownloadControls::dry_run_hidden(),
            CancellationToken::new(),
        )
        .await
        .expect("dry-run full sync should succeed");

        assert_eq!(
            result.stats.downloaded, 1,
            "asset present in an album pass must not also be counted through the unfiled pass"
        );
        assert!(
            matches!(result.outcome, DownloadOutcome::Success),
            "filtered duplicate should not make the run partial"
        );
    }

    #[tokio::test]
    async fn full_sync_deferred_unfiled_write_mode_exclusion_does_not_count_as_shortfall() {
        let records = mock_photo_records_with_filename("MASTER_1", "test.jpg");
        let album_session = MockPhotosFlow::new()
            .album_count(1)
            .query_page(records.clone(), Some("zone-token"))
            .build();
        let unfiled_session = MockPhotosFlow::new()
            .album_count(1)
            .query_page(records.clone(), Some("zone-token"))
            .build();
        let passes = vec![
            AlbumPass {
                kind: PassKind::Album,
                album: mock_album("Vacation", album_session),
                exclude_ids: Arc::new(FxHashSet::default()),
            },
            AlbumPass {
                kind: PassKind::Unfiled,
                album: mock_album("", unfiled_session),
                exclude_ids: Arc::new(FxHashSet::default()),
            },
        ];

        let mut config = test_config();
        let dir = TempDir::new().expect("temp dir");
        config.directory = Arc::from(dir.path());
        config.concurrent_downloads = 2;
        config.file_match_policy = FileMatchPolicy::NameId7;

        let asset = PhotoAsset::new(records[0].clone(), records[1].clone());
        let album_config = config.with_pass(&passes[0]);
        let expected_path = filter::expected_paths_for(&asset, &album_config)
            .into_iter()
            .next()
            .expect("mock asset should have an expected path");
        tokio::fs::create_dir_all(expected_path.path.parent().expect("path has parent"))
            .await
            .expect("create parent dir");
        tokio::fs::write(&expected_path.path, vec![0u8; 1024])
            .await
            .expect("seed existing file");
        seed_downloaded_state_for_expected_path(&mut config, &album_config, &asset, &expected_path)
            .await;

        let result = download_photos_full_with_token(
            &Client::new(),
            &passes,
            &Arc::new(config),
            DownloadControls::download_hidden(),
            CancellationToken::new(),
        )
        .await
        .expect("write-mode full sync should succeed");

        assert_eq!(
            result.stats.enumeration_errors, 0,
            "deferred unfiled exclusions are intentional and must not be counted as pagination undercount"
        );
        assert_eq!(
            result.stats.api_total_at_start,
            Some(1),
            "reliable full-enumeration count should be persisted for cross-cycle inventory checks"
        );
        assert_eq!(
            result.sync_token.as_deref(),
            Some("zone-token"),
            "clean write-mode enumeration should still advance the agreed sync token"
        );
        assert!(!result.stats.sync_token_blocked);
        assert_eq!(result.stats.sync_token_expected_receivers, Some(2));
        assert_eq!(result.stats.sync_token_receivers_with_token, Some(2));
        assert_eq!(result.stats.sync_token_receivers_missing, Some(0));
        assert_eq!(result.stats.sync_token_receivers_blank, Some(0));
        assert_eq!(result.stats.sync_token_receivers_dropped, Some(0));
        assert_eq!(result.stats.sync_token_unique_values, Some(1));
        assert!(
            matches!(result.outcome, DownloadOutcome::Success),
            "filtered duplicate should not make the write-mode run partial"
        );
    }

    #[tokio::test]
    async fn full_sync_deferred_unfiled_excludes_empty_album_count_from_pagination_total() {
        let records = mock_photo_records_with_filename("MASTER_UNFILED", "unfiled.jpg");
        let album_session = MockPhotosFlow::new()
            .album_count(1)
            .empty_query_page(Some("zone-token"))
            .build();
        let unfiled_session = MockPhotosFlow::new()
            .album_count(1)
            .query_page(records.clone(), Some("zone-token"))
            .build();
        let passes = vec![
            AlbumPass {
                kind: PassKind::Album,
                album: mock_album("Vacation", album_session),
                exclude_ids: Arc::new(FxHashSet::default()),
            },
            AlbumPass {
                kind: PassKind::Unfiled,
                album: mock_album("", unfiled_session),
                exclude_ids: Arc::new(FxHashSet::default()),
            },
        ];

        let mut config = test_config();
        let dir = TempDir::new().expect("temp dir");
        config.directory = Arc::from(dir.path());
        config.concurrent_downloads = 2;
        config.file_match_policy = FileMatchPolicy::NameId7;

        let asset = PhotoAsset::new(records[0].clone(), records[1].clone());
        seed_existing_file_for_asset(&mut config, &passes[1], &asset).await;

        let result = download_photos_full_with_token(
            &Client::new(),
            &passes,
            &Arc::new(config),
            DownloadControls::download_hidden(),
            CancellationToken::new(),
        )
        .await
        .expect("write-mode full sync should succeed");

        assert_eq!(
            result.stats.api_total_at_start,
            Some(1),
            "empty album-side count must not be added on top of the library-wide unfiled stream"
        );
        assert_eq!(result.stats.pagination_shortfall_warnings, 0);
        assert_eq!(result.stats.pagination_shortfall_assets, 0);
        assert!(!result.stats.sync_token_blocked);
        assert_eq!(result.sync_token.as_deref(), Some("zone-token"));
    }

    #[tokio::test]
    async fn full_sync_deferred_unfiled_opens_stream_after_album_passes_finish() {
        let (album_session, unfiled_session) = DeferredOrderSession::new_pair();
        let unfiled_probe = unfiled_session.clone();
        let passes = vec![
            AlbumPass {
                kind: PassKind::Album,
                album: album_with_session("PrimarySync", "Vacation", Box::new(album_session)),
                exclude_ids: Arc::new(FxHashSet::default()),
            },
            AlbumPass {
                kind: PassKind::Unfiled,
                album: album_with_session("PrimarySync", "", Box::new(unfiled_session)),
                exclude_ids: Arc::new(FxHashSet::default()),
            },
        ];

        let mut config = test_config();
        let dir = TempDir::new().expect("temp dir");
        config.directory = Arc::from(dir.path());
        config.concurrent_downloads = 2;
        for (pass, record_name) in [(&passes[0], "ORDER_ALBUM"), (&passes[1], "ORDER_UNFILED")] {
            let records = mock_photo_records_for_zone_with_filename(
                record_name,
                "PrimarySync",
                &format!("{record_name}.jpg"),
            );
            let asset = PhotoAsset::new(records[0].clone(), records[1].clone());
            seed_existing_file_for_asset(&mut config, pass, &asset).await;
        }

        let result = download_photos_full_with_token(
            &Client::new(),
            &passes,
            &Arc::new(config),
            DownloadControls::download_hidden(),
            CancellationToken::new(),
        )
        .await
        .expect("write-mode full sync should succeed");

        assert!(matches!(result.outcome, DownloadOutcome::Success));
        assert!(
            !unfiled_probe.unfiled_started_too_early(),
            "deferred unfiled must not enumerate URL-bearing assets while album passes are still running"
        );
        assert_eq!(
            result.stats.assets_seen, 2,
            "album and unfiled assets should both be processed after exclusions are known"
        );
    }

    #[tokio::test]
    async fn full_sync_deferred_unfiled_logs_heartbeat_and_completion() {
        let (capture, _guard) = TracingCapture::install();
        let album_session =
            DynamicRecentPhotosSession::from_ids(vec!["album-heartbeat-0000".to_string()])
                .with_filename_prefix("album-heartbeat")
                .with_token("zone-token");
        let unfiled_count = DEFERRED_UNFILED_HEARTBEAT_ASSETS + 1;
        let unfiled_session =
            DynamicRecentPhotosSession::from_ids(recent_ids("unfiled-heartbeat", unfiled_count))
                .with_filename_prefix("unfiled-heartbeat")
                .with_token("zone-token");
        let passes = vec![
            AlbumPass {
                kind: PassKind::Album,
                album: album_with_session("PrimarySync", "Vacation", Box::new(album_session)),
                exclude_ids: Arc::new(FxHashSet::default()),
            },
            AlbumPass {
                kind: PassKind::Unfiled,
                album: album_with_session("PrimarySync", "", Box::new(unfiled_session)),
                exclude_ids: Arc::new(FxHashSet::default()),
            },
        ];

        let mut config = test_config();
        let dir = TempDir::new().expect("temp dir");
        config.directory = Arc::from(dir.path());
        config.concurrent_downloads = 10;

        let result = download_photos_full_with_token(
            &Client::new(),
            &passes,
            &Arc::new(config),
            DownloadControls::dry_run_hidden(),
            CancellationToken::new(),
        )
        .await
        .expect("dry-run full sync should succeed");

        assert!(matches!(result.outcome, DownloadOutcome::Success));
        let events = capture.events();
        let start = events
            .iter()
            .find(|event| event.message() == Some("Deferred unfiled enumeration started"))
            .unwrap_or_else(|| panic!("missing deferred unfiled start event: {events:?}"));
        assert_eq!(start.field("library"), Some("PrimarySync"));
        assert_eq!(start.field("pass_type"), Some("unfiled"));
        assert_eq!(start.field("assets_enumerated"), Some("0"));

        let progress = events
            .iter()
            .find(|event| event.message() == Some("Deferred unfiled enumeration progress"))
            .unwrap_or_else(|| panic!("missing deferred unfiled progress event: {events:?}"));
        assert_eq!(progress.field("library"), Some("PrimarySync"));
        assert_eq!(progress.field("pass_type"), Some("unfiled"));
        let progress_assets = DEFERRED_UNFILED_HEARTBEAT_ASSETS.to_string();
        assert_eq!(
            progress.field("assets_enumerated"),
            Some(progress_assets.as_str())
        );
        assert!(
            progress
                .field("expected_assets")
                .is_some_and(|value| value.contains(&unfiled_count.to_string())),
            "progress event should include expected asset count: {progress:?}"
        );
        assert!(
            progress.field("elapsed").is_some(),
            "progress event should include elapsed time: {progress:?}"
        );

        let complete = events
            .iter()
            .find(|event| event.message() == Some("Deferred unfiled enumeration complete"))
            .unwrap_or_else(|| panic!("missing deferred unfiled completion event: {events:?}"));
        assert_eq!(complete.field("library"), Some("PrimarySync"));
        assert_eq!(complete.field("pass_type"), Some("unfiled"));
        let completion_assets = unfiled_count.to_string();
        assert_eq!(
            complete.field("assets_enumerated"),
            Some(completion_assets.as_str())
        );
        assert!(
            complete.field("elapsed").is_some(),
            "completion event should include elapsed time: {complete:?}"
        );
    }

    #[tokio::test]
    async fn full_sync_unfiled_only_does_not_log_deferred_unfiled_heartbeat() {
        let (capture, _guard) = TracingCapture::install();
        let unfiled_session = DynamicRecentPhotosSession::from_ids(recent_ids(
            "unfiled-only-heartbeat",
            DEFERRED_UNFILED_HEARTBEAT_ASSETS + 1,
        ))
        .with_filename_prefix("unfiled-only-heartbeat")
        .with_token("zone-token");
        let passes = vec![AlbumPass {
            kind: PassKind::Unfiled,
            album: album_with_session("PrimarySync", "", Box::new(unfiled_session)),
            exclude_ids: Arc::new(FxHashSet::default()),
        }];

        let mut config = test_config();
        let dir = TempDir::new().expect("temp dir");
        config.directory = Arc::from(dir.path());
        config.concurrent_downloads = 10;

        let result = download_photos_full_with_token(
            &Client::new(),
            &passes,
            &Arc::new(config),
            DownloadControls::dry_run_hidden(),
            CancellationToken::new(),
        )
        .await
        .expect("dry-run full sync should succeed");

        assert!(matches!(result.outcome, DownloadOutcome::Success));
        let events = capture.events();
        assert!(
            !events.iter().any(|event| event
                .message()
                .is_some_and(|message| message.starts_with("Deferred unfiled enumeration"))),
            "unfiled-only sync should not emit deferred unfiled heartbeat events: {events:?}"
        );
    }

    #[tokio::test]
    async fn full_album_pass_records_complete_snapshot_before_planning_skips() {
        let filtered_records = mock_photo_records_for_zone_with_filename_and_asset_date(
            "MASTER_FILTERED",
            "PrimarySync",
            "filtered.jpg",
            1_700_000_000_000,
        );
        let on_disk_records = mock_photo_records_for_zone_with_filename_and_asset_date(
            "MASTER_ON_DISK",
            "PrimarySync",
            "on-disk.jpg",
            1_699_000_000_000,
        );
        let mut records = filtered_records.clone();
        records.extend(on_disk_records.clone());
        let album_session = MockPhotosFlow::new()
            .album_count(2)
            .query_page(records, Some("zone-token"))
            .build();
        let passes = vec![AlbumPass {
            kind: PassKind::Album,
            album: mock_album_with_container("Vacation", "container-vacation", album_session),
            exclude_ids: Arc::new(FxHashSet::default()),
        }];

        let db = Arc::new(SqliteStateDb::open_in_memory().unwrap());
        let mut config = test_config();
        let dir = TempDir::new().expect("temp dir");
        config.directory = Arc::from(dir.path());
        config.state_db = Some(Arc::clone(&db) as Arc<dyn DownloadStore>);
        config.enum_config_hash = Some(Arc::from("hash-pr3"));
        config.skip_created_after = Some(crate::config::CreatedDateFilter::Instant(
            DateTime::from_timestamp_millis(1_699_999_999_000).expect("valid timestamp"),
        ));
        let on_disk_asset = PhotoAsset::new(on_disk_records[0].clone(), on_disk_records[1].clone());
        seed_existing_file_for_asset(&mut config, &passes[0], &on_disk_asset).await;

        let result = download_photos_full_with_token(
            &Client::new(),
            &passes,
            &Arc::new(config),
            DownloadControls::download_hidden(),
            CancellationToken::new(),
        )
        .await
        .expect("full album sync should complete");

        assert!(matches!(result.outcome, DownloadOutcome::Success));
        assert!(
            db.selected_album_containers_have_complete_snapshots(
                "PrimarySync",
                &["container-vacation"]
            )
            .await
            .unwrap(),
            "clean full pass should mark the album snapshot complete"
        );
        for (asset_record_name, master_record_name) in [
            ("asset-MASTER_FILTERED", "MASTER_FILTERED"),
            ("asset-MASTER_ON_DISK", "MASTER_ON_DISK"),
        ] {
            let memberships = db
                .get_live_selected_album_memberships_for_asset(
                    "PrimarySync",
                    asset_record_name,
                    &["container-vacation"],
                )
                .await
                .unwrap();
            assert_eq!(memberships.len(), 1, "{asset_record_name} membership");
            assert_eq!(
                memberships[0].master_record_name.as_deref(),
                Some(master_record_name)
            );
        }
    }

    #[tokio::test]
    async fn failed_album_pass_leaves_previous_complete_snapshot_trusted() {
        let db = Arc::new(SqliteStateDb::open_in_memory().unwrap());
        db.upsert_album_container("PrimarySync", "container-vacation", "Vacation", "album")
            .await
            .unwrap();
        let previous = db
            .start_album_membership_snapshot("PrimarySync", "container-vacation", Some("hash-old"))
            .await
            .unwrap();
        db.add_album_membership_to_snapshot(
            "PrimarySync",
            "container-vacation",
            previous,
            "asset-OLD",
            Some("MASTER_OLD"),
            "icloud",
        )
        .await
        .unwrap();
        db.complete_album_membership_snapshot("PrimarySync", "container-vacation", previous)
            .await
            .unwrap();

        let album_session = MockPhotosFlow::new()
            .album_count(1)
            .error("album stream failed")
            .build();
        let passes = vec![AlbumPass {
            kind: PassKind::Album,
            album: mock_album_with_container("Vacation", "container-vacation", album_session),
            exclude_ids: Arc::new(FxHashSet::default()),
        }];

        let mut config = test_config();
        let dir = TempDir::new().expect("temp dir");
        config.directory = Arc::from(dir.path());
        config.state_db = Some(Arc::clone(&db) as Arc<dyn DownloadStore>);
        config.enum_config_hash = Some(Arc::from("hash-new"));

        let result = download_photos_full_with_token(
            &Client::new(),
            &passes,
            &Arc::new(config),
            DownloadControls::download_hidden(),
            CancellationToken::new(),
        )
        .await
        .expect("stream errors should be reported in the sync result");

        assert!(result.stats.enumeration_errors > 0);
        assert!(
            db.selected_album_containers_have_complete_snapshots(
                "PrimarySync",
                &["container-vacation"]
            )
            .await
            .unwrap(),
            "failed replacement snapshot must not invalidate the old complete generation"
        );
        let memberships = db
            .get_live_selected_album_memberships_for_asset(
                "PrimarySync",
                "asset-OLD",
                &["container-vacation"],
            )
            .await
            .unwrap();
        assert_eq!(memberships.len(), 1);
        assert_eq!(
            memberships[0].master_record_name.as_deref(),
            Some("MASTER_OLD")
        );
    }

    #[tokio::test]
    async fn interrupted_album_pass_does_not_complete_snapshot() {
        let album_session = MockPhotosFlow::new()
            .album_count(1)
            .query_photo_page("MASTER_INTERRUPTED", Some("zone-token"))
            .build();
        let passes = vec![AlbumPass {
            kind: PassKind::Album,
            album: mock_album_with_container("Vacation", "container-vacation", album_session),
            exclude_ids: Arc::new(FxHashSet::default()),
        }];

        let db = Arc::new(SqliteStateDb::open_in_memory().unwrap());
        let mut config = test_config();
        let dir = TempDir::new().expect("temp dir");
        config.directory = Arc::from(dir.path());
        config.state_db = Some(Arc::clone(&db) as Arc<dyn DownloadStore>);
        config.enum_config_hash = Some(Arc::from("hash-interrupted"));
        let shutdown = CancellationToken::new();
        shutdown.cancel();

        let result = download_photos_full_with_token(
            &Client::new(),
            &passes,
            &Arc::new(config),
            DownloadControls::download_hidden(),
            shutdown,
        )
        .await
        .expect("interrupted stream should return a sync result");

        assert!(result.full_enumeration_ran);
        assert!(
            !db.selected_album_containers_have_complete_snapshots(
                "PrimarySync",
                &["container-vacation"]
            )
            .await
            .unwrap(),
            "interrupted album pass must leave the new snapshot incomplete"
        );
    }

    #[tokio::test]
    async fn full_enumeration_shortfall_warns_but_allows_sync_token() {
        let records = mock_photo_records_with_filename("MASTER_SHORTFALL", "shortfall.jpg");
        let session = MockPhotosFlow::new()
            .album_count(2)
            .query_page(records.clone(), Some("zone-token"))
            .empty_query_page(Some("zone-token"))
            .build();
        let passes = vec![AlbumPass {
            kind: PassKind::Album,
            album: mock_album("Hidden", session),
            exclude_ids: Arc::new(FxHashSet::default()),
        }];

        let mut config = test_config();
        let dir = TempDir::new().expect("temp dir");
        config.directory = Arc::from(dir.path());
        config.concurrent_downloads = 2;
        config.file_match_policy = FileMatchPolicy::NameId7;

        // Seed the destination so the single enumerated asset is skipped
        // on-disk and the test isolates count-only shortfall behavior.
        let asset = PhotoAsset::new(records[0].clone(), records[1].clone());
        seed_existing_file_for_asset(&mut config, &passes[0], &asset).await;

        let result = download_photos_full_with_token(
            &Client::new(),
            &passes,
            &Arc::new(config),
            DownloadControls::download_hidden(),
            CancellationToken::new(),
        )
        .await
        .expect("count shortfall should not error");

        assert!(
            matches!(result.outcome, DownloadOutcome::Success),
            "count-only shortfall should not be reported as partial failure"
        );
        assert_eq!(result.stats.failed, 0);
        assert_eq!(result.stats.enumeration_errors, 0);
        assert_eq!(result.stats.pagination_shortfall_warnings, 1);
        assert_eq!(result.stats.pagination_shortfall_assets, 1);
        assert!(!result.stats.sync_token_blocked);
        assert_eq!(result.stats.sync_token_blocked_reason, None);
        assert_eq!(result.stats.sync_token_blocked_source, None);
        assert_eq!(result.stats.sync_token_blocked_explanation, None);
        assert_eq!(result.stats.sync_token_expected_receivers, Some(1));
        assert_eq!(result.stats.sync_token_receivers_with_token, Some(1));
        assert_eq!(result.stats.sync_token_receivers_missing, Some(0));
        assert_eq!(result.stats.sync_token_receivers_blank, Some(0));
        assert_eq!(result.stats.sync_token_receivers_dropped, Some(0));
        assert_eq!(result.stats.sync_token_unique_values, Some(1));
        assert_eq!(
            result.sync_token.as_deref(),
            Some("zone-token"),
            "count-side-channel shortfall must stay diagnostic when records/query completed cleanly"
        );
    }

    #[tokio::test]
    async fn malformed_album_count_is_diagnostic_when_tail_proves_eof() {
        let records = mock_photo_records_with_filename("MASTER_BEFORE_GAP", "before-gap.jpg");
        let later_records = mock_photo_records_with_filename("MASTER_AFTER_GAP", "after-gap.jpg");
        let session = MockPhotosFlow::new()
            .album_count_response(json!({
                "batch": [{"records": [{"fields": {"itemCount": {"value": "not-a-count"}}}]}]
            }))
            .query_page(records.clone(), Some("zone-token"))
            .empty_query_page(Some("zone-token"))
            .empty_query_page(Some("zone-token"))
            .empty_query_page(Some("zone-token"))
            .empty_query_page(Some("zone-token"))
            .empty_query_page(Some("zone-token"))
            .query_page(later_records, Some("zone-token"))
            .build();
        let passes = vec![AlbumPass {
            kind: PassKind::Album,
            album: mock_album("Hidden", session),
            exclude_ids: Arc::new(FxHashSet::default()),
        }];

        let mut config = test_config();
        let dir = TempDir::new().expect("temp dir");
        config.directory = Arc::from(dir.path());
        config.concurrent_downloads = 1;
        config.file_match_policy = FileMatchPolicy::NameId7;

        let asset = PhotoAsset::new(records[0].clone(), records[1].clone());
        seed_existing_file_for_asset(&mut config, &passes[0], &asset).await;

        let result = download_photos_full_with_token(
            &Client::new(),
            &passes,
            &Arc::new(config),
            DownloadControls::download_hidden(),
            CancellationToken::new(),
        )
        .await
        .expect("malformed album count should produce a sync result");

        assert!(matches!(result.outcome, DownloadOutcome::Success));
        assert_eq!(
            result.stats.assets_seen, 1,
            "the finite empty-page ceiling is the natural EOF proof"
        );
        assert_eq!(result.stats.enumeration_errors, 0);
        assert_eq!(result.stats.count_probe_failures, 1);
        assert_eq!(result.stats.pagination_shortfall_warnings, 0);
        assert_eq!(result.stats.pagination_shortfall_assets, 0);
        assert!(!result.stats.sync_token_blocked);
        assert_eq!(result.stats.sync_token_blocked_reason, None);
        assert_eq!(result.sync_token.as_deref(), Some("zone-token"));
    }

    #[tokio::test]
    async fn missing_album_count_is_diagnostic_when_empty_inventory_proves_eof() {
        let session = MockPhotosFlow::new()
            .album_count_response(json!({
                "batch": [{"records": [{"fields": {}}]}]
            }))
            .empty_query_page(Some("zone-token"))
            .build();
        let passes = vec![AlbumPass {
            kind: PassKind::Album,
            album: mock_album("Hidden", session),
            exclude_ids: Arc::new(FxHashSet::default()),
        }];

        let mut config = test_config();
        let dir = TempDir::new().expect("temp dir");
        config.directory = Arc::from(dir.path());
        config.concurrent_downloads = 1;

        let result = download_photos_full_with_token(
            &Client::new(),
            &passes,
            &Arc::new(config),
            DownloadControls::download_hidden(),
            CancellationToken::new(),
        )
        .await
        .expect("missing album count should produce a sync result");

        assert!(matches!(result.outcome, DownloadOutcome::Success));
        assert_eq!(result.stats.assets_seen, 0);
        assert_eq!(result.stats.enumeration_errors, 0);
        assert_eq!(result.stats.count_probe_failures, 1);
        assert!(!result.stats.sync_token_blocked);
        assert_eq!(result.stats.sync_token_blocked_reason, None);
        assert_eq!(result.sync_token.as_deref(), Some("zone-token"));
    }

    #[tokio::test]
    async fn malformed_album_count_still_blocks_when_stream_errors() {
        let session = MockPhotosFlow::new()
            .album_count_response(json!({
                "batch": [{"records": [{"fields": {"itemCount": {"value": "not-a-count"}}}]}]
            }))
            .error("stream failed")
            .build();
        let passes = vec![AlbumPass {
            kind: PassKind::Album,
            album: mock_album("Hidden", session),
            exclude_ids: Arc::new(FxHashSet::default()),
        }];

        let mut config = test_config();
        let dir = TempDir::new().expect("temp dir");
        config.directory = Arc::from(dir.path());
        config.concurrent_downloads = 1;

        let result = download_photos_full_with_token(
            &Client::new(),
            &passes,
            &Arc::new(config),
            DownloadControls::download_hidden(),
            CancellationToken::new(),
        )
        .await
        .expect("stream error should produce a sync result");

        assert!(matches!(
            result.outcome,
            DownloadOutcome::PartialFailure { failed_count: 1 }
        ));
        assert_eq!(result.stats.enumeration_errors, 1);
        assert_eq!(result.stats.count_probe_failures, 1);
        assert!(result.stats.sync_token_blocked);
        assert_eq!(
            result.stats.sync_token_blocked_reason,
            Some(ICLOUD_ALBUM_COUNT_ERROR_REASON)
        );
        assert_eq!(result.sync_token, None);
    }

    #[tokio::test]
    async fn well_formed_zero_album_count_allows_empty_token_capture() {
        let session = MockPhotosFlow::new()
            .album_count(0)
            .empty_query_page(Some("zone-token"))
            .build();
        let passes = vec![AlbumPass {
            kind: PassKind::Album,
            album: mock_album("Hidden", session),
            exclude_ids: Arc::new(FxHashSet::default()),
        }];

        let mut config = test_config();
        let dir = TempDir::new().expect("temp dir");
        config.directory = Arc::from(dir.path());
        config.concurrent_downloads = 1;

        let result = download_photos_full_with_token(
            &Client::new(),
            &passes,
            &Arc::new(config),
            DownloadControls::download_hidden(),
            CancellationToken::new(),
        )
        .await
        .expect("well-formed zero count should complete cleanly");

        assert!(matches!(result.outcome, DownloadOutcome::Success));
        assert_eq!(result.stats.assets_seen, 0);
        assert_eq!(result.stats.enumeration_errors, 0);
        assert!(!result.stats.sync_token_blocked);
        assert_eq!(result.sync_token.as_deref(), Some("zone-token"));
    }

    #[tokio::test]
    async fn full_enumeration_duplicate_asset_ids_do_not_block_sync_token() {
        let records = mock_photo_records_with_filename("MASTER_DUPLICATE", "duplicate.jpg");
        let session = MockPhotosFlow::new()
            .album_count(2)
            .query_page(records.clone(), Some("zone-token"))
            .query_page(records.clone(), Some("zone-token"))
            .build();
        let passes = vec![AlbumPass {
            kind: PassKind::Album,
            album: mock_album("Hidden", session),
            exclude_ids: Arc::new(FxHashSet::default()),
        }];

        let mut config = test_config();
        let dir = TempDir::new().expect("temp dir");
        config.directory = Arc::from(dir.path());
        config.concurrent_downloads = 1;
        config.file_match_policy = FileMatchPolicy::NameId7;

        // Seed the destination so the unique asset is skipped on-disk and the
        // test isolates duplicate API asset-id accounting.
        let asset = PhotoAsset::new(records[0].clone(), records[1].clone());
        let pass_config = config.with_pass(&passes[0]);
        let expected_path = filter::expected_paths_for(&asset, &pass_config)
            .into_iter()
            .next()
            .expect("mock asset should have an expected path");
        tokio::fs::create_dir_all(expected_path.path.parent().expect("path has parent"))
            .await
            .expect("create parent dir");
        tokio::fs::write(&expected_path.path, vec![0u8; 1024])
            .await
            .expect("seed existing file");
        seed_downloaded_state_for_expected_path(&mut config, &pass_config, &asset, &expected_path)
            .await;

        let result = download_photos_full_with_token(
            &Client::new(),
            &passes,
            &Arc::new(config),
            DownloadControls::download_hidden(),
            CancellationToken::new(),
        )
        .await
        .expect("duplicate asset-id shortfall should not error");

        assert!(
            matches!(result.outcome, DownloadOutcome::Success),
            "duplicate asset IDs should be treated as producer skips, not partial failure"
        );
        assert_eq!(result.stats.assets_seen, 1);
        assert_eq!(result.stats.skipped.duplicates, 0);
        assert_eq!(result.stats.pagination_shortfall_warnings, 1);
        assert_eq!(result.stats.pagination_shortfall_assets, 1);
        assert!(!result.stats.sync_token_blocked);
        assert_eq!(result.stats.sync_token_expected_receivers, Some(1));
        assert_eq!(result.stats.sync_token_receivers_with_token, Some(1));
        assert_eq!(result.stats.sync_token_unique_values, Some(1));
        assert_eq!(result.sync_token.as_deref(), Some("zone-token"));
    }

    #[tokio::test]
    async fn full_sync_page_split_sibling_cplassets_keep_identity_and_download_nothing() {
        let records = vec![
            mock_asset_record_for("asset-sibling-a", "MASTER_SIBLING"),
            mock_master_record_with_filename("MASTER_SIBLING", "sibling.jpg"),
            mock_asset_record_for("asset-sibling-b", "MASTER_SIBLING"),
        ];
        let session = MockPhotosFlow::new()
            .album_count(2)
            .query_page(records.clone(), Some("zone-token"))
            .build();
        let passes = vec![AlbumPass {
            kind: PassKind::Album,
            album: mock_album("Hidden", session),
            exclude_ids: Arc::new(FxHashSet::default()),
        }];

        let mut config = test_config();
        let dir = TempDir::new().expect("temp dir");
        config.directory = Arc::from(dir.path());
        config.concurrent_downloads = 1;
        config.file_match_policy = FileMatchPolicy::NameId7;

        let pass_config = config.with_pass(&passes[0]);
        let first_asset = PhotoAsset::new(records[1].clone(), records[0].clone());
        let second_asset = PhotoAsset::new(records[1].clone(), records[2].clone())
            .with_state_record_name(Arc::from("asset-sibling-b"));
        let mut seeded_paths = Vec::new();
        for (index, asset) in [&first_asset, &second_asset].into_iter().enumerate() {
            let mut expected_path = filter::expected_paths_for(asset, &pass_config)
                .into_iter()
                .next()
                .expect("mock sibling asset should have an expected path");
            if index > 0 {
                let filename = expected_path
                    .path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .expect("expected path has filename");
                expected_path
                    .path
                    .set_file_name(paths::insert_asset_identity_suffix(
                        filename,
                        asset.state_id(),
                    ));
            }
            tokio::fs::create_dir_all(expected_path.path.parent().expect("path has parent"))
                .await
                .expect("create parent dir");
            tokio::fs::write(&expected_path.path, vec![0u8; 1024])
                .await
                .expect("seed existing file");
            seed_downloaded_state_for_expected_path(
                &mut config,
                &pass_config,
                asset,
                &expected_path,
            )
            .await;
            seeded_paths.push(expected_path.path);
        }

        let config = Arc::new(config);
        let first = download_photos_full_with_token(
            &Client::new(),
            &passes,
            &config,
            DownloadControls::download_hidden(),
            CancellationToken::new(),
        )
        .await
        .expect("sibling CPLAssets should complete");

        assert!(matches!(first.outcome, DownloadOutcome::Success));
        assert_eq!(first.stats.assets_seen, 2);
        assert_eq!(first.stats.downloaded, 0);
        assert_eq!(first.stats.failed, 0);
        assert_eq!(first.stats.skipped.duplicates, 0);
        assert_eq!(first.stats.pagination_shortfall_warnings, 0);
        assert_eq!(first.stats.pagination_shortfall_assets, 0);
        assert!(!first.stats.sync_token_blocked);
        assert_eq!(first.sync_token.as_deref(), Some("zone-token"));

        let split_session = MockPhotosFlow::new()
            .album_count(2)
            .query_page(
                vec![records[2].clone(), records[1].clone()],
                Some("zone-token-split"),
            )
            .query_page(vec![records[0].clone()], Some("zone-token-split"))
            .build();
        let split_passes = vec![AlbumPass {
            kind: PassKind::Album,
            album: mock_album("Hidden", split_session),
            exclude_ids: Arc::new(FxHashSet::default()),
        }];

        let second = download_photos_full_with_token(
            &Client::new(),
            &split_passes,
            &config,
            DownloadControls::download_hidden(),
            CancellationToken::new(),
        )
        .await
        .expect("page-split sibling sync should reuse both downloaded identities");

        assert!(matches!(second.outcome, DownloadOutcome::Success));
        assert_eq!(second.stats.assets_seen, 2);
        assert_eq!(second.stats.downloaded, 0);
        assert_eq!(second.stats.failed, 0);
        assert_eq!(second.stats.skipped.duplicates, 0);
        assert_eq!(second.stats.pagination_shortfall_warnings, 0);
        assert_eq!(second.stats.pagination_shortfall_assets, 0);
        assert!(!second.stats.sync_token_blocked);
        assert_eq!(second.sync_token.as_deref(), Some("zone-token-split"));
        assert!(seeded_paths.iter().all(|path| path.exists()));
        let downloaded_ids = config
            .state_db
            .as_ref()
            .expect("test state db")
            .get_downloaded_ids()
            .await
            .expect("downloaded state IDs");
        assert!(
            downloaded_ids
                .iter()
                .any(|(_, id, _)| id == "MASTER_SIBLING")
        );
        assert!(
            downloaded_ids
                .iter()
                .any(|(_, id, _)| id == "asset-sibling-b")
        );
        assert!(
            !downloaded_ids
                .iter()
                .any(|(_, id, _)| id == "asset-sibling-a")
        );
    }

    #[tokio::test]
    async fn full_sync_filtered_sibling_cannot_displace_legacy_master_owner_next_cycle() {
        let asset_a_record = mock_asset_record_for("asset-owner-a", "MASTER_OWNER");
        let asset_b_record = mock_asset_record_for("asset-owner-b", "MASTER_OWNER");
        let master_record = mock_master_record_with_filename("MASTER_OWNER", "owner.jpg");
        let excluded_ids = Arc::new(["asset-owner-b".to_string()].into_iter().collect());
        let first_session = MockPhotosFlow::new()
            .album_count(2)
            .query_page(
                vec![
                    asset_b_record.clone(),
                    master_record.clone(),
                    asset_a_record.clone(),
                ],
                Some("zone-token-first"),
            )
            .build();
        let first_passes = vec![AlbumPass {
            kind: PassKind::Album,
            album: mock_album("Hidden", first_session),
            exclude_ids: Arc::clone(&excluded_ids),
        }];

        let mut config = test_config();
        let dir = TempDir::new().expect("temp dir");
        config.directory = Arc::from(dir.path());
        config.concurrent_downloads = 1;
        config.file_match_policy = FileMatchPolicy::NameId7;
        let db = Arc::new(SqliteStateDb::open_in_memory().expect("state db"));
        config.state_db = Some(db.clone());
        let legacy_asset = PhotoAsset::new(master_record.clone(), asset_a_record.clone());
        seed_existing_file_for_asset(&mut config, &first_passes[0], &legacy_asset).await;
        db.fail_next_legacy_master_state_owner_claims(1);
        let config = Arc::new(config);

        let first = download_photos_full_with_token(
            &Client::new(),
            &first_passes,
            &config,
            DownloadControls::download_hidden(),
            CancellationToken::new(),
        )
        .await
        .expect("first owner-claim cycle should complete");
        assert!(matches!(first.outcome, DownloadOutcome::Success));
        assert_eq!(first.stats.downloaded, 0);
        assert_eq!(first.stats.skipped.by_excluded_album, 1);
        assert_eq!(
            db.remaining_legacy_master_state_owner_claim_failures(),
            0,
            "the first owner claim must hit the injected write failure"
        );
        assert_eq!(
            config
                .state_db
                .as_ref()
                .expect("test state db")
                .get_legacy_master_state_owners()
                .await
                .expect("legacy owner rows"),
            std::collections::HashSet::from([(
                "PrimarySync".to_string(),
                "MASTER_OWNER".to_string(),
                "asset-owner-a".to_string(),
            )])
        );

        let second_session = MockPhotosFlow::new()
            .album_count(2)
            .query_page(
                vec![asset_a_record, master_record, asset_b_record],
                Some("zone-token-second"),
            )
            .build();
        let second_passes = vec![AlbumPass {
            kind: PassKind::Album,
            album: mock_album("Hidden", second_session),
            exclude_ids: excluded_ids,
        }];
        let second = download_photos_full_with_token(
            &Client::new(),
            &second_passes,
            &config,
            DownloadControls::download_hidden(),
            CancellationToken::new(),
        )
        .await
        .expect("second owner-reuse cycle should complete");

        assert!(matches!(second.outcome, DownloadOutcome::Success));
        assert_eq!(second.stats.downloaded, 0);
        assert_eq!(second.stats.failed, 0);
        assert_eq!(second.stats.skipped.by_excluded_album, 1);
        let downloaded_ids = config
            .state_db
            .as_ref()
            .expect("test state db")
            .get_downloaded_ids()
            .await
            .expect("downloaded state IDs");
        assert!(downloaded_ids.iter().any(|(_, id, _)| id == "MASTER_OWNER"));
        assert!(
            !downloaded_ids
                .iter()
                .any(|(_, id, _)| id == "asset-owner-a")
        );
    }

    #[tokio::test]
    async fn full_sync_blank_query_sync_token_blocks_advancement() {
        let records = mock_photo_records_with_filename("MASTER_BLANK_TOKEN", "blank-token.jpg");
        let session = MockPhotosFlow::new()
            .album_count(1)
            .query_page(records.clone(), Some(""))
            .empty_query_page(Some(""))
            .build();
        let passes = vec![AlbumPass {
            kind: PassKind::Album,
            album: mock_album("Hidden", session),
            exclude_ids: Arc::new(FxHashSet::default()),
        }];

        let mut config = test_config();
        let dir = TempDir::new().expect("temp dir");
        config.directory = Arc::from(dir.path());
        config.concurrent_downloads = 2;
        config.file_match_policy = FileMatchPolicy::NameId7;

        // Seed the destination so the enumerated asset is skipped on-disk
        // and this test isolates token-capture behavior.
        let asset = PhotoAsset::new(records[0].clone(), records[1].clone());
        let pass_config = config.with_pass(&passes[0]);
        let expected_path = filter::expected_paths_for(&asset, &pass_config)
            .into_iter()
            .next()
            .expect("mock asset should have an expected path");
        tokio::fs::create_dir_all(expected_path.path.parent().expect("path has parent"))
            .await
            .expect("create parent dir");
        tokio::fs::write(&expected_path.path, vec![0u8; 1024])
            .await
            .expect("seed existing file");
        seed_downloaded_state_for_expected_path(&mut config, &pass_config, &asset, &expected_path)
            .await;

        let result = download_photos_full_with_token(
            &Client::new(),
            &passes,
            &Arc::new(config),
            DownloadControls::download_hidden(),
            CancellationToken::new(),
        )
        .await
        .expect("blank sync token should not error");

        assert!(
            matches!(result.outcome, DownloadOutcome::Success),
            "blank token should not force partial failure"
        );
        assert_eq!(result.stats.failed, 0);
        assert_eq!(result.stats.enumeration_errors, 0);
        assert_eq!(result.stats.pagination_shortfall_warnings, 0);
        assert_eq!(result.stats.pagination_shortfall_assets, 0);
        assert!(result.stats.sync_token_blocked);
        assert_eq!(
            result.stats.sync_token_blocked_reason,
            Some("icloud_blank_sync_token")
        );
        assert_eq!(result.stats.sync_token_blocked_source, Some("icloud"));
        assert_eq!(
            result.stats.sync_token_blocked_explanation,
            Some(sync_token_blocked_explanation("icloud_blank_sync_token"))
        );
        assert_eq!(result.stats.sync_token_expected_receivers, Some(1));
        assert_eq!(result.stats.sync_token_receivers_with_token, Some(0));
        assert_eq!(result.stats.sync_token_receivers_missing, Some(0));
        assert_eq!(result.stats.sync_token_receivers_blank, Some(1));
        assert_eq!(result.stats.sync_token_receivers_dropped, Some(0));
        assert_eq!(result.stats.sync_token_unique_values, Some(0));
        assert_eq!(result.sync_token, None, "blank token must not be persisted");
        assert_eq!(result.stats.same_cycle_recovery_attempts, 1);
        assert_eq!(result.stats.same_cycle_recovery_successes, 0);
    }

    #[tokio::test]
    async fn full_sync_repairs_missing_pass_token_in_same_cycle() {
        let records = mock_photo_records_with_filename("MASTER_RECOVER", "recover.jpg");
        let query_calls = Arc::new(AtomicUsize::new(0));
        let session = RecoveringPassTokenSession {
            query_calls: Arc::clone(&query_calls),
            records: Arc::new(records.clone()),
        };
        let passes = vec![AlbumPass {
            kind: PassKind::Album,
            album: album_with_session("PrimarySync", "Recovery", Box::new(session)),
            exclude_ids: Arc::new(FxHashSet::default()),
        }];

        let mut config = test_config();
        let dir = TempDir::new().expect("temp dir");
        config.directory = Arc::from(dir.path());
        config.concurrent_downloads = 1;
        config.file_match_policy = FileMatchPolicy::NameId7;

        let asset = PhotoAsset::new(records[0].clone(), records[1].clone());
        seed_existing_file_for_asset(&mut config, &passes[0], &asset).await;

        let result = download_photos_full_with_token(
            &Client::new(),
            &passes,
            &Arc::new(config),
            DownloadControls::download_hidden(),
            CancellationToken::new(),
        )
        .await
        .expect("same-cycle token recovery should complete");

        assert!(matches!(result.outcome, DownloadOutcome::Success));
        assert_eq!(result.sync_token.as_deref(), Some("zone-token-recovered"));
        assert_eq!(result.stats.same_cycle_recovery_attempts, 1);
        assert_eq!(result.stats.same_cycle_recovery_successes, 1);
        assert_eq!(result.stats.sync_token_receivers_with_token, Some(1));
        assert_eq!(result.stats.sync_token_receivers_missing, Some(0));
        assert_eq!(query_calls.load(Ordering::SeqCst), 12);
    }

    #[tokio::test]
    async fn full_sync_repairs_incomplete_pass_in_same_cycle() {
        let records = mock_photo_records_with_filename("MASTER_REPAIR", "repair.jpg");
        let query_calls = Arc::new(AtomicUsize::new(0));
        let session = RecoveringIncompletePassSession {
            query_calls: Arc::clone(&query_calls),
            records: Arc::new(records.clone()),
        };
        let passes = vec![AlbumPass {
            kind: PassKind::Album,
            album: album_with_session("PrimarySync", "Recovery", Box::new(session)),
            exclude_ids: Arc::new(FxHashSet::default()),
        }];

        let mut config = test_config();
        let dir = TempDir::new().expect("temp dir");
        config.directory = Arc::from(dir.path());
        config.concurrent_downloads = 1;
        config.file_match_policy = FileMatchPolicy::NameId7;
        let asset = PhotoAsset::new(records[0].clone(), records[1].clone());
        seed_existing_file_for_asset(&mut config, &passes[0], &asset).await;

        let result = download_photos_full_with_token(
            &Client::new(),
            &passes,
            &Arc::new(config),
            DownloadControls::download_hidden(),
            CancellationToken::new(),
        )
        .await
        .expect("incomplete pass should repair once in-cycle");

        assert!(matches!(result.outcome, DownloadOutcome::Success));
        assert_eq!(result.stats.enumeration_errors, 0);
        assert_eq!(result.sync_token.as_deref(), Some("zone-token-recovered"));
        assert_eq!(result.stats.same_cycle_recovery_attempts, 1);
        assert_eq!(result.stats.same_cycle_recovery_successes, 1);
        assert_eq!(query_calls.load(Ordering::SeqCst), 7);
    }

    #[test]
    fn test_hash_download_config_deterministic() {
        let config = test_config();
        let hash1 = hash_download_config(&config);
        let hash2 = hash_download_config(&config);
        assert_eq!(hash1, hash2);
        assert_eq!(hash1.len(), 16); // 8 bytes hex-encoded
    }

    #[test]
    fn test_hash_download_config_changes_on_directory() {
        let mut config1 = test_config();
        config1.directory = std::sync::Arc::from(std::path::Path::new("/photos/a"));
        let mut config2 = test_config();
        config2.directory = std::sync::Arc::from(std::path::Path::new("/photos/b"));
        assert_ne!(
            hash_download_config(&config1),
            hash_download_config(&config2)
        );
    }

    #[test]
    fn test_hash_download_config_changes_on_folder_structure() {
        let mut config1 = test_config();
        config1.folder_structure = "{:%Y/%m/%d}".to_string();
        let mut config2 = test_config();
        config2.folder_structure = "{:%Y/%m}".to_string();
        assert_ne!(
            hash_download_config(&config1),
            hash_download_config(&config2)
        );
    }

    #[test]
    fn test_should_download_fast_trust_state_returns_false() {
        let mut ctx = DownloadContext::default();
        ctx.downloaded_ids
            .entry("PrimarySync".into())
            .or_default()
            .entry("asset1".into())
            .or_default()
            .insert("original".into());
        ctx.downloaded_checksums
            .entry("PrimarySync".into())
            .or_default()
            .entry("asset1".into())
            .or_default()
            .insert("original".into(), "checksum_a".into());

        // trust_state=true: returns Some(false) for matching asset
        assert_eq!(
            ctx.should_download_fast(
                "PrimarySync",
                "asset1",
                VersionSizeKey::Original,
                "checksum_a",
                true
            ),
            Some(false)
        );

        // trust_state=false: returns None (needs filesystem check)
        assert_eq!(
            ctx.should_download_fast(
                "PrimarySync",
                "asset1",
                VersionSizeKey::Original,
                "checksum_a",
                false
            ),
            None
        );

        // Changed checksum: returns Some(true) regardless of trust_state
        assert_eq!(
            ctx.should_download_fast(
                "PrimarySync",
                "asset1",
                VersionSizeKey::Original,
                "checksum_b",
                true
            ),
            Some(true)
        );

        // Unknown asset: returns Some(true)
        assert_eq!(
            ctx.should_download_fast(
                "PrimarySync",
                "unknown",
                VersionSizeKey::Original,
                "x",
                true
            ),
            Some(true)
        );
    }

    // ── extract_skip_candidates tests ──────────────────────────────

    // ── hash_download_config additional sensitivity tests ──────────

    #[test]
    fn provider_metadata_refresh_is_a_noop_for_unchanged_hash() {
        let mut ctx = DownloadContext::default();
        ctx.downloaded_ids
            .entry("PrimarySync".into())
            .or_default()
            .entry("asset1".into())
            .or_default()
            .insert("original".into());
        ctx.downloaded_metadata_hashes
            .entry("PrimarySync".into())
            .or_default()
            .entry("asset1".into())
            .or_default()
            .insert("original".into(), "metadata-a".into());

        assert!(!ctx.has_provider_metadata_drift("PrimarySync", "asset1", Some("metadata-a")));
        assert!(ctx.has_provider_metadata_drift("PrimarySync", "asset1", Some("metadata-b")));
    }

    /// A tombstoned row cannot be refreshed, so no staleness check may
    /// report it as needing one.
    #[test]
    fn staleness_checks_ignore_soft_deleted_assets() {
        let mut ctx = DownloadContext::default();
        ctx.downloaded_ids
            .entry("PrimarySync".into())
            .or_default()
            .entry("asset1".into())
            .or_default()
            .insert("original".into());
        ctx.metadata_retry_markers
            .entry("PrimarySync".into())
            .or_default()
            .entry("asset1".into())
            .or_default()
            .insert("original".into());

        assert!(ctx.has_provider_metadata_drift("PrimarySync", "asset1", Some("metadata-b")));
        assert!(ctx.has_downloaded_metadata_retry_marker("PrimarySync", "asset1"));
        assert!(ctx.needs_metadata_rewrite(
            "PrimarySync",
            "asset1",
            VersionSizeKey::Original,
            Some("metadata-b")
        ));

        ctx.soft_deleted_ids
            .entry("PrimarySync".into())
            .or_default()
            .insert("asset1".into());

        assert!(!ctx.has_provider_metadata_drift("PrimarySync", "asset1", Some("metadata-b")));
        assert!(!ctx.has_downloaded_metadata_retry_marker("PrimarySync", "asset1"));
        assert!(!ctx.needs_metadata_rewrite(
            "PrimarySync",
            "asset1",
            VersionSizeKey::Original,
            Some("metadata-b")
        ));
    }

    #[test]
    fn test_hash_download_config_changes_on_file_match_policy() {
        let mut config1 = test_config();
        config1.file_match_policy = FileMatchPolicy::NameSizeDedupWithSuffix;
        let mut config2 = test_config();
        config2.file_match_policy = FileMatchPolicy::NameId7;
        assert_ne!(
            hash_download_config(&config1),
            hash_download_config(&config2)
        );
    }

    #[test]
    fn test_hash_download_config_changes_on_keep_unicode() {
        let mut config1 = test_config();
        config1.keep_unicode_in_filenames = false;
        let mut config2 = test_config();
        config2.keep_unicode_in_filenames = true;
        assert_ne!(
            hash_download_config(&config1),
            hash_download_config(&config2)
        );
    }

    #[test]
    fn test_hash_download_config_ignores_unrelated_fields() {
        let mut config1 = test_config();
        config1.concurrent_downloads = 1;
        let mut config2 = test_config();
        config2.concurrent_downloads = 16;
        // These fields don't affect download paths, so hash should be the same
        assert_eq!(
            hash_download_config(&config1),
            hash_download_config(&config2)
        );
    }

    // ── determine_media_type tests ──────────────────────────────────────

    // ── NameId7 filter tests ────────────────────────────────────────────

    // ── keep_unicode_in_filenames tests ─────────────────────────────────

    // ── Medium/Thumb size suffix tests ──────────────────────────────────

    // ── NormalizedPath direct tests ─────────────────────────────────────

    // ---------- SyncMode / SyncResult tests ----------

    #[test]
    fn test_sync_result_partial_failure() {
        let result = SyncResult {
            outcome: DownloadOutcome::PartialFailure { failed_count: 3 },
            sync_token: Some("tok".to_string()),
            stats: SyncStats::default(),
            full_enumeration_ran: false,
        };
        match result.outcome {
            DownloadOutcome::PartialFailure { failed_count } => {
                assert_eq!(failed_count, 3);
            }
            _ => panic!("Expected PartialFailure"),
        }
    }

    #[test]
    fn test_sync_result_session_expired() {
        let result = SyncResult {
            outcome: DownloadOutcome::SessionExpired {
                auth_error_count: 5,
            },
            sync_token: None,
            stats: SyncStats::default(),
            full_enumeration_ran: false,
        };
        match result.outcome {
            DownloadOutcome::SessionExpired { auth_error_count } => {
                assert_eq!(auth_error_count, 5);
            }
            _ => panic!("Expected SessionExpired"),
        }
    }

    fn hard_deleted_change_record(record_name: &str) -> Value {
        json!({
            "recordName": record_name,
            "recordType": null,
            "fields": {},
            "deleted": true,
        })
    }

    fn flagged_incremental_records(record_name: &str, flag: (&str, i64)) -> Vec<Value> {
        let mut records = incremental_photo_records(record_name);
        records[0]["fields"][flag.0] = json!({"value": flag.1, "type": "INT64"});
        records
    }

    fn flagged_cpl_asset_record(record_name: &str, master_ref: &str, flag: (&str, i64)) -> Value {
        let mut records = incremental_photo_records(master_ref);
        let mut asset = records.remove(1);
        asset["recordName"] = json!(record_name);
        asset["fields"][flag.0] = json!({"value": flag.1, "type": "INT64"});
        asset
    }

    fn soft_deleted_cpl_asset_record(record_name: &str, master_ref: &str) -> Value {
        flagged_cpl_asset_record(record_name, master_ref, ("isDeleted", 1))
    }

    fn hidden_cpl_asset_record(record_name: &str, master_ref: &str) -> Value {
        flagged_cpl_asset_record(record_name, master_ref, ("isHidden", 1))
    }

    fn soft_deleted_cpl_asset_record_without_master_ref(record_name: &str) -> Value {
        let mut asset = soft_deleted_cpl_asset_record(record_name, "MISSING_MASTER_REF");
        asset["fields"].as_object_mut().unwrap().remove("masterRef");
        asset
    }

    fn assert_source_flags(
        records: &[crate::state::AssetRecord],
        asset_id: &str,
        expected_deleted: bool,
        expected_hidden: bool,
    ) {
        let record = records
            .iter()
            .find(|record| record.id.as_ref() == asset_id)
            .unwrap_or_else(|| panic!("missing state row for {asset_id}"));
        assert_eq!(
            record.metadata.is_deleted, expected_deleted,
            "is_deleted mismatch for {asset_id}"
        );
        assert_eq!(
            record.metadata.is_hidden, expected_hidden,
            "is_hidden mismatch for {asset_id}"
        );
    }

    async fn run_incremental_change_records(
        db: Arc<crate::state::SqliteStateDb>,
        records: Vec<Value>,
    ) -> SyncResult {
        let session = MockPhotosFlow::new()
            .changes_zone_page(records, "zone-token-after", false)
            .build();
        let passes = vec![AlbumPass {
            kind: PassKind::Unfiled,
            album: mock_album("Library", session),
            exclude_ids: Arc::new(FxHashSet::default()),
        }];

        let mut config = test_config();
        config.state_db = Some(db);
        download_photos_incremental(
            &Client::new(),
            &passes,
            &Arc::new(config),
            "zone-token-before",
            DownloadControls::download_hidden(),
            CancellationToken::new(),
        )
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn download_incremental_delete_and_hidden_events_mark_state_without_downloads() {
        let db = Arc::new(crate::state::SqliteStateDb::open_in_memory().unwrap());
        for id in ["SOFT_DELETE", "HARD_DELETE", "HIDDEN_ASSET"] {
            db.upsert_seen(&TestAssetRecord::new(id).build())
                .await
                .unwrap();
        }

        let mut records = Vec::new();
        records.extend(flagged_incremental_records("SOFT_DELETE", ("isDeleted", 1)));
        records.push(hard_deleted_change_record("HARD_DELETE"));
        records.extend(flagged_incremental_records("HIDDEN_ASSET", ("isHidden", 1)));
        records.extend(incremental_photo_records("CREATED_ASSET"));
        let session = MockPhotosFlow::new()
            .changes_zone_page(records, "zone-token-after", false)
            .build();
        let passes = vec![AlbumPass {
            kind: PassKind::Album,
            album: mock_album("Library", session),
            exclude_ids: Arc::new(FxHashSet::default()),
        }];

        let mut config = test_config();
        let dir = TempDir::new().unwrap();
        config.directory = Arc::from(dir.path());
        config.state_db = Some(db.clone());

        let result = download_photos_incremental(
            &Client::new(),
            &passes,
            &Arc::new(config),
            "zone-token-before",
            DownloadControls::new(DownloadRunMode::PrintFilenames, DownloadReporting::hidden()),
            CancellationToken::new(),
        )
        .await
        .unwrap();

        assert!(matches!(result.outcome, DownloadOutcome::Success));
        assert_eq!(
            result.sync_token, None,
            "print-only incremental runs must not advance the sync token"
        );
        let pending = db.get_pending().await.unwrap();
        assert!(
            pending
                .iter()
                .all(|record| record.id.as_ref() != "SOFT_DELETE"
                    && record.id.as_ref() != "HARD_DELETE"),
            "source-deleted pending rows must be pruned: {pending:?}"
        );
        assert_source_flags(&pending, "HIDDEN_ASSET", false, true);
    }

    #[tokio::test]
    async fn incremental_soft_delete_write_error_blocks_sync_token() {
        let db = Arc::new(crate::state::SqliteStateDb::open_in_memory().unwrap());
        db.upsert_seen(&TestAssetRecord::new("SOFT_DELETE_ERR").build())
            .await
            .unwrap();
        {
            let conn = db.acquire_lock("test").unwrap();
            conn.execute("DROP TABLE assets", []).unwrap();
        }
        let result = run_incremental_change_records(
            db,
            flagged_incremental_records("SOFT_DELETE_ERR", ("isDeleted", 1)),
        )
        .await;

        assert!(matches!(
            result.outcome,
            DownloadOutcome::PartialFailure { failed_count: 1 }
        ));
        assert_eq!(result.sync_token, None);
        assert_eq!(result.stats.state_write_failures, 1);
        assert!(result.stats.sync_token_blocked);
        assert_eq!(
            result.stats.sync_token_blocked_reason,
            Some(INCREMENTAL_DELETE_STATE_WRITE_FAILED_REASON)
        );
    }

    #[tokio::test]
    async fn incremental_hidden_write_error_blocks_sync_token() {
        let db = Arc::new(crate::state::SqliteStateDb::open_in_memory().unwrap());
        db.upsert_seen(&TestAssetRecord::new("HIDDEN_ERR").build())
            .await
            .unwrap();
        {
            let conn = db.acquire_lock("test").unwrap();
            conn.execute("DROP TABLE assets", []).unwrap();
        }
        let result = run_incremental_change_records(
            db,
            flagged_incremental_records("HIDDEN_ERR", ("isHidden", 1)),
        )
        .await;

        assert!(matches!(
            result.outcome,
            DownloadOutcome::PartialFailure { failed_count: 1 }
        ));
        assert_eq!(result.sync_token, None);
        assert_eq!(result.stats.state_write_failures, 1);
        assert!(result.stats.sync_token_blocked);
        assert_eq!(
            result.stats.sync_token_blocked_reason,
            Some(INCREMENTAL_HIDDEN_STATE_WRITE_FAILED_REASON)
        );
    }

    #[tokio::test]
    async fn incremental_untracked_cplmaster_soft_delete_zero_rows_advances_sync_token() {
        let db = Arc::new(crate::state::SqliteStateDb::open_in_memory().unwrap());
        let result = run_incremental_change_records(
            db.clone(),
            flagged_incremental_records("UNTRACKED_SOFT_DELETE", ("isDeleted", 1)),
        )
        .await;

        assert!(matches!(result.outcome, DownloadOutcome::Success));
        assert_eq!(result.sync_token.as_deref(), Some("zone-token-after"));
        assert_eq!(result.stats.state_write_failures, 0);
        assert!(!result.stats.sync_token_blocked);
        assert!(db.get_pending().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn incremental_untracked_cplasset_soft_delete_zero_rows_advances_sync_token() {
        let db = Arc::new(crate::state::SqliteStateDb::open_in_memory().unwrap());
        let result = run_incremental_change_records(
            db,
            vec![soft_deleted_cpl_asset_record(
                "asset-UNTRACKED_SOFT_DELETE",
                "UNTRACKED_SOFT_DELETE",
            )],
        )
        .await;

        assert!(matches!(result.outcome, DownloadOutcome::Success));
        assert_eq!(result.sync_token.as_deref(), Some("zone-token-after"));
        assert_eq!(result.stats.state_write_failures, 0);
        assert!(!result.stats.sync_token_blocked);
    }

    #[tokio::test]
    async fn incremental_untracked_cplasset_soft_delete_without_master_ref_advances_sync_token() {
        let db = Arc::new(crate::state::SqliteStateDb::open_in_memory().unwrap());
        let result = run_incremental_change_records(
            db,
            vec![soft_deleted_cpl_asset_record_without_master_ref(
                "asset-UNTRACKED_SOFT_DELETE",
            )],
        )
        .await;

        assert!(matches!(result.outcome, DownloadOutcome::Success));
        assert_eq!(result.sync_token.as_deref(), Some("zone-token-after"));
        assert_eq!(result.stats.state_write_failures, 0);
        assert!(!result.stats.sync_token_blocked);
    }

    #[tokio::test]
    async fn incremental_cplasset_soft_delete_prunes_master_ref_pending_row() {
        let db = Arc::new(crate::state::SqliteStateDb::open_in_memory().unwrap());
        db.upsert_seen(&TestAssetRecord::new("TRACKED_MASTER").build())
            .await
            .unwrap();
        let result = run_incremental_change_records(
            db.clone(),
            vec![soft_deleted_cpl_asset_record(
                "asset-TRACKED_MASTER",
                "TRACKED_MASTER",
            )],
        )
        .await;

        assert!(matches!(result.outcome, DownloadOutcome::Success));
        assert_eq!(result.sync_token.as_deref(), Some("zone-token-after"));
        assert_eq!(result.stats.state_write_failures, 0);
        assert!(!result.stats.sync_token_blocked);
        assert!(
            db.get_pending().await.unwrap().is_empty(),
            "source-deleted pending row should be removed"
        );
    }

    #[tokio::test]
    async fn incremental_cplasset_hidden_marks_master_ref_row() {
        let db = Arc::new(crate::state::SqliteStateDb::open_in_memory().unwrap());
        db.upsert_seen(&TestAssetRecord::new("TRACKED_HIDDEN_MASTER").build())
            .await
            .unwrap();
        let result = run_incremental_change_records(
            db.clone(),
            vec![hidden_cpl_asset_record(
                "asset-TRACKED_HIDDEN_MASTER",
                "TRACKED_HIDDEN_MASTER",
            )],
        )
        .await;

        assert!(matches!(result.outcome, DownloadOutcome::Success));
        assert_eq!(result.sync_token.as_deref(), Some("zone-token-after"));
        assert_eq!(result.stats.state_write_failures, 0);
        assert!(!result.stats.sync_token_blocked);
        let pending = db.get_pending().await.unwrap();
        assert_source_flags(&pending, "TRACKED_HIDDEN_MASTER", false, true);
    }

    #[tokio::test]
    async fn incremental_cplasset_soft_delete_prunes_sibling_pending_row_before_master_ref() {
        let db = Arc::new(crate::state::SqliteStateDb::open_in_memory().unwrap());
        db.upsert_seen(&TestAssetRecord::new("TRACKED_MASTER").build())
            .await
            .unwrap();
        db.upsert_seen(&TestAssetRecord::new("asset-TRACKED_SIBLING").build())
            .await
            .unwrap();
        db.upsert_asset_master_mapping("PrimarySync", "asset-TRACKED_SIBLING", "TRACKED_MASTER")
            .await
            .unwrap();

        let result = run_incremental_change_records(
            db.clone(),
            vec![soft_deleted_cpl_asset_record(
                "asset-TRACKED_SIBLING",
                "TRACKED_MASTER",
            )],
        )
        .await;

        assert!(matches!(result.outcome, DownloadOutcome::Success));
        assert_eq!(result.sync_token.as_deref(), Some("zone-token-after"));
        assert_eq!(result.stats.state_write_failures, 0);
        assert!(!result.stats.sync_token_blocked);
        let pending = db.get_pending().await.unwrap();
        assert_source_flags(&pending, "TRACKED_MASTER", false, false);
        assert!(
            pending
                .iter()
                .all(|record| record.id.as_ref() != "asset-TRACKED_SIBLING"),
            "source-deleted sibling pending row should be removed: {pending:?}"
        );
    }

    #[tokio::test]
    async fn incremental_cplasset_hidden_marks_sibling_state_row_before_master_ref() {
        let db = Arc::new(crate::state::SqliteStateDb::open_in_memory().unwrap());
        db.upsert_seen(&TestAssetRecord::new("TRACKED_HIDDEN_MASTER").build())
            .await
            .unwrap();
        db.upsert_seen(&TestAssetRecord::new("asset-TRACKED_HIDDEN_SIBLING").build())
            .await
            .unwrap();
        db.upsert_asset_master_mapping(
            "PrimarySync",
            "asset-TRACKED_HIDDEN_SIBLING",
            "TRACKED_HIDDEN_MASTER",
        )
        .await
        .unwrap();

        let result = run_incremental_change_records(
            db.clone(),
            vec![hidden_cpl_asset_record(
                "asset-TRACKED_HIDDEN_SIBLING",
                "TRACKED_HIDDEN_MASTER",
            )],
        )
        .await;

        assert!(matches!(result.outcome, DownloadOutcome::Success));
        assert_eq!(result.sync_token.as_deref(), Some("zone-token-after"));
        assert_eq!(result.stats.state_write_failures, 0);
        assert!(!result.stats.sync_token_blocked);
        let pending = db.get_pending().await.unwrap();
        assert_source_flags(&pending, "TRACKED_HIDDEN_MASTER", false, false);
        assert_source_flags(&pending, "asset-TRACKED_HIDDEN_SIBLING", false, true);
    }

    #[tokio::test]
    async fn incremental_change_persists_asset_master_mapping() {
        let db = Arc::new(crate::state::SqliteStateDb::open_in_memory().unwrap());
        let result = run_incremental_change_records(
            db.clone(),
            flagged_incremental_records("MAPPED_MASTER", ("isDeleted", 1)),
        )
        .await;

        assert!(matches!(result.outcome, DownloadOutcome::Success));
        assert_eq!(
            db.get_master_record_name_for_asset("PrimarySync", "asset-MAPPED_MASTER")
                .await
                .unwrap()
                .as_deref(),
            Some("MAPPED_MASTER")
        );
    }

    #[tokio::test]
    async fn incremental_untracked_hidden_zero_rows_advances_sync_token() {
        let db = Arc::new(crate::state::SqliteStateDb::open_in_memory().unwrap());
        let result = run_incremental_change_records(
            db,
            flagged_incremental_records("UNTRACKED_HIDDEN", ("isHidden", 1)),
        )
        .await;

        assert!(matches!(result.outcome, DownloadOutcome::Success));
        assert_eq!(result.sync_token.as_deref(), Some("zone-token-after"));
        assert_eq!(result.stats.state_write_failures, 0);
        assert!(!result.stats.sync_token_blocked);
    }

    #[tokio::test]
    async fn incremental_unresolved_hard_delete_zero_rows_advances_sync_token() {
        let db = Arc::new(crate::state::SqliteStateDb::open_in_memory().unwrap());
        db.upsert_seen(&TestAssetRecord::new("TRACKED_MASTER").build())
            .await
            .unwrap();
        let result = run_incremental_change_records(
            db.clone(),
            vec![hard_deleted_change_record("asset-TRACKED_MASTER")],
        )
        .await;

        assert!(matches!(result.outcome, DownloadOutcome::Success));
        assert_eq!(result.sync_token.as_deref(), Some("zone-token-after"));
        assert_eq!(result.stats.state_write_failures, 0);
        assert!(!result.stats.sync_token_blocked);
        let pending = db.get_pending().await.unwrap();
        assert_source_flags(&pending, "TRACKED_MASTER", false, false);
    }

    #[tokio::test]
    async fn incremental_unresolved_master_hard_delete_prunes_sibling_pending_rows() {
        let db = Arc::new(crate::state::SqliteStateDb::open_in_memory().unwrap());
        db.upsert_seen(&TestAssetRecord::new("TRACKED_MASTER").build())
            .await
            .unwrap();
        db.upsert_seen(&TestAssetRecord::new("asset-SIBLING").build())
            .await
            .unwrap();
        db.upsert_asset_master_mapping("PrimarySync", "asset-SIBLING", "TRACKED_MASTER")
            .await
            .unwrap();

        let result = run_incremental_change_records(
            db.clone(),
            vec![hard_deleted_change_record("TRACKED_MASTER")],
        )
        .await;

        assert!(matches!(result.outcome, DownloadOutcome::Success));
        assert_eq!(result.sync_token.as_deref(), Some("zone-token-after"));
        assert_eq!(result.stats.state_write_failures, 0);
        assert!(!result.stats.sync_token_blocked);
        assert!(
            db.get_pending().await.unwrap().is_empty(),
            "master-family hard delete should remove pending retry rows"
        );
    }

    #[tokio::test]
    async fn incremental_hard_delete_uses_library_scoped_asset_master_mapping() {
        let db = Arc::new(crate::state::SqliteStateDb::open_in_memory().unwrap());
        db.upsert_seen(
            &TestAssetRecord::new("PRIMARY_MASTER")
                .library("PrimarySync")
                .build(),
        )
        .await
        .unwrap();
        db.upsert_seen(
            &TestAssetRecord::new("SHARED_MASTER")
                .library("SharedSync-AAAA")
                .build(),
        )
        .await
        .unwrap();
        db.upsert_asset_master_mapping("PrimarySync", "asset-SAME", "PRIMARY_MASTER")
            .await
            .unwrap();
        db.upsert_asset_master_mapping("SharedSync-AAAA", "asset-SAME", "SHARED_MASTER")
            .await
            .unwrap();

        let result = run_incremental_change_records(
            db.clone(),
            vec![hard_deleted_change_record("asset-SAME")],
        )
        .await;

        assert!(matches!(result.outcome, DownloadOutcome::Success));
        assert_eq!(result.sync_token.as_deref(), Some("zone-token-after"));
        assert_eq!(result.stats.state_write_failures, 0);
        assert!(!result.stats.sync_token_blocked);

        let pending = db.get_pending().await.unwrap();
        assert!(
            pending
                .iter()
                .all(|record| record.library.as_ref() != "PrimarySync"),
            "primary source-deleted pending row should be removed: {pending:?}"
        );
        let shared = pending
            .iter()
            .find(|record| record.library.as_ref() == "SharedSync-AAAA")
            .expect("shared row");
        assert_eq!(shared.id.as_ref(), "SHARED_MASTER");
        assert!(
            !shared.metadata.is_deleted,
            "same CPLAsset record name in another library must stay isolated"
        );
    }

    #[tokio::test]
    async fn incremental_hard_delete_recovers_mapping_from_album_history() {
        let db = Arc::new(crate::state::SqliteStateDb::open_in_memory().unwrap());
        db.upsert_seen(
            &TestAssetRecord::new("TRACKED_MASTER")
                .library("PrimarySync")
                .build(),
        )
        .await
        .unwrap();
        db.mark_downloaded(
            "PrimarySync",
            "TRACKED_MASTER",
            VersionSizeKey::Original.as_str(),
            std::path::Path::new("/tmp/codex/kei/tests/tracked-master.jpg"),
            "seeded-local-sha256",
            None,
        )
        .await
        .unwrap();
        db.upsert_album_container("PrimarySync", "container-a", "Vacation", "album")
            .await
            .unwrap();
        db.upsert_album_membership_delta(
            "PrimarySync",
            "container-a",
            "asset-TRACKED_MASTER",
            Some("TRACKED_MASTER"),
            "icloud",
        )
        .await
        .unwrap();
        assert_eq!(
            db.get_master_record_name_for_asset("PrimarySync", "asset-TRACKED_MASTER")
                .await
                .unwrap(),
            None
        );

        let session = MockPhotosFlow::new()
            .changes_zone_page(
                vec![hard_deleted_change_record("asset-TRACKED_MASTER")],
                "zone-token-after",
                false,
            )
            .build();
        let passes = vec![AlbumPass {
            kind: PassKind::Unfiled,
            album: mock_album("Library", session),
            exclude_ids: Arc::new(FxHashSet::default()),
        }];
        let mut config = test_config();
        config.sync_mode = SyncMode::Incremental {
            zone_sync_token: "zone-token-before".to_string(),
        };
        config.state_db = Some(db.clone());

        let result = download_photos_with_sync(
            &Client::new(),
            &passes,
            Arc::new(config),
            DownloadControls::download_hidden(),
            CancellationToken::new(),
        )
        .await
        .unwrap();

        assert!(matches!(result.outcome, DownloadOutcome::Success));
        assert_eq!(result.sync_token.as_deref(), Some("zone-token-after"));
        assert_eq!(result.stats.state_write_failures, 0);
        assert!(!result.stats.sync_token_blocked);
        assert_eq!(
            db.get_master_record_name_for_asset("PrimarySync", "asset-TRACKED_MASTER")
                .await
                .unwrap()
                .as_deref(),
            Some("TRACKED_MASTER")
        );
        let is_deleted: i64 = db
            .acquire_lock("test")
            .unwrap()
            .query_row(
                "SELECT is_deleted FROM assets \
                 WHERE library = 'PrimarySync' AND id = 'TRACKED_MASTER'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(is_deleted, 1);
    }

    #[tokio::test]
    async fn changes_stream_mixed_malformed_asset_relation_preserves_valid_and_blocks_token() {
        let db = Arc::new(crate::state::SqliteStateDb::open_in_memory().unwrap());
        db.upsert_album_container("PrimarySync", "container-a", "Vacation", "album")
            .await
            .unwrap();
        let mut malformed = incremental_photo_records("MALFORMED_ASSET");
        malformed[0]["fields"]["resOriginalRes"]["value"]
            .as_object_mut()
            .expect("resource value object")
            .remove("downloadURL");
        let mut records = incremental_photo_records("VALID_ASSET");
        records.extend(malformed);
        records.push(relation_delta_record("container-a", "asset-VALID_ASSET"));
        let session = MockPhotosFlow::new()
            .changes_zone_page(records, "zone-token-after", false)
            .build();
        let passes = vec![
            AlbumPass {
                kind: PassKind::Album,
                album: changes_album_with_container("Vacation", Some("container-a"), session),
                exclude_ids: Arc::new(FxHashSet::default()),
            },
            unused_unfiled_changes_pass(),
        ];

        let mut config = test_config();
        let dir = TempDir::new().unwrap();
        config.directory = Arc::from(dir.path());
        config.state_db = Some(db.clone());
        let result = download_photos_incremental(
            &Client::new(),
            &passes,
            &Arc::new(config),
            "zone-token-before",
            DownloadControls::new(DownloadRunMode::PrintFilenames, DownloadReporting::hidden()),
            CancellationToken::new(),
        )
        .await
        .unwrap();

        assert!(matches!(
            result.outcome,
            DownloadOutcome::PartialFailure { failed_count: 1 }
        ));
        assert_eq!(result.sync_token, None);
        assert_eq!(result.stats.enumeration_errors, 1);
        let pending = db.get_pending().await.unwrap();
        assert!(
            pending
                .iter()
                .any(|record| record.id.as_ref() == "asset-VALID_ASSET"),
            "valid incremental asset should still be recorded for the planned work"
        );
        let memberships = db
            .get_live_selected_album_memberships_for_asset(
                "PrimarySync",
                "asset-VALID_ASSET",
                &["container-a"],
            )
            .await
            .unwrap();
        assert_eq!(memberships.len(), 1);
    }

    // ── NormalizedPath additional tests ──────────────────────────────────

    // ── hash_download_config additional sensitivity ─────────────────────

    #[test]
    fn test_hash_download_config_changes_on_resolution() {
        let mut config1 = test_config();
        config1.resolution = crate::types::PhotoResolution::Original;
        let mut config2 = test_config();
        config2.resolution = crate::types::PhotoResolution::Medium;
        assert_ne!(
            hash_download_config(&config1),
            hash_download_config(&config2)
        );
    }

    #[test]
    fn test_hash_download_config_changes_on_live_resolution() {
        let mut config1 = test_config();
        config1.live_resolution = AssetVersionSize::LiveOriginal;
        let mut config2 = test_config();
        config2.live_resolution = AssetVersionSize::LiveMedium;
        assert_ne!(
            hash_download_config(&config1),
            hash_download_config(&config2)
        );
    }

    #[test]
    fn test_hash_download_config_changes_on_live_photo_mov_filename_policy() {
        let mut config1 = test_config();
        config1.live_photo_mov_filename_policy = crate::types::LivePhotoMovFilenamePolicy::Suffix;
        let mut config2 = test_config();
        config2.live_photo_mov_filename_policy = crate::types::LivePhotoMovFilenamePolicy::Original;
        assert_ne!(
            hash_download_config(&config1),
            hash_download_config(&config2)
        );
    }

    #[test]
    fn test_hash_download_config_changes_on_raw_policy() {
        let mut config1 = test_config();
        config1.raw_policy = RawPolicy::AsIs;
        let mut config2 = test_config();
        config2.raw_policy = RawPolicy::PreferRaw;
        assert_ne!(
            hash_download_config(&config1),
            hash_download_config(&config2)
        );
    }

    #[test]
    fn test_hash_download_config_ignores_skip_created_before() {
        let mut config1 = test_config();
        config1.skip_created_before = None;
        let mut config2 = test_config();
        config2.skip_created_before = Some(crate::config::CreatedDateFilter::Instant(
            DateTime::parse_from_rfc3339("2020-01-01T00:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
        ));
        assert_eq!(
            hash_download_config(&config1),
            hash_download_config(&config2)
        );
    }

    #[test]
    fn test_hash_download_config_ignores_skip_created_after() {
        let mut config1 = test_config();
        config1.skip_created_after = None;
        let mut config2 = test_config();
        config2.skip_created_after = Some(crate::config::CreatedDateFilter::Instant(
            DateTime::parse_from_rfc3339("2024-12-31T00:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
        ));
        assert_eq!(
            hash_download_config(&config1),
            hash_download_config(&config2)
        );
    }

    #[test]
    fn test_hash_download_config_ignores_created_date_semantics() {
        let date = chrono::NaiveDate::from_ymd_opt(2025, 2, 1).unwrap();
        let mut instant = test_config();
        instant.skip_created_before = Some(crate::config::CreatedDateFilter::Instant(
            date.and_time(chrono::NaiveTime::MIN).and_utc(),
        ));
        let mut capture_date = instant.clone();
        capture_date.skip_created_before =
            Some(crate::config::CreatedDateFilter::CaptureDate(date));

        assert_eq!(
            hash_download_config(&instant),
            hash_download_config(&capture_date)
        );
    }

    #[test]
    fn test_legacy_download_config_hash_changes_on_skip_created_before() {
        let mut config1 = test_config();
        config1.skip_created_before = None;
        let mut config2 = test_config();
        config2.skip_created_before = Some(crate::config::CreatedDateFilter::Instant(
            DateTime::parse_from_rfc3339("2020-01-01T00:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
        ));
        assert_ne!(
            hash_legacy_download_config(&config1),
            hash_legacy_download_config(&config2)
        );
    }

    #[test]
    fn test_hash_download_config_changes_on_recent() {
        let mut config1 = test_config();
        config1.recent = None;
        let mut config2 = test_config();
        config2.recent = Some(100);
        assert_ne!(
            hash_download_config(&config1),
            hash_download_config(&config2)
        );
    }

    #[test]
    fn test_hash_download_config_changes_on_recent_scope_when_recent_is_set() {
        let mut config1 = test_config();
        config1.recent = Some(100);
        config1.recent_scope = crate::cli::RecentScope::Global;
        let mut config2 = test_config();
        config2.recent = Some(100);
        config2.recent_scope = crate::cli::RecentScope::PerFilter;
        assert_ne!(
            hash_download_config(&config1),
            hash_download_config(&config2)
        );
    }

    #[test]
    fn test_hash_download_config_changes_on_force_resolution() {
        let mut config1 = test_config();
        config1.force_resolution = false;
        let mut config2 = test_config();
        config2.force_resolution = true;
        assert_ne!(
            hash_download_config(&config1),
            hash_download_config(&config2)
        );
    }

    #[test]
    fn test_hash_download_config_changes_on_media_videos() {
        let mut config1 = test_config();
        config1.media.videos = true;
        let mut config2 = test_config();
        config2.media.videos = false;
        assert_ne!(
            hash_download_config(&config1),
            hash_download_config(&config2)
        );
    }

    #[test]
    fn test_hash_download_config_changes_on_media_photos() {
        let mut config1 = test_config();
        config1.media.photos = true;
        let mut config2 = test_config();
        config2.media.photos = false;
        assert_ne!(
            hash_download_config(&config1),
            hash_download_config(&config2)
        );
    }

    #[test]
    fn test_hash_download_config_is_16_hex_chars() {
        let config = test_config();
        let hash = hash_download_config(&config);
        assert_eq!(hash.len(), 16);
        assert!(
            hash.chars().all(|c| c.is_ascii_hexdigit()),
            "Hash should be hex chars only, got: {hash}"
        );
    }

    // ── compute_config_hash equivalence ────────────────────────────────

    /// `compute_config_hash` tracks CloudKit token safety, not local path
    /// trust-state. Verify it produces a valid hex hash and is deterministic.
    #[test]
    fn test_compute_config_hash_matches_hash_download_config() {
        use crate::config::Config;

        let dl_config = test_config();
        let globals = crate::config::GlobalArgs {
            username: Some("u@example.com".to_string()),
            domain: None,
            data_dir: Some("/tmp".to_string()),
        };
        let app_config = Config::build(
            &globals,
            &crate::cli::PasswordArgs::default(),
            crate::cli::SyncArgs {
                recent: dl_config.recent.map(crate::cli::RecentLimit::Count),
                config_overrides: crate::config::SyncConfigOverrides {
                    download_dir: Some(dl_config.directory.display().to_string()),
                    folder_structure: Some(dl_config.folder_structure.clone()),
                    resolution: Some(crate::types::PhotoResolution::Original),
                    ..Default::default()
                },
                no_progress_bar: true,
                ..Default::default()
            },
            None,
        )
        .unwrap();

        // compute_config_hash tracks only CloudKit-token safety fields. Verify
        // it is deterministic and valid hex.
        let hash1 = compute_config_hash(&app_config);
        let hash2 = compute_config_hash(&app_config);
        assert_eq!(hash1, hash2, "compute_config_hash must be deterministic");
        assert_eq!(hash1.len(), 16);
        assert!(hash1.chars().all(|c| c.is_ascii_hexdigit()));

        // Album changes are handled by membership snapshots and targeted
        // backfill, not by invalidating the zone token.
        let mut config_with_album = app_config;
        config_with_album.filters.selection.albums =
            crate::selection::parse_album_selector(&["Favorites".to_string()], true).unwrap();
        let hash3 = compute_config_hash(&config_with_album);
        assert_eq!(
            hash1, hash3,
            "adding an album must keep the zone-token hash"
        );
    }

    // ── should_download_fast additional tests ───────────────────────────

    #[test]
    fn test_should_download_fast_unknown_asset_returns_true() {
        let ctx = DownloadContext::default();
        assert_eq!(
            ctx.should_download_fast(
                "PrimarySync",
                "never_seen",
                VersionSizeKey::Original,
                "any_ck",
                true
            ),
            Some(true)
        );
        assert_eq!(
            ctx.should_download_fast(
                "PrimarySync",
                "never_seen",
                VersionSizeKey::Original,
                "any_ck",
                false
            ),
            Some(true)
        );
    }

    #[tokio::test]
    async fn unfiled_only_incremental_ignores_inactive_album_path_templates() {
        let calls = Arc::new(AtomicUsize::new(0));
        let session = changes_zone_session(
            Arc::clone(&calls),
            incremental_photo_records("MASTER_CHANGED"),
        );
        let passes = vec![AlbumPass {
            kind: PassKind::Unfiled,
            album: changes_album("", session),
            exclude_ids: Arc::new(FxHashSet::default()),
        }];

        let mut config = test_config();
        assert!(
            config.requires_per_pass_paths(),
            "default inactive album templates still contain per-pass tokens"
        );
        let dir = TempDir::new().expect("temp dir");
        config.directory = Arc::from(dir.path());
        config.sync_mode = SyncMode::Incremental {
            zone_sync_token: "zone-token-prev".to_string(),
        };

        let result = download_photos_with_sync(
            &Client::new(),
            &passes,
            Arc::new(config),
            DownloadControls::new(DownloadRunMode::PrintFilenames, DownloadReporting::hidden()),
            CancellationToken::new(),
        )
        .await
        .expect("unfiled-only incremental sync should succeed");

        assert!(matches!(result.outcome, DownloadOutcome::Success));
        assert!(
            !result.full_enumeration_ran,
            "inactive album/smart-folder templates must not force full enumeration"
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "unfiled-only incremental sync should query changes/zone"
        );
    }

    #[tokio::test]
    async fn smart_folder_incremental_with_unfiled_refreshes_smart_without_library_all() {
        let changes_calls = Arc::new(AtomicUsize::new(0));
        let smart_session = CountingQuerySession::new(
            mock_photo_query_page("SMART_CHANGED", Some("zone-token")),
            1,
        );
        let passes = smart_folder_unfiled_passes(
            Arc::clone(&changes_calls),
            incremental_photo_records("MASTER_CHANGED"),
            smart_session.clone(),
        );
        let dir = TempDir::new().expect("temp dir");
        let config = incremental_test_config(&dir);

        let result = run_print_incremental_sync(&passes, config).await;

        assert!(matches!(result.outcome, DownloadOutcome::Success));
        assert!(
            result.full_enumeration_ran,
            "the selected smart-folder stream still refreshes by records/query"
        );
        assert_eq!(
            changes_calls.load(Ordering::SeqCst),
            1,
            "unfiled follow-up work should use changes/zone"
        );
        assert_eq!(
            smart_session.count_query_count(),
            1,
            "smart-folder refresh should probe the selected smart folder"
        );
        assert_eq!(
            smart_session.records_query_count(),
            1 + crate::icloud::photos::MAX_EMPTY_PAGE_PROBES as usize,
            "smart-folder refresh should enumerate the selected smart folder"
        );
        assert!(
            !result.stats.sync_token_blocked,
            "successful smart-folder refresh should not block an otherwise safe incremental cycle"
        );
    }

    #[tokio::test]
    async fn smart_folder_refresh_blank_query_token_does_not_block_incremental_zone_token() {
        let changes_calls = Arc::new(AtomicUsize::new(0));
        let smart_session = CountingQuerySession::new(
            json!({
                "records": [],
                "syncToken": ""
            }),
            0,
        );
        let passes = smart_folder_unfiled_passes(
            Arc::clone(&changes_calls),
            Vec::new(),
            smart_session.clone(),
        );
        let dir = TempDir::new().expect("temp dir");
        let config = incremental_test_config(&dir);

        let result = download_photos_with_sync(
            &Client::new(),
            &passes,
            Arc::new(config),
            DownloadControls::download_hidden(),
            CancellationToken::new(),
        )
        .await
        .expect("incremental sync plus smart-folder refresh should succeed");

        assert!(matches!(result.outcome, DownloadOutcome::Success));
        assert_eq!(
            changes_calls.load(Ordering::SeqCst),
            1,
            "unfiled incremental changes should still be checked"
        );
        assert_eq!(
            smart_session.records_query_count(),
            crate::icloud::photos::MAX_EMPTY_PAGE_PROBES as usize
        );
        assert_eq!(result.sync_token.as_deref(), Some("zone-token-next"));
        assert!(
            !result.stats.sync_token_blocked,
            "blank smart-folder query token is telemetry for the refresh, not a reason to block the incremental zone token"
        );
    }

    #[tokio::test]
    async fn smart_folder_incremental_recent_global_does_not_build_library_frontier() {
        let changes_calls = Arc::new(AtomicUsize::new(0));
        let smart_session = CountingQuerySession::new(
            mock_photo_query_page("SMART_CHANGED", Some("zone-token")),
            1,
        );
        let passes = smart_folder_unfiled_passes(
            Arc::clone(&changes_calls),
            Vec::new(),
            smart_session.clone(),
        );
        let dir = TempDir::new().expect("temp dir");
        let mut config = incremental_test_config(&dir);
        config.recent = Some(1);
        config.recent_scope = crate::cli::RecentScope::Global;

        let result = run_print_incremental_sync(&passes, config).await;

        assert!(matches!(result.outcome, DownloadOutcome::Success));
        assert_eq!(changes_calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            smart_session.records_query_count(),
            1 + crate::icloud::photos::MAX_EMPTY_PAGE_PROBES as usize,
            "smart-folder refresh must enumerate only the selected smart-folder stream"
        );
    }

    #[tokio::test]
    async fn smart_folder_refresh_failure_blocks_incremental_token() {
        let changes_calls = Arc::new(AtomicUsize::new(0));
        let smart_session = CountingQuerySession::failing(
            mock_photo_query_page("SMART_CHANGED", Some("zone-token")),
            1,
        );
        let passes = smart_folder_unfiled_passes(
            Arc::clone(&changes_calls),
            Vec::new(),
            smart_session.clone(),
        );
        let dir = TempDir::new().expect("temp dir");
        let config = incremental_test_config(&dir);

        let result = run_print_incremental_sync(&passes, config).await;

        assert!(matches!(
            result.outcome,
            DownloadOutcome::PartialFailure { failed_count: 1 }
        ));
        assert_eq!(
            changes_calls.load(Ordering::SeqCst),
            1,
            "unfiled incremental changes should still be checked"
        );
        assert_eq!(smart_session.records_query_count(), 1);
        assert!(result.stats.sync_token_blocked);
        assert_eq!(
            result.stats.sync_token_blocked_reason,
            Some(SMART_FOLDER_REFRESH_FAILED_REASON)
        );
        assert_eq!(result.sync_token, None);
    }

    #[tokio::test]
    async fn album_incremental_records_relation_hydration_full_enumeration_reason() {
        let session = MockPhotosFlow::new()
            .album_count(0)
            .empty_query_page(Some("zone-token-next"))
            .build();
        let passes = vec![AlbumPass {
            kind: PassKind::Album,
            album: mock_album("Vacation", session),
            exclude_ids: Arc::new(FxHashSet::default()),
        }];

        let mut config = test_config();
        let dir = TempDir::new().expect("temp dir");
        config.directory = Arc::from(dir.path());
        config.sync_mode = SyncMode::Incremental {
            zone_sync_token: "zone-token-prev".to_string(),
        };

        let result = download_photos_with_sync(
            &Client::new(),
            &passes,
            Arc::new(config),
            DownloadControls::new(DownloadRunMode::PrintFilenames, DownloadReporting::hidden()),
            CancellationToken::new(),
        )
        .await
        .expect("album incremental should fall back to full enumeration");

        assert!(result.full_enumeration_ran);
        assert_eq!(
            result.stats.full_enumeration_reason,
            Some(FullEnumerationReason::AlbumRelationHydrationIncomplete)
        );
    }

    #[tokio::test]
    async fn album_incremental_with_complete_snapshot_uses_changes_zone() {
        let db = Arc::new(SqliteStateDb::open_in_memory().expect("state db"));
        seed_complete_album_snapshot(&db, "container-vacation", "Vacation", &[]).await;
        let calls = Arc::new(AtomicUsize::new(0));
        let session = changes_zone_session(Arc::clone(&calls), Vec::new());
        let passes = vec![
            AlbumPass {
                kind: PassKind::Album,
                album: changes_album_with_container(
                    "Vacation",
                    Some("container-vacation"),
                    session,
                ),
                exclude_ids: Arc::new(FxHashSet::default()),
            },
            unused_unfiled_changes_pass(),
        ];

        let mut config = test_config();
        let dir = TempDir::new().expect("temp dir");
        config.directory = Arc::from(dir.path());
        config.state_db = Some(Arc::clone(&db) as Arc<dyn DownloadStore>);
        config.sync_mode = SyncMode::Incremental {
            zone_sync_token: "zone-token-prev".to_string(),
        };

        let result = download_photos_with_sync(
            &Client::new(),
            &passes,
            Arc::new(config),
            DownloadControls::new(DownloadRunMode::PrintFilenames, DownloadReporting::hidden()),
            CancellationToken::new(),
        )
        .await
        .expect("trusted album snapshot should allow incremental sync");

        assert!(matches!(result.outcome, DownloadOutcome::Success));
        assert!(
            !result.full_enumeration_ran,
            "complete album snapshots should avoid whole-library fallback"
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "trusted album incremental sync should query changes/zone once"
        );
    }

    #[tokio::test]
    async fn album_incremental_missing_snapshot_runs_targeted_backfill() {
        let changes_zone_calls = Arc::new(AtomicUsize::new(0));
        let album_session = BackfillAndChangesSession {
            changes_zone_calls: Arc::clone(&changes_zone_calls),
        };
        let unfiled_session =
            CountingQuerySession::new(json!({"records": [], "syncToken": "unfiled-token"}), 0);
        let passes = vec![
            AlbumPass {
                kind: PassKind::Album,
                album: changes_album_with_container(
                    "Vacation",
                    Some("container-vacation"),
                    album_session,
                ),
                exclude_ids: Arc::new(FxHashSet::default()),
            },
            AlbumPass {
                kind: PassKind::Unfiled,
                album: changes_album("", unfiled_session.clone()),
                exclude_ids: Arc::new(FxHashSet::default()),
            },
        ];

        let db = Arc::new(SqliteStateDb::open_in_memory().expect("state db"));
        let mut config = test_config();
        let dir = TempDir::new().expect("temp dir");
        config.directory = Arc::from(dir.path());
        config.state_db = Some(Arc::clone(&db) as Arc<dyn DownloadStore>);
        config.sync_mode = SyncMode::Incremental {
            zone_sync_token: "zone-token-prev".to_string(),
        };

        let result = download_photos_with_sync(
            &Client::new(),
            &passes,
            Arc::new(config),
            DownloadControls::download_hidden(),
            CancellationToken::new(),
        )
        .await
        .expect("missing snapshot should run targeted backfill");

        assert!(result.full_enumeration_ran);
        assert_eq!(
            result.stats.full_enumeration_reason,
            Some(FullEnumerationReason::AlbumRelationHydrationIncomplete)
        );
        assert_eq!(result.sync_token.as_deref(), Some("zone-token-next"));
        assert_eq!(
            changes_zone_calls.load(Ordering::SeqCst),
            1,
            "targeted backfill should still drain the zone delta once"
        );
        assert_eq!(
            unfiled_session.records_query_count(),
            0,
            "targeted backfill must not enumerate the library-wide unfiled pass"
        );
        assert!(
            db.selected_album_containers_have_complete_snapshots(
                "PrimarySync",
                &["container-vacation"],
            )
            .await
            .unwrap(),
            "targeted backfill should complete the missing album snapshot"
        );
    }

    #[tokio::test]
    async fn targeted_album_backfill_failure_blocks_incremental_token() {
        let album_session =
            CountingQuerySession::failing(json!({"records": [], "syncToken": "album-token"}), 1);
        let passes = vec![AlbumPass {
            kind: PassKind::Album,
            album: changes_album_with_container(
                "Vacation",
                Some("container-vacation"),
                album_session,
            ),
            exclude_ids: Arc::new(FxHashSet::default()),
        }];

        let db = Arc::new(SqliteStateDb::open_in_memory().expect("state db"));
        let mut config = test_config();
        let dir = TempDir::new().expect("temp dir");
        config.directory = Arc::from(dir.path());
        config.state_db = Some(db);
        config.sync_mode = SyncMode::Incremental {
            zone_sync_token: "zone-token-prev".to_string(),
        };

        let result = download_photos_with_sync(
            &Client::new(),
            &passes,
            Arc::new(config),
            DownloadControls::download_hidden(),
            CancellationToken::new(),
        )
        .await
        .expect("targeted backfill failure should return a token-blocked result");

        assert_eq!(result.sync_token, None);
        assert!(result.stats.sync_token_blocked);
    }

    #[tokio::test]
    async fn incremental_with_failed_rows_uses_targeted_retry_not_full_enumeration() {
        let db = Arc::new(crate::state::SqliteStateDb::open_in_memory().expect("state db"));
        let record = crate::test_helpers::TestAssetRecord::new("FAILED_BEFORE_SYNC")
            .filename("failed-before-sync.jpg")
            .checksum("ck_failed_before_sync")
            .size(1024)
            .build();
        db.upsert_seen(&record).await.expect("seed pending row");
        db.mark_failed(
            "PrimarySync",
            "FAILED_BEFORE_SYNC",
            "original",
            "prior download failure",
        )
        .await
        .expect("mark failed");
        db.upsert_asset_master_mapping(
            "PrimarySync",
            "asset-FAILED_BEFORE_SYNC",
            "FAILED_BEFORE_SYNC",
        )
        .await
        .expect("seed asset/master mapping");

        let session = PendingLookupSession {
            records: Arc::new(mock_photo_records_for_zone_with_filename(
                "FAILED_BEFORE_SYNC",
                "PrimarySync",
                "failed-before-sync.jpg",
            )),
        };
        let passes = vec![AlbumPass {
            kind: PassKind::Unfiled,
            album: album_with_session("PrimarySync", "", Box::new(session)),
            exclude_ids: Arc::new(FxHashSet::default()),
        }];

        let mut config = test_config();
        let dir = TempDir::new().expect("temp dir");
        config.directory = Arc::from(dir.path());
        config.state_db = Some(db.clone());
        config.sync_mode = SyncMode::Incremental {
            zone_sync_token: "zone-token-prev".to_string(),
        };

        let result = download_photos_with_sync(
            &Client::new(),
            &passes,
            Arc::new(config),
            DownloadControls::new(DownloadRunMode::PrintFilenames, DownloadReporting::hidden()),
            CancellationToken::new(),
        )
        .await
        .expect("failed rows should use targeted retry");

        assert!(
            !result.full_enumeration_ran,
            "normal sync with failed rows should not force full enumeration"
        );
        assert_eq!(result.stats.full_enumeration_reason, None);
        assert!(
            matches!(result.outcome, DownloadOutcome::Success),
            "result: {result:?}"
        );
        assert_eq!(
            result.sync_token, None,
            "print-only targeted retry must not advance the zone token"
        );
    }

    async fn seed_pending_retry_with_recorded_path(
        db: &crate::state::SqliteStateDb,
        asset: &PhotoAsset,
        checksum: &str,
        size: u64,
        recorded_path: &Path,
    ) {
        let filename = recorded_path
            .file_name()
            .and_then(|name| name.to_str())
            .expect("recorded filename");
        let record = TestAssetRecord::new(asset.state_id())
            .filename(filename)
            .checksum(checksum)
            .size(size)
            .build();
        db.upsert_seen(&record).await.expect("seed state row");
        db.mark_downloaded(
            "PrimarySync",
            asset.state_id(),
            VersionSizeKey::Original.as_str(),
            recorded_path,
            "previous-local-checksum",
            None,
        )
        .await
        .expect("seed recorded path");
        db.mark_failed(
            "PrimarySync",
            asset.state_id(),
            VersionSizeKey::Original.as_str(),
            "retry recorded path",
        )
        .await
        .expect("mark retry pending");
        db.prepare_for_retry(
            Some("PrimarySync"),
            crate::state::RetryErrorRetention::Clear,
        )
        .await
        .expect("prepare failed row for retry");
        db.upsert_asset_master_mapping("PrimarySync", asset.asset_record_name(), asset.state_id())
            .await
            .expect("seed asset/master mapping");
    }

    #[tokio::test]
    async fn pending_retry_reuses_missing_recorded_path_when_it_matches_planned_path() {
        let db = Arc::new(crate::state::SqliteStateDb::open_in_memory().expect("state db"));
        let dir = TempDir::new().expect("temp dir");
        let mut config = test_config();
        config.directory = Arc::from(dir.path());
        config.state_db = Some(db.clone());

        let mut records = incremental_photo_records_with_url(
            "MISSING_PATH",
            "missing.jpg",
            "https://p01.icloud-content.com/missing.jpg",
            8,
        );
        records[0]["fields"]["resOriginalRes"]["value"]["fileChecksum"] = json!("ck_missing");
        let asset = PhotoAsset::new(records[0].clone(), records[1].clone());
        let recorded_path = filter::expected_paths_for(&asset, &config)
            .into_iter()
            .next()
            .expect("asset should derive a path")
            .path;
        seed_pending_retry_with_recorded_path(db.as_ref(), &asset, "ck_missing", 8, &recorded_path)
            .await;

        let passes = vec![AlbumPass {
            kind: PassKind::Unfiled,
            album: album_with_session(
                "PrimarySync",
                "",
                Box::new(PendingLookupSession {
                    records: Arc::new(records),
                }),
            ),
            exclude_ids: Arc::new(FxHashSet::default()),
        }];
        let plan = build_pending_retry_download_tasks(&passes, &config, CancellationToken::new())
            .await
            .expect("plan targeted retry");

        assert_eq!(plan.tasks.len(), 1);
        assert_eq!(plan.tasks[0].download_path, recorded_path);
    }

    #[tokio::test]
    async fn name_id7_targeted_retry_adopts_intact_recorded_path_without_planned_task() {
        let db = Arc::new(crate::state::SqliteStateDb::open_in_memory().expect("state db"));
        let dir = TempDir::new().expect("temp dir");
        let mut config = test_config();
        config.directory = Arc::from(dir.path());
        config.state_db = Some(db.clone());
        config.file_match_policy = FileMatchPolicy::NameId7;

        let mut records = incremental_photo_records_with_url(
            "ID7_ADOPT",
            "id7.jpg",
            "https://p01.icloud-content.com/id7.jpg",
            8,
        );
        records[0]["fields"]["resOriginalRes"]["value"]["fileChecksum"] = json!("ck_id7");
        let asset = PhotoAsset::new(records[0].clone(), records[1].clone());
        let recorded_path = filter::expected_paths_for(&asset, &config)
            .into_iter()
            .next()
            .expect("asset should derive a path")
            .path;
        tokio::fs::create_dir_all(recorded_path.parent().expect("recorded path parent"))
            .await
            .expect("create recorded path parent");
        tokio::fs::write(&recorded_path, b"12345678")
            .await
            .expect("seed intact recorded file");
        seed_pending_retry_with_recorded_path(db.as_ref(), &asset, "ck_id7", 8, &recorded_path)
            .await;
        let passes = vec![AlbumPass {
            kind: PassKind::Unfiled,
            album: album_with_session(
                "PrimarySync",
                "",
                Box::new(PendingLookupSession {
                    records: Arc::new(records),
                }),
            ),
            exclude_ids: Arc::new(FxHashSet::default()),
        }];

        let plan = build_pending_retry_download_tasks(&passes, &config, CancellationToken::new())
            .await
            .expect("adopt targeted retry");

        assert!(plan.tasks.is_empty());
        let summary = db.get_summary().await.expect("state summary");
        assert_eq!(summary.downloaded, 1);
        assert_eq!(summary.policy_excluded, 0);
        assert_eq!(summary.pending, 0);
        assert_eq!(summary.failed, 0);
        let downloaded = db.get_downloaded_page(0, 10).await.expect("downloaded row");
        assert_eq!(
            downloaded[0].local_path.as_deref(),
            Some(recorded_path.as_path())
        );
    }

    #[tokio::test]
    async fn targeted_retry_adopts_smaller_metadata_rewritten_recorded_file() {
        let db = Arc::new(crate::state::SqliteStateDb::open_in_memory().expect("state db"));
        let dir = TempDir::new().expect("temp dir");
        let mut config = test_config();
        config.directory = Arc::from(dir.path());
        config.state_db = Some(db.clone());

        let mut records = incremental_photo_records_with_url(
            "METADATA_REWRITTEN_RETRY",
            "rewritten.jpg",
            "https://p01.icloud-content.com/rewritten.jpg",
            8,
        );
        records[0]["fields"]["resOriginalRes"]["value"]["fileChecksum"] =
            json!("ck_metadata_rewritten_retry");
        let asset = PhotoAsset::new(records[0].clone(), records[1].clone());
        let recorded_path = filter::expected_paths_for(&asset, &config)
            .into_iter()
            .next()
            .expect("asset should derive a path")
            .path;
        tokio::fs::create_dir_all(recorded_path.parent().expect("recorded path parent"))
            .await
            .expect("create recorded path parent");
        tokio::fs::write(&recorded_path, b"shorter")
            .await
            .expect("seed metadata-rewritten file");
        let local_checksum = file::compute_sha256(&recorded_path)
            .await
            .expect("hash metadata-rewritten file");

        let record = TestAssetRecord::new(asset.state_id())
            .filename("rewritten.jpg")
            .checksum("ck_metadata_rewritten_retry")
            .size(8)
            .build();
        db.upsert_seen(&record).await.expect("seed state row");
        db.mark_downloaded(
            "PrimarySync",
            asset.state_id(),
            VersionSizeKey::Original.as_str(),
            &recorded_path,
            &local_checksum,
            Some("download-checksum-before-metadata"),
        )
        .await
        .expect("seed metadata-rewritten state");
        db.mark_failed(
            "PrimarySync",
            asset.state_id(),
            VersionSizeKey::Original.as_str(),
            "prior false truncation",
        )
        .await
        .expect("mark row failed");
        db.prepare_for_retry(
            Some("PrimarySync"),
            crate::state::RetryErrorRetention::Clear,
        )
        .await
        .expect("prepare row for retry");
        db.upsert_asset_master_mapping("PrimarySync", asset.asset_record_name(), asset.state_id())
            .await
            .expect("seed asset/master mapping");

        let passes = vec![AlbumPass {
            kind: PassKind::Unfiled,
            album: album_with_session(
                "PrimarySync",
                "",
                Box::new(PendingLookupSession {
                    records: Arc::new(records),
                }),
            ),
            exclude_ids: Arc::new(FxHashSet::default()),
        }];
        let plan = build_pending_retry_download_tasks(&passes, &config, CancellationToken::new())
            .await
            .expect("adopt metadata-rewritten retry");

        assert!(plan.tasks.is_empty());
        assert_eq!(tokio::fs::read(&recorded_path).await.unwrap(), b"shorter");
        let downloaded = db.get_downloaded_page(0, 10).await.expect("downloaded row");
        assert_eq!(downloaded.len(), 1);
        assert_eq!(
            downloaded[0].local_path.as_deref(),
            Some(recorded_path.as_path())
        );
        assert_eq!(
            downloaded[0].local_checksum.as_deref(),
            Some(local_checksum.as_str())
        );
        assert_eq!(
            downloaded[0].download_checksum.as_deref(),
            Some("download-checksum-before-metadata")
        );
    }

    #[tokio::test]
    async fn approved_truncated_reconcile_retry_replaces_the_recorded_path() {
        use base64::Engine as _;
        use sha2::{Digest, Sha256};
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};

        let server = crate::start_wiremock_or_skip!();
        let body = vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0x4A, 0x46];
        let checksum = base64::engine::general_purpose::STANDARD.encode(Sha256::digest(&body));
        Mock::given(method("GET"))
            .and(path("/photo.jpg"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_bytes(body.clone())
                    .insert_header("content-type", "image/jpeg"),
            )
            .expect(1)
            .mount(&server)
            .await;

        let db = Arc::new(crate::state::SqliteStateDb::open_in_memory().expect("state db"));
        let dir = TempDir::new().expect("temp dir");
        let recorded_path = dir.path().join("2019/11/2019-11-28/photo.jpg");
        tokio::fs::create_dir_all(recorded_path.parent().expect("recorded parent"))
            .await
            .expect("create recorded directory");
        tokio::fs::write(&recorded_path, &body)
            .await
            .expect("seed intact download");
        let original_local_checksum = file::compute_sha256(&recorded_path)
            .await
            .expect("hash intact download");
        let record = crate::test_helpers::TestAssetRecord::new("FAILED_WITH_PATH")
            .filename("photo.jpg")
            .checksum(&checksum)
            .size(body.len() as u64)
            .build();
        db.upsert_seen(&record).await.expect("seed pending row");
        db.mark_downloaded(
            "PrimarySync",
            "FAILED_WITH_PATH",
            "original",
            &recorded_path,
            &original_local_checksum,
            Some(&original_local_checksum),
        )
        .await
        .expect("seed recorded local path");
        tokio::fs::write(&recorded_path, b"bad")
            .await
            .expect("truncate recorded download");

        let (counts, drift) = crate::commands::reconcile::scan_local_drift(
            db.as_ref(),
            |_: &crate::commands::reconcile::LocalDriftAsset| {},
            |_: &str| {},
        )
        .await
        .expect("scan truncated download");
        assert_eq!(counts.damaged, 1);
        assert_eq!(drift.len(), 1);
        for update in &drift {
            db.mark_failed(
                &update.library,
                &update.id,
                update.version_size.as_str(),
                update.kind.reason(),
            )
            .await
            .expect("mark truncated download failed");
        }
        db.upsert_asset_master_mapping("PrimarySync", "asset-FAILED_WITH_PATH", "FAILED_WITH_PATH")
            .await
            .expect("seed asset/master mapping");

        let download_url = format!("{}/photo.jpg", server.uri());
        let mut records = incremental_photo_records_with_url(
            "FAILED_WITH_PATH",
            "photo.jpg",
            &download_url,
            body.len() as u64,
        );
        records[0]["fields"]["resOriginalRes"]["value"]["fileChecksum"] = json!(checksum);
        let session = PendingLookupSession {
            records: Arc::new(records),
        };
        let passes = vec![AlbumPass {
            kind: PassKind::Album,
            album: album_with_session("PrimarySync", "AlbumOne", Box::new(session)),
            exclude_ids: Arc::new(FxHashSet::default()),
        }];
        let mut config = test_config();
        config.directory = Arc::from(dir.path());
        config.state_db = Some(db.clone());
        config.repair_truncated = true;
        config.sync_mode = SyncMode::Incremental {
            zone_sync_token: "zone-token-prev".to_string(),
        };

        let result = download_photos_with_sync(
            &Client::new(),
            &passes,
            Arc::new(config),
            DownloadControls::download_hidden(),
            CancellationToken::new(),
        )
        .await
        .expect("retry truncated download through the production pipeline");

        assert!(matches!(result.outcome, DownloadOutcome::Success));
        assert_eq!(result.stats.downloaded, 1);
        assert_eq!(tokio::fs::read(&recorded_path).await.unwrap(), body);
        let downloaded = db.get_downloaded_page(0, 10).await.expect("downloaded row");
        assert_eq!(downloaded.len(), 1);
        let repaired_path = downloaded[0]
            .local_path
            .as_ref()
            .expect("repair records its final path");
        assert_eq!(repaired_path, &recorded_path);
        let mut directory = tokio::fs::read_dir(recorded_path.parent().unwrap())
            .await
            .expect("read repair directory");
        let mut names = Vec::new();
        while let Some(entry) = directory.next_entry().await.expect("read directory entry") {
            names.push(entry.file_name());
        }
        assert_eq!(names, vec![recorded_path.file_name().unwrap()]);
        let summary = db.get_summary().await.expect("summary");
        assert_eq!(summary.downloaded, 1);
        assert_eq!(summary.pending, 0);
        assert_eq!(summary.failed, 0);
    }

    #[tokio::test]
    async fn truncated_repair_provider_version_change_uses_sibling() {
        use base64::Engine as _;
        use sha2::{Digest, Sha256};
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};

        let server = crate::start_wiremock_or_skip!();
        let current_body = vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0x4A, 0x46];
        let current_checksum =
            base64::engine::general_purpose::STANDARD.encode(Sha256::digest(&current_body));
        Mock::given(method("GET"))
            .and(path("/photo.jpg"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_bytes(current_body.clone())
                    .insert_header("content-type", "image/jpeg"),
            )
            .expect(1)
            .mount(&server)
            .await;

        let db = Arc::new(crate::state::SqliteStateDb::open_in_memory().expect("state db"));
        let dir = TempDir::new().expect("temp dir");
        let recorded_path = dir.path().join("2019/11/2019-11-28/photo.jpg");
        tokio::fs::create_dir_all(recorded_path.parent().expect("recorded parent"))
            .await
            .expect("create recorded directory");
        let prior_body = &current_body[..current_body.len() - 1];
        tokio::fs::write(&recorded_path, prior_body)
            .await
            .expect("seed prior provider version");
        let prior_local_checksum = file::compute_sha256(&recorded_path)
            .await
            .expect("hash prior provider version");
        let record = crate::test_helpers::TestAssetRecord::new("CHANGED_TRUNCATED_PATH")
            .filename("photo.jpg")
            .checksum("old-provider-checksum")
            .size(prior_body.len() as u64)
            .build();
        db.upsert_seen(&record).await.expect("seed state row");
        db.mark_downloaded(
            "PrimarySync",
            "CHANGED_TRUNCATED_PATH",
            "original",
            &recorded_path,
            &prior_local_checksum,
            Some(&prior_local_checksum),
        )
        .await
        .expect("seed recorded local path");
        tokio::fs::write(&recorded_path, b"bad")
            .await
            .expect("truncate recorded download");

        let (counts, drift) = crate::commands::reconcile::scan_local_drift(
            db.as_ref(),
            |_: &crate::commands::reconcile::LocalDriftAsset| {},
            |_: &str| {},
        )
        .await
        .expect("scan truncated download");
        assert_eq!(counts.damaged, 1);
        assert_eq!(drift.len(), 1);
        for update in &drift {
            db.mark_failed(
                &update.library,
                &update.id,
                update.version_size.as_str(),
                update.kind.reason(),
            )
            .await
            .expect("mark truncated download failed");
        }
        db.upsert_asset_master_mapping(
            "PrimarySync",
            "asset-CHANGED_TRUNCATED_PATH",
            "CHANGED_TRUNCATED_PATH",
        )
        .await
        .expect("seed asset/master mapping");

        let download_url = format!("{}/photo.jpg", server.uri());
        let mut records = incremental_photo_records_with_url(
            "CHANGED_TRUNCATED_PATH",
            "photo.jpg",
            &download_url,
            current_body.len() as u64,
        );
        records[0]["fields"]["resOriginalRes"]["value"]["fileChecksum"] = json!(current_checksum);
        let passes = vec![AlbumPass {
            kind: PassKind::Album,
            album: album_with_session(
                "PrimarySync",
                "AlbumOne",
                Box::new(PendingLookupSession {
                    records: Arc::new(records),
                }),
            ),
            exclude_ids: Arc::new(FxHashSet::default()),
        }];
        let mut config = test_config();
        config.directory = Arc::from(dir.path());
        config.state_db = Some(db.clone());
        config.repair_truncated = true;
        config.sync_mode = SyncMode::Incremental {
            zone_sync_token: "zone-token-prev".to_string(),
        };

        let result = download_photos_with_sync(
            &Client::new(),
            &passes,
            Arc::new(config),
            DownloadControls::download_hidden(),
            CancellationToken::new(),
        )
        .await
        .expect("retry changed provider version through production pipeline");

        assert!(matches!(result.outcome, DownloadOutcome::Success));
        assert_eq!(result.stats.downloaded, 1);
        assert_eq!(tokio::fs::read(&recorded_path).await.unwrap(), b"bad");
        let downloaded = db.get_downloaded_page(0, 10).await.expect("downloaded row");
        assert_eq!(downloaded.len(), 1);
        let current_path = downloaded[0]
            .local_path
            .as_ref()
            .expect("current provider version has a local path");
        assert_ne!(current_path, &recorded_path);
        assert_eq!(tokio::fs::read(current_path).await.unwrap(), current_body);
        assert_eq!(current_path.parent(), recorded_path.parent());
        let summary = db.get_summary().await.expect("summary");
        assert_eq!(summary.downloaded, 1);
        assert_eq!(summary.pending, 0);
        assert_eq!(summary.failed, 0);
    }

    #[tokio::test]
    async fn same_size_provider_change_retry_does_not_adopt_stale_recorded_path() {
        use base64::Engine as _;
        use sha2::{Digest, Sha256};
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};

        let failed_server = crate::start_wiremock_or_skip!();
        Mock::given(method("GET"))
            .and(path("/changed.jpg"))
            .respond_with(ResponseTemplate::new(500))
            .expect(1)
            .mount(&failed_server)
            .await;

        let new_body = vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0x4A, 0x46];
        let new_checksum =
            base64::engine::general_purpose::STANDARD.encode(Sha256::digest(&new_body));
        let failed_url = format!("{}/changed.jpg", failed_server.uri());
        let mut changed_records = incremental_photo_records_with_url(
            "SAME_SIZE_CHANGED",
            "changed.jpg",
            &failed_url,
            new_body.len() as u64,
        );
        changed_records[0]["fields"]["resOriginalRes"]["value"]["fileChecksum"] =
            json!(new_checksum);
        let changed_asset = PhotoAsset::new(changed_records[0].clone(), changed_records[1].clone());

        let db = Arc::new(crate::state::SqliteStateDb::open_in_memory().expect("state db"));
        let dir = TempDir::new().expect("temp dir");
        let mut config = test_config();
        config.directory = Arc::from(dir.path());
        config.state_db = Some(db.clone());
        config.sync_mode = SyncMode::Incremental {
            zone_sync_token: "zone-token-prev".to_string(),
        };
        config.retry = RetryConfig {
            max_retries: 0,
            base_delay_secs: 0,
            max_delay_secs: 0,
        };
        let config = Arc::new(config);

        let recorded_path = filter::expected_paths_for(&changed_asset, config.as_ref())
            .into_iter()
            .next()
            .expect("asset should derive a path")
            .path;
        tokio::fs::create_dir_all(recorded_path.parent().expect("recorded parent"))
            .await
            .expect("create recorded directory");
        let old_body = vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0x4A, 0x45];
        tokio::fs::write(&recorded_path, &old_body)
            .await
            .expect("seed old provider version");
        let old_local_checksum = file::compute_sha256(&recorded_path)
            .await
            .expect("hash old provider version");
        let old_record = TestAssetRecord::new("SAME_SIZE_CHANGED")
            .filename("changed.jpg")
            .checksum("old-provider-checksum")
            .size(old_body.len() as u64)
            .build();
        db.upsert_seen(&old_record)
            .await
            .expect("seed old state row");
        db.mark_downloaded(
            "PrimarySync",
            "SAME_SIZE_CHANGED",
            VersionSizeKey::Original.as_str(),
            &recorded_path,
            &old_local_checksum,
            None,
        )
        .await
        .expect("record old provider version");

        let first = stream_and_download_from_stream(
            &Client::new(),
            stream::iter(vec![Ok::<PhotoAsset, anyhow::Error>(changed_asset)]),
            &config,
            DownloadControls::download_hidden(),
            1,
            CancellationToken::new(),
            StreamRuntime::new(None, None),
        )
        .await
        .expect("changed provider version should reach the download pipeline");
        assert_eq!(first.downloaded, 0);
        assert_eq!(first.failed.len(), 1);
        assert_eq!(tokio::fs::read(&recorded_path).await.unwrap(), old_body);
        let failed = db.get_failed().await.expect("failed row");
        assert!(failed[0].downloaded_at.is_none());

        let success_server = crate::start_wiremock_or_skip!();
        Mock::given(method("GET"))
            .and(path("/changed.jpg"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_bytes(new_body.clone())
                    .insert_header("content-type", "image/jpeg"),
            )
            .expect(1)
            .mount(&success_server)
            .await;
        changed_records[0]["fields"]["resOriginalRes"]["value"]["downloadURL"] =
            json!(format!("{}/changed.jpg", success_server.uri()));
        let retry_passes = vec![AlbumPass {
            kind: PassKind::Unfiled,
            album: album_with_session(
                "PrimarySync",
                "",
                Box::new(PendingLookupSession {
                    records: Arc::new(changed_records),
                }),
            ),
            exclude_ids: Arc::new(FxHashSet::default()),
        }];

        let retry = download_photos_with_sync(
            &Client::new(),
            &retry_passes,
            config,
            DownloadControls::download_hidden(),
            CancellationToken::new(),
        )
        .await
        .expect("targeted retry should download the changed provider version");

        assert!(matches!(retry.outcome, DownloadOutcome::Success));
        assert_eq!(retry.stats.downloaded, 1);
        assert_eq!(tokio::fs::read(&recorded_path).await.unwrap(), old_body);
        let downloaded = db.get_downloaded_page(0, 10).await.expect("downloaded row");
        assert_eq!(downloaded.len(), 2);
        let changed_path = downloaded
            .iter()
            .find(|record| record.id.as_ref() == "asset-SAME_SIZE_CHANGED")
            .expect("changed child has a downloaded row")
            .local_path
            .as_ref()
            .expect("changed provider version has a local path");
        assert_ne!(changed_path, &recorded_path);
        assert_eq!(tokio::fs::read(changed_path).await.unwrap(), new_body);
        let summary = db.get_summary().await.expect("summary");
        assert_eq!(summary.downloaded, 2);
        assert_eq!(summary.pending, 0);
        assert_eq!(summary.failed, 0);
    }

    #[tokio::test]
    async fn path_reconciliation_copies_catalog_file_without_provider_inventory() {
        #[derive(Clone, Debug)]
        struct LookupOnlySession {
            records: Arc<Vec<Value>>,
        }

        #[async_trait::async_trait]
        impl PhotosSession for LookupOnlySession {
            async fn post(
                &self,
                url: &str,
                _body: String,
                _headers: &[(&str, &str)],
            ) -> anyhow::Result<Value> {
                if url.contains("/records/lookup?") {
                    return Ok(json!({"records": self.records.as_ref().clone()}));
                }
                anyhow::bail!("path reconciliation made an unexpected provider request: {url}")
            }

            fn clone_box(&self) -> Box<dyn PhotosSession> {
                Box::new(self.clone())
            }
        }

        let db = Arc::new(crate::state::SqliteStateDb::open_in_memory().expect("state db"));
        let old_dir = TempDir::new().expect("old dir");
        let new_dir = TempDir::new().expect("new dir");
        let old_path = old_dir.path().join("reconcile.jpg");
        tokio::fs::write(&old_path, b"catalog bytes").await.unwrap();
        let local_checksum = file::compute_sha256(&old_path).await.unwrap();
        let record = crate::test_helpers::TestAssetRecord::new("RECONCILE")
            .filename("reconcile.jpg")
            .checksum("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=")
            .size(1024)
            .build();
        db.upsert_seen(&record).await.unwrap();
        db.mark_downloaded(
            "PrimarySync",
            "RECONCILE",
            "original",
            &old_path,
            &local_checksum,
            Some("provider-checksum"),
        )
        .await
        .unwrap();
        db.upsert_asset_master_mapping("PrimarySync", "asset-RECONCILE", "RECONCILE")
            .await
            .unwrap();

        let mut records =
            mock_photo_records_for_zone_with_filename("RECONCILE", "PrimarySync", "reconcile.jpg");
        records[1]["fields"]["resJPEGFullRes"] = json!({"value": {
            "downloadURL": "https://p01.icloud-content.com/reconcile-adjusted.jpg",
            "size": 512,
            "fileChecksum": "BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB="
        }});
        records[1]["fields"]["resJPEGFullFileType"] = json!({"value": "public.jpeg"});
        let asset = PhotoAsset::new(records[0].clone(), records[1].clone());
        let passes = vec![AlbumPass {
            kind: PassKind::Unfiled,
            album: album_with_session(
                "PrimarySync",
                "",
                Box::new(LookupOnlySession {
                    records: Arc::new(records),
                }),
            ),
            exclude_ids: Arc::new(FxHashSet::default()),
        }];
        let mut config = test_config();
        config.directory = Arc::from(new_dir.path());
        config.state_db = Some(db.clone());
        config.edited = true;
        let expected_paths = filter::expected_paths_for(&asset, &config);
        let expected_path = expected_paths
            .into_iter()
            .find(|path| path.version_size == VersionSizeKey::Original)
            .unwrap()
            .path;
        let adjusted_path = filter::expected_paths_for(&asset, &config)
            .into_iter()
            .find(|path| path.version_size == VersionSizeKey::Adjusted)
            .unwrap()
            .path;

        let result = reconcile_catalog_paths(&passes, Arc::new(config), CancellationToken::new())
            .await
            .expect("path reconciliation");

        assert!(result.complete);
        assert_eq!(result.stats.downloaded, 1);
        assert_eq!(result.stats.failed, 0);
        assert_eq!(tokio::fs::read(&old_path).await.unwrap(), b"catalog bytes");
        assert_eq!(
            tokio::fs::read(&expected_path).await.unwrap(),
            b"catalog bytes"
        );
        assert!(!adjusted_path.exists());
        let downloaded = db.get_downloaded_page(0, 10).await.unwrap();
        assert_eq!(downloaded.len(), 1);
        assert_eq!(
            downloaded[0].local_path.as_deref(),
            Some(expected_path.as_path())
        );

        tokio::fs::remove_file(&expected_path).await.unwrap();
        let deferred = reconcile_catalog_paths(
            &passes,
            Arc::new({
                let mut config = test_config();
                config.directory = Arc::from(new_dir.path());
                config.state_db = Some(db.clone());
                config.edited = true;
                config
            }),
            CancellationToken::new(),
        )
        .await
        .expect("missing local file should defer to targeted retry");
        assert!(!deferred.complete);
        assert_eq!(deferred.stats.failed, 0);
        let summary = db.get_summary().await.unwrap();
        assert_eq!(summary.pending, 0);
        assert_eq!(summary.failed, 1);
    }

    #[tokio::test]
    async fn incremental_pending_retry_dry_run_counts_planned_retry_without_token() {
        let db = Arc::new(crate::state::SqliteStateDb::open_in_memory().expect("state db"));
        let record = crate::test_helpers::TestAssetRecord::new("DRY_RUN_PENDING")
            .filename("dry-run-pending.jpg")
            .checksum("ck_dry_run_pending")
            .size(1024)
            .build();
        db.upsert_seen(&record).await.expect("seed pending row");
        db.upsert_asset_master_mapping("PrimarySync", "asset-DRY_RUN_PENDING", "DRY_RUN_PENDING")
            .await
            .expect("seed asset/master mapping");

        let session = PendingLookupSession {
            records: Arc::new(mock_photo_records_for_zone_with_filename(
                "DRY_RUN_PENDING",
                "PrimarySync",
                "dry-run-pending.jpg",
            )),
        };
        let passes = vec![AlbumPass {
            kind: PassKind::Unfiled,
            album: album_with_session("PrimarySync", "", Box::new(session)),
            exclude_ids: Arc::new(FxHashSet::default()),
        }];

        let mut config = test_config();
        let dir = TempDir::new().expect("temp dir");
        config.directory = Arc::from(dir.path());
        config.state_db = Some(db);
        config.sync_mode = SyncMode::Incremental {
            zone_sync_token: "zone-token-prev".to_string(),
        };

        let result = download_photos_with_sync(
            &Client::new(),
            &passes,
            Arc::new(config),
            DownloadControls::new(DownloadRunMode::DryRun, DownloadReporting::hidden()),
            CancellationToken::new(),
        )
        .await
        .expect("dry-run pending retry should report planned work");

        assert!(
            !result.full_enumeration_ran,
            "dry-run pending retry should not force full enumeration"
        );
        assert!(matches!(result.outcome, DownloadOutcome::Success));
        assert_eq!(result.stats.downloaded, 1);
        assert_eq!(result.sync_token, None);
        assert!(!result.stats.sync_token_blocked);
    }

    #[tokio::test]
    async fn incremental_with_failed_rows_retries_real_download_after_zone_delta() {
        use base64::Engine as _;
        use sha2::{Digest, Sha256};
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};

        let server = crate::start_wiremock_or_skip!();
        let body = vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0x4A, 0x46];
        let checksum = base64::engine::general_purpose::STANDARD.encode(Sha256::digest(&body));
        Mock::given(method("GET"))
            .and(path("/failed-before-sync.jpg"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_bytes(body)
                    .insert_header("content-type", "image/jpeg"),
            )
            .mount(&server)
            .await;

        let db = Arc::new(crate::state::SqliteStateDb::open_in_memory().expect("state db"));
        let record = crate::test_helpers::TestAssetRecord::new("FAILED_BEFORE_SYNC")
            .filename("failed-before-sync.jpg")
            .checksum("ck_failed_before_sync")
            .size(1024)
            .build();
        db.upsert_seen(&record).await.expect("seed pending row");
        db.mark_failed(
            "PrimarySync",
            "FAILED_BEFORE_SYNC",
            "original",
            "prior download failure",
        )
        .await
        .expect("mark failed");
        db.upsert_asset_master_mapping(
            "PrimarySync",
            "asset-FAILED_BEFORE_SYNC",
            "FAILED_BEFORE_SYNC",
        )
        .await
        .expect("seed asset/master mapping");

        let download_url = format!("{}/failed-before-sync.jpg", server.uri());
        let mut records = incremental_photo_records_with_url(
            "FAILED_BEFORE_SYNC",
            "failed-before-sync.jpg",
            &download_url,
            8,
        );
        records[0]["fields"]["resOriginalRes"]["value"]["fileChecksum"] = json!(checksum);
        let session = PendingLookupSession {
            records: Arc::new(records),
        };
        let passes = vec![AlbumPass {
            kind: PassKind::Unfiled,
            album: album_with_session("PrimarySync", "", Box::new(session)),
            exclude_ids: Arc::new(FxHashSet::default()),
        }];

        let mut config = test_config();
        let dir = TempDir::new().expect("temp dir");
        config.directory = Arc::from(dir.path());
        config.state_db = Some(db.clone());
        config.sync_mode = SyncMode::Incremental {
            zone_sync_token: "zone-token-prev".to_string(),
        };

        let result = download_photos_with_sync(
            &Client::new(),
            &passes,
            Arc::new(config),
            DownloadControls::download_hidden(),
            CancellationToken::new(),
        )
        .await
        .expect("failed rows should retry through the real download path");

        assert!(
            !result.full_enumeration_ran,
            "normal sync with failed rows should not force full enumeration"
        );
        assert_eq!(result.stats.full_enumeration_reason, None);
        assert!(matches!(result.outcome, DownloadOutcome::Success));
        assert_eq!(result.sync_token.as_deref(), Some("zone-token-next"));
        let summary = db.get_summary().await.expect("summary");
        assert_eq!(summary.downloaded, 1);
        assert_eq!(summary.pending, 0);
        assert_eq!(summary.failed, 0);
        let downloaded = db
            .get_downloaded_page(0, 10)
            .await
            .expect("downloaded page");
        let local_path = downloaded[0]
            .local_path
            .as_ref()
            .expect("downloaded row has a local path");
        assert!(
            tokio::fs::metadata(local_path).await.is_ok(),
            "targeted retry should finalize the downloaded file"
        );
    }

    #[tokio::test]
    async fn pending_retry_expired_url_hydrates_current_records() {
        use base64::Engine as _;
        use sha2::{Digest, Sha256};
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};

        let server = crate::start_wiremock_or_skip!();
        Mock::given(method("GET"))
            .and(path("/expired-pending.jpg"))
            .respond_with(ResponseTemplate::new(410))
            .mount(&server)
            .await;

        let body = vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0x4A, 0x46];
        let checksum = base64::engine::general_purpose::STANDARD.encode(Sha256::digest(&body));
        Mock::given(method("GET"))
            .and(path("/fresh-pending.jpg"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_bytes(body)
                    .insert_header("content-type", "image/jpeg"),
            )
            .mount(&server)
            .await;

        let db = Arc::new(crate::state::SqliteStateDb::open_in_memory().expect("state db"));
        let record = crate::test_helpers::TestAssetRecord::new("PENDING_EXPIRED_URL")
            .filename("pending-expired-url.jpg")
            .checksum("ck_pending_expired_url")
            .size(1024)
            .build();
        db.upsert_seen(&record).await.expect("seed pending row");
        db.upsert_asset_master_mapping(
            "PrimarySync",
            "asset-PENDING_EXPIRED_URL",
            "PENDING_EXPIRED_URL",
        )
        .await
        .expect("seed asset/master mapping");

        let expired_url = format!("{}/expired-pending.jpg", server.uri());
        let fresh_url = format!("{}/fresh-pending.jpg", server.uri());
        let mut expired_records = incremental_photo_records_with_url(
            "PENDING_EXPIRED_URL",
            "pending-expired-url.jpg",
            &expired_url,
            8,
        );
        expired_records[0]["fields"]["resOriginalRes"]["value"]["fileChecksum"] =
            json!(checksum.clone());
        let mut fresh_records = incremental_photo_records_with_url(
            "PENDING_EXPIRED_URL",
            "pending-expired-url.jpg",
            &fresh_url,
            8,
        );
        fresh_records[0]["fields"]["resOriginalRes"]["value"]["fileChecksum"] = json!(checksum);
        let session = PendingRetryExpiredUrlSession::new(expired_records, fresh_records);
        let passes = vec![AlbumPass {
            kind: PassKind::Unfiled,
            album: album_with_session("PrimarySync", "", Box::new(session)),
            exclude_ids: Arc::new(FxHashSet::default()),
        }];

        let mut config = test_config();
        let dir = TempDir::new().expect("temp dir");
        config.directory = Arc::from(dir.path());
        config.state_db = Some(db.clone());
        config.sync_mode = SyncMode::Incremental {
            zone_sync_token: "zone-token-prev".to_string(),
        };

        let result = download_photos_with_sync(
            &Client::new(),
            &passes,
            Arc::new(config),
            DownloadControls::download_hidden(),
            CancellationToken::new(),
        )
        .await
        .expect("expired pending retry URL should hydrate and retry");

        assert!(
            matches!(result.outcome, DownloadOutcome::Success),
            "result: {result:?}"
        );
        assert!(!result.stats.interrupted);
        assert_eq!(result.sync_token.as_deref(), Some("zone-token-next"));
        let summary = db.get_summary().await.expect("summary");
        assert_eq!(summary.downloaded, 1);
        assert_eq!(summary.pending, 0);
        assert_eq!(summary.failed, 0);
    }

    #[tokio::test]
    async fn incremental_with_unmatched_pending_rows_does_not_block_source_checkpoint() {
        let db = Arc::new(crate::state::SqliteStateDb::open_in_memory().expect("state db"));
        let record = crate::test_helpers::TestAssetRecord::new("PENDING_BEFORE_SYNC")
            .filename("pending-before-sync.jpg")
            .checksum("ck_pending_before_sync")
            .size(1024)
            .build();
        db.upsert_seen(&record).await.expect("seed pending row");

        let session = MockPhotosFlow::new()
            .changes_zone_page(Vec::new(), "zone-token-next", false)
            .empty_query_page(Some("ignored-query-token"))
            .build();
        let passes = vec![AlbumPass {
            kind: PassKind::Unfiled,
            album: mock_album("", session),
            exclude_ids: Arc::new(FxHashSet::default()),
        }];

        let mut config = test_config();
        let dir = TempDir::new().expect("temp dir");
        config.directory = Arc::from(dir.path());
        config.state_db = Some(db);
        config.sync_mode = SyncMode::Incremental {
            zone_sync_token: "zone-token-prev".to_string(),
        };

        let result = download_photos_with_sync(
            &Client::new(),
            &passes,
            Arc::new(config),
            DownloadControls::new(DownloadRunMode::PrintFilenames, DownloadReporting::hidden()),
            CancellationToken::new(),
        )
        .await
        .expect("pending rows should remain durable without forcing provider recovery");

        assert!(
            !result.full_enumeration_ran,
            "unmatched pending rows should not force full enumeration"
        );
        assert!(matches!(
            result.outcome,
            DownloadOutcome::PartialFailure { failed_count: 1 }
        ));
        assert_eq!(result.sync_token, None);
        assert_eq!(result.stats.downloaded, 0);
        assert!(!result.stats.sync_token_blocked);
        assert_eq!(result.stats.sync_token_blocked_reason, None);
    }

    #[tokio::test]
    async fn incremental_with_unmatched_pending_rows_retains_state_without_deletion_evidence() {
        let db = Arc::new(crate::state::SqliteStateDb::open_in_memory().expect("state db"));
        let record = TestAssetRecord::new("PENDING_DELETED_UPSTREAM")
            .filename("pending-deleted-upstream.jpg")
            .checksum("ck_pending_deleted_upstream")
            .size(1024)
            .build();
        db.upsert_seen(&record).await.expect("seed pending row");

        let session = MockPhotosFlow::new()
            .changes_zone_page(Vec::new(), "zone-token-next", false)
            .empty_query_page(Some("ignored-query-token"))
            .build();
        let passes = vec![AlbumPass {
            kind: PassKind::Unfiled,
            album: mock_album("", session),
            exclude_ids: Arc::new(FxHashSet::default()),
        }];

        let mut config = test_config();
        let dir = TempDir::new().expect("temp dir");
        config.directory = Arc::from(dir.path());
        config.state_db = Some(db.clone());
        config.sync_mode = SyncMode::Incremental {
            zone_sync_token: "zone-token-prev".to_string(),
        };

        let result = download_photos_with_sync(
            &Client::new(),
            &passes,
            Arc::new(config),
            DownloadControls::download_hidden(),
            CancellationToken::new(),
        )
        .await
        .expect("unmatched pending rows should remain represented after targeted retry");

        assert!(
            !result.full_enumeration_ran,
            "retaining unresolved pending state should not force full enumeration"
        );
        assert!(matches!(
            result.outcome,
            DownloadOutcome::PartialFailure { failed_count: 1 }
        ));
        assert_eq!(result.sync_token.as_deref(), Some("zone-token-next"));
        assert!(!result.stats.sync_token_blocked);
        assert_eq!(result.stats.sync_token_blocked_reason, None);
        assert_eq!(result.stats.stale_pending_pruned, 0);
        let summary = db.get_summary().await.expect("summary");
        assert_eq!(summary.pending, 1);
        assert_eq!(summary.awaiting_provider_verification, 1);
        assert_eq!(db.get_pending().await.expect("pending page").len(), 1);
    }

    #[tokio::test]
    async fn full_enumeration_query_absence_never_deletes_pending_state() {
        let db = Arc::new(crate::state::SqliteStateDb::open_in_memory().expect("state db"));
        let record = TestAssetRecord::new("ABSENT_FROM_FULL_QUERY")
            .filename("absent-from-full-query.jpg")
            .checksum("ck_absent_from_full_query")
            .size(1024)
            .build();
        db.upsert_seen(&record).await.expect("seed pending row");

        let session = MockPhotosFlow::new()
            .empty_query_page(Some("zone-token-next"))
            .build();
        let passes = vec![AlbumPass {
            kind: PassKind::Unfiled,
            album: mock_album("", session),
            exclude_ids: Arc::new(FxHashSet::default()),
        }];
        let mut config = test_config();
        let dir = TempDir::new().expect("temp dir");
        config.directory = Arc::from(dir.path());
        config.state_db = Some(db.clone());
        config.sync_mode = SyncMode::Full;

        download_photos_with_sync(
            &Client::new(),
            &passes,
            Arc::new(config),
            DownloadControls::download_hidden(),
            CancellationToken::new(),
        )
        .await
        .expect("clean full enumeration");

        let summary = db.get_summary().await.expect("summary");
        assert_eq!(summary.total_assets, 1);
        assert_eq!(summary.source_deleted, 0);
        assert_eq!(summary.pending + summary.failed, 1);
        assert_eq!(summary.awaiting_provider_verification, 1);
    }

    #[tokio::test]
    async fn incremental_legacy_pending_master_lookup_clears_explicit_provider_deletion() {
        let db = Arc::new(crate::state::SqliteStateDb::open_in_memory().expect("state db"));
        let record = TestAssetRecord::new("PENDING_CONFIRMED_DELETED")
            .filename("confirmed-deleted.jpg")
            .build();
        db.upsert_seen(&record).await.expect("seed pending row");
        let session = PendingLookupSession {
            records: Arc::new(vec![json!({
                "recordName": "PENDING_CONFIRMED_DELETED",
                "serverErrorCode": "UNKNOWN_ITEM",
                "reason": "record not found"
            })]),
        };
        let passes = vec![AlbumPass {
            kind: PassKind::Unfiled,
            album: album_with_session("PrimarySync", "", Box::new(session)),
            exclude_ids: Arc::new(FxHashSet::default()),
        }];

        let mut config = test_config();
        let dir = TempDir::new().expect("temp dir");
        config.directory = Arc::from(dir.path());
        config.state_db = Some(db.clone());
        config.sync_mode = SyncMode::Incremental {
            zone_sync_token: "zone-token-prev".to_string(),
        };

        let result = download_photos_with_sync(
            &Client::new(),
            &passes,
            Arc::new(config),
            DownloadControls::download_hidden(),
            CancellationToken::new(),
        )
        .await
        .expect("explicit provider deletion should resolve pending work");

        assert!(matches!(result.outcome, DownloadOutcome::Success));
        assert_eq!(result.sync_token.as_deref(), Some("zone-token-next"));
        let summary = db.get_summary().await.expect("summary");
        assert_eq!(summary.pending, 0);
        assert_eq!(summary.failed, 0);
        assert_eq!(summary.awaiting_provider_verification, 0);
        assert_eq!(summary.source_deleted, 1);
    }

    #[tokio::test]
    async fn bounded_full_sync_revalidates_explicit_provider_deletion() {
        let db = Arc::new(crate::state::SqliteStateDb::open_in_memory().expect("state db"));
        let record = TestAssetRecord::new("BOUNDED_FULL_PENDING_DELETED")
            .filename("bounded-full-pending-deleted.jpg")
            .build();
        db.upsert_seen(&record).await.expect("seed pending row");
        let session = PendingLookupSession {
            records: Arc::new(vec![json!({
                "recordName": "BOUNDED_FULL_PENDING_DELETED",
                "serverErrorCode": "UNKNOWN_ITEM",
                "reason": "record not found"
            })]),
        };
        let passes = vec![AlbumPass {
            kind: PassKind::Unfiled,
            album: album_with_session("PrimarySync", "", Box::new(session)),
            exclude_ids: Arc::new(FxHashSet::default()),
        }];

        let mut config = test_config();
        let dir = TempDir::new().expect("temp dir");
        config.directory = Arc::from(dir.path());
        config.state_db = Some(db.clone());
        config.sync_mode = SyncMode::Full;
        config.recent = Some(300);

        let result = download_photos_with_sync(
            &Client::new(),
            &passes,
            Arc::new(config),
            DownloadControls::download_hidden(),
            CancellationToken::new(),
        )
        .await
        .expect("bounded full sync should revalidate pending work");

        assert!(result.full_enumeration_ran);
        assert!(matches!(result.outcome, DownloadOutcome::Success));
        let summary = db.get_summary().await.expect("summary");
        assert_eq!(summary.pending, 0);
        assert_eq!(summary.failed, 0);
        assert_eq!(summary.awaiting_provider_verification, 0);
        assert_eq!(summary.source_deleted, 1);
    }

    #[tokio::test]
    async fn bounded_full_sync_hydrates_live_legacy_pending_master() {
        use base64::Engine as _;
        use sha2::{Digest, Sha256};
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};

        let server = crate::start_wiremock_or_skip!();
        let body = vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0x4A, 0x46];
        let checksum = base64::engine::general_purpose::STANDARD.encode(Sha256::digest(&body));
        Mock::given(method("GET"))
            .and(path("/legacy-pending.jpg"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_bytes(body)
                    .insert_header("content-type", "image/jpeg"),
            )
            .mount(&server)
            .await;

        let db = Arc::new(crate::state::SqliteStateDb::open_in_memory().expect("state db"));
        let record = TestAssetRecord::new("LEGACY_PENDING_PRESENT")
            .filename("legacy-pending.jpg")
            .checksum(&checksum)
            .size(8)
            .build();
        db.upsert_seen(&record).await.expect("seed pending row");

        let download_url = format!("{}/legacy-pending.jpg", server.uri());
        let mut records = incremental_photo_records_with_url(
            "LEGACY_PENDING_PRESENT",
            "legacy-pending.jpg",
            &download_url,
            8,
        );
        records[0]["fields"]["resOriginalRes"]["value"]["fileChecksum"] = json!(checksum);
        let session = LegacyPendingHydrationSession {
            lookup_records: Arc::new(vec![records[0].clone()]),
            hydration_records: Arc::new(records),
            hydration_error: None,
        };
        let passes = vec![AlbumPass {
            kind: PassKind::Unfiled,
            album: album_with_session("PrimarySync", "", Box::new(session)),
            exclude_ids: Arc::new(FxHashSet::default()),
        }];

        let mut config = test_config();
        let dir = TempDir::new().expect("temp dir");
        config.directory = Arc::from(dir.path());
        config.state_db = Some(db.clone());
        config.sync_mode = SyncMode::Full;
        config.recent = Some(300);

        let result = download_photos_with_sync(
            &Client::new(),
            &passes,
            Arc::new(config),
            DownloadControls::download_hidden(),
            CancellationToken::new(),
        )
        .await
        .expect("live legacy pending master should hydrate and download");

        assert!(result.full_enumeration_ran);
        assert!(matches!(result.outcome, DownloadOutcome::Success));
        let summary = db.get_summary().await.expect("summary");
        assert_eq!(summary.downloaded, 1);
        assert_eq!(summary.pending, 0);
        assert_eq!(summary.failed, 0);
        assert_eq!(summary.awaiting_provider_verification, 0);
        assert_eq!(summary.source_deleted, 0);
        assert_eq!(
            db.get_master_record_name_for_asset("PrimarySync", "asset-LEGACY_PENDING_PRESENT")
                .await
                .expect("mapping lookup")
                .as_deref(),
            Some("LEGACY_PENDING_PRESENT")
        );
        let downloaded = db
            .get_downloaded_page(0, 10)
            .await
            .expect("downloaded page");
        let local_path = downloaded[0]
            .local_path
            .as_ref()
            .expect("downloaded row has local path");
        assert!(tokio::fs::metadata(local_path).await.is_ok());
    }

    #[tokio::test]
    async fn bounded_full_sync_adopts_filtered_legacy_pending_file() {
        use base64::Engine as _;
        use sha2::{Digest, Sha256};

        let body = vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0x4A, 0x46];
        let checksum = base64::engine::general_purpose::STANDARD.encode(Sha256::digest(&body));
        let mut records = incremental_photo_records_with_url(
            "LEGACY_FILTERED_ON_DISK",
            "IMG_8905.HEIC",
            "https://p01.icloud-content.com/IMG_8905.HEIC",
            16,
        );
        records[0]["fields"]["resOriginalVidComplRes"] = json!({"value": {
            "downloadURL": "https://p01.icloud-content.com/IMG_8905.MOV",
            "fileChecksum": checksum,
            "size": 8,
        }});
        records[0]["fields"]["resOriginalVidComplFileType"] =
            json!({"value": "com.apple.quicktime-movie"});
        let db = Arc::new(crate::state::SqliteStateDb::open_in_memory().expect("state db"));
        let session = LegacyPendingHydrationSession {
            lookup_records: Arc::new(vec![records[0].clone()]),
            hydration_records: Arc::new(records.clone()),
            hydration_error: None,
        };
        let passes = vec![AlbumPass {
            kind: PassKind::Unfiled,
            album: album_with_session("PrimarySync", "", Box::new(session)),
            exclude_ids: Arc::new(FxHashSet::default()),
        }];

        let dir = TempDir::new().expect("temp dir");
        let mut config = test_config();
        config.directory = Arc::from(dir.path());
        config.state_db = Some(db.clone());
        config.sync_mode = SyncMode::Full;
        config.recent = Some(300);
        config.skip_created_before = Some(crate::config::CreatedDateFilter::Instant(
            Utc.timestamp_opt(1_800_000_000, 0).unwrap(),
        ));
        let asset = PhotoAsset::new(records[0].clone(), records[1].clone());
        let expected_path = filter::expected_paths_for(&asset, &config)
            .into_iter()
            .find(|path| path.version_size == VersionSizeKey::LiveOriginal)
            .expect("legacy live photo should derive its MOV path")
            .path;
        let pending_filename = expected_path
            .file_name()
            .and_then(|name| name.to_str())
            .expect("expected MOV path should have a filename");
        let record = TestAssetRecord::new("LEGACY_FILTERED_ON_DISK")
            .version_size(VersionSizeKey::LiveOriginal)
            .filename(pending_filename)
            .checksum(&checksum)
            .size(8)
            .build();
        db.upsert_seen(&record).await.expect("seed pending row");
        tokio::fs::create_dir_all(expected_path.parent().expect("expected path parent"))
            .await
            .expect("create expected path parent");
        tokio::fs::write(&expected_path, &body)
            .await
            .expect("seed existing media");

        let result = download_photos_with_sync(
            &Client::new(),
            &passes,
            Arc::new(config),
            DownloadControls::download_hidden(),
            CancellationToken::new(),
        )
        .await
        .expect("filtered legacy pending file should be adopted");

        assert!(matches!(result.outcome, DownloadOutcome::Success));
        let summary = db.get_summary().await.expect("summary");
        assert_eq!(summary.downloaded, 1);
        assert_eq!(summary.pending, 0);
        assert_eq!(summary.failed, 0);
        assert_eq!(summary.awaiting_provider_verification, 0);
        let downloaded = db
            .get_downloaded_page(0, 10)
            .await
            .expect("downloaded page");
        assert_eq!(
            downloaded[0].local_path.as_deref(),
            Some(expected_path.as_path())
        );
    }

    #[tokio::test]
    async fn bounded_full_sync_revalidates_policy_excluded_asset_after_later_deletion() {
        let db = Arc::new(crate::state::SqliteStateDb::open_in_memory().expect("state db"));
        let record = TestAssetRecord::new("LEGACY_FILTERED_MISSING")
            .filename("legacy-filtered-missing.jpg")
            .checksum("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=")
            .size(1024)
            .build();
        db.upsert_seen(&record).await.expect("seed pending row");
        db.backdate_last_seen(
            "LEGACY_FILTERED_MISSING",
            Utc::now().timestamp().saturating_sub(86_400),
        );

        let live_records = mock_photo_records_with_filename(
            "LEGACY_FILTERED_MISSING",
            "legacy-filtered-missing.jpg",
        );
        let session = LegacyPendingHydrationSession {
            lookup_records: Arc::new(vec![live_records[0].clone()]),
            hydration_records: Arc::new(live_records.clone()),
            hydration_error: None,
        };
        let passes = vec![AlbumPass {
            kind: PassKind::Unfiled,
            album: album_with_session("PrimarySync", "", Box::new(session)),
            exclude_ids: Arc::new(FxHashSet::default()),
        }];

        let dir = TempDir::new().expect("temp dir");
        let mut config = test_config();
        config.directory = Arc::from(dir.path());
        config.state_db = Some(db.clone());
        config.sync_mode = SyncMode::Full;
        config.recent = Some(300);
        config.skip_created_before = Some(crate::config::CreatedDateFilter::Instant(
            Utc.timestamp_opt(1_800_000_000, 0).unwrap(),
        ));

        let result = download_photos_with_sync(
            &Client::new(),
            &passes,
            Arc::new(config.clone()),
            DownloadControls::download_hidden(),
            CancellationToken::new(),
        )
        .await
        .expect("filtered live pending asset should be policy-excluded");

        assert!(matches!(result.outcome, DownloadOutcome::Success));
        let summary = db.get_summary().await.expect("summary");
        assert_eq!(summary.pending, 0);
        assert_eq!(summary.policy_excluded, 1);
        assert_eq!(summary.failed, 0);
        assert_eq!(summary.awaiting_provider_verification, 0);
        assert_eq!(
            db.get_master_record_name_for_asset("PrimarySync", "asset-LEGACY_FILTERED_MISSING")
                .await
                .expect("mapping lookup")
                .as_deref(),
            Some("LEGACY_FILTERED_MISSING")
        );

        let present_passes = vec![AlbumPass {
            kind: PassKind::Unfiled,
            album: album_with_session(
                "PrimarySync",
                "",
                Box::new(BoundedPolicyLookupSession {
                    records: Arc::new(live_records),
                }),
            ),
            exclude_ids: Arc::new(FxHashSet::default()),
        }];
        let present_result = download_photos_with_sync(
            &Client::new(),
            &present_passes,
            Arc::new(config.clone()),
            DownloadControls::download_hidden(),
            CancellationToken::new(),
        )
        .await
        .expect("present filtered asset should remain policy-excluded");
        assert!(matches!(present_result.outcome, DownloadOutcome::Success));
        assert_eq!(present_result.sync_token, None);
        let summary = db.get_summary().await.expect("present summary");
        assert_eq!(summary.policy_excluded, 1);
        assert_eq!(summary.source_deleted, 0);

        let deleted_passes = vec![AlbumPass {
            kind: PassKind::Unfiled,
            album: album_with_session(
                "PrimarySync",
                "",
                Box::new(BoundedPolicyLookupSession {
                    records: Arc::new(vec![json!({
                        "recordName": "LEGACY_FILTERED_MISSING",
                        "serverErrorCode": "UNKNOWN_ITEM",
                        "reason": "record not found"
                    })]),
                }),
            ),
            exclude_ids: Arc::new(FxHashSet::default()),
        }];
        let deleted_result = download_photos_with_sync(
            &Client::new(),
            &deleted_passes,
            Arc::new(config),
            DownloadControls::download_hidden(),
            CancellationToken::new(),
        )
        .await
        .expect("explicit provider deletion should supersede policy exclusion");

        assert!(matches!(deleted_result.outcome, DownloadOutcome::Success));
        assert_eq!(deleted_result.sync_token, None);
        let summary = db.get_summary().await.expect("deleted summary");
        assert_eq!(summary.pending, 0);
        assert_eq!(summary.policy_excluded, 0);
        assert_eq!(summary.failed, 0);
        assert_eq!(summary.source_deleted, 1);
    }

    #[tokio::test]
    async fn contract_unknown_provider_identity_remains_pending_without_durable_match() {
        let db = Arc::new(crate::state::SqliteStateDb::open_in_memory().expect("state db"));
        let record = TestAssetRecord::new("LEGACY_AMBIGUOUS")
            .checksum("no-sibling-matches")
            .size(999)
            .build();
        db.upsert_seen(&record).await.expect("seed pending row");

        let master = mock_master_record_with_filename("LEGACY_AMBIGUOUS", "ambiguous.jpg");
        let hydration_records = vec![
            master.clone(),
            mock_asset_record_for("asset-ambiguous-a", "LEGACY_AMBIGUOUS"),
            mock_asset_record_for("asset-ambiguous-b", "LEGACY_AMBIGUOUS"),
        ];
        let session = LegacyPendingHydrationSession {
            lookup_records: Arc::new(vec![master]),
            hydration_records: Arc::new(hydration_records),
            hydration_error: None,
        };
        let passes = vec![AlbumPass {
            kind: PassKind::Unfiled,
            album: album_with_session("PrimarySync", "", Box::new(session)),
            exclude_ids: Arc::new(FxHashSet::default()),
        }];
        let mut config = test_config();
        let dir = TempDir::new().expect("temp dir");
        config.directory = Arc::from(dir.path());
        config.state_db = Some(db.clone());

        let plan = build_pending_retry_download_tasks(&passes, &config, CancellationToken::new())
            .await
            .expect("non-matching siblings should remain safely unresolved");

        assert!(plan.tasks.is_empty());
        assert_eq!(plan.unmatched_targets.len(), 1);
        let summary = db.get_summary().await.expect("summary");
        assert_eq!(summary.pending, 1);
        assert_eq!(summary.awaiting_provider_verification, 1);
        assert_eq!(
            db.get_master_record_name_for_asset("PrimarySync", "asset-ambiguous-a")
                .await
                .expect("mapping lookup"),
            None
        );
    }

    #[tokio::test]
    async fn pending_retry_uses_legacy_owner_to_resolve_matching_siblings() {
        let db = Arc::new(crate::state::SqliteStateDb::open_in_memory().expect("state db"));
        let record = TestAssetRecord::new("LEGACY_OWNED_RETRY")
            .checksum("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=")
            .filename("owned-retry.jpg")
            .size(1024)
            .build();
        db.upsert_seen(&record).await.expect("seed pending row");
        assert!(
            db.claim_legacy_master_state_owner(
                "PrimarySync",
                "LEGACY_OWNED_RETRY",
                "asset-owned-b",
            )
            .await
            .expect("claim legacy owner")
        );

        let master = mock_master_record_with_filename("LEGACY_OWNED_RETRY", "owned-retry.jpg");
        let session = LegacyPendingHydrationSession {
            lookup_records: Arc::new(vec![master.clone()]),
            hydration_records: Arc::new(vec![
                master,
                mock_asset_record_for("asset-owned-a", "LEGACY_OWNED_RETRY"),
                mock_asset_record_for("asset-owned-b", "LEGACY_OWNED_RETRY"),
            ]),
            hydration_error: None,
        };
        let passes = vec![AlbumPass {
            kind: PassKind::Unfiled,
            album: album_with_session("PrimarySync", "", Box::new(session)),
            exclude_ids: Arc::new(FxHashSet::default()),
        }];
        let mut config = test_config();
        let dir = TempDir::new().expect("temp dir");
        config.directory = Arc::from(dir.path());
        config.state_db = Some(db);

        let plan = build_pending_retry_download_tasks(&passes, &config, CancellationToken::new())
            .await
            .expect("persisted owner should resolve matching siblings");

        assert_eq!(plan.tasks.len(), 1);
        assert_eq!(plan.unmatched_targets.len(), 0);
        assert_eq!(plan.tasks[0].asset_id.as_ref(), "LEGACY_OWNED_RETRY");
        assert_eq!(plan.tasks[0].asset_record_name.as_ref(), "asset-owned-b");
    }

    #[tokio::test]
    async fn pending_retry_retains_transient_legacy_hydration_failure() {
        let db = Arc::new(crate::state::SqliteStateDb::open_in_memory().expect("state db"));
        let record = TestAssetRecord::new("LEGACY_TRANSIENT").build();
        db.upsert_seen(&record).await.expect("seed pending row");
        let session = LegacyPendingHydrationSession {
            lookup_records: Arc::new(vec![mock_master_record_with_filename(
                "LEGACY_TRANSIENT",
                "transient.jpg",
            )]),
            hydration_records: Arc::new(Vec::new()),
            hydration_error: Some(Arc::from("temporary changes failure")),
        };
        let passes = vec![AlbumPass {
            kind: PassKind::Unfiled,
            album: album_with_session("PrimarySync", "", Box::new(session)),
            exclude_ids: Arc::new(FxHashSet::default()),
        }];
        let mut config = test_config();
        config.state_db = Some(db.clone());

        let plan = build_pending_retry_download_tasks(&passes, &config, CancellationToken::new())
            .await
            .expect("transient hydration failure should retain durable work");

        assert!(plan.tasks.is_empty());
        assert_eq!(plan.unmatched_targets.len(), 1);
        let summary = db.get_summary().await.expect("summary");
        assert_eq!(summary.pending, 1);
        assert_eq!(summary.awaiting_provider_verification, 1);
    }

    #[tokio::test]
    async fn pending_retry_deleted_sibling_does_not_tombstone_present_master_state() {
        let db = Arc::new(crate::state::SqliteStateDb::open_in_memory().expect("state db"));
        let record = TestAssetRecord::new("MASTER_WITH_SIBLINGS")
            .filename("master-with-siblings.jpg")
            .build();
        db.upsert_seen(&record).await.expect("seed pending row");
        db.upsert_asset_master_mapping("PrimarySync", "asset-a-deleted", "MASTER_WITH_SIBLINGS")
            .await
            .expect("seed deleted sibling mapping");
        db.upsert_asset_master_mapping("PrimarySync", "asset-b-present", "MASTER_WITH_SIBLINGS")
            .await
            .expect("seed present sibling mapping");

        let session = PendingLookupSession {
            records: Arc::new(vec![
                json!({
                    "recordName": "asset-a-deleted",
                    "serverErrorCode": "UNKNOWN_ITEM",
                    "reason": "record not found"
                }),
                mock_master_record_with_filename(
                    "MASTER_WITH_SIBLINGS",
                    "master-with-siblings.jpg",
                ),
                mock_asset_record_for("asset-b-present", "MASTER_WITH_SIBLINGS"),
            ]),
        };
        let passes = vec![AlbumPass {
            kind: PassKind::Unfiled,
            album: album_with_session("PrimarySync", "", Box::new(session)),
            exclude_ids: Arc::new(FxHashSet::default()),
        }];
        let mut config = test_config();
        let dir = TempDir::new().expect("temp dir");
        config.directory = Arc::from(dir.path());
        config.state_db = Some(db.clone());

        let plan = build_pending_retry_download_tasks(&passes, &config, CancellationToken::new())
            .await
            .expect("build pending retry plan");

        assert_eq!(plan.unmatched_targets.len(), 0);
        let summary = db.get_summary().await.expect("summary");
        assert_eq!(summary.source_deleted, 0);
        assert_eq!(summary.pending, 1);
    }

    async fn run_bounded_incremental_sync(
        db: Arc<crate::state::SqliteStateDb>,
        records: Vec<Value>,
    ) -> SyncResult {
        let session = MockPhotosFlow::new()
            .changes_zone_page(records, "zone-token-next", false)
            .build();
        let passes = vec![AlbumPass {
            kind: PassKind::Unfiled,
            album: mock_album("", session),
            exclude_ids: Arc::new(FxHashSet::default()),
        }];

        let mut config = test_config();
        let dir = TempDir::new().expect("temp dir");
        config.directory = Arc::from(dir.path());
        config.state_db = Some(db);
        config.sync_mode = SyncMode::Incremental {
            zone_sync_token: "zone-token-prev".to_string(),
        };
        config.recent = Some(300);
        config.skip_created_before = Some(crate::config::CreatedDateFilter::Instant(
            Utc.timestamp_opt(1_746_994_800, 0).unwrap(),
        ));

        download_photos_with_sync(
            &Client::new(),
            &passes,
            Arc::new(config),
            DownloadControls::download_hidden(),
            CancellationToken::new(),
        )
        .await
        .expect("bounded incremental sync should complete")
    }

    #[tokio::test]
    async fn incremental_source_delete_prunes_pending_row_in_bounded_sync() {
        let db = Arc::new(crate::state::SqliteStateDb::open_in_memory().expect("state db"));
        let record = TestAssetRecord::new("PENDING_SOURCE_DELETED")
            .filename("pending-source-deleted.jpg")
            .checksum("ck_pending_source_deleted")
            .size(1024)
            .build();
        db.upsert_seen(&record).await.expect("seed pending row");

        let result = run_bounded_incremental_sync(
            db.clone(),
            vec![hard_deleted_change_record("PENDING_SOURCE_DELETED")],
        )
        .await;

        assert!(
            !result.full_enumeration_ran,
            "source delete cleanup should not force full enumeration"
        );
        assert!(matches!(result.outcome, DownloadOutcome::Success));
        assert_eq!(result.sync_token.as_deref(), Some("zone-token-next"));
        assert!(!result.stats.sync_token_blocked);
        assert!(db.get_pending().await.expect("pending page").is_empty());
    }

    #[tokio::test]
    async fn incremental_prunes_existing_source_deleted_pending_row_without_wide_sync() {
        let db = Arc::new(crate::state::SqliteStateDb::open_in_memory().expect("state db"));
        let record = TestAssetRecord::new("STALE_SOURCE_DELETED")
            .filename("stale-source-deleted.jpg")
            .checksum("ck_stale_source_deleted")
            .size(1024)
            .build();
        db.upsert_seen(&record).await.expect("seed pending row");
        db.mark_soft_deleted("PrimarySync", "STALE_SOURCE_DELETED", None)
            .await
            .expect("simulate old source-deleted pending row");

        let result = run_bounded_incremental_sync(db.clone(), Vec::new()).await;

        assert!(
            !result.full_enumeration_ran,
            "cleanup should not force full enumeration"
        );
        assert!(matches!(result.outcome, DownloadOutcome::Success));
        assert_eq!(result.sync_token.as_deref(), Some("zone-token-next"));
        assert!(!result.stats.sync_token_blocked);
        assert!(db.get_pending().await.expect("pending page").is_empty());
    }

    #[tokio::test]
    async fn incremental_ignores_pending_rows_from_other_libraries() {
        let db = Arc::new(crate::state::SqliteStateDb::open_in_memory().expect("state db"));
        let record = crate::test_helpers::TestAssetRecord::new("SHARED_PENDING_BEFORE_SYNC")
            .library("SharedSync-ONE")
            .filename("shared-pending-before-sync.jpg")
            .checksum("ck_shared_pending_before_sync")
            .size(1024)
            .build();
        db.upsert_seen(&record)
            .await
            .expect("seed shared pending row");

        let session = MockPhotosFlow::new()
            .changes_zone_page(Vec::new(), "zone-token-next", false)
            .build();
        let passes = vec![AlbumPass {
            kind: PassKind::Unfiled,
            album: mock_album("", session),
            exclude_ids: Arc::new(FxHashSet::default()),
        }];

        let mut config = test_config();
        let dir = TempDir::new().expect("temp dir");
        config.directory = Arc::from(dir.path());
        config.state_db = Some(db);
        config.sync_mode = SyncMode::Incremental {
            zone_sync_token: "zone-token-prev".to_string(),
        };

        let result = download_photos_with_sync(
            &Client::new(),
            &passes,
            Arc::new(config),
            DownloadControls::download_hidden(),
            CancellationToken::new(),
        )
        .await
        .expect("other-library pending rows must not force full enumeration");

        assert!(
            !result.full_enumeration_ran,
            "PrimarySync should remain incremental when only SharedSync has pending work"
        );
        assert_eq!(result.sync_token.as_deref(), Some("zone-token-next"));
    }

    #[tokio::test]
    async fn incremental_with_metadata_backfill_records_full_enumeration_reason() {
        let db = Arc::new(crate::state::SqliteStateDb::open_in_memory().expect("state db"));
        let dir = TempDir::new().expect("temp dir");
        let record = crate::test_helpers::TestAssetRecord::new("BACKFILL_BEFORE_SYNC")
            .filename("backfill-before-sync.jpg")
            .checksum("ck_backfill_before_sync")
            .size(1024)
            .build();
        db.upsert_seen(&record).await.expect("seed pending row");
        let path = dir.path().join("backfill-before-sync.jpg");
        tokio::fs::write(&path, vec![0u8; 1024])
            .await
            .expect("write local file");
        db.mark_downloaded(
            "PrimarySync",
            "BACKFILL_BEFORE_SYNC",
            "original",
            &path,
            "local_hash",
            None,
        )
        .await
        .expect("mark downloaded");
        db.clear_metadata_hash_for_test("PrimarySync", "BACKFILL_BEFORE_SYNC", "original");
        assert!(db.has_downloaded_without_metadata_hash().await.unwrap());

        let session = MockPhotosFlow::new()
            .album_count(0)
            .empty_query_page(Some("zone-token-next"))
            .build();
        let passes = vec![AlbumPass {
            kind: PassKind::Unfiled,
            album: mock_album("", session),
            exclude_ids: Arc::new(FxHashSet::default()),
        }];

        let mut config = test_config();
        config.directory = Arc::from(dir.path());
        config.state_db = Some(db);
        config.sync_mode = SyncMode::Incremental {
            zone_sync_token: "zone-token-prev".to_string(),
        };

        let result = download_photos_with_sync(
            &Client::new(),
            &passes,
            Arc::new(config),
            DownloadControls::new(DownloadRunMode::PrintFilenames, DownloadReporting::hidden()),
            CancellationToken::new(),
        )
        .await
        .expect("metadata backfill should fall back to full enumeration");

        assert!(result.full_enumeration_ran);
        assert_eq!(
            result.stats.full_enumeration_reason,
            Some(FullEnumerationReason::MetadataBackfill)
        );
    }

    #[tokio::test]
    async fn contract_metadata_capture_revision_repair_is_durable() {
        let db = Arc::new(SqliteStateDb::open_in_memory().expect("state db"));
        let dir = TempDir::new().expect("temp dir");
        let stored_records = incremental_photo_records_with_favorite("CAPTURE_REVISION", false);
        let stored_asset = PhotoAsset::new(stored_records[0].clone(), stored_records[1].clone());
        let changed_records = incremental_photo_records_with_favorite("CAPTURE_REVISION", true);
        let pass = AlbumPass {
            kind: PassKind::Unfiled,
            album: album_with_session(
                "PrimarySync",
                "",
                Box::new(PendingLookupSession {
                    records: Arc::new(changed_records),
                }),
            ),
            exclude_ids: Arc::new(FxHashSet::default()),
        };
        let mut config = test_config();
        config.directory = Arc::from(dir.path());
        config.state_db = Some(Arc::clone(&db) as Arc<dyn DownloadStore>);
        config.sync_mode = SyncMode::Incremental {
            zone_sync_token: "zone-token-prev".to_string(),
        };
        let media_path =
            seed_downloaded_metadata_asset(db.as_ref(), &config, &pass, &stored_asset).await;
        db.upsert_asset_master_mapping("PrimarySync", "asset-CAPTURE_REVISION", "CAPTURE_REVISION")
            .await
            .expect("seed durable provider identity");
        assert!(
            db.claim_legacy_master_state_owner(
                "PrimarySync",
                "CAPTURE_REVISION",
                "asset-CAPTURE_REVISION",
            )
            .await
            .expect("seed legacy state owner")
        );
        db.set_metadata_capture_revision_for_test("PrimarySync", "CAPTURE_REVISION", 0);
        let before = tokio::fs::read(&media_path)
            .await
            .expect("read seeded media");

        let result = download_photos_with_sync(
            &Client::new(),
            std::slice::from_ref(&pass),
            Arc::new(config),
            DownloadControls::download_hidden(),
            CancellationToken::new(),
        )
        .await
        .expect("normal sync should repair stale capture metadata");

        assert!(
            matches!(result.outcome, DownloadOutcome::Success),
            "unexpected outcome: {:?}, stats: {:?}",
            result.outcome,
            result.stats
        );
        assert_eq!(result.stats.downloaded, 0);
        assert_eq!(result.stats.metadata_capture_revision, Some(1));
        assert_eq!(result.stats.metadata_capture_refreshed, 1);
        assert_eq!(result.stats.metadata_capture_failures, 0);
        assert_eq!(result.stats.metadata_capture_remaining, 0);
        assert_eq!(result.sync_token.as_deref(), Some("zone-token-next"));
        let refreshed = db
            .get_downloaded_page(0, 1)
            .await
            .expect("read refreshed row")
            .remove(0);
        assert!(refreshed.metadata.is_favorite);
        assert_eq!(refreshed.local_path.as_deref(), Some(media_path.as_path()));
        assert_eq!(
            tokio::fs::read(&media_path)
                .await
                .expect("read repaired media"),
            before,
            "catalogue-only repair must not mutate media when outputs are disabled"
        );
        let summary = db.get_summary().await.expect("summary");
        let capture = summary
            .metadata_capture
            .iter()
            .find(|status| status.library == "PrimarySync")
            .expect("capture status");
        assert_eq!(capture.active_revision, 1);
        assert_eq!(capture.pending_revision, None);
        assert_eq!(capture.remaining_assets, 0);
    }

    #[tokio::test]
    async fn metadata_capture_revision_repair_progresses_across_bounded_cycles() {
        #[derive(Clone, Debug)]
        struct CaptureScaleSession {
            records: Arc<HashMap<String, Value>>,
            rate_limited_once: Arc<AtomicBool>,
        }

        #[async_trait::async_trait]
        impl PhotosSession for CaptureScaleSession {
            async fn post(
                &self,
                url: &str,
                body: String,
                _headers: &[(&str, &str)],
            ) -> anyhow::Result<Value> {
                if !url.contains("/records/lookup?") {
                    return Ok(if url.contains("/changes/zone?") {
                        changes_zone_response(Vec::new(), "zone-token-next")
                    } else {
                        json!({"records": [], "syncToken": "ignored-query-token"})
                    });
                }
                if !self.rate_limited_once.swap(true, Ordering::SeqCst) {
                    return Err(crate::icloud::photos::session::HttpStatusError {
                        status: 429,
                        url: url.to_owned(),
                        retry_after: None,
                        body: None,
                    }
                    .into());
                }
                let request: Value = serde_json::from_str(&body)?;
                let records = request["records"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|record| record["recordName"].as_str())
                    .filter_map(|record_name| self.records.get(record_name).cloned())
                    .collect::<Vec<_>>();
                Ok(json!({"records": records}))
            }

            fn clone_box(&self) -> Box<dyn PhotosSession> {
                Box::new(self.clone())
            }
        }

        const STALE_ASSETS: usize = 1_201;
        let stale_assets = u64::try_from(STALE_ASSETS).expect("asset count fits u64");
        let db = Arc::new(SqliteStateDb::open_in_memory().expect("state db"));
        let mut provider_records = HashMap::with_capacity(STALE_ASSETS * 2);
        for index in 0..STALE_ASSETS {
            let master_id = format!("CAPTURE_SCALE_{index:04}");
            let asset_id = format!("asset-{master_id}");
            for record in incremental_photo_records_with_favorite(&master_id, true) {
                provider_records.insert(
                    record["recordName"]
                        .as_str()
                        .expect("provider record name")
                        .to_owned(),
                    record,
                );
            }
            let record = TestAssetRecord::new(&asset_id)
                .filename("metadata-capture-scale.jpg")
                .checksum("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=")
                .size(1024)
                .build();
            db.upsert_seen(&record).await.expect("seed state row");
            db.mark_downloaded(
                "PrimarySync",
                &asset_id,
                "original",
                Path::new("/photos/metadata-capture-scale.jpg"),
                "seeded-local-sha256",
                None,
            )
            .await
            .expect("mark state row downloaded");
            db.upsert_asset_master_mapping("PrimarySync", &asset_id, &master_id)
                .await
                .expect("seed provider mapping");
        }
        db.acquire_lock("test_metadata_capture_scale_stale")
            .expect("state lock")
            .execute("DELETE FROM asset_metadata_capture_revisions", [])
            .expect("mark every asset stale");

        let pass = AlbumPass {
            kind: PassKind::Unfiled,
            album: album_with_session_and_retry_config(
                "PrimarySync",
                "",
                None,
                RetryConfig {
                    max_retries: 1,
                    base_delay_secs: 0,
                    max_delay_secs: 0,
                },
                Box::new(CaptureScaleSession {
                    records: Arc::new(provider_records),
                    rate_limited_once: Arc::new(AtomicBool::new(false)),
                }),
            ),
            exclude_ids: Arc::new(FxHashSet::default()),
        };
        let mut config = test_config();
        let dir = TempDir::new().expect("temp dir");
        config.directory = Arc::from(dir.path());
        config.state_db = Some(Arc::clone(&db) as Arc<dyn DownloadStore>);
        config.sync_mode = SyncMode::Incremental {
            zone_sync_token: "zone-token-prev".to_string(),
        };

        for (cycle, refreshed, remaining, active_revision, rate_limited) in [
            (1, 500, 701, 0, 1),
            (2, 500, 201, 0, 0),
            (3, 201, 0, crate::state::METADATA_CAPTURE_REVISION, 0),
        ] {
            let result = download_photos_with_sync(
                &Client::new(),
                std::slice::from_ref(&pass),
                Arc::new(config.clone()),
                DownloadControls::download_hidden(),
                CancellationToken::new(),
            )
            .await
            .expect("metadata-capture cycle");

            assert!(
                matches!(result.outcome, DownloadOutcome::Success),
                "cycle {cycle}: {:?}",
                result.stats
            );
            assert_eq!(
                result.stats.metadata_capture_refreshed, refreshed,
                "cycle {cycle}"
            );
            assert_eq!(
                result.stats.metadata_capture_remaining, remaining,
                "cycle {cycle}"
            );
            assert_eq!(result.stats.rate_limited, rate_limited, "cycle {cycle}");
            assert_eq!(
                result.stats.has_rate_limit_pressure(),
                rate_limited > 0,
                "cycle {cycle}"
            );
            let summary = db.get_summary().await.expect("capture status");
            let capture = summary
                .metadata_capture
                .iter()
                .find(|status| status.library == "PrimarySync")
                .expect("primary capture status");
            assert_eq!(capture.active_revision, active_revision, "cycle {cycle}");
            assert_eq!(
                capture.pending_revision,
                (remaining > 0).then_some(crate::state::METADATA_CAPTURE_REVISION),
                "cycle {cycle}"
            );
            assert_eq!(
                capture.processed_assets,
                stale_assets - remaining,
                "cycle {cycle}"
            );
            assert_eq!(capture.remaining_assets, remaining, "cycle {cycle}");
        }
    }

    #[tokio::test]
    async fn metadata_capture_source_deletions_count_as_clean_progress() {
        const DELETED_ASSETS: usize = 500;
        const STALE_ASSETS: usize = DELETED_ASSETS + 1;

        let db = Arc::new(SqliteStateDb::open_in_memory().expect("state db"));
        let mut provider_records = Vec::with_capacity(DELETED_ASSETS * 2);
        for index in 0..STALE_ASSETS {
            let master_id = format!("CAPTURE_DELETED_{index:04}");
            let asset_id = format!("asset-{master_id}");
            if index < DELETED_ASSETS {
                let master = incremental_photo_records(&master_id)
                    .into_iter()
                    .next()
                    .expect("master record");
                provider_records.push(master);
                provider_records.push(json!({
                    "recordName": asset_id,
                    "serverErrorCode": "UNKNOWN_ITEM",
                    "reason": "record not found"
                }));
            }

            let record = TestAssetRecord::new(&asset_id)
                .filename("metadata-capture-deleted.jpg")
                .checksum("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=")
                .size(1024)
                .build();
            db.upsert_seen(&record).await.expect("seed state row");
            db.mark_downloaded(
                "PrimarySync",
                &asset_id,
                "original",
                Path::new("/photos/metadata-capture-deleted.jpg"),
                "seeded-local-sha256",
                None,
            )
            .await
            .expect("mark state row downloaded");
            db.upsert_asset_master_mapping("PrimarySync", &asset_id, &master_id)
                .await
                .expect("seed provider mapping");
        }
        db.acquire_lock("test_metadata_capture_deleted_stale")
            .expect("state lock")
            .execute("DELETE FROM asset_metadata_capture_revisions", [])
            .expect("mark every asset stale");

        let pass = AlbumPass {
            kind: PassKind::Unfiled,
            album: album_with_session(
                "PrimarySync",
                "",
                Box::new(PendingLookupSession {
                    records: Arc::new(provider_records),
                }),
            ),
            exclude_ids: Arc::new(FxHashSet::default()),
        };
        let mut config = test_config();
        let dir = TempDir::new().expect("temp dir");
        config.directory = Arc::from(dir.path());
        config.state_db = Some(Arc::clone(&db) as Arc<dyn DownloadStore>);
        config.sync_mode = SyncMode::Incremental {
            zone_sync_token: "zone-token-prev".to_string(),
        };

        let result = download_photos_with_sync(
            &Client::new(),
            std::slice::from_ref(&pass),
            Arc::new(config),
            DownloadControls::download_hidden(),
            CancellationToken::new(),
        )
        .await
        .expect("source-deleted capture batch");

        assert!(matches!(result.outcome, DownloadOutcome::Success));
        assert_eq!(result.stats.metadata_capture_refreshed, 0);
        assert_eq!(result.stats.metadata_capture_failures, 0);
        assert_eq!(result.stats.metadata_capture_remaining, 1);
        assert!(result.stats.metadata_capture_progressed);
        assert_eq!(
            db.get_summary().await.expect("summary").source_deleted,
            u64::try_from(DELETED_ASSETS).expect("deleted count fits u64")
        );
    }

    #[tokio::test]
    async fn current_capture_revision_adds_no_provider_lookup_requests() {
        #[derive(Clone, Debug)]
        struct CountingCaptureSession {
            lookups: Arc<AtomicUsize>,
            changes: Arc<AtomicUsize>,
        }

        #[async_trait::async_trait]
        impl PhotosSession for CountingCaptureSession {
            async fn post(
                &self,
                url: &str,
                _body: String,
                _headers: &[(&str, &str)],
            ) -> anyhow::Result<Value> {
                if url.contains("/records/lookup?") {
                    self.lookups.fetch_add(1, Ordering::SeqCst);
                    return Ok(json!({"records": []}));
                }
                if url.contains("/changes/zone?") {
                    self.changes.fetch_add(1, Ordering::SeqCst);
                    return Ok(changes_zone_response(Vec::new(), "zone-token-next"));
                }
                Ok(json!({"records": [], "syncToken": "ignored-query-token"}))
            }

            fn clone_box(&self) -> Box<dyn PhotosSession> {
                Box::new(self.clone())
            }
        }

        let db = Arc::new(SqliteStateDb::open_in_memory().expect("state db"));
        let dir = TempDir::new().expect("temp dir");
        let records = incremental_photo_records("CAPTURE_CURRENT");
        let asset = PhotoAsset::new(records[0].clone(), records[1].clone());
        let lookups = Arc::new(AtomicUsize::new(0));
        let changes = Arc::new(AtomicUsize::new(0));
        let pass = AlbumPass {
            kind: PassKind::Unfiled,
            album: album_with_session(
                "PrimarySync",
                "",
                Box::new(CountingCaptureSession {
                    lookups: Arc::clone(&lookups),
                    changes: Arc::clone(&changes),
                }),
            ),
            exclude_ids: Arc::new(FxHashSet::default()),
        };
        let mut config = test_config();
        config.directory = Arc::from(dir.path());
        config.state_db = Some(Arc::clone(&db) as Arc<dyn DownloadStore>);
        config.sync_mode = SyncMode::Incremental {
            zone_sync_token: "zone-token-prev".to_string(),
        };
        seed_downloaded_metadata_asset(db.as_ref(), &config, &pass, &asset).await;

        let result = download_photos_with_sync(
            &Client::new(),
            &[pass],
            Arc::new(config),
            DownloadControls::download_hidden(),
            CancellationToken::new(),
        )
        .await
        .expect("current capture revision should use the normal incremental path");

        assert!(matches!(result.outcome, DownloadOutcome::Success));
        assert_eq!(lookups.load(Ordering::SeqCst), 0);
        assert_eq!(changes.load(Ordering::SeqCst), 1);
        assert_eq!(result.stats.metadata_capture_refreshed, 0);
        assert_eq!(result.stats.metadata_capture_remaining, 0);
    }

    #[cfg(feature = "xmp")]
    #[tokio::test]
    async fn normal_sync_writes_configured_xmp_after_capture_revision_repair() {
        let db = Arc::new(SqliteStateDb::open_in_memory().expect("state db"));
        let dir = TempDir::new().expect("temp dir");
        let stored_records = incremental_photo_records_with_favorite("CAPTURE_XMP", false);
        let stored_asset = PhotoAsset::new(stored_records[0].clone(), stored_records[1].clone());
        let changed_records = incremental_photo_records_with_favorite("CAPTURE_XMP", true);
        let pass = AlbumPass {
            kind: PassKind::Unfiled,
            album: album_with_session(
                "PrimarySync",
                "",
                Box::new(PendingLookupSession {
                    records: Arc::new(changed_records),
                }),
            ),
            exclude_ids: Arc::new(FxHashSet::default()),
        };
        let mut config = test_config();
        config.directory = Arc::from(dir.path());
        config.metadata.xmp_sidecar = true;
        config.state_db = Some(Arc::clone(&db) as Arc<dyn DownloadStore>);
        config.sync_mode = SyncMode::Incremental {
            zone_sync_token: "zone-token-prev".to_string(),
        };
        let media_path =
            seed_downloaded_metadata_asset(db.as_ref(), &config, &pass, &stored_asset).await;
        db.upsert_asset_master_mapping("PrimarySync", "asset-CAPTURE_XMP", "CAPTURE_XMP")
            .await
            .expect("seed durable provider identity");
        assert!(
            db.claim_legacy_master_state_owner("PrimarySync", "CAPTURE_XMP", "asset-CAPTURE_XMP",)
                .await
                .expect("seed legacy state owner")
        );
        db.set_metadata_capture_revision_for_test("PrimarySync", "CAPTURE_XMP", 0);

        let result = download_photos_with_sync(
            &Client::new(),
            &[pass],
            Arc::new(config),
            DownloadControls::download_hidden(),
            CancellationToken::new(),
        )
        .await
        .expect("normal sync should repair metadata and drain the configured XMP write");

        assert!(matches!(result.outcome, DownloadOutcome::Success));
        assert_eq!(result.stats.downloaded, 0);
        assert_eq!(result.stats.metadata_capture_refreshed, 1);
        let sidecar_name = format!(
            "{}.xmp",
            media_path
                .file_name()
                .expect("media path has filename")
                .to_string_lossy()
        );
        assert!(media_path.with_file_name(sidecar_name).exists());
        assert!(
            db.get_pending_metadata_rewrites(10)
                .await
                .expect("read rewrite queue")
                .is_empty()
        );
    }

    #[tokio::test]
    async fn failed_capture_lookup_preserves_incremental_checkpoint_and_retries() {
        #[derive(Clone, Debug)]
        struct FailingCaptureLookupSession;

        #[async_trait::async_trait]
        impl PhotosSession for FailingCaptureLookupSession {
            async fn post(
                &self,
                url: &str,
                _body: String,
                _headers: &[(&str, &str)],
            ) -> anyhow::Result<Value> {
                if url.contains("/records/lookup?") {
                    anyhow::bail!("temporary lookup failure");
                }
                if url.contains("/changes/zone?") {
                    return Ok(changes_zone_response(Vec::new(), "zone-token-next"));
                }
                Ok(json!({"records": [], "syncToken": "ignored-query-token"}))
            }

            fn clone_box(&self) -> Box<dyn PhotosSession> {
                Box::new(self.clone())
            }
        }

        let db = Arc::new(SqliteStateDb::open_in_memory().expect("state db"));
        let dir = TempDir::new().expect("temp dir");
        let records = incremental_photo_records("CAPTURE_RETRY");
        let asset = PhotoAsset::new(records[0].clone(), records[1].clone());
        let pass = AlbumPass {
            kind: PassKind::Unfiled,
            album: album_with_session("PrimarySync", "", Box::new(FailingCaptureLookupSession)),
            exclude_ids: Arc::new(FxHashSet::default()),
        };
        let mut config = test_config();
        config.directory = Arc::from(dir.path());
        config.state_db = Some(Arc::clone(&db) as Arc<dyn DownloadStore>);
        config.sync_mode = SyncMode::Incremental {
            zone_sync_token: "zone-token-prev".to_string(),
        };
        seed_downloaded_metadata_asset(db.as_ref(), &config, &pass, &asset).await;
        db.upsert_asset_master_mapping("PrimarySync", "asset-CAPTURE_RETRY", "CAPTURE_RETRY")
            .await
            .unwrap();
        db.set_metadata_capture_revision_for_test("PrimarySync", "CAPTURE_RETRY", 0);

        let result = download_photos_with_sync(
            &Client::new(),
            &[pass],
            Arc::new(config),
            DownloadControls::download_hidden(),
            CancellationToken::new(),
        )
        .await
        .expect("lookup failure should remain a reported durable repair");

        assert!(matches!(
            result.outcome,
            DownloadOutcome::PartialFailure { failed_count: 1 }
        ));
        assert_eq!(result.sync_token, None);
        assert!(result.stats.sync_token_blocked);
        assert_eq!(
            result.stats.sync_token_blocked_reason,
            Some(METADATA_CAPTURE_REPAIR_FAILED_REASON)
        );
        assert_eq!(result.stats.metadata_capture_failures, 1);
        assert_eq!(result.stats.metadata_capture_remaining, 1);
        assert!(
            db.has_metadata_capture_work(
                &["PrimarySync"],
                crate::state::METADATA_CAPTURE_REVISION,
            )
                .await
                .unwrap(),
            "failed lookup must leave durable retry work"
        );
    }

    #[tokio::test]
    async fn interrupted_capture_repair_keeps_unprocessed_rows_pending_without_failure() {
        let db = Arc::new(SqliteStateDb::open_in_memory().expect("state db"));
        let dir = TempDir::new().expect("temp dir");
        let records = incremental_photo_records("CAPTURE_INTERRUPTED");
        let asset = PhotoAsset::new(records[0].clone(), records[1].clone());
        let pass = AlbumPass {
            kind: PassKind::Unfiled,
            album: album_with_session(
                "PrimarySync",
                "",
                Box::new(PendingLookupSession {
                    records: Arc::new(records),
                }),
            ),
            exclude_ids: Arc::new(FxHashSet::default()),
        };
        let mut config = test_config();
        config.directory = Arc::from(dir.path());
        config.state_db = Some(Arc::clone(&db) as Arc<dyn DownloadStore>);
        seed_downloaded_metadata_asset(db.as_ref(), &config, &pass, &asset).await;
        db.upsert_asset_master_mapping(
            "PrimarySync",
            "asset-CAPTURE_INTERRUPTED",
            "CAPTURE_INTERRUPTED",
        )
        .await
        .expect("seed durable provider identity");
        db.set_metadata_capture_revision_for_test("PrimarySync", "CAPTURE_INTERRUPTED", 0);
        let shutdown = CancellationToken::new();
        shutdown.cancel();

        let repair = run_metadata_capture_repair(
            std::slice::from_ref(&pass),
            &config,
            DownloadControls::download_hidden(),
            &shutdown,
        )
        .await;

        assert!(repair.stats.interrupted);
        assert!(repair.stats.sync_token_blocked);
        assert_eq!(repair.stats.metadata_capture_refreshed, 0);
        assert_eq!(repair.stats.metadata_capture_failures, 0);
        assert_eq!(repair.stats.metadata_capture_remaining, 1);
        let summary = db.get_summary().await.expect("summary");
        let capture = summary
            .metadata_capture
            .iter()
            .find(|status| status.library == "PrimarySync")
            .expect("capture status");
        assert_eq!(capture.pending_revision, Some(1));
        assert_eq!(capture.failed_assets, 0);
        assert_eq!(capture.remaining_assets, 1);
    }

    #[tokio::test]
    async fn incremental_sync_queries_zone_changes_once_per_library() {
        let calls = Arc::new(AtomicUsize::new(0));
        let session = changes_zone_session(
            Arc::clone(&calls),
            incremental_photo_records("MASTER_CHANGED"),
        );
        let passes = vec![
            AlbumPass {
                kind: PassKind::Album,
                album: changes_album("album_a", session.clone()),
                exclude_ids: Arc::new(FxHashSet::default()),
            },
            AlbumPass {
                kind: PassKind::Album,
                album: changes_album("album_b", session),
                exclude_ids: Arc::new(FxHashSet::default()),
            },
        ];

        let mut config = test_config();
        let dir = TempDir::new().expect("temp dir");
        config.directory = Arc::from(dir.path());

        let result = download_photos_incremental(
            &Client::new(),
            &passes,
            &Arc::new(config),
            "zone-token-prev",
            DownloadControls::new(DownloadRunMode::PrintFilenames, DownloadReporting::hidden()),
            CancellationToken::new(),
        )
        .await
        .expect("print-only incremental sync should succeed");

        assert!(matches!(result.outcome, DownloadOutcome::Success));
        assert_eq!(
            result.sync_token, None,
            "print-only mode must not advance the sync token"
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "changes/zone is zone-scoped; querying once per pass repeats the same delta"
        );
    }

    #[cfg(feature = "xmp")]
    #[tokio::test]
    async fn album_membership_rewrites_every_finalized_path_after_restart() {
        use base64::Engine as _;
        use sha2::{Digest, Sha256};
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};
        use xmp_toolkit::{OpenFileOptions, XmpFile, XmpMeta, xmp_ns};

        for embed_xmp in [false, true] {
            let server = crate::start_wiremock_or_skip!();
            let body = vec![
                0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0x4A, 0x46, 0x49, 0x46, 0x00, 0x01, 0x01, 0x00,
                0x00, 0x01, 0x00, 0x01, 0x00, 0x00, 0xFF, 0xD9,
            ];
            Mock::given(method("GET"))
                .and(path("/copies.jpg"))
                .respond_with(ResponseTemplate::new(200).set_body_bytes(body.clone()))
                .expect(2)
                .mount(&server)
                .await;
            let mut records = incremental_photo_records_with_url(
                "COPIES",
                "copies.jpg",
                &format!("{}/copies.jpg", server.uri()),
                body.len() as u64,
            );
            records[0]["fields"]["resOriginalRes"]["value"]["fileChecksum"] =
                json!(base64::engine::general_purpose::STANDARD.encode(Sha256::digest(&body)));
            let passes: Vec<_> = [("Family", "family"), ("Trip", "trip")]
                .into_iter()
                .map(|(name, id)| AlbumPass {
                    kind: PassKind::Album,
                    album: mock_album_with_container(
                        name,
                        id,
                        MockPhotosFlow::new()
                            .album_count(1)
                            .query_page(records.clone(), Some("token-full"))
                            .build(),
                    ),
                    exclude_ids: Arc::new(FxHashSet::default()),
                })
                .collect();
            let dir = TempDir::new().unwrap();
            let db_path = dir.path().join("state.db");
            let db = Arc::new(SqliteStateDb::open(&db_path).await.unwrap());
            let mut config = test_config();
            config.directory = Arc::from(dir.path().join("media"));
            config.metadata.xmp_sidecar = true;
            config.metadata.embed_xmp = embed_xmp;
            config.state_db = Some(db.clone());
            let initial = download_photos_with_sync(
                &Client::new(),
                &passes,
                Arc::new(config.clone()),
                DownloadControls::download_hidden(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
            assert!(
                matches!(initial.outcome, DownloadOutcome::Success),
                "{initial:?}"
            );
            assert_eq!(initial.stats.downloaded, 2);
            let paths: Vec<PathBuf> = {
                let conn = db.acquire_lock("test_finalized_paths").unwrap();
                let mut stmt = conn
                    .prepare("SELECT local_path FROM asset_metadata_paths ORDER BY local_path")
                    .unwrap();
                stmt.query_map([], |row| row.get::<_, String>(0))
                    .unwrap()
                    .map(|row| PathBuf::from(row.unwrap()))
                    .collect()
            };
            assert_eq!(paths.len(), 2);
            assert_eq!(
                paths
                    .iter()
                    .map(|path| path
                        .parent()
                        .unwrap()
                        .file_name()
                        .unwrap()
                        .to_str()
                        .unwrap())
                    .collect::<Vec<_>>(),
                ["Family", "Trip"]
            );
            let sidecars: Vec<_> = paths
                .iter()
                .map(|path| {
                    let mut name = path.file_name().unwrap().to_os_string();
                    name.push(".xmp");
                    path.with_file_name(name)
                })
                .collect();
            let subjects = |contents: &str| {
                let meta: XmpMeta = contents.parse().unwrap();
                let mut values: Vec<_> = meta
                    .property_array(xmp_ns::DC, "subject")
                    .map(|item| item.value)
                    .collect();
                values.sort();
                values
            };
            let embedded_subjects = |path: &Path| {
                let mut file = XmpFile::new().unwrap();
                file.open_file(path, OpenFileOptions::default().for_read())
                    .unwrap();
                let meta = file.xmp().unwrap();
                let mut values: Vec<_> = meta
                    .property_array(xmp_ns::DC, "subject")
                    .map(|item| item.value)
                    .collect();
                values.sort();
                values
            };
            if embed_xmp {
                for path in &paths {
                    assert_eq!(embedded_subjects(path), ["Family", "Trip"]);
                }
            }
            for path in &sidecars {
                assert_eq!(
                    subjects(&tokio::fs::read_to_string(path).await.unwrap()),
                    ["Family", "Trip"]
                );
            }
            assert!(
                db.get_pending_metadata_rewrites(10)
                    .await
                    .unwrap()
                    .is_empty()
            );
            assert_eq!(
                db.acquire_lock("test_registered_paths")
                    .unwrap()
                    .query_row("SELECT COUNT(*) FROM asset_metadata_paths", [], |row| row
                        .get::<_, i64>(
                        0
                    ))
                    .unwrap(),
                2
            );
            config.state_db = None;
            drop(db);
            let db = Arc::new(SqliteStateDb::open(&db_path).await.unwrap());
            config.state_db = Some(db.clone());
            config.recent = Some(10);
            let pass = AlbumPass {
                kind: PassKind::Unfiled,
                album: changes_album(
                    "",
                    changes_zone_session(
                        Arc::new(AtomicUsize::new(0)),
                        vec![relation_delete_record("trip", "asset-COPIES")],
                    ),
                ),
                exclude_ids: Arc::new(FxHashSet::default()),
            };
            // One independently damaged sidecar must not prevent the other copy's
            // rewrite, or clear the damaged copy's durable retry marker.
            let original_sidecar = tokio::fs::read(&sidecars[0]).await.unwrap();
            tokio::fs::write(&sidecars[0], b"not parseable XMP")
                .await
                .unwrap();
            let failed = download_photos_incremental(
                &Client::new(),
                std::slice::from_ref(&pass),
                &Arc::new(config.clone()),
                "token-full",
                DownloadControls::download_hidden(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
            assert!(
                matches!(failed.outcome, DownloadOutcome::PartialFailure { .. }),
                "{failed:?}"
            );
            assert_eq!(failed.stats.downloaded, 0);
            assert_eq!(
                tokio::fs::read(&sidecars[0]).await.unwrap(),
                b"not parseable XMP"
            );
            assert_eq!(
                subjects(&tokio::fs::read_to_string(&sidecars[1]).await.unwrap()),
                ["Family"]
            );
            let pending = db.get_pending_metadata_rewrites(10).await.unwrap();
            assert_eq!(pending.len(), 1);
            assert_eq!(pending[0].local_path.as_ref(), Some(&paths[0]));
            tokio::fs::write(&sidecars[0], original_sidecar)
                .await
                .unwrap();
            let repaired = download_photos_incremental(
                &Client::new(),
                std::slice::from_ref(&pass),
                &Arc::new(config.clone()),
                "token-full",
                DownloadControls::download_hidden(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
            assert!(
                matches!(repaired.outcome, DownloadOutcome::Success),
                "{repaired:?}"
            );
            assert_eq!(repaired.stats.downloaded, 0);
            let mut mtimes = Vec::new();
            for (media, sidecar) in paths.iter().zip(&sidecars) {
                if embed_xmp {
                    assert_eq!(embedded_subjects(media), ["Family"]);
                } else {
                    assert_eq!(tokio::fs::read(media).await.unwrap(), body);
                }
                let checksum = file::compute_sha256(media).await.unwrap();
                let recorded: String = db
                    .acquire_lock("test_path_fingerprints")
                    .unwrap()
                    .query_row(
                        "SELECT local_checksum FROM asset_metadata_paths WHERE local_path = ?1",
                        [media.to_string_lossy()],
                        |row| row.get(0),
                    )
                    .unwrap();
                assert_eq!(recorded, checksum);
                assert_eq!(
                    subjects(&tokio::fs::read_to_string(sidecar).await.unwrap()),
                    ["Family"]
                );
                mtimes.push(
                    tokio::fs::metadata(sidecar)
                        .await
                        .unwrap()
                        .modified()
                        .unwrap(),
                );
            }
            assert!(
                db.get_pending_metadata_rewrites(10)
                    .await
                    .unwrap()
                    .is_empty()
            );
            let steady = download_photos_incremental(
                &Client::new(),
                &[pass],
                &Arc::new(config),
                "token-full",
                DownloadControls::download_hidden(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
            assert!(
                matches!(steady.outcome, DownloadOutcome::Success),
                "{steady:?}"
            );
            assert_eq!(steady.stats.downloaded, 0);
            for (sidecar, before) in sidecars.iter().zip(mtimes) {
                assert_eq!(
                    tokio::fs::metadata(sidecar)
                        .await
                        .unwrap()
                        .modified()
                        .unwrap(),
                    before
                );
            }
        }
    }

    #[cfg(feature = "xmp")]
    #[tokio::test]
    async fn album_membership_first_xmp_and_tracked_path_removal() {
        use base64::Engine as _;
        use sha2::{Digest, Sha256};
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};
        use xmp_toolkit::{XmpMeta, xmp_ns};

        let server = crate::start_wiremock_or_skip!();
        let body = vec![
            0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0x4A, 0x46, 0x49, 0x46, 0x00, 0x01, 0x01, 0x00,
            0x00, 0x01, 0x00, 0x01, 0x00, 0x00, 0xFF, 0xD9,
        ];
        Mock::given(method("GET"))
            .and(path("/album.jpg"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(body.clone()))
            .expect(1)
            .mount(&server)
            .await;
        let mut records = incremental_photo_records_with_url(
            "FIRST_ALBUM",
            "album.jpg",
            &format!("{}/album.jpg", server.uri()),
            body.len() as u64,
        );
        records[0]["fields"]["resOriginalRes"]["value"]["fileChecksum"] =
            json!(base64::engine::general_purpose::STANDARD.encode(Sha256::digest(&body)));
        let asset = PhotoAsset::new(records[0].clone(), records[1].clone());
        let dir = TempDir::new().unwrap();
        let db = Arc::new(
            SqliteStateDb::open(&dir.path().join("state.db"))
                .await
                .unwrap(),
        );
        let mut config = test_config();
        config.directory = Arc::from(dir.path().join("media"));
        config.album_name = Some(Arc::from("Trip"));
        config.metadata.xmp_sidecar = true;
        config.state_db = Some(db.clone());
        assert!(
            db.get_all_asset_albums("PrimarySync")
                .await
                .unwrap()
                .is_empty()
        );
        let first = stream_and_download_from_stream(
            &Client::new(),
            futures_util::stream::iter(vec![Ok(asset.clone())]),
            &Arc::new(config.clone()),
            DownloadControls::download_hidden(),
            1,
            CancellationToken::new(),
            StreamRuntime::new(None, None).deferring_metadata_drain(),
        )
        .await
        .unwrap();
        assert_eq!(first.downloaded, 1);
        assert_eq!(first.exif_failures, 0);
        let row = db.get_downloaded_page(0, 10).await.unwrap().remove(0);
        let media_path = row.local_path.as_ref().unwrap();
        let mut sidecar_name = media_path.file_name().unwrap().to_os_string();
        sidecar_name.push(".xmp");
        let sidecar_path = media_path.with_file_name(sidecar_name);
        let subjects = |contents: &str| {
            let xmp: XmpMeta = contents.parse().expect("parse actual sidecar");
            let mut names: Vec<_> = xmp
                .property_array(xmp_ns::DC, "subject")
                .map(|value| value.value)
                .collect();
            names.sort();
            names
        };
        let first_xmp = tokio::fs::read_to_string(&sidecar_path).await.unwrap();
        assert_eq!(
            subjects(&first_xmp),
            ["Trip"],
            "inspect before any rewrite drain"
        );
        assert_eq!(tokio::fs::read(media_path).await.unwrap(), body);

        seed_complete_album_snapshot(&db, "trip", "Trip", &[("asset-FIRST_ALBUM", "FIRST_ALBUM")])
            .await;
        seed_complete_album_snapshot(
            &db,
            "family",
            "Family",
            &[("asset-FIRST_ALBUM", "FIRST_ALBUM")],
        )
        .await;
        config.album_name = Some(Arc::from("Family"));
        let second = stream_and_download_from_stream(
            &Client::new(),
            futures_util::stream::iter(vec![Ok(asset)]),
            &Arc::new(config.clone()),
            DownloadControls::download_hidden(),
            1,
            CancellationToken::new(),
            StreamRuntime::new(None, None),
        )
        .await
        .unwrap();
        assert_eq!(second.downloaded, 0);
        assert_eq!(second.exif_failures, 0);
        assert_eq!(
            subjects(&tokio::fs::read_to_string(&sidecar_path).await.unwrap()),
            ["Family", "Trip"]
        );
        assert!(
            db.get_pending_metadata_rewrites(10)
                .await
                .unwrap()
                .is_empty()
        );

        config.album_name = None;
        config.recent = Some(10);
        let pass = AlbumPass {
            kind: PassKind::Unfiled,
            album: changes_album(
                "",
                changes_zone_session(
                    Arc::new(AtomicUsize::new(0)),
                    vec![relation_delete_record("trip", "asset-FIRST_ALBUM")],
                ),
            ),
            exclude_ids: Arc::new(FxHashSet::default()),
        };
        for _ in 0..2 {
            let removed = download_photos_incremental(
                &Client::new(),
                std::slice::from_ref(&pass),
                &Arc::new(config.clone()),
                "zone-token-prev",
                DownloadControls::download_hidden(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
            assert!(
                matches!(removed.outcome, DownloadOutcome::Success),
                "{removed:?}"
            );
            assert_eq!(removed.stats.downloaded, 0);
            assert_eq!(
                subjects(&tokio::fs::read_to_string(&sidecar_path).await.unwrap()),
                ["Family"]
            );
            assert_eq!(tokio::fs::read(media_path).await.unwrap(), body);
            assert!(
                db.get_pending_metadata_rewrites(10)
                    .await
                    .unwrap()
                    .is_empty()
            );
            let rows = db.get_downloaded_page(0, 10).await.unwrap();
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].local_path, row.local_path);
        }
    }

    #[tokio::test]
    async fn collecting_album_membership_failure_replays_without_redownloading() {
        let db = Arc::new(SqliteStateDb::open_in_memory().expect("state db"));
        let dir = TempDir::new().expect("temp dir");
        let records = incremental_photo_records("COLLECTED_GROUPING");
        let asset = PhotoAsset::new(records[0].clone(), records[1].clone());
        seed_complete_album_snapshot(
            db.as_ref(),
            "container-vacation",
            "Vacation",
            &[("asset-COLLECTED_GROUPING", "COLLECTED_GROUPING")],
        )
        .await;
        let change_calls = Arc::new(AtomicUsize::new(0));
        let pass = AlbumPass {
            kind: PassKind::Album,
            album: changes_album_with_container(
                "Vacation",
                Some("container-vacation"),
                changes_zone_session(Arc::clone(&change_calls), records),
            ),
            exclude_ids: Arc::new(FxHashSet::default()),
        };
        let mut config = test_config();
        config.directory = Arc::from(dir.path());
        config.recent = Some(10);
        config.state_db = Some(Arc::clone(&db) as Arc<dyn DownloadStore>);
        let media_path = seed_downloaded_metadata_asset(db.as_ref(), &config, &pass, &asset).await;
        let config = Arc::new(config);
        let media_before = tokio::fs::read(&media_path)
            .await
            .expect("read seeded media");

        db.fail_asset_album_writes_for_test();
        let failed = download_photos_incremental(
            &Client::new(),
            std::slice::from_ref(&pass),
            &config,
            "zone-token-prev",
            DownloadControls::download_hidden(),
            CancellationToken::new(),
        )
        .await
        .expect("membership failure should return a partial result");

        assert!(matches!(
            failed.outcome,
            DownloadOutcome::PartialFailure { failed_count: 1 }
        ));
        assert_eq!(failed.stats.state_write_failures, 1);
        assert_eq!(failed.stats.downloaded, 0);
        assert!(
            db.get_all_asset_albums("PrimarySync")
                .await
                .expect("read missing album membership")
                .is_empty(),
            "the injected failure must leave the compatibility relationship absent"
        );

        db.allow_asset_album_writes_for_test();
        for cycle in 2..=3 {
            let recovered = download_photos_incremental(
                &Client::new(),
                std::slice::from_ref(&pass),
                &config,
                "zone-token-prev",
                DownloadControls::download_hidden(),
                CancellationToken::new(),
            )
            .await
            .unwrap_or_else(|error| panic!("grouping replay cycle {cycle} failed: {error}"));

            assert!(matches!(recovered.outcome, DownloadOutcome::Success));
            assert_eq!(recovered.stats.state_write_failures, 0);
            assert_eq!(
                recovered.stats.downloaded, 0,
                "cycle {cycle} must not redownload landed media"
            );
            assert_eq!(
                db.get_all_asset_albums("PrimarySync")
                    .await
                    .expect("read recovered album membership"),
                vec![("COLLECTED_GROUPING".to_string(), "Vacation".to_string())],
                "cycle {cycle} must leave one idempotent relationship"
            );
        }
        assert_eq!(change_calls.load(Ordering::SeqCst), 3);
        assert_eq!(
            tokio::fs::read(&media_path)
                .await
                .expect("read stable media"),
            media_before,
            "grouping replay must not change landed media"
        );
    }

    #[tokio::test]
    async fn collecting_metadata_edit_refreshes_catalogue_without_duplicate_for_both_path_policies()
    {
        for policy in [
            FileMatchPolicy::NameSizeDedupWithSuffix,
            FileMatchPolicy::NameId7,
        ] {
            let db = Arc::new(SqliteStateDb::open_in_memory().expect("state db"));
            let dir = TempDir::new().expect("temp dir");
            let stored_records = incremental_photo_records_with_favorite("METADATA_EDIT", false);
            let stored_asset =
                PhotoAsset::new(stored_records[0].clone(), stored_records[1].clone());
            let changed_records = incremental_photo_records_with_favorite("METADATA_EDIT", true);
            let pass = AlbumPass {
                kind: PassKind::Unfiled,
                album: changes_album(
                    "",
                    changes_zone_session(Arc::new(AtomicUsize::new(0)), changed_records),
                ),
                exclude_ids: Arc::new(FxHashSet::default()),
            };
            let mut config = test_config();
            config.directory = Arc::from(dir.path());
            config.file_match_policy = policy;
            config.recent = Some(10);
            config.state_db = Some(Arc::clone(&db) as Arc<dyn DownloadStore>);
            let media_path =
                seed_downloaded_metadata_asset(db.as_ref(), &config, &pass, &stored_asset).await;

            let result = download_photos_incremental(
                &Client::new(),
                std::slice::from_ref(&pass),
                &Arc::new(config),
                "zone-token-prev",
                DownloadControls::download_hidden(),
                CancellationToken::new(),
            )
            .await
            .expect("metadata-only incremental sync should complete");

            assert!(matches!(result.outcome, DownloadOutcome::Success));
            assert_eq!(result.stats.downloaded, 0, "policy: {policy:?}");
            assert_eq!(result.sync_token.as_deref(), Some("zone-token-next"));
            let refreshed = db
                .get_downloaded_page(0, 1)
                .await
                .expect("read refreshed row")
                .remove(0);
            assert!(refreshed.metadata.is_favorite, "policy: {policy:?}");
            assert!(media_path.exists(), "policy: {policy:?}");
            assert_eq!(
                std::fs::read_dir(media_path.parent().expect("media parent"))
                    .expect("read media parent")
                    .count(),
                1,
                "metadata-only edit must not create a suffixed duplicate under {policy:?}"
            );
        }
    }

    #[cfg(feature = "xmp")]
    #[tokio::test]
    async fn collecting_unrouted_metadata_edit_refreshes_catalogue_and_queues_rewrite() {
        let db = Arc::new(SqliteStateDb::open_in_memory().expect("state db"));
        let dir = TempDir::new().expect("temp dir");
        let (passes, mut config, media_path) =
            relation_removed_metadata_edit_fixture(&db, &dir).await;
        config.metadata.xmp_sidecar = true;
        db.fail_metadata_marker_clear_for_test();

        let result = download_photos_incremental(
            &Client::new(),
            &passes,
            &Arc::new(config),
            "zone-token-prev",
            DownloadControls::download_hidden(),
            CancellationToken::new(),
        )
        .await
        .expect("unrouted metadata edit should remain durable");

        assert!(matches!(
            result.outcome,
            DownloadOutcome::PartialFailure { failed_count: 1 }
        ));
        assert_eq!(result.stats.downloaded, 0);
        assert_eq!(result.sync_token.as_deref(), Some("zone-token-next"));
        let refreshed = db
            .get_downloaded_page(0, 1)
            .await
            .expect("read refreshed row")
            .remove(0);
        assert!(refreshed.metadata.is_favorite);
        assert!(media_path.exists());
        let sidecar_name = format!(
            "{}.xmp",
            media_path
                .file_name()
                .expect("media path has filename")
                .to_string_lossy()
        );
        assert!(media_path.with_file_name(sidecar_name).exists());
        assert_eq!(
            db.get_pending_metadata_rewrites(10)
                .await
                .expect("read rewrite queue")
                .len(),
            1,
            "the configured rewrite marker must remain visible when clearing it fails"
        );
        assert!(
            db.get_live_selected_album_memberships_for_asset(
                "PrimarySync",
                "asset-ROUTED_METADATA",
                &["container-vacation"],
            )
            .await
            .expect("read album memberships")
            .is_empty(),
            "the relation removal must leave the changed asset unrouted"
        );
    }

    #[tokio::test]
    async fn collecting_unrouted_metadata_write_failure_preserves_incremental_checkpoint() {
        let db = Arc::new(SqliteStateDb::open_in_memory().expect("state db"));
        let dir = TempDir::new().expect("temp dir");
        let (passes, mut config, _) = relation_removed_metadata_edit_fixture(&db, &dir).await;
        config.metadata.set_exif_rating = true;
        db.fail_provider_metadata_refresh_for_test();

        let result = download_photos_incremental(
            &Client::new(),
            &passes,
            &Arc::new(config),
            "zone-token-prev",
            DownloadControls::download_hidden(),
            CancellationToken::new(),
        )
        .await
        .expect("failed unrouted metadata edit should return a safe partial result");

        assert!(matches!(
            result.outcome,
            DownloadOutcome::PartialFailure { failed_count: 2 }
        ));
        assert_eq!(result.stats.downloaded, 0);
        assert_eq!(result.sync_token, None);
        assert!(result.stats.sync_token_blocked);
        assert_eq!(
            result.stats.sync_token_blocked_reason,
            Some(PROVIDER_METADATA_STATE_WRITE_FAILED_REASON)
        );
        let unchanged = db
            .get_downloaded_page(0, 1)
            .await
            .expect("read unchanged row")
            .remove(0);
        assert!(!unchanged.metadata.is_favorite);
        let pending = db
            .get_pending_metadata_rewrites(10)
            .await
            .expect("read rewrite queue");
        assert_eq!(
            pending.len(),
            1,
            "the independent album removal must retain retry evidence"
        );
        assert!(
            !pending[0].metadata.is_favorite,
            "failed provider metadata must not leak into the rewrite"
        );
        assert!(
            db.get_all_asset_albums("PrimarySync")
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn collecting_unchanged_metadata_is_a_production_path_noop() {
        let db = Arc::new(SqliteStateDb::open_in_memory().expect("state db"));
        let dir = TempDir::new().expect("temp dir");
        let records = incremental_photo_records_with_favorite("METADATA_UNCHANGED", false);
        let stored_asset = PhotoAsset::new(records[0].clone(), records[1].clone());
        let pass = AlbumPass {
            kind: PassKind::Unfiled,
            album: changes_album(
                "",
                changes_zone_session(Arc::new(AtomicUsize::new(0)), records),
            ),
            exclude_ids: Arc::new(FxHashSet::default()),
        };
        let mut config = test_config();
        config.directory = Arc::from(dir.path());
        config.recent = Some(10);
        config.state_db = Some(Arc::clone(&db) as Arc<dyn DownloadStore>);
        let media_path =
            seed_downloaded_metadata_asset(db.as_ref(), &config, &pass, &stored_asset).await;
        db.fail_provider_metadata_refresh_for_test();

        let result = download_photos_incremental(
            &Client::new(),
            std::slice::from_ref(&pass),
            &Arc::new(config),
            "zone-token-prev",
            DownloadControls::download_hidden(),
            CancellationToken::new(),
        )
        .await
        .expect("unchanged metadata should not call the failing refresh operation");

        assert!(matches!(result.outcome, DownloadOutcome::Success));
        assert_eq!(result.stats.downloaded, 0);
        assert_eq!(result.stats.state_write_failures, 0);
        assert_eq!(result.sync_token.as_deref(), Some("zone-token-next"));
        assert!(media_path.exists());
        assert!(
            db.get_pending_metadata_rewrites(10)
                .await
                .expect("read rewrite queue")
                .is_empty()
        );
    }

    #[tokio::test]
    async fn asset_only_delta_recovers_identity_refreshes_metadata_and_advances_token() {
        #[derive(Debug, Clone, Copy)]
        enum IdentitySource {
            Delta,
            State,
            ProviderLookup,
        }

        for identity_source in [
            IdentitySource::Delta,
            IdentitySource::State,
            IdentitySource::ProviderLookup,
        ] {
            let db = Arc::new(SqliteStateDb::open_in_memory().expect("state db"));
            let dir = TempDir::new().expect("temp dir");
            let stored_records = incremental_photo_records_with_favorite("ASSET_ONLY", false);
            let stored_asset =
                PhotoAsset::new(stored_records[0].clone(), stored_records[1].clone());
            let changed_records = incremental_photo_records_with_favorite("ASSET_ONLY", true);
            let mut delta_record = changed_records[1].clone();
            if !matches!(identity_source, IdentitySource::Delta) {
                delta_record["fields"]
                    .as_object_mut()
                    .expect("asset fields")
                    .remove("masterRef");
            }
            if matches!(identity_source, IdentitySource::State) {
                db.upsert_asset_master_mapping("PrimarySync", "asset-ASSET_ONLY", "ASSET_ONLY")
                    .await
                    .expect("seed durable mapping");
            }
            let session = changes_zone_session_with_query_page(
                Arc::new(AtomicUsize::new(0)),
                vec![delta_record],
                json!({"records": changed_records}),
                0,
            );
            let session_probe = session.clone();
            let pass = AlbumPass {
                kind: PassKind::Unfiled,
                album: changes_album("", session),
                exclude_ids: Arc::new(FxHashSet::default()),
            };
            let mut config = test_config();
            config.directory = Arc::from(dir.path());
            config.state_db = Some(Arc::clone(&db) as Arc<dyn DownloadStore>);
            let media_path =
                seed_downloaded_metadata_asset(db.as_ref(), &config, &pass, &stored_asset).await;

            let result = download_photos_incremental(
                &Client::new(),
                std::slice::from_ref(&pass),
                &Arc::new(config),
                "zone-token-prev",
                DownloadControls::download_hidden(),
                CancellationToken::new(),
            )
            .await
            .expect("asset-only delta should hydrate");

            assert!(matches!(result.outcome, DownloadOutcome::Success));
            assert_eq!(result.stats.downloaded, 0, "{identity_source:?}");
            assert_eq!(result.sync_token.as_deref(), Some("zone-token-next"));
            assert!(!result.stats.sync_token_blocked);
            assert_eq!(
                session_probe.records_query_count(),
                match identity_source {
                    IdentitySource::ProviderLookup => 2,
                    IdentitySource::Delta | IdentitySource::State => 1,
                },
                "{identity_source:?}"
            );
            assert_eq!(
                db.get_master_record_name_for_asset("PrimarySync", "asset-ASSET_ONLY")
                    .await
                    .expect("read durable mapping")
                    .as_deref(),
                Some("ASSET_ONLY"),
                "{identity_source:?}"
            );
            let refreshed = db
                .get_downloaded_page(0, 1)
                .await
                .expect("read refreshed row")
                .remove(0);
            assert!(refreshed.metadata.is_favorite);
            assert!(media_path.exists());
        }
    }

    #[tokio::test]
    async fn unresolved_asset_only_delta_preserves_incremental_token() {
        let records = incremental_photo_records_with_favorite("ASSET_UNKNOWN", true);
        let session = changes_zone_session_with_query_page(
            Arc::new(AtomicUsize::new(0)),
            vec![records[1].clone()],
            json!({"records": []}),
            0,
        );
        let pass = AlbumPass {
            kind: PassKind::Unfiled,
            album: changes_album("", session),
            exclude_ids: Arc::new(FxHashSet::default()),
        };
        let mut config = test_config();
        config.state_db = Some(Arc::new(SqliteStateDb::open_in_memory().expect("state db")));

        let result = download_photos_incremental(
            &Client::new(),
            std::slice::from_ref(&pass),
            &Arc::new(config),
            "zone-token-prev",
            DownloadControls::download_hidden(),
            CancellationToken::new(),
        )
        .await
        .expect("inconclusive hydration should remain a safe partial sync");

        assert_eq!(result.sync_token, None);
        assert!(result.stats.sync_token_blocked);
        assert_eq!(
            result.stats.sync_token_blocked_reason,
            Some(ASSET_DELTA_HYDRATION_INCOMPLETE_REASON)
        );
    }

    #[tokio::test]
    async fn asset_only_delta_master_deletion_tombstones_entire_family() {
        let db = Arc::new(SqliteStateDb::open_in_memory().expect("state db"));
        for record_name in [
            "ASSET_ONLY_DELETED",
            "asset-ASSET_ONLY_DELETED",
            "asset-ASSET_ONLY_DELETED-SIBLING",
        ] {
            db.upsert_seen(&TestAssetRecord::new(record_name).build())
                .await
                .expect("seed family state row");
        }
        for asset_record_name in [
            "asset-ASSET_ONLY_DELETED",
            "asset-ASSET_ONLY_DELETED-SIBLING",
        ] {
            db.upsert_asset_master_mapping("PrimarySync", asset_record_name, "ASSET_ONLY_DELETED")
                .await
                .expect("seed family mapping");
        }

        let records = incremental_photo_records("ASSET_ONLY_DELETED");
        let session = changes_zone_session_with_query_page(
            Arc::new(AtomicUsize::new(0)),
            vec![records[1].clone()],
            json!({
                "records": [{
                    "recordName": "ASSET_ONLY_DELETED",
                    "serverErrorCode": "UNKNOWN_ITEM",
                    "reason": "record not found"
                }]
            }),
            0,
        );
        let pass = AlbumPass {
            kind: PassKind::Unfiled,
            album: changes_album("", session),
            exclude_ids: Arc::new(FxHashSet::default()),
        };
        let mut config = test_config();
        config.state_db = Some(Arc::clone(&db) as Arc<dyn DownloadStore>);

        let result = download_photos_incremental(
            &Client::new(),
            std::slice::from_ref(&pass),
            &Arc::new(config),
            "zone-token-prev",
            DownloadControls::download_hidden(),
            CancellationToken::new(),
        )
        .await
        .expect("master deletion should resolve the complete family");

        assert!(matches!(result.outcome, DownloadOutcome::Success));
        assert_eq!(result.sync_token.as_deref(), Some("zone-token-next"));
        assert!(!result.stats.sync_token_blocked);
        let summary = db.get_summary().await.expect("read state summary");
        assert_eq!(summary.source_deleted, 3);
        assert_eq!(summary.pending, 0);
    }

    #[cfg(feature = "xmp")]
    #[tokio::test]
    async fn collecting_zero_download_cycle_drains_metadata_rewrite_batch() {
        let db = Arc::new(SqliteStateDb::open_in_memory().expect("state db"));
        let dir = TempDir::new().expect("temp dir");
        let media_path = dir.path().join("rewrite-only.jpg");
        tokio::fs::write(&media_path, b"existing media")
            .await
            .expect("seed media");
        let record = TestAssetRecord::new("REWRITE_ONLY")
            .filename("rewrite-only.jpg")
            .checksum("provider-checksum")
            .size(14)
            .build();
        db.upsert_seen(&record).await.expect("seed state row");
        db.mark_downloaded(
            "PrimarySync",
            "REWRITE_ONLY",
            "original",
            &media_path,
            "local-checksum",
            None,
        )
        .await
        .expect("mark downloaded");
        db.record_metadata_write_failure("PrimarySync", "REWRITE_ONLY", "original")
            .await
            .expect("queue rewrite");

        let pass = AlbumPass {
            kind: PassKind::Unfiled,
            album: changes_album(
                "",
                changes_zone_session(Arc::new(AtomicUsize::new(0)), Vec::new()),
            ),
            exclude_ids: Arc::new(FxHashSet::default()),
        };
        let mut config = test_config();
        config.directory = Arc::from(dir.path());
        config.recent = Some(10);
        config.metadata.xmp_sidecar = true;
        config.state_db = Some(Arc::clone(&db) as Arc<dyn DownloadStore>);

        let result = download_photos_incremental(
            &Client::new(),
            std::slice::from_ref(&pass),
            &Arc::new(config),
            "zone-token-prev",
            DownloadControls::download_hidden(),
            CancellationToken::new(),
        )
        .await
        .expect("rewrite-only collecting cycle should complete");

        assert!(matches!(result.outcome, DownloadOutcome::Success));
        assert_eq!(result.stats.downloaded, 0);
        assert_eq!(result.sync_token.as_deref(), Some("zone-token-next"));
        assert!(media_path.with_file_name("rewrite-only.jpg.xmp").exists());
        assert!(
            db.get_pending_metadata_rewrites(10)
                .await
                .expect("read rewrite queue")
                .is_empty()
        );
    }

    #[cfg(feature = "xmp")]
    #[tokio::test]
    async fn collecting_rewrite_failure_retains_marker_and_advances_durable_checkpoint() {
        let db = Arc::new(SqliteStateDb::open_in_memory().expect("state db"));
        let dir = TempDir::new().expect("temp dir");
        let media_path = dir.path().join("rewrite-retry.jpg");
        tokio::fs::write(&media_path, b"existing media")
            .await
            .expect("seed media");
        let mut metadata = crate::state::AssetMetadata {
            title: Some("fresh catalogue title".to_string()),
            ..crate::state::AssetMetadata::default()
        };
        metadata.refresh_hash();
        let record = TestAssetRecord::new("REWRITE_RETRY")
            .filename("rewrite-retry.jpg")
            .checksum("provider-checksum")
            .size(14)
            .metadata(metadata)
            .build();
        db.upsert_seen(&record).await.expect("seed state row");
        db.mark_downloaded(
            "PrimarySync",
            "REWRITE_RETRY",
            "original",
            &media_path,
            "local-checksum",
            None,
        )
        .await
        .expect("mark downloaded");
        db.record_metadata_write_failure("PrimarySync", "REWRITE_RETRY", "original")
            .await
            .expect("queue rewrite");
        db.fail_metadata_marker_clear_for_test();

        let pass = AlbumPass {
            kind: PassKind::Unfiled,
            album: changes_album(
                "",
                changes_zone_session(Arc::new(AtomicUsize::new(0)), Vec::new()),
            ),
            exclude_ids: Arc::new(FxHashSet::default()),
        };
        let mut config = test_config();
        config.directory = Arc::from(dir.path());
        config.recent = Some(10);
        config.metadata.xmp_sidecar = true;
        config.state_db = Some(Arc::clone(&db) as Arc<dyn DownloadStore>);

        let result = download_photos_incremental(
            &Client::new(),
            std::slice::from_ref(&pass),
            &Arc::new(config),
            "zone-token-prev",
            DownloadControls::download_hidden(),
            CancellationToken::new(),
        )
        .await
        .expect("rewrite failure should remain a durable partial result");

        assert!(matches!(
            result.outcome,
            DownloadOutcome::PartialFailure { failed_count: 1 }
        ));
        assert_eq!(result.stats.downloaded, 0);
        assert_eq!(result.stats.exif_failures, 1);
        assert_eq!(result.sync_token.as_deref(), Some("zone-token-next"));
        assert!(media_path.with_file_name("rewrite-retry.jpg.xmp").exists());
        let pending = db
            .get_pending_metadata_rewrites(10)
            .await
            .expect("read rewrite queue");
        assert_eq!(pending.len(), 1);
        assert_eq!(
            pending[0].metadata.title.as_deref(),
            Some("fresh catalogue title"),
            "the retained marker must reference the durable fresh catalogue state"
        );
    }

    #[tokio::test]
    async fn incremental_changed_asset_in_selected_album_routes_to_album_path() {
        let db = Arc::new(SqliteStateDb::open_in_memory().expect("state db"));
        seed_complete_album_snapshot(
            &db,
            "container-vacation",
            "Vacation",
            &[("asset-MASTER_CHANGED", "MASTER_CHANGED")],
        )
        .await;
        let calls = Arc::new(AtomicUsize::new(0));
        let session = changes_zone_session(
            Arc::clone(&calls),
            incremental_photo_records("MASTER_CHANGED"),
        );
        let passes = vec![
            AlbumPass {
                kind: PassKind::Album,
                album: changes_album_with_container(
                    "Vacation",
                    Some("container-vacation"),
                    session,
                ),
                exclude_ids: Arc::new(FxHashSet::default()),
            },
            unused_unfiled_changes_pass(),
        ];

        let mut config = test_config();
        let dir = TempDir::new().expect("temp dir");
        config.directory = Arc::from(dir.path());
        config.folder_structure = "Unfiled".to_string();
        config.folder_structure_albums = Arc::from("{album}");
        config.state_db = Some(Arc::clone(&db) as Arc<dyn DownloadStore>);
        let result = download_photos_incremental(
            &Client::new(),
            &passes,
            &Arc::new(config),
            "zone-token-prev",
            DownloadControls::new(DownloadRunMode::PrintFilenames, DownloadReporting::hidden()),
            CancellationToken::new(),
        )
        .await
        .expect("album-routed incremental sync should succeed");

        assert!(matches!(result.outcome, DownloadOutcome::Success));
        let album_rows = db.get_all_asset_albums("PrimarySync").await.unwrap();
        assert_eq!(
            album_rows,
            vec![("asset-MASTER_CHANGED".to_string(), "Vacation".to_string())],
            "selected album asset should route through the album pass"
        );
    }

    #[tokio::test]
    async fn incremental_changed_asset_without_selected_album_routes_to_unfiled() {
        let db = Arc::new(SqliteStateDb::open_in_memory().expect("state db"));
        seed_complete_album_snapshot(&db, "container-vacation", "Vacation", &[]).await;
        let calls = Arc::new(AtomicUsize::new(0));
        let session = changes_zone_session(
            Arc::clone(&calls),
            incremental_photo_records("MASTER_CHANGED"),
        );
        let passes = vec![
            AlbumPass {
                kind: PassKind::Album,
                album: changes_album_with_container(
                    "Vacation",
                    Some("container-vacation"),
                    session,
                ),
                exclude_ids: Arc::new(FxHashSet::default()),
            },
            unused_unfiled_changes_pass(),
        ];

        let mut config = test_config();
        let dir = TempDir::new().expect("temp dir");
        config.directory = Arc::from(dir.path());
        config.folder_structure = "Unfiled".to_string();
        config.folder_structure_albums = Arc::from("{album}");
        config.state_db = Some(Arc::clone(&db) as Arc<dyn DownloadStore>);
        let result = download_photos_incremental(
            &Client::new(),
            &passes,
            &Arc::new(config),
            "zone-token-prev",
            DownloadControls::new(DownloadRunMode::PrintFilenames, DownloadReporting::hidden()),
            CancellationToken::new(),
        )
        .await
        .expect("unfiled-routed incremental sync should succeed");

        assert!(matches!(result.outcome, DownloadOutcome::Success));
        let album_rows = db.get_all_asset_albums("PrimarySync").await.unwrap();
        assert!(
            album_rows.is_empty(),
            "asset outside selected albums should route only through the unfiled pass"
        );
    }

    #[tokio::test]
    async fn incremental_multi_album_unfiled_uses_membership_without_album_enumeration() {
        let db = Arc::new(SqliteStateDb::open_in_memory().expect("state db"));
        seed_complete_album_snapshot(&db, "container-vacation", "Vacation", &[]).await;
        let changes_calls = Arc::new(AtomicUsize::new(0));
        let session = changes_zone_session_with_query_page(
            Arc::clone(&changes_calls),
            incremental_photo_records("MASTER_CHANGED"),
            mock_photo_query_page("VACATION_EXISTING", Some("album-token")),
            1,
        );
        let passes = vec![
            AlbumPass {
                kind: PassKind::Album,
                album: changes_album_with_container(
                    "Vacation",
                    Some("container-vacation"),
                    session.clone(),
                ),
                exclude_ids: Arc::new(FxHashSet::default()),
            },
            unused_unfiled_changes_pass(),
        ];

        let mut config = test_config();
        let dir = TempDir::new().expect("temp dir");
        config.directory = Arc::from(dir.path());
        config.folder_structure = "Unfiled".to_string();
        config.folder_structure_albums = Arc::from("{album}");
        config.state_db = Some(Arc::clone(&db) as Arc<dyn DownloadStore>);

        let result = download_photos_incremental(
            &Client::new(),
            &passes,
            &Arc::new(config),
            "zone-token-prev",
            DownloadControls::new(DownloadRunMode::PrintFilenames, DownloadReporting::hidden()),
            CancellationToken::new(),
        )
        .await
        .expect("incremental routing should not enumerate album passes");

        assert!(matches!(result.outcome, DownloadOutcome::Success));
        assert_eq!(
            changes_calls.load(Ordering::SeqCst),
            1,
            "incremental sync should still read the zone delta once"
        );
        assert_eq!(
            session.count_query_count(),
            0,
            "membership-backed incremental routing must not count albums before planning downloads"
        );
        assert_eq!(
            session.records_query_count(),
            0,
            "membership-backed incremental routing must not enumerate albums before planning downloads"
        );
    }

    #[tokio::test]
    async fn incremental_expired_urls_are_refreshed_and_retried_same_cycle() {
        use base64::Engine as _;
        use sha2::{Digest, Sha256};
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};

        let server = crate::start_wiremock_or_skip!();
        Mock::given(method("GET"))
            .and(path("/expired-url-1.jpg"))
            .respond_with(ResponseTemplate::new(410))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/expired-url-2.jpg"))
            .respond_with(ResponseTemplate::new(410))
            .mount(&server)
            .await;

        let body = vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0x4A, 0x46];
        let checksum = base64::engine::general_purpose::STANDARD.encode(Sha256::digest(&body));
        Mock::given(method("GET"))
            .and(path("/fresh-url-1.jpg"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_bytes(body.clone())
                    .insert_header("content-type", "image/jpeg"),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/fresh-url-2.jpg"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_bytes(body)
                    .insert_header("content-type", "image/jpeg"),
            )
            .mount(&server)
            .await;

        let stale_url_1 = format!("{}/expired-url-1.jpg", server.uri());
        let stale_url_2 = format!("{}/expired-url-2.jpg", server.uri());
        let fresh_url_1 = format!("{}/fresh-url-1.jpg", server.uri());
        let fresh_url_2 = format!("{}/fresh-url-2.jpg", server.uri());
        let mut stale_records =
            incremental_photo_records_with_url("URL_AGED_ONE", "url-aged-1.jpg", &stale_url_1, 8);
        stale_records[0]["fields"]["resOriginalRes"]["value"]["fileChecksum"] =
            json!(checksum.clone());
        let mut second_stale =
            incremental_photo_records_with_url("URL_AGED_TWO", "url-aged-2.jpg", &stale_url_2, 8);
        second_stale[0]["fields"]["resOriginalRes"]["value"]["fileChecksum"] =
            json!(checksum.clone());
        stale_records.extend(second_stale);

        let mut fresh_records =
            incremental_photo_records_with_url("URL_AGED_ONE", "url-aged-1.jpg", &fresh_url_1, 8);
        fresh_records[0]["fields"]["resOriginalRes"]["value"]["fileChecksum"] =
            json!(checksum.clone());
        let mut second_fresh =
            incremental_photo_records_with_url("URL_AGED_TWO", "url-aged-2.jpg", &fresh_url_2, 8);
        second_fresh[0]["fields"]["resOriginalRes"]["value"]["fileChecksum"] = json!(checksum);
        fresh_records.extend(second_fresh);

        let db = Arc::new(SqliteStateDb::open_in_memory().expect("state db"));
        seed_complete_album_snapshot(&db, "container-vacation", "Vacation", &[]).await;
        let album_session = SharedChangesZoneSession::new(vec![
            changes_zone_response(stale_records, "zone-token-next"),
            changes_zone_response(fresh_records, "zone-token-next"),
        ]);
        let passes = vec![
            AlbumPass {
                kind: PassKind::Album,
                album: album_with_session_and_container(
                    "PrimarySync",
                    "Vacation",
                    Some("container-vacation"),
                    Box::new(album_session),
                ),
                exclude_ids: Arc::new(FxHashSet::default()),
            },
            AlbumPass {
                kind: PassKind::Unfiled,
                album: mock_album("", MockPhotosFlow::new().build()),
                exclude_ids: Arc::new(FxHashSet::default()),
            },
        ];

        let mut config = test_config();
        let dir = TempDir::new().expect("temp dir");
        config.directory = Arc::from(dir.path());
        config.concurrent_downloads = 1;
        config.state_db = Some(db.clone());
        config.sync_mode = SyncMode::Incremental {
            zone_sync_token: "zone-token-prev".to_string(),
        };

        let result = download_photos_with_sync(
            &Client::new(),
            &passes,
            Arc::new(config),
            DownloadControls::download_hidden(),
            CancellationToken::new(),
        )
        .await
        .expect("expired incremental URLs should be refreshed and retried");

        assert!(matches!(result.outcome, DownloadOutcome::Success));
        assert_eq!(result.stats.downloaded, 2);
        assert_eq!(result.stats.failed, 0);
        assert!(
            !result.stats.interrupted,
            "a recovered expired URL should not leave the cycle interrupted"
        );
        assert_eq!(result.sync_token.as_deref(), Some("zone-token-next"));
        let summary = db.get_summary().await.expect("summary");
        assert_eq!(summary.downloaded, 2);
        assert_eq!(summary.failed, 0);
    }

    #[tokio::test]
    async fn incremental_preflight_refreshes_aged_urls_before_first_download() {
        use base64::Engine as _;
        use sha2::{Digest, Sha256};
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};

        let server = crate::start_wiremock_or_skip!();
        Mock::given(method("GET"))
            .and(path("/expired-before-download.jpg"))
            .respond_with(ResponseTemplate::new(410))
            .expect(0)
            .mount(&server)
            .await;

        let body = vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0x4A, 0x46];
        let checksum = base64::engine::general_purpose::STANDARD.encode(Sha256::digest(&body));
        Mock::given(method("GET"))
            .and(path("/fresh-before-download.jpg"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_bytes(body)
                    .insert_header("content-type", "image/jpeg"),
            )
            .expect(1)
            .mount(&server)
            .await;

        let stale_url = format!("{}/expired-before-download.jpg", server.uri());
        let fresh_url = format!("{}/fresh-before-download.jpg", server.uri());
        let mut stale_records =
            incremental_photo_records_with_url("URL_PREFLIGHT", "preflight.jpg", &stale_url, 8);
        stale_records[0]["fields"]["resOriginalRes"]["value"]["fileChecksum"] =
            json!(checksum.clone());
        let mut fresh_records =
            incremental_photo_records_with_url("URL_PREFLIGHT", "preflight.jpg", &fresh_url, 8);
        fresh_records[0]["fields"]["resOriginalRes"]["value"]["fileChecksum"] = json!(checksum);

        let session = SplitChangesZoneSession::new(
            vec![changes_zone_response(stale_records, "zone-token-next")],
            vec![changes_zone_response(fresh_records, "zone-token-next")],
        );
        let passes = vec![AlbumPass {
            kind: PassKind::Unfiled,
            album: album_with_session("PrimarySync", "", Box::new(session)),
            exclude_ids: Arc::new(FxHashSet::default()),
        }];

        let db = Arc::new(SqliteStateDb::open_in_memory().expect("state db"));
        let mut config = test_config();
        let dir = TempDir::new().expect("temp dir");
        config.directory = Arc::from(dir.path());
        config.concurrent_downloads = 1;
        config.state_db = Some(db.clone());

        let result = download_photos_incremental_collecting_inner(
            &Client::new(),
            &passes,
            &Arc::new(config),
            "zone-token-prev",
            DownloadControls::download_hidden(),
            CancellationToken::new(),
            Duration::ZERO,
        )
        .await
        .expect("preflight refresh should replace aged URLs before first download");

        assert!(matches!(result.outcome, DownloadOutcome::Success));
        assert_eq!(result.stats.downloaded, 1);
        assert_eq!(result.stats.failed, 0);
        assert_eq!(result.sync_token.as_deref(), Some("zone-token-next"));
        let summary = db.get_summary().await.expect("summary");
        assert_eq!(summary.downloaded, 1);
        assert_eq!(summary.failed, 0);
    }

    #[tokio::test]
    async fn incremental_sync_skips_smaller_metadata_rewritten_file() {
        let mut records = incremental_photo_records_with_url(
            "INCREMENTAL_METADATA_REWRITTEN",
            "incremental-rewritten.jpg",
            "https://p01.icloud-content.com/incremental-rewritten.jpg",
            8,
        );
        records[0]["fields"]["resOriginalRes"]["value"]["fileChecksum"] =
            json!("ck_incremental_metadata_rewritten");
        let asset = PhotoAsset::new(records[0].clone(), records[1].clone());

        let db = Arc::new(SqliteStateDb::open_in_memory().expect("state db"));
        db.upsert_asset_master_mapping("PrimarySync", asset.asset_record_name(), asset.id())
            .await
            .expect("seed asset/master mapping");
        let dir = TempDir::new().expect("temp dir");
        let mut config = test_config();
        config.directory = Arc::from(dir.path());
        config.state_db = Some(db.clone());
        let target_path = filter::expected_paths_for(&asset, &config)
            .into_iter()
            .next()
            .expect("asset should derive a path")
            .path;
        tokio::fs::create_dir_all(target_path.parent().expect("target parent"))
            .await
            .expect("create target parent");
        tokio::fs::write(&target_path, b"shorter")
            .await
            .expect("seed metadata-rewritten file");
        let local_checksum = file::compute_sha256(&target_path)
            .await
            .expect("hash metadata-rewritten file");
        let state_id = asset.asset_record_name();
        let state_record = TestAssetRecord::new(state_id)
            .filename("incremental-rewritten.jpg")
            .checksum("ck_incremental_metadata_rewritten")
            .size(8)
            .build();
        db.upsert_seen(&state_record)
            .await
            .expect("seed downloaded row");
        db.mark_downloaded(
            "PrimarySync",
            state_id,
            VersionSizeKey::Original.as_str(),
            &target_path,
            &local_checksum,
            Some("download-checksum-before-metadata"),
        )
        .await
        .expect("seed metadata-rewritten state");

        let passes = vec![AlbumPass {
            kind: PassKind::Unfiled,
            album: mock_album(
                "Library",
                MockPhotosFlow::new()
                    .changes_zone_page(records, "zone-token-next", false)
                    .build(),
            ),
            exclude_ids: Arc::new(FxHashSet::default()),
        }];
        let result = download_photos_incremental_collecting_inner(
            &Client::new(),
            &passes,
            &Arc::new(config),
            "zone-token-prev",
            DownloadControls::download_hidden(),
            CancellationToken::new(),
            Duration::ZERO,
        )
        .await
        .expect("incremental sync should skip the verified local file");

        assert!(matches!(result.outcome, DownloadOutcome::Success));
        assert_eq!(result.stats.downloaded, 0);
        assert_eq!(result.stats.failed, 0);
        assert_eq!(result.sync_token.as_deref(), Some("zone-token-next"));
        assert_eq!(tokio::fs::read(&target_path).await.unwrap(), b"shorter");
        assert!(
            !target_path
                .with_file_name("incremental-rewritten-8.jpg")
                .exists(),
            "incremental sync must not create a size-suffixed duplicate"
        );
    }

    #[tokio::test]
    async fn incremental_collecting_paired_asset_preserves_child_state_identity() {
        use base64::Engine as _;
        use sha2::{Digest, Sha256};
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};

        let server = crate::start_wiremock_or_skip!();
        let original_body = vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0x4A, 0x46];
        let changed_body = vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0x4A, 0x46, 0x00];
        Mock::given(method("GET"))
            .and(path("/collecting-original.jpg"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_bytes(original_body.clone())
                    .insert_header("content-type", "image/jpeg"),
            )
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/collecting-changed.jpg"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_bytes(changed_body.clone())
                    .insert_header("content-type", "image/jpeg"),
            )
            .expect(1)
            .mount(&server)
            .await;

        let original_url = format!("{}/collecting-original.jpg", server.uri());
        let changed_url = format!("{}/collecting-changed.jpg", server.uri());
        let mut original_records = incremental_photo_records_with_url(
            "COLLECTING_IDENTITY",
            "collecting.jpg",
            &original_url,
            original_body.len() as u64,
        );
        original_records[0]["fields"]["resOriginalRes"]["value"]["fileChecksum"] =
            json!(base64::engine::general_purpose::STANDARD.encode(Sha256::digest(&original_body)));
        let mut changed_records = incremental_photo_records_with_url(
            "COLLECTING_IDENTITY",
            "collecting.jpg",
            &changed_url,
            changed_body.len() as u64,
        );
        changed_records[0]["fields"]["resOriginalRes"]["value"]["fileChecksum"] =
            json!(base64::engine::general_purpose::STANDARD.encode(Sha256::digest(&changed_body)));

        let full_passes = vec![AlbumPass {
            kind: PassKind::Unfiled,
            album: mock_album(
                "Library",
                MockPhotosFlow::new()
                    .album_count(1)
                    .query_page(original_records, Some("zone-token-full"))
                    .build(),
            ),
            exclude_ids: Arc::new(FxHashSet::default()),
        }];
        let db = Arc::new(SqliteStateDb::open_in_memory().expect("state db"));
        let dir = TempDir::new().expect("temp dir");
        let mut config = test_config();
        config.directory = Arc::from(dir.path());
        config.concurrent_downloads = 1;
        config.state_db = Some(db.clone());

        let full = download_photos_with_sync(
            &Client::new(),
            &full_passes,
            Arc::new(config.clone()),
            DownloadControls::download_hidden(),
            CancellationToken::new(),
        )
        .await
        .expect("full sync should store the child identity");
        assert!(matches!(full.outcome, DownloadOutcome::Success));
        assert_eq!(full.stats.downloaded, 1);
        let full_rows = db.get_downloaded_page(0, 10).await.expect("full state row");
        assert_eq!(full_rows.len(), 1);
        assert_eq!(full_rows[0].id.as_ref(), "asset-COLLECTING_IDENTITY");
        let original_path = full_rows[0]
            .local_path
            .clone()
            .expect("full state row has a local path");

        let incremental_passes = vec![AlbumPass {
            kind: PassKind::Unfiled,
            album: mock_album(
                "Library",
                MockPhotosFlow::new()
                    .changes_zone_page(changed_records, "zone-token-next", false)
                    .build(),
            ),
            exclude_ids: Arc::new(FxHashSet::default()),
        }];
        config.recent = Some(10);
        config.sync_mode = SyncMode::Incremental {
            zone_sync_token: "zone-token-full".to_string(),
        };

        let incremental = download_photos_with_sync(
            &Client::new(),
            &incremental_passes,
            Arc::new(config),
            DownloadControls::download_hidden(),
            CancellationToken::new(),
        )
        .await
        .expect("collecting incremental sync should keep the child identity");

        assert!(matches!(incremental.outcome, DownloadOutcome::Success));
        assert_eq!(incremental.stats.downloaded, 1);
        assert_eq!(incremental.stats.failed, 0);
        assert_eq!(incremental.sync_token.as_deref(), Some("zone-token-next"));
        let downloaded = db.get_downloaded_page(0, 10).await.expect("downloaded row");
        assert_eq!(downloaded.len(), 1);
        assert_eq!(downloaded[0].id.as_ref(), "asset-COLLECTING_IDENTITY");
        assert_ne!(
            downloaded[0].local_path.as_deref(),
            Some(original_path.as_path())
        );
        assert!(
            original_path.exists(),
            "the prior provider bytes must remain on disk"
        );
    }

    #[tokio::test]
    async fn incremental_asset_only_expired_url_retry_preserves_child_state_identity() {
        use base64::Engine as _;
        use sha2::{Digest, Sha256};
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};

        let server = crate::start_wiremock_or_skip!();
        Mock::given(method("GET"))
            .and(path("/expired-asset-only.jpg"))
            .respond_with(ResponseTemplate::new(410))
            .expect(1)
            .mount(&server)
            .await;

        let body = vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0x4A, 0x46];
        let checksum = base64::engine::general_purpose::STANDARD.encode(Sha256::digest(&body));
        Mock::given(method("GET"))
            .and(path("/fresh-asset-only.jpg"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_bytes(body)
                    .insert_header("content-type", "image/jpeg"),
            )
            .expect(1)
            .mount(&server)
            .await;

        let stale_url = format!("{}/expired-asset-only.jpg", server.uri());
        let fresh_url = format!("{}/fresh-asset-only.jpg", server.uri());
        let mut stale_records =
            incremental_photo_records_with_url("URL_ASSET_ONLY", "asset-only.jpg", &stale_url, 8);
        stale_records[0]["fields"]["resOriginalRes"]["value"]["fileChecksum"] =
            json!(checksum.clone());
        let mut fresh_records =
            incremental_photo_records_with_url("URL_ASSET_ONLY", "asset-only.jpg", &fresh_url, 8);
        fresh_records[0]["fields"]["resOriginalRes"]["value"]["fileChecksum"] = json!(checksum);

        let asset_only_delta = stale_records[1].clone();
        let session = AssetOnlyExpiredUrlSession {
            delta_records: Arc::new(vec![asset_only_delta]),
            lookup_records: Arc::new(stale_records),
            hydration_records: Arc::new(fresh_records),
        };
        let passes = vec![AlbumPass {
            kind: PassKind::Unfiled,
            album: album_with_session("PrimarySync", "", Box::new(session)),
            exclude_ids: Arc::new(FxHashSet::default()),
        }];

        let db = Arc::new(SqliteStateDb::open_in_memory().expect("state db"));
        let mut config = test_config();
        let dir = TempDir::new().expect("temp dir");
        config.directory = Arc::from(dir.path());
        config.concurrent_downloads = 1;
        config.recent = Some(10);
        config.state_db = Some(db.clone());
        config.sync_mode = SyncMode::Incremental {
            zone_sync_token: "zone-token-prev".to_string(),
        };

        let result = download_photos_with_sync(
            &Client::new(),
            &passes,
            Arc::new(config),
            DownloadControls::download_hidden(),
            CancellationToken::new(),
        )
        .await
        .expect("asset-only retry should keep its child state identity");

        assert!(matches!(result.outcome, DownloadOutcome::Success));
        assert_eq!(result.stats.downloaded, 1);
        assert_eq!(result.stats.failed, 0);
        assert_eq!(result.sync_token.as_deref(), Some("zone-token-next"));
        let downloaded = db.get_downloaded_page(0, 10).await.expect("downloaded row");
        assert_eq!(downloaded.len(), 1);
        assert_eq!(downloaded[0].id.as_ref(), "asset-URL_ASSET_ONLY");
    }

    #[tokio::test]
    async fn incremental_expired_url_retry_hydrates_instead_of_replaying_stale_delta() {
        use base64::Engine as _;
        use sha2::{Digest, Sha256};
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};

        let server = crate::start_wiremock_or_skip!();
        Mock::given(method("GET"))
            .and(path("/expired-replay.jpg"))
            .respond_with(ResponseTemplate::new(410))
            .expect(1)
            .mount(&server)
            .await;

        let body = vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0x4A, 0x46];
        let checksum = base64::engine::general_purpose::STANDARD.encode(Sha256::digest(&body));
        Mock::given(method("GET"))
            .and(path("/fresh-hydrated-replay.jpg"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_bytes(body)
                    .insert_header("content-type", "image/jpeg"),
            )
            .expect(1)
            .mount(&server)
            .await;

        let stale_url = format!("{}/expired-replay.jpg", server.uri());
        let fresh_url = format!("{}/fresh-hydrated-replay.jpg", server.uri());
        let mut stale_records = incremental_photo_records_with_url(
            "URL_REPLAY_STALE",
            "replay-stale.jpg",
            &stale_url,
            8,
        );
        stale_records[0]["fields"]["resOriginalRes"]["value"]["fileChecksum"] =
            json!(checksum.clone());
        let mut fresh_records = incremental_photo_records_with_url(
            "URL_REPLAY_STALE",
            "replay-stale.jpg",
            &fresh_url,
            8,
        );
        fresh_records[0]["fields"]["resOriginalRes"]["value"]["fileChecksum"] = json!(checksum);

        let session = SplitChangesZoneSession::new(
            vec![
                changes_zone_response(stale_records.clone(), "zone-token-next"),
                changes_zone_response(stale_records, "zone-token-next"),
            ],
            vec![changes_zone_response(fresh_records, "zone-token-next")],
        );
        let passes = vec![AlbumPass {
            kind: PassKind::Unfiled,
            album: album_with_session("PrimarySync", "", Box::new(session)),
            exclude_ids: Arc::new(FxHashSet::default()),
        }];

        let db = Arc::new(SqliteStateDb::open_in_memory().expect("state db"));
        let mut config = test_config();
        let dir = TempDir::new().expect("temp dir");
        config.directory = Arc::from(dir.path());
        config.concurrent_downloads = 1;
        config.recent = Some(10);
        config.state_db = Some(db.clone());
        config.sync_mode = SyncMode::Incremental {
            zone_sync_token: "zone-token-prev".to_string(),
        };

        let result = download_photos_with_sync(
            &Client::new(),
            &passes,
            Arc::new(config),
            DownloadControls::download_hidden(),
            CancellationToken::new(),
        )
        .await
        .expect("expired URL retry should hydrate current records, not replay stale delta URLs");

        assert!(matches!(result.outcome, DownloadOutcome::Success));
        assert_eq!(result.stats.downloaded, 1);
        assert_eq!(result.stats.failed, 0);
        assert!(
            !result.stats.interrupted,
            "hydrated expired URL recovery should not leave the cycle interrupted"
        );
        assert_eq!(result.sync_token.as_deref(), Some("zone-token-next"));
        let summary = db.get_summary().await.expect("summary");
        assert_eq!(summary.downloaded, 1);
        assert_eq!(summary.failed, 0);
    }

    #[tokio::test]
    async fn incremental_expired_url_retry_hydrates_relation_only_album_assets() {
        use base64::Engine as _;
        use sha2::{Digest, Sha256};
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};

        let server = crate::start_wiremock_or_skip!();
        Mock::given(method("GET"))
            .and(path("/expired-hydrated.jpg"))
            .respond_with(ResponseTemplate::new(410))
            .mount(&server)
            .await;

        let body = vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0x4A, 0x46];
        let checksum = base64::engine::general_purpose::STANDARD.encode(Sha256::digest(&body));
        Mock::given(method("GET"))
            .and(path("/fresh-hydrated.jpg"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_bytes(body)
                    .insert_header("content-type", "image/jpeg"),
            )
            .mount(&server)
            .await;

        let stale_url = format!("{}/expired-hydrated.jpg", server.uri());
        let fresh_url = format!("{}/fresh-hydrated.jpg", server.uri());
        let mut stale_records =
            incremental_photo_records_with_url("MASTER_HYDRATED", "hydrated.jpg", &stale_url, 8);
        stale_records[0]["fields"]["resOriginalRes"]["value"]["fileChecksum"] =
            json!(checksum.clone());
        let mut fresh_records =
            incremental_photo_records_with_url("MASTER_HYDRATED", "hydrated.jpg", &fresh_url, 8);
        fresh_records[0]["fields"]["resOriginalRes"]["value"]["fileChecksum"] = json!(checksum);
        let relation_records = || {
            vec![relation_delta_record(
                "container-vacation",
                "asset-MASTER_HYDRATED",
            )]
        };

        let db = Arc::new(SqliteStateDb::open_in_memory().expect("state db"));
        seed_complete_album_snapshot(&db, "container-vacation", "Vacation", &[]).await;
        let album_session = SharedChangesZoneSession::new(vec![
            changes_zone_response(relation_records(), "zone-token-next"),
            changes_zone_response(stale_records, "zone-token-next"),
            changes_zone_response(fresh_records, "zone-token-next"),
        ]);
        let passes = vec![AlbumPass {
            kind: PassKind::Album,
            album: album_with_session_and_container(
                "PrimarySync",
                "Vacation",
                Some("container-vacation"),
                Box::new(album_session),
            ),
            exclude_ids: Arc::new(FxHashSet::default()),
        }];

        let mut config = test_config();
        let dir = TempDir::new().expect("temp dir");
        config.directory = Arc::from(dir.path());
        config.concurrent_downloads = 1;
        config.folder_structure_albums = Arc::from("{album}");
        config.state_db = Some(db.clone());
        config.sync_mode = SyncMode::Incremental {
            zone_sync_token: "zone-token-prev".to_string(),
        };

        let result = download_photos_with_sync(
            &Client::new(),
            &passes,
            Arc::new(config),
            DownloadControls::download_hidden(),
            CancellationToken::new(),
        )
        .await
        .expect("relation-only incremental retry should hydrate fresh URLs");

        assert!(matches!(result.outcome, DownloadOutcome::Success));
        assert_eq!(result.stats.downloaded, 1);
        assert_eq!(result.stats.failed, 0);
        assert!(
            !result.stats.interrupted,
            "relation-only expired URL recovery should not leave the cycle interrupted"
        );
        assert_eq!(result.sync_token.as_deref(), Some("zone-token-next"));
        let summary = db.get_summary().await.expect("summary");
        assert_eq!(summary.downloaded, 1);
        assert_eq!(summary.failed, 0);
    }

    #[tokio::test]
    async fn incremental_relation_add_before_photo_routes_to_album_not_unfiled() {
        let db = Arc::new(SqliteStateDb::open_in_memory().expect("state db"));
        seed_complete_album_snapshot(&db, "container-vacation", "Vacation", &[]).await;
        let mut records = vec![relation_delta_record(
            "container-vacation",
            "asset-MASTER_CHANGED",
        )];
        records.extend(incremental_photo_records("MASTER_CHANGED"));
        let calls = Arc::new(AtomicUsize::new(0));
        let session = changes_zone_session(Arc::clone(&calls), records);
        let passes = vec![
            AlbumPass {
                kind: PassKind::Album,
                album: changes_album_with_container(
                    "Vacation",
                    Some("container-vacation"),
                    session,
                ),
                exclude_ids: Arc::new(FxHashSet::default()),
            },
            unused_unfiled_changes_pass(),
        ];

        let mut config = test_config();
        let dir = TempDir::new().expect("temp dir");
        config.directory = Arc::from(dir.path());
        config.folder_structure = "Unfiled".to_string();
        config.folder_structure_albums = Arc::from("{album}");
        config.state_db = Some(Arc::clone(&db) as Arc<dyn DownloadStore>);
        let result = download_photos_incremental(
            &Client::new(),
            &passes,
            &Arc::new(config),
            "zone-token-prev",
            DownloadControls::new(DownloadRunMode::PrintFilenames, DownloadReporting::hidden()),
            CancellationToken::new(),
        )
        .await
        .expect("relation-add routing should succeed");

        assert!(matches!(result.outcome, DownloadOutcome::Success));
        let album_rows = db.get_all_asset_albums("PrimarySync").await.unwrap();
        assert_eq!(
            album_rows,
            vec![("asset-MASTER_CHANGED".to_string(), "Vacation".to_string())],
            "relation add should route the photo event through the album pass"
        );
    }

    #[tokio::test]
    async fn incremental_relation_delete_before_photo_routes_to_unfiled() {
        let db = Arc::new(SqliteStateDb::open_in_memory().expect("state db"));
        seed_complete_album_snapshot(
            &db,
            "container-vacation",
            "Vacation",
            &[("asset-MASTER_CHANGED", "MASTER_CHANGED")],
        )
        .await;
        let mut records = vec![relation_delete_record(
            "container-vacation",
            "asset-MASTER_CHANGED",
        )];
        records.extend(incremental_photo_records("MASTER_CHANGED"));
        let calls = Arc::new(AtomicUsize::new(0));
        let session = changes_zone_session(Arc::clone(&calls), records);
        let passes = vec![
            AlbumPass {
                kind: PassKind::Album,
                album: changes_album_with_container(
                    "Vacation",
                    Some("container-vacation"),
                    session,
                ),
                exclude_ids: Arc::new(FxHashSet::default()),
            },
            unused_unfiled_changes_pass(),
        ];

        let mut config = test_config();
        let dir = TempDir::new().expect("temp dir");
        config.directory = Arc::from(dir.path());
        config.folder_structure = "Unfiled".to_string();
        config.folder_structure_albums = Arc::from("{album}");
        config.state_db = Some(Arc::clone(&db) as Arc<dyn DownloadStore>);
        let result = download_photos_incremental(
            &Client::new(),
            &passes,
            &Arc::new(config),
            "zone-token-prev",
            DownloadControls::new(DownloadRunMode::PrintFilenames, DownloadReporting::hidden()),
            CancellationToken::new(),
        )
        .await
        .expect("relation-delete routing should succeed");

        assert!(matches!(result.outcome, DownloadOutcome::Success));
        let album_rows = db.get_all_asset_albums("PrimarySync").await.unwrap();
        assert!(
            album_rows.is_empty(),
            "relation delete should route the photo event only through the unfiled pass"
        );
    }

    #[tokio::test]
    async fn selected_relation_add_without_photo_blocks_incremental_token() {
        let db = Arc::new(SqliteStateDb::open_in_memory().expect("state db"));
        seed_complete_album_snapshot(&db, "container-vacation", "Vacation", &[]).await;
        let calls = Arc::new(AtomicUsize::new(0));
        let session = changes_zone_session(
            Arc::clone(&calls),
            vec![relation_delta_record(
                "container-vacation",
                "asset-MASTER_UNKNOWN",
            )],
        );
        let passes = vec![AlbumPass {
            kind: PassKind::Album,
            album: changes_album_with_container("Vacation", Some("container-vacation"), session),
            exclude_ids: Arc::new(FxHashSet::default()),
        }];

        let mut config = test_config();
        config.state_db = Some(Arc::clone(&db) as Arc<dyn DownloadStore>);
        let result = download_photos_incremental(
            &Client::new(),
            &passes,
            &Arc::new(config),
            "zone-token-prev",
            DownloadControls::new(DownloadRunMode::Download, DownloadReporting::hidden()),
            CancellationToken::new(),
        )
        .await
        .expect("unknown selected relation add should not fall back to full here");

        assert!(matches!(result.outcome, DownloadOutcome::Success));
        assert_eq!(result.sync_token, None);
        assert!(result.stats.sync_token_blocked);
        assert_eq!(
            result.stats.sync_token_blocked_reason,
            Some(UNKNOWN_ALBUM_RELATION_ASSET_REASON)
        );
    }

    #[tokio::test]
    async fn selected_relation_add_without_photo_uses_persisted_asset_master_mapping() {
        let db = Arc::new(SqliteStateDb::open_in_memory().expect("state db"));
        seed_complete_album_snapshot(&db, "container-vacation", "Vacation", &[]).await;
        db.upsert_asset_master_mapping("PrimarySync", "asset-MASTER_KNOWN", "MASTER_KNOWN")
            .await
            .unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let session = changes_zone_session(
            Arc::clone(&calls),
            vec![relation_delta_record(
                "container-vacation",
                "asset-MASTER_KNOWN",
            )],
        );
        let passes = vec![AlbumPass {
            kind: PassKind::Album,
            album: changes_album_with_container("Vacation", Some("container-vacation"), session),
            exclude_ids: Arc::new(FxHashSet::default()),
        }];

        let mut config = test_config();
        config.state_db = Some(Arc::clone(&db) as Arc<dyn DownloadStore>);
        let result = download_photos_incremental(
            &Client::new(),
            &passes,
            &Arc::new(config),
            "zone-token-prev",
            DownloadControls::new(DownloadRunMode::Download, DownloadReporting::hidden()),
            CancellationToken::new(),
        )
        .await
        .expect("known selected relation add should not fall back to full here");

        assert!(matches!(result.outcome, DownloadOutcome::Success));
        assert_eq!(result.sync_token.as_deref(), Some("zone-token-next"));
        assert!(!result.stats.sync_token_blocked);
        let memberships = db
            .get_live_selected_album_memberships_for_asset(
                "PrimarySync",
                "asset-MASTER_KNOWN",
                &["container-vacation"],
            )
            .await
            .unwrap();
        assert_eq!(memberships.len(), 1);
        assert_eq!(
            memberships[0].master_record_name.as_deref(),
            Some("MASTER_KNOWN")
        );
    }

    #[tokio::test]
    async fn selected_relation_add_without_photo_hydrates_missing_asset() {
        let db = Arc::new(SqliteStateDb::open_in_memory().expect("state db"));
        seed_complete_album_snapshot(&db, "container-vacation", "Vacation", &[]).await;
        let session = RelationHydrationSession {
            incremental_calls: Arc::new(AtomicUsize::new(0)),
            hydrate_calls: Arc::new(AtomicUsize::new(0)),
        };
        let passes = vec![AlbumPass {
            kind: PassKind::Album,
            album: changes_album_with_container(
                "Vacation",
                Some("container-vacation"),
                session.clone(),
            ),
            exclude_ids: Arc::new(FxHashSet::default()),
        }];

        let mut config = test_config();
        config.state_db = Some(Arc::clone(&db) as Arc<dyn DownloadStore>);
        let result = download_photos_incremental(
            &Client::new(),
            &passes,
            &Arc::new(config),
            "zone-token-prev",
            DownloadControls::new(DownloadRunMode::PrintFilenames, DownloadReporting::hidden()),
            CancellationToken::new(),
        )
        .await
        .expect("missing selected relation asset should hydrate");

        assert!(matches!(result.outcome, DownloadOutcome::Success));
        assert!(!result.stats.sync_token_blocked);
        assert_eq!(
            session.incremental_calls.load(Ordering::SeqCst),
            1,
            "incremental changes should be read once"
        );
        assert_eq!(
            session.hydrate_calls.load(Ordering::SeqCst),
            1,
            "missing relation asset should trigger one bounded hydrate scan"
        );
        let memberships = db
            .get_live_selected_album_memberships_for_asset(
                "PrimarySync",
                "asset-MASTER_HYDRATED",
                &["container-vacation"],
            )
            .await
            .unwrap();
        assert_eq!(memberships.len(), 1);
        assert_eq!(
            memberships[0].master_record_name.as_deref(),
            Some("MASTER_HYDRATED")
        );
    }

    #[tokio::test]
    async fn incremental_relation_add_unknown_unselected_container_advances_sync_token() {
        let db = Arc::new(SqliteStateDb::open_in_memory().expect("state db"));
        let calls = Arc::new(AtomicUsize::new(0));
        let session = changes_zone_session(
            Arc::clone(&calls),
            vec![relation_delta_record(
                "container-missing",
                "asset-MASTER_UNKNOWN",
            )],
        );
        let passes = vec![AlbumPass {
            kind: PassKind::Unfiled,
            album: changes_album("", session),
            exclude_ids: Arc::new(FxHashSet::default()),
        }];

        let mut config = test_config();
        config.state_db = Some(db);
        let result = download_photos_incremental(
            &Client::new(),
            &passes,
            &Arc::new(config),
            "zone-token-prev",
            DownloadControls::new(DownloadRunMode::Download, DownloadReporting::hidden()),
            CancellationToken::new(),
        )
        .await
        .expect("unknown relation container should not fall back to full here");

        assert!(matches!(result.outcome, DownloadOutcome::Success));
        assert_eq!(result.sync_token.as_deref(), Some("zone-token-next"));
        assert!(!result.stats.sync_token_blocked);
    }

    #[tokio::test]
    async fn incremental_album_delta_delete_invalidates_snapshot_through_download_flow() {
        let db = Arc::new(SqliteStateDb::open_in_memory().expect("state db"));
        seed_complete_album_snapshot(
            &db,
            "container-vacation",
            "Vacation",
            &[("asset-MASTER_OLD", "MASTER_OLD")],
        )
        .await;
        let calls = Arc::new(AtomicUsize::new(0));
        let session = changes_zone_session(
            Arc::clone(&calls),
            vec![deleted_album_delta_record("container-vacation")],
        );
        let passes = vec![AlbumPass {
            kind: PassKind::Album,
            album: changes_album_with_container("Vacation", Some("container-vacation"), session),
            exclude_ids: Arc::new(FxHashSet::default()),
        }];

        let mut config = test_config();
        config.state_db = Some(Arc::clone(&db) as Arc<dyn DownloadStore>);
        let result = download_photos_incremental(
            &Client::new(),
            &passes,
            &Arc::new(config),
            "zone-token-prev",
            DownloadControls::new(DownloadRunMode::Download, DownloadReporting::hidden()),
            CancellationToken::new(),
        )
        .await
        .expect("album delete delta should be applied through incremental flow");

        assert!(matches!(result.outcome, DownloadOutcome::Success));
        assert_eq!(result.sync_token.as_deref(), Some("zone-token-next"));
        assert!(
            !db.selected_album_containers_have_complete_snapshots(
                "PrimarySync",
                &["container-vacation"]
            )
            .await
            .unwrap(),
            "deleted album delta must invalidate trusted membership snapshots"
        );
    }

    #[tokio::test]
    async fn incremental_relation_add_writes_membership_after_album_delta() {
        let db = Arc::new(SqliteStateDb::open_in_memory().expect("state db"));
        let calls = Arc::new(AtomicUsize::new(0));
        let session = changes_zone_session(
            Arc::clone(&calls),
            vec![
                relation_delta_record("container-a", "asset-record-a"),
                album_delta_record("container-a", "Vacation"),
            ],
        );
        let passes = vec![AlbumPass {
            kind: PassKind::Unfiled,
            album: changes_album("", session),
            exclude_ids: Arc::new(FxHashSet::default()),
        }];

        let mut config = test_config();
        config.state_db = Some(db.clone());
        let result = download_photos_incremental(
            &Client::new(),
            &passes,
            &Arc::new(config),
            "zone-token-prev",
            DownloadControls::new(DownloadRunMode::Download, DownloadReporting::hidden()),
            CancellationToken::new(),
        )
        .await
        .expect("incremental relation delta should succeed");

        assert!(matches!(result.outcome, DownloadOutcome::Success));
        assert_eq!(result.sync_token.as_deref(), Some("zone-token-next"));
        let memberships = db
            .get_live_selected_album_memberships_for_asset(
                "PrimarySync",
                "asset-record-a",
                &["container-a"],
            )
            .await
            .unwrap();
        assert_eq!(memberships.len(), 1);
        assert_eq!(memberships[0].container_id, "container-a");
    }

    #[tokio::test]
    async fn incremental_relation_delete_marks_membership_deleted() {
        let db = Arc::new(SqliteStateDb::open_in_memory().expect("state db"));
        db.upsert_album_container("PrimarySync", "container-a", "Vacation", "album")
            .await
            .unwrap();
        db.upsert_album_membership_delta(
            "PrimarySync",
            "container-a",
            "asset-record-a",
            Some("master-a"),
            "icloud",
        )
        .await
        .unwrap();

        let calls = Arc::new(AtomicUsize::new(0));
        let session = changes_zone_session(
            Arc::clone(&calls),
            vec![relation_delete_record("container-a", "asset-record-a")],
        );
        let passes = vec![AlbumPass {
            kind: PassKind::Album,
            album: album_with_session_and_container(
                "PrimarySync",
                "Vacation",
                Some("container-a"),
                Box::new(session),
            ),
            exclude_ids: Arc::new(FxHashSet::default()),
        }];

        let mut config = test_config();
        config.state_db = Some(db.clone());
        let result = download_photos_incremental(
            &Client::new(),
            &passes,
            &Arc::new(config),
            "zone-token-prev",
            DownloadControls::new(DownloadRunMode::Download, DownloadReporting::hidden()),
            CancellationToken::new(),
        )
        .await
        .expect("incremental relation delete should succeed");

        assert!(matches!(result.outcome, DownloadOutcome::Success));
        let memberships = db
            .get_live_selected_album_memberships_for_asset(
                "PrimarySync",
                "asset-record-a",
                &["container-a"],
            )
            .await
            .unwrap();
        assert!(memberships.is_empty());
    }

    #[tokio::test]
    async fn unparsable_relation_delete_blocks_incremental_token() {
        let calls = Arc::new(AtomicUsize::new(0));
        let session = changes_zone_session(
            Arc::clone(&calls),
            vec![unparsable_relation_delete_record()],
        );
        let passes = vec![AlbumPass {
            kind: PassKind::Unfiled,
            album: changes_album("", session),
            exclude_ids: Arc::new(FxHashSet::default()),
        }];

        let config = Arc::new(test_config());
        let result = download_photos_incremental(
            &Client::new(),
            &passes,
            &config,
            "zone-token-prev",
            DownloadControls::new(DownloadRunMode::Download, DownloadReporting::hidden()),
            CancellationToken::new(),
        )
        .await
        .expect("unparsable relation delete should not fall back to full here");

        assert!(matches!(result.outcome, DownloadOutcome::Success));
        assert_eq!(result.sync_token, None);
        assert!(result.stats.sync_token_blocked);
        assert_eq!(
            result.stats.sync_token_blocked_reason,
            Some(UNPARSABLE_RELATION_DELTA_REASON)
        );
    }

    #[test]
    fn needs_metadata_rewrite_detects_hash_change() {
        let mut ctx = DownloadContext::default();
        ctx.downloaded_metadata_hashes
            .entry("PrimarySync".into())
            .or_default()
            .entry("asset_md".into())
            .or_default()
            .insert("original".into(), "hash-OLD".into());

        // Same hash -> no rewrite needed.
        assert!(!ctx.needs_metadata_rewrite(
            "PrimarySync",
            "asset_md",
            VersionSizeKey::Original,
            Some("hash-OLD")
        ));
        // Different hash -> rewrite.
        assert!(ctx.needs_metadata_rewrite(
            "PrimarySync",
            "asset_md",
            VersionSizeKey::Original,
            Some("hash-NEW")
        ));
        // Unknown new hash -> no rewrite (nothing to compare to).
        assert!(!ctx.needs_metadata_rewrite(
            "PrimarySync",
            "asset_md",
            VersionSizeKey::Original,
            None
        ));
    }

    #[test]
    fn needs_metadata_rewrite_honors_retry_marker() {
        let mut ctx = DownloadContext::default();
        ctx.metadata_retry_markers
            .entry("PrimarySync".into())
            .or_default()
            .entry("asset_retry".into())
            .or_default()
            .insert("original".into());
        // No stored hash at all, but marker is set -> rewrite needed.
        assert!(ctx.needs_metadata_rewrite(
            "PrimarySync",
            "asset_retry",
            VersionSizeKey::Original,
            None
        ));
        // Marker set -> rewrite even if hashes match.
        ctx.downloaded_metadata_hashes
            .entry("PrimarySync".into())
            .or_default()
            .entry("asset_retry".into())
            .or_default()
            .insert("original".into(), "h".into());
        assert!(ctx.needs_metadata_rewrite(
            "PrimarySync",
            "asset_retry",
            VersionSizeKey::Original,
            Some("h")
        ));
    }

    #[test]
    fn needs_metadata_rewrite_refreshes_null_stored_hash() {
        // Pre-v5 downloaded rows have metadata_hash IS NULL; even without a
        // retry marker, a fresh hash should trigger a rewrite so the XMP
        // gets the source state this tree has never recorded.
        let ctx = DownloadContext::default();
        assert!(ctx.needs_metadata_rewrite(
            "PrimarySync",
            "asset_no_stored_hash",
            VersionSizeKey::Original,
            Some("new-hash")
        ));
    }

    #[test]
    fn legacy_master_state_requires_compatible_unambiguous_history() {
        let master = mock_master_record_with_filename("master-family", "family.jpg");
        let asset_a = PhotoAsset::new(
            master.clone(),
            mock_asset_record_for("asset-a", "master-family"),
        );
        let asset_b = PhotoAsset::new(master, mock_asset_record_for("asset-b", "master-family"));
        let checksum = asset_a
            .versions()
            .first()
            .expect("mock asset has an original version")
            .1
            .checksum
            .clone();

        let mut ctx = DownloadContext::default();
        ctx.downloaded_ids
            .entry("PrimarySync".into())
            .or_default()
            .entry("master-family".into())
            .or_default()
            .insert("original".into());
        ctx.downloaded_checksums
            .entry("PrimarySync".into())
            .or_default()
            .entry("master-family".into())
            .or_default()
            .insert("original".into(), checksum);

        assert!(!ctx.should_use_legacy_master_state("PrimarySync", &asset_a));

        ctx.asset_master_mappings = Some(FxHashMap::default());
        ctx.legacy_master_state_owners = Some(FxHashMap::default());
        assert!(ctx.should_use_legacy_master_state("PrimarySync", &asset_a));

        ctx.asset_master_mappings
            .as_mut()
            .expect("mapping snapshot")
            .entry("PrimarySync".into())
            .or_default()
            .entry("master-family".into())
            .or_default()
            .extend([Arc::from("asset-a"), Arc::from("asset-b")]);
        assert!(!ctx.should_use_legacy_master_state("PrimarySync", &asset_a));
        assert!(!ctx.should_use_legacy_master_state("PrimarySync", &asset_b));

        ctx.downloaded_ids
            .entry("PrimarySync".into())
            .or_default()
            .entry("asset-b".into())
            .or_default()
            .insert("original".into());
        assert!(ctx.should_use_legacy_master_state("PrimarySync", &asset_a));
        assert!(!ctx.should_use_legacy_master_state("PrimarySync", &asset_b));

        ctx.legacy_master_state_owners
            .as_mut()
            .expect("owner snapshot")
            .entry("PrimarySync".into())
            .or_default()
            .insert("master-family".into(), "asset-a".into());
        assert!(ctx.should_use_legacy_master_state("PrimarySync", &asset_a));
        assert!(!ctx.should_use_legacy_master_state("PrimarySync", &asset_b));
    }

    #[test]
    fn legacy_master_state_accepts_matching_pending_retry() {
        let records = mock_photo_records_with_filename("master-pending", "pending.jpg");
        let asset = PhotoAsset::new(records[0].clone(), records[1].clone());
        let checksum = asset
            .versions()
            .first()
            .expect("mock asset has an original version")
            .1
            .checksum
            .clone();
        let mut ctx = DownloadContext {
            asset_master_mappings: Some(FxHashMap::default()),
            legacy_master_state_owners: Some(FxHashMap::default()),
            ..DownloadContext::default()
        };
        ctx.pending_ids
            .entry("PrimarySync".into())
            .or_default()
            .entry("master-pending".into())
            .or_default()
            .insert("original".into());
        ctx.pending_checksums
            .entry("PrimarySync".into())
            .or_default()
            .entry("master-pending".into())
            .or_default()
            .insert("original".into(), checksum);

        assert!(ctx.should_use_legacy_master_state("PrimarySync", &asset));
    }

    #[test]
    fn test_should_download_fast_downloaded_matching_checksum() {
        let mut ctx = DownloadContext::default();
        ctx.downloaded_ids
            .entry("PrimarySync".into())
            .or_default()
            .entry("asset_x".into())
            .or_default()
            .insert("original".into());
        ctx.downloaded_checksums
            .entry("PrimarySync".into())
            .or_default()
            .entry("asset_x".into())
            .or_default()
            .insert("original".into(), "ck_match".into());

        // trust_state=true => hard skip
        assert_eq!(
            ctx.should_download_fast(
                "PrimarySync",
                "asset_x",
                VersionSizeKey::Original,
                "ck_match",
                true
            ),
            Some(false)
        );
        // trust_state=false => needs filesystem check
        assert_eq!(
            ctx.should_download_fast(
                "PrimarySync",
                "asset_x",
                VersionSizeKey::Original,
                "ck_match",
                false
            ),
            None
        );
    }

    #[test]
    fn test_should_download_fast_downloaded_changed_checksum() {
        let mut ctx = DownloadContext::default();
        ctx.downloaded_ids
            .entry("PrimarySync".into())
            .or_default()
            .entry("asset_y".into())
            .or_default()
            .insert("original".into());
        ctx.downloaded_checksums
            .entry("PrimarySync".into())
            .or_default()
            .entry("asset_y".into())
            .or_default()
            .insert("original".into(), "old_ck".into());

        // Changed checksum => needs re-download regardless of trust_state
        assert_eq!(
            ctx.should_download_fast(
                "PrimarySync",
                "asset_y",
                VersionSizeKey::Original,
                "new_ck",
                true
            ),
            Some(true)
        );
        assert_eq!(
            ctx.should_download_fast(
                "PrimarySync",
                "asset_y",
                VersionSizeKey::Original,
                "new_ck",
                false
            ),
            Some(true)
        );
    }

    #[test]
    fn test_should_download_fast_different_version_size() {
        let mut ctx = DownloadContext::default();
        ctx.downloaded_ids
            .entry("PrimarySync".into())
            .or_default()
            .entry("asset_z".into())
            .or_default()
            .insert("original".into());

        // Medium version not downloaded
        assert_eq!(
            ctx.should_download_fast(
                "PrimarySync",
                "asset_z",
                VersionSizeKey::Medium,
                "any_ck",
                true
            ),
            Some(true)
        );
    }

    #[test]
    fn test_download_context_known_ids_populated_for_retry_only() {
        // Simulate retry-only mode: known_ids is populated
        let mut ctx = DownloadContext::default();
        ctx.known_ids
            .entry("PrimarySync".into())
            .or_default()
            .insert("known_asset".into());

        // A known asset that's not in downloaded_ids needs download
        assert_eq!(
            ctx.should_download_fast(
                "PrimarySync",
                "known_asset",
                VersionSizeKey::Original,
                "ck",
                true
            ),
            Some(true)
        );
        // The known_ids set is used externally to decide whether to skip new assets;
        // verify the set membership works
        assert!(ctx.is_known("PrimarySync", "known_asset"));
        assert!(!ctx.is_known("PrimarySync", "new_asset"));
        assert!(
            !ctx.is_known("SharedSync-AAAA", "known_asset"),
            "retry-only known IDs are library-scoped"
        );
    }

    #[test]
    fn download_context_detects_downloaded_rows_missing_metadata_hashes() {
        let mut ctx = DownloadContext::default();
        ctx.downloaded_ids
            .entry("PrimarySync".into())
            .or_default()
            .entry("asset_meta".into())
            .or_default()
            .insert("original".into());

        ctx.downloaded_without_metadata_hash = count_version_set_entries(&ctx.downloaded_ids)
            > count_value_map_entries(&ctx.downloaded_metadata_hashes);

        assert!(
            ctx.has_downloaded_without_metadata_hash(),
            "a downloaded row with no matching metadata hash needs the backfill notice"
        );

        ctx.downloaded_metadata_hashes
            .entry("PrimarySync".into())
            .or_default()
            .entry("asset_meta".into())
            .or_default()
            .insert("original".into(), "metadata_hash".into());

        ctx.downloaded_without_metadata_hash = count_version_set_entries(&ctx.downloaded_ids)
            > count_value_map_entries(&ctx.downloaded_metadata_hashes);

        assert!(
            !ctx.has_downloaded_without_metadata_hash(),
            "matching downloaded and metadata-hash sets should avoid the extra SQLite scan"
        );
    }

    // ── Gap coverage: empty versions, path traversal, empty filename ───

    #[test]
    fn should_download_fast_empty_checksum_never_hard_skips() {
        // Empty remote checksum is malformed provider input. Even if a stale
        // DB row also has an empty checksum, this must not turn into a hard
        // skip: the provider parser rejects new empty checksums, and this
        // fast path stays defensive for legacy/corrupt state.
        let mut ctx = DownloadContext::default();
        ctx.downloaded_ids
            .entry("PrimarySync".into())
            .or_default()
            .entry("asset_empty_ck".into())
            .or_default()
            .insert("original".into());
        ctx.downloaded_checksums
            .entry("PrimarySync".into())
            .or_default()
            .entry("asset_empty_ck".into())
            .or_default()
            .insert("original".into(), "".into());

        assert_eq!(
            ctx.should_download_fast(
                "PrimarySync",
                "asset_empty_ck",
                VersionSizeKey::Original,
                "",
                true
            ),
            Some(true)
        );
        assert_eq!(
            ctx.should_download_fast(
                "PrimarySync",
                "asset_empty_ck",
                VersionSizeKey::Original,
                "",
                false
            ),
            Some(true)
        );
        assert_eq!(
            ctx.should_download_fast(
                "PrimarySync",
                "asset_empty_ck",
                VersionSizeKey::Original,
                "abc123def456",
                true,
            ),
            Some(true)
        );
    }

    // ── Gap coverage: should_download_fast with no checksum in DB ────────

    #[test]
    fn should_download_fast_no_checksum_trust_true_returns_false() {
        // Asset is in downloaded_ids but has no entry in downloaded_checksums.
        // With trust_state=true the method should hard-skip (Some(false))
        // because the absence of a stored checksum means "nothing to compare".
        let mut ctx = DownloadContext::default();
        ctx.downloaded_ids
            .entry("PrimarySync".into())
            .or_default()
            .entry("asset_no_ck".into())
            .or_default()
            .insert("original".into());
        // No entry in downloaded_checksums

        assert_eq!(
            ctx.should_download_fast(
                "PrimarySync",
                "asset_no_ck",
                VersionSizeKey::Original,
                "any",
                true
            ),
            Some(false)
        );
    }

    #[test]
    fn should_download_fast_no_checksum_trust_false_returns_none() {
        // Same scenario but trust_state=false: needs filesystem check (None).
        let mut ctx = DownloadContext::default();
        ctx.downloaded_ids
            .entry("PrimarySync".into())
            .or_default()
            .entry("asset_no_ck".into())
            .or_default()
            .insert("original".into());

        assert_eq!(
            ctx.should_download_fast(
                "PrimarySync",
                "asset_no_ck",
                VersionSizeKey::Original,
                "any",
                false
            ),
            None
        );
    }

    // ── Gap coverage: retry_only known_ids filtering ────────────────────

    // ── Gap coverage: skip_created_before AND skip_created_after ────────

    // ── Gap coverage: incremental Modified events are downloadable ──────

    // ── Gap coverage: NameId7 produces task when file at original path ──

    // ── compute_config_hash tests ──────────────────────────────────

    /// Build a `Config` via `Config::build` with the given overrides.
    /// Uses a tempdir for cookie_directory so tests don't touch the real filesystem.
    fn build_config_with(
        cookie_dir: &std::path::Path,
        directory: &str,
        overrides: impl FnOnce(&mut crate::cli::SyncArgs),
    ) -> crate::config::Config {
        use crate::cli::SyncArgs;
        use crate::config::GlobalArgs;

        let globals = GlobalArgs {
            username: Some("test@example.com".to_string()),
            domain: None,
            data_dir: Some(cookie_dir.to_string_lossy().into_owned()),
        };
        let mut sync = SyncArgs {
            config_overrides: crate::config::SyncConfigOverrides {
                download_dir: Some(directory.to_string()),
                ..Default::default()
            },
            ..SyncArgs::default()
        };
        overrides(&mut sync);
        crate::config::Config::build(&globals, &crate::cli::PasswordArgs::default(), sync, None)
            .expect("Config::build should succeed")
    }

    #[test]
    fn test_compute_config_hash_same_config_same_hash() {
        let tmp = TempDir::new().unwrap();
        let a = build_config_with(tmp.path(), "/photos", |_| {});
        let b = build_config_with(tmp.path(), "/photos", |_| {});
        assert_eq!(compute_config_hash(&a), compute_config_hash(&b));
    }

    #[test]
    fn test_compute_config_hash_different_directory() {
        let tmp = TempDir::new().unwrap();
        let a = build_config_with(tmp.path(), "/photos/a", |_| {});
        let b = build_config_with(tmp.path(), "/photos/b", |_| {});
        assert_eq!(
            compute_config_hash(&a),
            compute_config_hash(&b),
            "download directory is path-only and must not invalidate the CloudKit zone token"
        );
    }

    #[test]
    fn test_compute_config_hash_different_size() {
        let tmp = TempDir::new().unwrap();
        let a = build_config_with(tmp.path(), "/photos", |_| {});
        let b = build_config_with(tmp.path(), "/photos", |s| {
            s.config_overrides.resolution = Some(crate::types::PhotoResolution::Medium);
        });
        assert_ne!(compute_config_hash(&a), compute_config_hash(&b));
    }

    #[test]
    fn test_compute_config_hash_different_skip_videos() {
        let tmp = TempDir::new().unwrap();
        let a = build_config_with(tmp.path(), "/photos", |_| {});
        let b = build_config_with(tmp.path(), "/photos", |s| {
            s.config_overrides.skip_videos = Some(true);
        });
        assert_ne!(compute_config_hash(&a), compute_config_hash(&b));
    }

    #[test]
    fn test_compute_config_hash_different_albums() {
        let tmp = TempDir::new().unwrap();
        let a = build_config_with(tmp.path(), "/photos", |_| {});
        let b = build_config_with(tmp.path(), "/photos", |s| {
            s.config_overrides.albums = vec!["Favorites".to_string()];
        });
        assert_eq!(
            compute_config_hash(&a),
            compute_config_hash(&b),
            "album selection changes are handled by membership snapshots and targeted backfill"
        );
    }

    #[test]
    fn test_compute_config_hash_different_inline_album_excludes() {
        let tmp = TempDir::new().unwrap();
        let a = build_config_with(tmp.path(), "/photos", |_| {});
        let b = build_config_with(tmp.path(), "/photos", |s| {
            s.config_overrides.albums = vec!["!Hidden".to_string()];
        });
        assert_eq!(
            compute_config_hash(&a),
            compute_config_hash(&b),
            "removing albums should not invalidate the CloudKit zone token"
        );
    }

    #[test]
    fn test_compute_config_hash_different_live_photo_mode() {
        let tmp = TempDir::new().unwrap();
        let a = build_config_with(tmp.path(), "/photos", |_| {});
        let b = build_config_with(tmp.path(), "/photos", |s| {
            s.config_overrides.live_photo_mode = Some(LivePhotoMode::Skip);
        });
        assert_ne!(compute_config_hash(&a), compute_config_hash(&b));
    }

    #[test]
    fn test_compute_config_hash_different_smart_folders() {
        let tmp = TempDir::new().unwrap();
        let a = build_config_with(tmp.path(), "/photos", |_| {});
        let b = build_config_with(tmp.path(), "/photos", |s| {
            s.config_overrides.smart_folders = vec!["Favorites".to_string()];
        });
        assert_eq!(
            compute_config_hash(&a),
            compute_config_hash(&b),
            "smart-folder selection changes are handled by targeted refresh"
        );
    }

    #[test]
    fn test_compute_config_hash_different_unfiled() {
        // Same silent-miss vector for the unfiled selector: --unfiled true
        // (default) → false changes whether the unfiled pass runs at all.
        // A regression that omits this from the hash leaves a stale token
        // pointing past assets the previous cycle would have caught.
        let tmp = TempDir::new().unwrap();
        let a = build_config_with(tmp.path(), "/photos", |_| {});
        let b = build_config_with(tmp.path(), "/photos", |s| {
            s.config_overrides.unfiled = Some(false);
        });
        assert_ne!(
            compute_config_hash(&a),
            compute_config_hash(&b),
            "changing --unfiled must change the config hash so the \
             stored sync token is invalidated"
        );
    }

    #[test]
    fn test_compute_config_hash_different_library() {
        let tmp = TempDir::new().unwrap();
        let a = build_config_with(tmp.path(), "/photos", |_| {});
        let b = build_config_with(tmp.path(), "/photos", |s| {
            s.config_overrides.libraries = vec!["all".to_string()];
        });
        assert_eq!(
            compute_config_hash(&a),
            compute_config_hash(&b),
            "library selection changes should hydrate only newly selected libraries"
        );
    }

    #[test]
    fn test_compute_config_hash_path_only_changes_same_hash() {
        let tmp = TempDir::new().unwrap();
        let a = build_config_with(tmp.path(), "/photos-a", |_| {});
        let b = build_config_with(tmp.path(), "/photos-b", |s| {
            s.config_overrides.folder_structure = Some("%Y/%m".to_string());
            s.config_overrides.folder_structure_albums = Some("{album}/albums/%Y".to_string());
            s.config_overrides.folder_structure_smart_folders =
                Some("{smart-folder}/%Y".to_string());
            s.config_overrides.keep_unicode_in_filenames = Some(true);
        });
        assert_eq!(
            compute_config_hash(&a),
            compute_config_hash(&b),
            "path-only changes must not invalidate the CloudKit zone token"
        );
    }

    #[test]
    fn test_compute_config_hash_different_recent_same_hash() {
        let tmp = TempDir::new().unwrap();
        let a = build_config_with(tmp.path(), "/photos", |_| {});
        let b = build_config_with(tmp.path(), "/photos", |s| {
            s.recent = Some(crate::cli::RecentLimit::Count(100));
        });
        assert_eq!(
            compute_config_hash(&a),
            compute_config_hash(&b),
            "recent is intentionally excluded from the config hash"
        );
    }

    #[test]
    fn test_compute_config_hash_different_dry_run_same_hash() {
        let tmp = TempDir::new().unwrap();
        let a = build_config_with(tmp.path(), "/photos", |_| {});
        let b = build_config_with(tmp.path(), "/photos", |s| {
            s.dry_run = true;
        });
        assert_eq!(
            compute_config_hash(&a),
            compute_config_hash(&b),
            "dry_run is a per-run flag and should not affect the config hash"
        );
    }

    // ── filter_asset_to_tasks edge-case tests ──────────────────────

    // ── LivePhotoMode + filename_exclude filter tests ─────────────

    // ── exclude_asset_ids filter tests ─────────────────────────────

    #[test]
    fn test_hash_changes_on_live_photo_mode() {
        let config1 = test_config();
        let mut config2 = test_config();
        config2.live_photo_mode = LivePhotoMode::Skip;
        assert_ne!(
            hash_download_config(&config1),
            hash_download_config(&config2)
        );
    }

    #[test]
    fn test_hash_changes_on_filename_exclude() {
        let config1 = test_config();
        let mut config2 = test_config();
        config2.filename_exclude = std::sync::Arc::from(vec![glob::Pattern::new("*.AAE").unwrap()]);
        assert_ne!(
            hash_download_config(&config1),
            hash_download_config(&config2)
        );
    }

    // ── requires_per_pass_paths predicate ──────────────────────────

    fn config_with_templates(base: &str, albums: &str, smart_folders: &str) -> DownloadConfig {
        let mut c = test_config();
        c.folder_structure = base.to_string();
        c.folder_structure_albums = Arc::from(albums);
        c.folder_structure_smart_folders = Arc::from(smart_folders);
        c
    }

    #[test]
    fn incremental_full_enumeration_gate_ignores_unfiled_only_pass() {
        let session = crate::test_helpers::MockPhotosSession::new();
        let passes = vec![AlbumPass {
            kind: PassKind::Unfiled,
            album: mock_album("", session),
            exclude_ids: Arc::new(FxHashSet::default()),
        }];

        assert!(
            !incremental_requires_full_enumeration(&passes),
            "unfiled-only sync can use zone-level incremental changes"
        );
    }

    #[test]
    fn incremental_full_enumeration_gate_fires_on_album_pass() {
        let session = crate::test_helpers::MockPhotosSession::new();
        let passes = vec![AlbumPass {
            kind: PassKind::Album,
            album: mock_album("Vacation", session),
            exclude_ids: Arc::new(FxHashSet::default()),
        }];

        assert!(
            incremental_requires_full_enumeration(&passes),
            "album-scoped sync needs full enumeration to preserve membership"
        );
    }

    #[test]
    fn requires_per_pass_paths_fires_on_v013_defaults() {
        // v0.13 defaults carry per-pass tokens in the per-category fields.
        // Returning false here was the regression that silently routed every
        // album-pass photo through the unfiled template.
        assert!(test_config().requires_per_pass_paths());
    }

    #[test]
    fn requires_per_pass_paths_fires_on_legacy_album_in_base() {
        assert!(
            config_with_templates("{album}/%Y", "{album}/%Y", "{album}/%Y")
                .requires_per_pass_paths()
        );
    }

    #[test]
    fn requires_per_pass_paths_fires_on_smart_folder_token() {
        assert!(
            config_with_templates("%Y/%m/%d", "%Y/%m/%d", "{smart-folder}")
                .requires_per_pass_paths()
        );
    }

    #[test]
    fn requires_per_pass_paths_fires_on_library_token() {
        assert!(
            config_with_templates("{library}/%Y", "{library}/%Y", "{library}/%Y")
                .requires_per_pass_paths()
        );
    }

    #[test]
    fn requires_per_pass_paths_fires_on_per_category_template_diverging_from_base() {
        assert!(
            config_with_templates("%Y/%m/%d", "MyAlbums/%Y/%m", "%Y/%m/%d")
                .requires_per_pass_paths()
        );
    }

    #[test]
    fn requires_per_pass_paths_false_when_all_templates_are_identical_literals() {
        // Pure-literal, identical across all three fields, no per-pass token:
        // the merged-stream branch is safe.
        assert!(
            !config_with_templates("%Y/%m/%d", "%Y/%m/%d", "%Y/%m/%d").requires_per_pass_paths()
        );
    }

    // ── with_pass per-kind template selection ─────────────────────

    fn make_pass(kind: crate::commands::PassKind, name: &str) -> crate::commands::AlbumPass {
        use crate::icloud::photos::PhotoAlbum;
        crate::commands::AlbumPass {
            kind,
            album: PhotoAlbum::stub_for_test(Arc::from(name)),
            exclude_ids: std::sync::Arc::new(rustc_hash::FxHashSet::default()),
        }
    }

    #[test]
    fn test_with_pass_album_uses_albums_template() {
        use crate::commands::PassKind;
        let mut config = test_config();
        config.folder_structure_albums = Arc::from("{album}/%Y/%m/%d");
        let derived = config.with_pass(&make_pass(PassKind::Album, "Vacation"));
        assert_eq!(derived.folder_structure, "Vacation/%Y/%m/%d");
        assert_eq!(derived.album_name.as_deref(), Some("Vacation"));
    }

    #[test]
    fn test_with_pass_smart_folder_uses_smart_folders_template() {
        use crate::commands::PassKind;
        let mut config = test_config();
        config.folder_structure_smart_folders = Arc::from("{smart-folder}/%Y");
        let derived = config.with_pass(&make_pass(PassKind::SmartFolder, "Favorites"));
        assert_eq!(derived.folder_structure, "Favorites/%Y");
    }

    #[test]
    fn test_with_pass_smart_folder_ignores_albums_template() {
        // Spec: smart-folder passes use folder_structure_smart_folders, not
        // folder_structure_albums. Using the wrong template would cause every
        // smart-folder pass to substitute the smart-folder name into a
        // user-customised album path (e.g. "My/Albums/{album}/..." would
        // mis-render as "My/Albums/Favorites/...").
        use crate::commands::PassKind;
        let mut config = test_config();
        config.folder_structure_albums = Arc::from("{album}/album-tree");
        config.folder_structure_smart_folders = Arc::from("{smart-folder}/sf-tree");
        let derived = config.with_pass(&make_pass(PassKind::SmartFolder, "Videos"));
        assert!(derived.folder_structure.contains("sf-tree"));
        assert!(!derived.folder_structure.contains("album-tree"));
    }

    #[test]
    fn test_with_pass_unfiled_uses_base_folder_structure() {
        // Unfiled pass keeps the legacy `{album}` token in `folder_structure`
        // so existing configs with `--folder-structure "{album}/..."` still
        // produce the same on-disk tree.
        use crate::commands::PassKind;
        let mut config = test_config();
        config.folder_structure = "%Y/%m/%d".to_string();
        let derived = config.with_pass(&make_pass(PassKind::Unfiled, ""));
        assert_eq!(derived.folder_structure, "%Y/%m/%d");
    }

    #[test]
    fn test_with_pass_unfiled_collapses_album_token_to_empty() {
        use crate::commands::PassKind;
        let mut config = test_config();
        config.folder_structure = "{album}/%Y/%m/%d".to_string();
        let derived = config.with_pass(&make_pass(PassKind::Unfiled, ""));
        // Empty name strips the `{album}` segment for backwards compat.
        assert!(!derived.folder_structure.contains("{album}"));
    }

    #[test]
    fn test_with_pass_album_sanitizes_name() {
        use crate::commands::PassKind;
        let mut config = test_config();
        config.folder_structure_albums = Arc::from("{album}/%Y");
        let derived = config.with_pass(&make_pass(PassKind::Album, "My/Album"));
        // Path separators in album names must be sanitised before substitution.
        assert!(!derived.folder_structure.starts_with("My/Album"));
    }

    #[test]
    fn test_with_pass_expands_library_token_with_truncation() {
        use crate::commands::PassKind;
        let mut config = test_config();
        config.folder_structure_albums = Arc::from("{library}/{album}/%Y");
        config.library = Arc::from("SharedSync-A1B2C3D4-E5F6-7890-ABCD-EF1234567890");
        let derived = config.with_pass(&make_pass(PassKind::Album, "Vacation"));
        assert_eq!(
            derived.folder_structure, "SharedSync-A1B2C3D4/Vacation/%Y",
            "shared-zone UUIDs must truncate to 8 chars in path output"
        );
    }

    #[test]
    fn test_with_pass_library_token_passthrough_for_primary() {
        use crate::commands::PassKind;
        let mut config = test_config();
        config.folder_structure = "{library}/%Y/%m/%d".to_string();
        // Default `library` is "PrimarySync" via `test_default`.
        let derived = config.with_pass(&make_pass(PassKind::Unfiled, ""));
        assert_eq!(derived.folder_structure, "PrimarySync/%Y/%m/%d");
    }

    #[test]
    fn test_with_pass_library_token_in_smart_folder_template() {
        use crate::commands::PassKind;
        let mut config = test_config();
        config.folder_structure_smart_folders = Arc::from("{library}/{smart-folder}");
        config.library = Arc::from("SharedSync-DEADBEEF-aaaa-bbbb-cccc-dddddddddddd");
        let derived = config.with_pass(&make_pass(PassKind::SmartFolder, "Favorites"));
        assert_eq!(derived.folder_structure, "SharedSync-DEADBEEF/Favorites");
    }

    #[test]
    fn test_with_pass_state_db_library_uses_full_zone_name() {
        // Path rendering truncates the zone for readability, but the
        // state-DB key (DownloadConfig.library) keeps the full zone name
        // verbatim so two zones whose 8-char prefixes happen to collide
        // still get distinct PKs in the assets table.
        use crate::commands::PassKind;
        let mut config = test_config();
        config.library = Arc::from("SharedSync-A1B2C3D4-E5F6-7890-ABCD-EF1234567890");
        let derived = config.with_pass(&make_pass(PassKind::Album, "Trip"));
        assert_eq!(
            &*derived.library,
            "SharedSync-A1B2C3D4-E5F6-7890-ABCD-EF1234567890"
        );
    }

    #[test]
    fn test_with_pass_preserves_all_fields() {
        use crate::commands::PassKind;
        let mut config = test_config();
        config.folder_structure_albums = Arc::from("{album}/%Y");
        config.media.photos = false;
        config.media.videos = false;
        config.live_photo_mode = LivePhotoMode::ImageOnly;
        config.force_resolution = true;
        config.keep_unicode_in_filenames = true;
        config.metadata.set_exif_datetime = true;
        config.filename_exclude = std::sync::Arc::from(vec![glob::Pattern::new("*.AAE").unwrap()]);
        config.temp_suffix = std::sync::Arc::from(".custom-tmp");
        let derived = config.with_pass(&make_pass(PassKind::Album, "Test"));
        assert!(!derived.media.photos);
        assert!(!derived.media.videos);
        assert_eq!(derived.live_photo_mode, LivePhotoMode::ImageOnly);
        assert!(derived.force_resolution);
        assert!(derived.keep_unicode_in_filenames);
        assert!(derived.metadata.set_exif_datetime);
        assert_eq!(derived.filename_exclude.len(), 1);
        assert_eq!(&*derived.temp_suffix, ".custom-tmp");
        assert_eq!(derived.directory, config.directory);
    }

    // ── extract_skip_candidates: filename_exclude ─────────────────

    // ── compute_config_hash: filename_exclude ─────────────────────

    #[test]
    fn test_compute_config_hash_different_filename_exclude() {
        let tmp = TempDir::new().unwrap();
        let a = build_config_with(tmp.path(), "/photos", |_| {});
        let b = build_config_with(tmp.path(), "/photos", |s| {
            s.config_overrides.filename_exclude = vec!["*.AAE".to_string()];
        });
        assert_ne!(
            compute_config_hash(&a),
            compute_config_hash(&b),
            "changing filename_exclude should change the config hash"
        );
    }

    #[test]
    fn test_hash_changes_on_folder_structure_albums() {
        // Per-category templates affect path resolution, so the trust-state
        // hash must change with them or stale records pin assets to the wrong
        // tree on the next run.
        let mut config1 = test_config();
        let mut config2 = test_config();
        config1.folder_structure_albums = Arc::from("{album}");
        config2.folder_structure_albums = Arc::from("{album}/%Y");
        assert_ne!(
            hash_download_config(&config1),
            hash_download_config(&config2)
        );
    }

    #[test]
    fn test_hash_changes_on_folder_structure_smart_folders() {
        let mut config1 = test_config();
        let mut config2 = test_config();
        config1.folder_structure_smart_folders = Arc::from("{smart-folder}");
        config2.folder_structure_smart_folders = Arc::from("{smart-folder}/%Y");
        assert_ne!(
            hash_download_config(&config1),
            hash_download_config(&config2)
        );
    }

    // ── Golden-hash stability tests ─────────────────────────────────
    //
    // These pin specific config values to specific hex outputs. If any
    // test fails, it means the hash encoding changed -- which would
    // trigger unnecessary full re-syncs for all users. Only update the
    // expected values when the hash change is intentional.

    #[test]
    fn golden_hash_download_config_defaults() {
        let config = test_config();
        let hash = hash_download_config(&config);
        assert_eq!(
            hash, "20baba076dd97143",
            "hash_download_config golden hash changed -- this will trigger full re-syncs"
        );
        assert_eq!(
            hash_legacy_download_config(&config),
            "c3f2be1a9e394951",
            "legacy hash changed -- matching stored hashes would no longer migrate safely"
        );
    }

    #[test]
    fn golden_hash_download_config_non_defaults() {
        let mut config = test_config();
        config.directory = std::sync::Arc::from(std::path::Path::new("/my/photos"));
        config.folder_structure = "{:%Y/%m}".to_string();
        config.resolution = crate::types::PhotoResolution::Medium;
        config.live_resolution = AssetVersionSize::LiveMedium;
        config.file_match_policy = FileMatchPolicy::NameId7;
        config.live_photo_mov_filename_policy = crate::types::LivePhotoMovFilenamePolicy::Original;
        config.raw_policy = RawPolicy::PreferJpeg;
        config.keep_unicode_in_filenames = true;
        config.skip_created_before = Some(crate::config::CreatedDateFilter::Instant(
            DateTime::parse_from_rfc3339("2020-06-15T00:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
        ));
        config.skip_created_after = Some(crate::config::CreatedDateFilter::Instant(
            DateTime::parse_from_rfc3339("2024-12-31T00:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
        ));
        config.recent = Some(500);
        config.force_resolution = true;
        config.media.videos = false;
        config.live_photo_mode = LivePhotoMode::ImageOnly;
        config.filename_exclude = std::sync::Arc::from(vec![
            glob::Pattern::new("*.AAE").unwrap(),
            glob::Pattern::new("*.THM").unwrap(),
        ]);
        let hash = hash_download_config(&config);
        assert_eq!(
            hash, "4d5a9f1a5a0bf903",
            "hash_download_config golden hash changed -- this will trigger full re-syncs"
        );
        assert_eq!(
            hash_legacy_download_config(&config),
            "d327fda31e8bec04",
            "legacy hash changed -- matching stored hashes would no longer migrate safely"
        );
    }

    #[test]
    fn golden_compute_config_hash_defaults() {
        let tmp = TempDir::new().unwrap();
        let config = build_config_with(tmp.path(), "/photos", |_| {});
        let hash = compute_config_hash(&config);
        assert_eq!(
            hash, "9c00642f0507dce7",
            "compute_config_hash golden hash changed -- this will invalidate sync tokens"
        );
    }

    #[test]
    fn golden_compute_config_hash_with_albums() {
        let tmp = TempDir::new().unwrap();
        let config = build_config_with(tmp.path(), "/photos", |s| {
            s.config_overrides.albums = vec![
                "Favorites".to_string(),
                "Travel".to_string(),
                "!Hidden".to_string(),
            ];
        });
        let hash = compute_config_hash(&config);
        assert_eq!(
            hash, "9c00642f0507dce7",
            "album selection should not change the CloudKit zone-token hash"
        );
    }

    #[test]
    fn golden_compute_config_hash_with_smart_folders() {
        // Smart-folder selection is intentionally excluded from the token
        // safety hash. This pins that selection-only changes stay stable.
        let tmp = TempDir::new().unwrap();
        let config = build_config_with(tmp.path(), "/photos", |s| {
            s.config_overrides.smart_folders = vec!["Favorites".to_string(), "Videos".to_string()];
        });
        let hash = compute_config_hash(&config);
        assert_eq!(
            hash, "9c00642f0507dce7",
            "smart-folder selection should not change the CloudKit zone-token hash"
        );
    }

    #[test]
    fn golden_compute_config_hash_with_unfiled_false() {
        // Drift detection for the unfiled branch of the hash. The
        // `unfiled = true` default is implicit in `golden_..._defaults`;
        // this pin covers the explicit-false case so a regression
        // collapsing the two branches is caught.
        let tmp = TempDir::new().unwrap();
        let config = build_config_with(tmp.path(), "/photos", |s| {
            s.config_overrides.unfiled = Some(false);
        });
        let hash = compute_config_hash(&config);
        assert_eq!(
            hash, "c9ea2589956cbb98",
            "compute_config_hash golden hash changed -- this will invalidate sync tokens"
        );
    }

    // ── Gap: DownloadContext attempt_counts used by producer ──────────

    #[test]
    fn download_context_attempt_counts_track_per_asset() {
        let mut ctx = DownloadContext::default();
        ctx.attempt_counts
            .entry("PrimarySync".into())
            .or_default()
            .insert("asset_high".into(), 15);
        ctx.attempt_counts
            .entry("PrimarySync".into())
            .or_default()
            .insert("asset_low".into(), 2);
        ctx.attempt_counts
            .entry("SharedSync-AAAA".into())
            .or_default()
            .insert("asset_high".into(), 1);

        // Simulate the producer's retry-exhaustion check
        let max_attempts = 10u32;
        assert!(
            ctx.attempt_count("PrimarySync", "asset_high")
                .is_some_and(|c| c >= max_attempts),
            "asset_high should exceed max_download_attempts"
        );
        assert!(
            ctx.attempt_count("PrimarySync", "asset_low")
                .is_none_or(|c| c < max_attempts),
            "asset_low should not exceed max_download_attempts"
        );
        assert!(
            ctx.attempt_count("SharedSync-AAAA", "asset_high")
                .is_none_or(|c| c < max_attempts),
            "same asset id in another library should not inherit primary attempts"
        );
        assert!(
            ctx.attempt_count("PrimarySync", "asset_never_failed")
                .is_none(),
            "unknown asset should not be in attempt_counts"
        );
    }

    // ── Gap: should_download_fast with downloaded but different version ──

    #[test]
    fn should_download_fast_downloaded_original_but_medium_requested() {
        // Asset is downloaded as Original, but now we ask about Medium.
        // should_download_fast should return Some(true) because Medium
        // was never downloaded.
        let mut ctx = DownloadContext::default();
        ctx.downloaded_ids
            .entry("PrimarySync".into())
            .or_default()
            .entry("asset_multi".into())
            .or_default()
            .insert("original".into());
        ctx.downloaded_checksums
            .entry("PrimarySync".into())
            .or_default()
            .entry("asset_multi".into())
            .or_default()
            .insert("original".into(), "ck_orig".into());

        assert_eq!(
            ctx.should_download_fast(
                "PrimarySync",
                "asset_multi",
                VersionSizeKey::Medium,
                "ck_med",
                true
            ),
            Some(true),
            "Medium version not in downloaded set should need download"
        );
    }

    // ── Mutation-pinning sibling: operator inversion ──────────
    //
    // The existing tests already assert the decision (Some(true) /
    // Some(false)), not just field equality. What's missing is a test
    // that pins the checksum-comparison **operator**: if a refactor
    // swaps `stored.as_ref() != checksum` for `==`, the decision
    // inverts and every downloaded asset re-downloads (or vice versa).
    //
    // Mutation: in `should_download_fast`, swap `!=` to `==` on the
    // stored-vs-current checksum line. With the existing fixtures,
    // both sides happen to land on `Some(false)` for the matching
    // case via the trust_state path, so the assertion below is the
    // only one in the suite that fails on operator inversion: paired
    // probes with opposite checksum equality, asserting opposite
    // decisions.
    #[test]
    fn should_download_fast_inverts_when_checksum_operator_flips() {
        let mut ctx = DownloadContext::default();
        ctx.downloaded_ids
            .entry("PrimarySync".into())
            .or_default()
            .entry("asset_op".into())
            .or_default()
            .insert("original".into());
        ctx.downloaded_checksums
            .entry("PrimarySync".into())
            .or_default()
            .entry("asset_op".into())
            .or_default()
            .insert("original".into(), "ck_stored".into());

        // Matching checksum + trust_state=true → skip (Some(false)).
        let matching = ctx.should_download_fast(
            "PrimarySync",
            "asset_op",
            VersionSizeKey::Original,
            "ck_stored",
            true,
        );
        // Different checksum + trust_state=true → re-download
        // (Some(true)).
        let different = ctx.should_download_fast(
            "PrimarySync",
            "asset_op",
            VersionSizeKey::Original,
            "ck_changed",
            true,
        );

        // Pin the inversion. If `!=` were swapped for `==`, both probes
        // would return the same Some(_) — collapsing the decision
        // surface. Asserting opposite values catches that.
        assert_eq!(matching, Some(false), "matching checksum must skip");
        assert_eq!(different, Some(true), "changed checksum must re-download");
        assert_ne!(
            matching, different,
            "matching and changed checksums must produce opposite decisions \
             (catches `!=` ↔ `==` operator swap on stored-vs-current compare)"
        );
    }

    // ── Gap: should_download_fast with multiple version sizes ─────────

    #[test]
    fn should_download_fast_multiple_versions_independent() {
        // Both Original and LiveOriginal downloaded, each with own checksum.
        let mut ctx = DownloadContext::default();
        ctx.downloaded_ids
            .entry("PrimarySync".into())
            .or_default()
            .entry("live_asset".into())
            .or_default()
            .insert("original".into());
        ctx.downloaded_ids
            .entry("PrimarySync".into())
            .or_default()
            .entry("live_asset".into())
            .or_default()
            .insert("live_original".into());
        ctx.downloaded_checksums
            .entry("PrimarySync".into())
            .or_default()
            .entry("live_asset".into())
            .or_default()
            .insert("original".into(), "ck_img".into());
        ctx.downloaded_checksums
            .entry("PrimarySync".into())
            .or_default()
            .entry("live_asset".into())
            .or_default()
            .insert("live_original".into(), "ck_mov".into());

        // Image: matching checksum, trusted
        assert_eq!(
            ctx.should_download_fast(
                "PrimarySync",
                "live_asset",
                VersionSizeKey::Original,
                "ck_img",
                true
            ),
            Some(false)
        );
        // MOV: matching checksum, trusted
        assert_eq!(
            ctx.should_download_fast(
                "PrimarySync",
                "live_asset",
                VersionSizeKey::LiveOriginal,
                "ck_mov",
                true
            ),
            Some(false)
        );
        // MOV: changed checksum -- re-download even though image is fine
        assert_eq!(
            ctx.should_download_fast(
                "PrimarySync",
                "live_asset",
                VersionSizeKey::LiveOriginal,
                "ck_mov_v2",
                true
            ),
            Some(true),
            "changed MOV checksum should trigger re-download"
        );
    }

    // ── Gap: retry_only mode filters new assets ──────────────────────

    #[test]
    fn download_context_retry_only_known_ids_filtering() {
        let mut ctx = DownloadContext::default();
        ctx.known_ids
            .entry("PrimarySync".into())
            .or_default()
            .insert("previously_synced".into());

        // Known asset: should_download_fast returns Some(true) (it needs
        // download because it's not in downloaded_ids)
        assert_eq!(
            ctx.should_download_fast(
                "PrimarySync",
                "previously_synced",
                VersionSizeKey::Original,
                "ck",
                true
            ),
            Some(true)
        );
        // The producer checks known_ids separately before forwarding:
        assert!(ctx.is_known("PrimarySync", "previously_synced"));
        assert!(
            !ctx.is_known("PrimarySync", "brand_new_asset"),
            "new asset should not be in known_ids in retry_only mode"
        );
        assert!(
            !ctx.is_known("SharedSync-AAAA", "previously_synced"),
            "same asset id in another library should stay retry-only-new"
        );
    }

    /// `SyncStats::accumulate` is the sole sum used to fold per-library
    /// stats into a cycle-wide total. Pin every counter so a future refactor
    /// (or a new field added without updating `accumulate`) cannot silently
    /// drop one. Touches every numeric field plus `interrupted` plus the
    /// nested `SkipBreakdown`.
    ///
    /// The earlier inline accumulator in `sync_loop::run_cycle` missed
    /// `rate_limited` -- this test pins that field too so the bug cannot
    /// regress.
    #[test]
    fn sync_loop_run_cycle_aggregates_stats_across_libraries() {
        let lib_a = SyncStats {
            assets_seen: 10,
            api_total_at_start: Some(12),
            api_total_at_start_partial: false,
            downloaded: 4,
            failed: 1,
            skipped: SkipBreakdown {
                by_state: 2,
                on_disk: 3,
                by_media_type: 4,
                by_date_range: 5,
                by_live_photo: 6,
                by_filename: 7,
                by_excluded_album: 8,
                ampm_variant: 9,
                duplicates: 10,
                retry_exhausted: 11,
                retry_only: 12,
            },
            bytes_downloaded: 1_000,
            disk_bytes_written: 900,
            exif_failures: 1,
            metadata_capture_revision: Some(1),
            metadata_capture_refreshed: 2,
            metadata_capture_failures: 1,
            metadata_capture_remaining: 5,
            metadata_capture_progressed: false,
            state_write_failures: 2,
            enumeration_errors: 3,
            count_probe_failures: 4,
            stale_pending_pruned: 5,
            pagination_shortfall_warnings: 1,
            pagination_shortfall_assets: 9,
            tail_probes: 2,
            count_undercount_assets: 3,
            enumeration_incomplete: false,
            inventory_drop_warnings: 1,
            inventory_drop_assets: 5,
            inventory_drop_percent: Some(5.0),
            inventory_drop_previous_total: Some(100),
            inventory_drop_current_total: Some(95),
            inventory_drop_library: Some("PrimarySync".to_string()),
            sync_token_blocked: true,
            sync_token_blocked_reason: Some("icloud_blank_sync_token"),
            sync_token_blocked_source: Some("icloud"),
            sync_token_blocked_explanation: Some(sync_token_blocked_explanation(
                "icloud_blank_sync_token",
            )),
            sync_token_blocked_zone: Some("PrimarySync".to_string()),
            sync_token_expected_receivers: Some(3),
            sync_token_receivers_with_token: Some(2),
            sync_token_receivers_missing: Some(1),
            sync_token_receivers_blank: Some(0),
            sync_token_receivers_dropped: Some(0),
            sync_token_unique_values: Some(1),
            same_cycle_recovery_attempts: 1,
            same_cycle_recovery_successes: 1,
            checkpoint_retry_passes: vec![PassKey {
                index: 0,
                kind: PassKind::Album,
                label: "album".to_string(),
            }],
            checkpoint_revalidate_records: vec![ProviderRecordId::new("master-a")],
            full_enumeration_reason: Some(FullEnumerationReason::NoStoredToken),
            elapsed_secs: 1.5,
            interrupted: false,
            rate_limited: 7,
            photos_downloaded: 3,
            videos_downloaded: 1,
            recap: recap::RunRecap::default(),
        };

        let lib_b = SyncStats {
            assets_seen: 20,
            api_total_at_start: Some(22),
            api_total_at_start_partial: true,
            downloaded: 11,
            failed: 2,
            skipped: SkipBreakdown {
                by_state: 1,
                on_disk: 1,
                by_media_type: 1,
                by_date_range: 1,
                by_live_photo: 1,
                by_filename: 1,
                by_excluded_album: 1,
                ampm_variant: 1,
                duplicates: 1,
                retry_exhausted: 1,
                retry_only: 1,
            },
            bytes_downloaded: 2_500,
            disk_bytes_written: 2_400,
            exif_failures: 4,
            metadata_capture_revision: Some(1),
            metadata_capture_refreshed: 3,
            metadata_capture_failures: 2,
            metadata_capture_remaining: 7,
            metadata_capture_progressed: true,
            state_write_failures: 5,
            enumeration_errors: 6,
            count_probe_failures: 7,
            stale_pending_pruned: 8,
            pagination_shortfall_warnings: 2,
            pagination_shortfall_assets: 11,
            tail_probes: 4,
            count_undercount_assets: 5,
            enumeration_incomplete: true,
            inventory_drop_warnings: 2,
            inventory_drop_assets: 11,
            inventory_drop_percent: Some(10.0),
            inventory_drop_previous_total: Some(110),
            inventory_drop_current_total: Some(99),
            inventory_drop_library: Some("SharedSync-abc".to_string()),
            sync_token_blocked: false,
            sync_token_blocked_reason: None,
            sync_token_blocked_source: Some("kei"),
            sync_token_blocked_explanation: Some("should not overwrite first"),
            sync_token_blocked_zone: Some("SharedSync-abc".to_string()),
            sync_token_expected_receivers: Some(9),
            sync_token_receivers_with_token: Some(9),
            sync_token_receivers_missing: Some(0),
            sync_token_receivers_blank: Some(0),
            sync_token_receivers_dropped: Some(0),
            sync_token_unique_values: Some(1),
            same_cycle_recovery_attempts: 2,
            same_cycle_recovery_successes: 1,
            checkpoint_retry_passes: vec![PassKey {
                index: 1,
                kind: PassKind::Unfiled,
                label: "unfiled".to_string(),
            }],
            checkpoint_revalidate_records: vec![ProviderRecordId::new("master-b")],
            full_enumeration_reason: Some(FullEnumerationReason::MetadataBackfill),
            elapsed_secs: 0.75,
            interrupted: true,
            rate_limited: 3,
            photos_downloaded: 8,
            videos_downloaded: 3,
            recap: recap::RunRecap::default(),
        };

        let mut acc = SyncStats::default();
        acc.accumulate(&lib_a);
        acc.accumulate(&lib_b);

        assert_eq!(acc.assets_seen, 30, "assets_seen must sum");
        assert_eq!(
            acc.api_total_at_start,
            Some(34),
            "api_total_at_start must sum known library totals"
        );
        assert!(
            acc.api_total_at_start_partial,
            "api_total_at_start_partial must OR"
        );
        assert_eq!(acc.downloaded, 15, "downloaded must sum");
        assert_eq!(acc.failed, 3, "failed must sum");
        assert_eq!(acc.bytes_downloaded, 3_500, "bytes_downloaded must sum");
        assert_eq!(acc.disk_bytes_written, 3_300, "disk_bytes_written must sum");
        assert_eq!(acc.exif_failures, 5, "exif_failures must sum");
        assert_eq!(acc.metadata_capture_revision, Some(1));
        assert_eq!(acc.metadata_capture_refreshed, 5);
        assert_eq!(acc.metadata_capture_failures, 3);
        assert_eq!(acc.metadata_capture_remaining, 12);
        assert!(acc.metadata_capture_progressed);
        assert_eq!(acc.state_write_failures, 7, "state_write_failures must sum");
        assert_eq!(acc.enumeration_errors, 9, "enumeration_errors must sum");
        assert_eq!(
            acc.count_probe_failures, 11,
            "count_probe_failures must sum"
        );
        assert_eq!(
            acc.stale_pending_pruned, 13,
            "stale_pending_pruned must sum"
        );
        assert_eq!(
            acc.pagination_shortfall_warnings, 3,
            "pagination shortfall warnings must sum"
        );
        assert_eq!(
            acc.pagination_shortfall_assets, 20,
            "pagination shortfall assets must sum"
        );
        assert_eq!(acc.tail_probes, 6, "tail probes must sum");
        assert_eq!(
            acc.count_undercount_assets, 8,
            "count undercount assets must sum"
        );
        assert!(acc.enumeration_incomplete, "enumeration_incomplete must OR");
        assert_eq!(
            acc.inventory_drop_warnings, 3,
            "inventory drop warnings must sum"
        );
        assert_eq!(
            acc.inventory_drop_assets, 11,
            "largest inventory drop must win"
        );
        assert_eq!(acc.inventory_drop_percent, Some(10.0));
        assert_eq!(acc.inventory_drop_previous_total, Some(110));
        assert_eq!(acc.inventory_drop_current_total, Some(99));
        assert_eq!(
            acc.inventory_drop_library,
            Some("SharedSync-abc".to_string())
        );
        assert!(acc.sync_token_blocked, "sync_token_blocked must OR");
        assert_eq!(
            acc.sync_token_blocked_reason,
            Some("icloud_blank_sync_token")
        );
        assert_eq!(acc.sync_token_blocked_source, Some("icloud"));
        assert_eq!(
            acc.sync_token_blocked_explanation,
            Some(sync_token_blocked_explanation("icloud_blank_sync_token"))
        );
        assert_eq!(acc.sync_token_blocked_zone.as_deref(), Some("PrimarySync"));
        assert_eq!(acc.sync_token_expected_receivers, Some(3));
        assert_eq!(acc.sync_token_receivers_with_token, Some(2));
        assert_eq!(acc.sync_token_receivers_missing, Some(1));
        assert_eq!(acc.sync_token_receivers_blank, Some(0));
        assert_eq!(acc.sync_token_receivers_dropped, Some(0));
        assert_eq!(acc.sync_token_unique_values, Some(1));
        assert_eq!(acc.same_cycle_recovery_attempts, 3);
        assert_eq!(acc.same_cycle_recovery_successes, 2);
        assert_eq!(acc.checkpoint_retry_passes.len(), 2);
        assert_eq!(acc.checkpoint_revalidate_records.len(), 2);
        assert_eq!(
            acc.full_enumeration_reason,
            Some(FullEnumerationReason::NoStoredToken)
        );
        assert!(
            (acc.elapsed_secs - 2.25).abs() < 1e-9,
            "elapsed_secs must sum (got {})",
            acc.elapsed_secs
        );
        assert!(
            acc.interrupted,
            "interrupted must OR -- any library interrupted -> cycle interrupted"
        );
        assert_eq!(
            acc.rate_limited, 10,
            "rate_limited must sum -- pre-fix the inline accumulator dropped this field"
        );

        assert_eq!(acc.skipped.by_state, 3);
        assert_eq!(acc.skipped.on_disk, 4);
        assert_eq!(acc.skipped.by_media_type, 5);
        assert_eq!(acc.skipped.by_date_range, 6);
        assert_eq!(acc.skipped.by_live_photo, 7);
        assert_eq!(acc.skipped.by_filename, 8);
        assert_eq!(acc.skipped.by_excluded_album, 9);
        assert_eq!(acc.skipped.ampm_variant, 10);
        assert_eq!(acc.skipped.duplicates, 11);
        assert_eq!(acc.skipped.retry_exhausted, 12);
        assert_eq!(acc.skipped.retry_only, 13);
        assert_eq!(
            acc.skipped.total(),
            3 + 4 + 5 + 6 + 7 + 8 + 9 + 10 + 11 + 12 + 13,
            "skip total must reflect summed breakdown"
        );
    }

    /// When multiple libraries block token advancement in one cycle, the
    /// aggregated cycle stats preserve the first blocked diagnostic payload.
    #[test]
    fn sync_stats_accumulate_preserves_first_token_blocked_diagnostics() {
        let first = SyncStats {
            sync_token_blocked: true,
            sync_token_blocked_reason: Some("icloud_blank_sync_token"),
            sync_token_blocked_source: Some("icloud"),
            sync_token_blocked_explanation: Some(sync_token_blocked_explanation(
                "icloud_blank_sync_token",
            )),
            sync_token_blocked_zone: Some("PrimarySync".to_string()),
            sync_token_expected_receivers: Some(2),
            sync_token_receivers_with_token: Some(0),
            sync_token_receivers_missing: Some(0),
            sync_token_receivers_blank: Some(2),
            sync_token_receivers_dropped: Some(0),
            sync_token_unique_values: Some(0),
            ..SyncStats::default()
        };
        let second = SyncStats {
            sync_token_blocked: true,
            sync_token_blocked_reason: Some("icloud_sync_token_mismatch"),
            sync_token_blocked_source: Some("icloud"),
            sync_token_blocked_explanation: Some(sync_token_blocked_explanation(
                "icloud_sync_token_mismatch",
            )),
            sync_token_blocked_zone: Some("SharedSync-XYZ".to_string()),
            sync_token_expected_receivers: Some(3),
            sync_token_receivers_with_token: Some(3),
            sync_token_receivers_missing: Some(0),
            sync_token_receivers_blank: Some(0),
            sync_token_receivers_dropped: Some(0),
            sync_token_unique_values: Some(2),
            ..SyncStats::default()
        };

        let mut acc = SyncStats::default();
        acc.accumulate(&first);
        acc.accumulate(&second);

        assert!(acc.sync_token_blocked);
        assert_eq!(
            acc.sync_token_blocked_reason,
            first.sync_token_blocked_reason
        );
        assert_eq!(
            acc.sync_token_blocked_source,
            first.sync_token_blocked_source
        );
        assert_eq!(
            acc.sync_token_blocked_explanation,
            first.sync_token_blocked_explanation
        );
        assert_eq!(acc.sync_token_blocked_zone, first.sync_token_blocked_zone);
        assert_eq!(
            acc.sync_token_expected_receivers,
            first.sync_token_expected_receivers
        );
        assert_eq!(
            acc.sync_token_receivers_with_token,
            first.sync_token_receivers_with_token
        );
        assert_eq!(
            acc.sync_token_receivers_missing,
            first.sync_token_receivers_missing
        );
        assert_eq!(
            acc.sync_token_receivers_blank,
            first.sync_token_receivers_blank
        );
        assert_eq!(
            acc.sync_token_receivers_dropped,
            first.sync_token_receivers_dropped
        );
        assert_eq!(acc.sync_token_unique_values, first.sync_token_unique_values);
    }

    /// A transient `pass.album.len()` failure must not
    /// reduce to a 0-count that silently advances the sync token. Folding
    /// the per-pass results must surface the failure as an error count so
    /// downstream gates can suppress token advancement.
    #[test]
    fn fold_pass_count_results_counts_errors_and_zeroes_failed_passes() {
        use crate::commands::{AlbumPass, PassKind};
        use crate::icloud::photos::PhotoAlbum;
        use rustc_hash::FxHashSet;
        use std::sync::Arc;

        let passes = vec![
            AlbumPass {
                kind: PassKind::Album,
                album: PhotoAlbum::stub_for_test(Arc::from("album_a")),
                exclude_ids: Arc::new(FxHashSet::default()),
            },
            AlbumPass {
                kind: PassKind::Album,
                album: PhotoAlbum::stub_for_test(Arc::from("album_b")),
                exclude_ids: Arc::new(FxHashSet::default()),
            },
            AlbumPass {
                kind: PassKind::Album,
                album: PhotoAlbum::stub_for_test(Arc::from("album_c")),
                exclude_ids: Arc::new(FxHashSet::default()),
            },
        ];

        // First and third pass succeed; second pass fails (transient len()
        // error). The failed pass must contribute 0 to the counts vector and
        // increment the error count by exactly 1.
        let results = vec![
            Ok(100),
            Err(anyhow::anyhow!("simulated transient len() failure")),
            Ok(50),
        ];

        let (counts, errors) = fold_pass_count_results(results, &passes);

        assert_eq!(counts, vec![100, 0, 50]);
        assert_eq!(
            errors, 1,
            "exactly one len() error must surface so token advancement is blocked"
        );
    }

    /// Pin the all-failures case: every pass's `len()` errors out → counts
    /// are all zero AND the error count equals the pass count, so the cycle
    /// cannot be mistaken for a clean empty enumeration.
    #[test]
    fn fold_pass_count_results_all_errors_yields_full_error_count() {
        use crate::commands::{AlbumPass, PassKind};
        use crate::icloud::photos::PhotoAlbum;
        use rustc_hash::FxHashSet;
        use std::sync::Arc;

        let passes = vec![
            AlbumPass {
                kind: PassKind::Album,
                album: PhotoAlbum::stub_for_test(Arc::from("album_a")),
                exclude_ids: Arc::new(FxHashSet::default()),
            },
            AlbumPass {
                kind: PassKind::Album,
                album: PhotoAlbum::stub_for_test(Arc::from("album_b")),
                exclude_ids: Arc::new(FxHashSet::default()),
            },
        ];

        let results = vec![
            Err(anyhow::anyhow!("first failure")),
            Err(anyhow::anyhow!("second failure")),
        ];

        let (counts, errors) = fold_pass_count_results(results, &passes);

        assert_eq!(counts, vec![0, 0]);
        assert_eq!(errors, 2);
    }

    #[test]
    fn recent_runs_skip_pass_count_fetch() {
        let mut config = test_config();
        config.recent = Some(25);

        assert!(
            should_skip_pass_count_fetch(&config),
            "recent-limited runs are not complete enumerations, so the \
             full-pass count is not an exact pagination bound"
        );
    }

    #[test]
    fn skip_created_before_runs_skip_pass_count_fetch() {
        let mut config = test_config();
        config.skip_created_before = Some(crate::config::CreatedDateFilter::Instant(
            DateTime::from_timestamp_millis(1_700_000_000_000).expect("valid test timestamp"),
        ));

        assert!(
            should_skip_pass_count_fetch(&config),
            "lower-date-bounded runs stop before the full pass is drained, so \
             the full-pass count is not an exact pagination bound"
        );
    }

    #[test]
    fn stream_created_lower_bound_uses_the_stricter_date_or_recent_frontier() {
        let capture_date = chrono::NaiveDate::from_ymd_opt(2025, 2, 1).unwrap();
        let date_bound =
            capture_date.and_time(chrono::NaiveTime::MIN).and_utc() - chrono::Duration::days(1);
        let mut config = test_config();
        config.skip_created_before =
            Some(crate::config::CreatedDateFilter::CaptureDate(capture_date));

        for (name, frontier_bound, expected) in [
            (
                "date bound",
                date_bound - chrono::Duration::hours(1),
                date_bound,
            ),
            (
                "recent frontier",
                date_bound + chrono::Duration::hours(1),
                date_bound + chrono::Duration::hours(1),
            ),
        ] {
            let frontier = RecentFrontier {
                asset_ids: Arc::new(FxHashSet::default()),
                oldest_created: Some(frontier_bound),
            };
            assert_eq!(
                stream_created_lower_bound(&config, Some(&frontier)),
                Some(expected),
                "{name}"
            );
        }
    }

    #[test]
    fn skip_created_after_runs_keep_pass_count_fetch() {
        let mut config = test_config();
        config.skip_created_after = Some(crate::config::CreatedDateFilter::Instant(
            DateTime::from_timestamp_millis(1_700_000_000_000).expect("valid test timestamp"),
        ));

        assert!(
            !should_skip_pass_count_fetch(&config),
            "upper-date filters must still drain the stream because older \
             assets after the skipped prefix can still match"
        );
    }

    #[test]
    fn unbounded_runs_keep_pass_count_fetch() {
        let mut config = test_config();
        config.recent = None;

        assert!(
            !should_skip_pass_count_fetch(&config),
            "unbounded runs still use exact counts for progress bounds and \
             pagination-underflow detection"
        );
    }

    #[tokio::test]
    async fn build_pass_count_plan_uses_recent_bound_without_exact_counts_for_read_only_runs() {
        use crate::commands::{AlbumPass, PassKind};
        use crate::icloud::photos::PhotoAlbum;
        use rustc_hash::FxHashSet;
        use std::sync::Arc;

        let passes = vec![
            AlbumPass {
                kind: PassKind::Album,
                album: PhotoAlbum::stub_for_test(Arc::from("album_a")),
                exclude_ids: Arc::new(FxHashSet::default()),
            },
            AlbumPass {
                kind: PassKind::Album,
                album: PhotoAlbum::stub_for_test(Arc::from("album_b")),
                exclude_ids: Arc::new(FxHashSet::default()),
            },
        ];
        let mut config = test_config();
        config.recent = Some(10);

        let plan =
            build_pass_count_plan(&passes, &config, DownloadControls::dry_run_hidden()).await;

        assert_eq!(plan.display_counts, vec![10, 10]);
        assert_eq!(plan.stream_total_counts, vec![None, None]);
        assert_eq!(plan.exact_total, None);
        assert_eq!(plan.len_errors, 0);
    }

    #[tokio::test]
    async fn full_sync_recent_album_passes_use_scope_frontier() {
        let all_assets = recent_scope_assets("frontier", 300, 1_700_000_000_000);
        let mut album_assets = vec![all_assets[0].clone()];
        album_assets.extend(recent_scope_assets("old-album", 500, 1_699_000_000_000));
        let asset = recent_scope_photo_asset(&all_assets[0]);
        let session = RecentScopeSession::new(all_assets, album_assets);

        let passes: Vec<AlbumPass> = (0..10)
            .map(|index| AlbumPass {
                kind: PassKind::Album,
                album: recent_scope_album(&format!("Album {index}"), session.clone()),
                exclude_ids: Arc::new(FxHashSet::default()),
            })
            .collect();

        let mut config = test_config();
        let dir = TempDir::new().expect("temp dir");
        config.directory = Arc::from(dir.path());
        config.concurrent_downloads = 1;
        config.recent = Some(300);
        for pass in &passes {
            seed_existing_file_for_asset(&mut config, pass, &asset).await;
        }

        let result = download_photos_full_with_token(
            &Client::new(),
            &passes,
            &Arc::new(config),
            DownloadControls::download_hidden(),
            CancellationToken::new(),
        )
        .await
        .expect("scope-frontier recent sync should complete");

        assert!(matches!(result.outcome, DownloadOutcome::Success));
        assert_eq!(
            result.stats.assets_seen,
            10,
            "each album should plan only assets inside the library-wide recent frontier; offsets={:?}",
            session.album_offsets()
        );
        assert_eq!(result.stats.pagination_shortfall_warnings, 0);
        assert_eq!(result.stats.enumeration_errors, 0);
        assert!(
            session.album_offsets().len() < 100,
            "album enumeration should stop near the frontier boundary instead of \
             applying recent=300 to every album pass"
        );
        assert_eq!(result.sync_token, None);
    }

    #[tokio::test]
    async fn full_sync_recent_per_filter_scope_limits_each_pass_independently() {
        let all_assets = recent_scope_assets("frontier", 6, 1_700_000_000_000);
        let mut album_assets = vec![all_assets[0].clone()];
        album_assets.extend(recent_scope_assets(
            "old-per-filter-album",
            20,
            1_699_000_000_000,
        ));
        let expected_assets = album_assets
            .iter()
            .take(6)
            .map(recent_scope_photo_asset)
            .collect::<Vec<_>>();
        let session = RecentScopeSession::new(all_assets, album_assets);

        let passes: Vec<AlbumPass> = (0..3)
            .map(|index| AlbumPass {
                kind: PassKind::Album,
                album: recent_scope_album(&format!("Album {index}"), session.clone()),
                exclude_ids: Arc::new(FxHashSet::default()),
            })
            .collect();

        let mut config = test_config();
        let dir = TempDir::new().expect("temp dir");
        config.directory = Arc::from(dir.path());
        config.concurrent_downloads = 1;
        config.recent = Some(6);
        config.recent_scope = crate::cli::RecentScope::PerFilter;

        for pass in &passes {
            for asset in &expected_assets {
                seed_existing_file_for_asset(&mut config, pass, asset).await;
            }
        }

        let result = download_photos_full_with_token(
            &Client::new(),
            &passes,
            &Arc::new(config),
            DownloadControls::download_hidden(),
            CancellationToken::new(),
        )
        .await
        .expect("per-filter recent sync should complete");

        assert!(matches!(result.outcome, DownloadOutcome::Success));
        assert_eq!(
            result.stats.assets_seen, 18,
            "per-filter recent scope should take the recent limit from each album pass"
        );
        assert_eq!(result.stats.pagination_shortfall_warnings, 0);
        assert_eq!(result.stats.enumeration_errors, 0);
        assert!(
            session.album_offsets().len() >= 9,
            "per-filter scope should enumerate each album's recent window, not stop at the global frontier"
        );
        assert_eq!(result.sync_token, None);
    }

    #[tokio::test]
    async fn full_sync_recent_single_album_filters_library_frontier() {
        let all_assets = recent_scope_assets("frontier", 300, 1_700_000_000_000);
        let mut album_assets = vec![all_assets[0].clone()];
        album_assets.extend(recent_scope_assets(
            "old-single-album",
            500,
            1_699_000_000_000,
        ));
        let asset = recent_scope_photo_asset(&all_assets[0]);
        let session = RecentScopeSession::new(all_assets, album_assets);
        let passes = vec![AlbumPass {
            kind: PassKind::Album,
            album: recent_scope_album("Vacation", session.clone()),
            exclude_ids: Arc::new(FxHashSet::default()),
        }];

        let mut config = test_config();
        let dir = TempDir::new().expect("temp dir");
        config.directory = Arc::from(dir.path());
        config.concurrent_downloads = 1;
        config.recent = Some(300);
        seed_existing_file_for_asset(&mut config, &passes[0], &asset).await;

        let result = download_photos_full_with_token(
            &Client::new(),
            &passes,
            &Arc::new(config),
            DownloadControls::download_hidden(),
            CancellationToken::new(),
        )
        .await
        .expect("single-album scope-frontier recent sync should complete");

        assert!(matches!(result.outcome, DownloadOutcome::Success));
        assert_eq!(
            result.stats.assets_seen, 1,
            "album filter should pare down the library-wide recent frontier"
        );
        assert_eq!(result.stats.pagination_shortfall_warnings, 0);
        assert_eq!(result.stats.enumeration_errors, 0);
        assert!(
            session.album_offsets().len() < 10,
            "single album enumeration should stop at the frontier boundary"
        );
        assert_eq!(result.sync_token, None);
        assert!(result.stats.sync_token_blocked);
        assert_eq!(
            result.stats.sync_token_blocked_reason,
            Some(RECENT_LIMITED_FULL_ENUMERATION_REASON)
        );
    }

    #[tokio::test]
    async fn full_sync_skip_created_before_stops_at_date_boundary() {
        let newer_assets = recent_scope_assets("date-new", 5, 1_700_000_000_000);
        let older_assets = recent_scope_assets("date-old", 20, 1_699_000_000_000);
        let mut album_assets = newer_assets.clone();
        album_assets.extend(older_assets);
        let session = RecentScopeSession::new(album_assets.clone(), album_assets);
        let passes = vec![AlbumPass {
            kind: PassKind::Album,
            album: recent_scope_album("Vacation", session.clone()),
            exclude_ids: Arc::new(FxHashSet::default()),
        }];

        let mut config = test_config();
        let dir = TempDir::new().expect("temp dir");
        config.directory = Arc::from(dir.path());
        config.concurrent_downloads = 1;
        config.skip_created_before = Some(crate::config::CreatedDateFilter::Instant(
            DateTime::from_timestamp_millis(1_699_999_000_000).expect("valid test timestamp"),
        ));
        for asset in newer_assets.iter().map(recent_scope_photo_asset) {
            seed_existing_file_for_asset(&mut config, &passes[0], &asset).await;
        }

        let result = download_photos_full_with_token(
            &Client::new(),
            &passes,
            &Arc::new(config),
            DownloadControls::download_hidden(),
            CancellationToken::new(),
        )
        .await
        .expect("date-bounded full sync should complete");

        assert!(matches!(result.outcome, DownloadOutcome::Success));
        assert_eq!(
            result.stats.assets_seen, 5,
            "lower-date-bound enumeration should stop before older assets are \
             handed to the download pipeline"
        );
        assert_eq!(result.stats.pagination_shortfall_warnings, 0);
        assert_eq!(result.stats.enumeration_errors, 0);
        assert!(
            session.album_offsets().len() <= 5,
            "lower-date-bound enumeration should stop near the first old page; offsets={:?}",
            session.album_offsets()
        );
        assert_eq!(
            result.sync_token, None,
            "date-bounded full sync must not advance a zone token"
        );
        assert!(result.stats.sync_token_blocked);
        assert_eq!(
            result.stats.sync_token_blocked_reason,
            Some(DATE_BOUNDED_FULL_ENUMERATION_REASON)
        );
    }

    #[tokio::test]
    async fn full_sync_skip_created_before_saves_token_when_boundary_does_not_truncate() {
        let newer_assets = recent_scope_assets("date-newer-only", 5, 1_700_000_000_000);
        let session = RecentScopeSession::new(newer_assets.clone(), newer_assets.clone());
        let passes = vec![AlbumPass {
            kind: PassKind::Album,
            album: recent_scope_album("Vacation", session),
            exclude_ids: Arc::new(FxHashSet::default()),
        }];

        let mut config = test_config();
        let dir = TempDir::new().expect("temp dir");
        config.directory = Arc::from(dir.path());
        config.concurrent_downloads = 1;
        config.skip_created_before = Some(crate::config::CreatedDateFilter::Instant(
            DateTime::from_timestamp_millis(1_699_000_000_000).expect("valid test timestamp"),
        ));
        for asset in newer_assets.iter().map(recent_scope_photo_asset) {
            seed_existing_file_for_asset(&mut config, &passes[0], &asset).await;
        }

        let result = download_photos_full_with_token(
            &Client::new(),
            &passes,
            &Arc::new(config),
            DownloadControls::download_hidden(),
            CancellationToken::new(),
        )
        .await
        .expect("non-truncating date-bounded full sync should complete");

        assert!(matches!(result.outcome, DownloadOutcome::Success));
        assert_eq!(result.stats.assets_seen, 5);
        assert_eq!(result.stats.enumeration_errors, 0);
        assert_eq!(result.sync_token.as_deref(), Some("zone-token"));
        assert!(!result.stats.sync_token_blocked);
    }

    #[tokio::test]
    async fn full_sync_skip_created_after_drains_past_newer_prefix() {
        let newer_assets = recent_scope_assets("date-after-new", 5, 1_700_000_000_000);
        let older_assets = recent_scope_assets("date-after-old", 5, 1_699_000_000_000);
        let mut album_assets = newer_assets;
        album_assets.extend(older_assets.clone());
        let session = RecentScopeSession::new(album_assets.clone(), album_assets);
        let passes = vec![AlbumPass {
            kind: PassKind::Album,
            album: recent_scope_album("Vacation", session.clone()),
            exclude_ids: Arc::new(FxHashSet::default()),
        }];

        let mut config = test_config();
        let dir = TempDir::new().expect("temp dir");
        config.directory = Arc::from(dir.path());
        config.concurrent_downloads = 1;
        config.skip_created_after = Some(crate::config::CreatedDateFilter::Instant(
            DateTime::from_timestamp_millis(1_699_999_000_000).expect("valid test timestamp"),
        ));
        for asset in older_assets.iter().map(recent_scope_photo_asset) {
            seed_existing_file_for_asset(&mut config, &passes[0], &asset).await;
        }

        let result = download_photos_full_with_token(
            &Client::new(),
            &passes,
            &Arc::new(config),
            DownloadControls::download_hidden(),
            CancellationToken::new(),
        )
        .await
        .expect("upper-date-filtered full sync should complete");

        assert!(matches!(result.outcome, DownloadOutcome::Success));
        assert_eq!(
            result.stats.assets_seen, 10,
            "upper-date filters skip the newer prefix but must keep enumerating \
             because older assets can still match"
        );
        assert_eq!(result.stats.pagination_shortfall_warnings, 0);
        assert_eq!(result.stats.enumeration_errors, 0);
        assert!(
            session.album_offsets().len() > 3,
            "upper-date filters must not stop near the newer prefix; offsets={:?}",
            session.album_offsets()
        );
        assert_eq!(result.sync_token.as_deref(), Some("zone-token"));
    }

    #[tokio::test]
    async fn full_sync_recent_download_saves_token_when_cap_does_not_bind() {
        let records = mock_photo_records_with_filename("MASTER_RECENT", "recent.jpg");
        let album_session = MockPhotosFlow::new()
            .query_page(records.clone(), Some("zone-token"))
            .empty_query_page(Some("zone-token"))
            .build();
        let passes = vec![AlbumPass {
            kind: PassKind::Album,
            album: mock_album("Vacation", album_session),
            exclude_ids: Arc::new(FxHashSet::default()),
        }];

        let mut config = test_config();
        let dir = TempDir::new().expect("temp dir");
        config.directory = Arc::from(dir.path());
        config.recent = Some(100);
        config.file_match_policy = FileMatchPolicy::NameId7;

        let asset = PhotoAsset::new(records[0].clone(), records[1].clone());
        let pass_config = config.with_pass(&passes[0]);
        let expected_path = filter::expected_paths_for(&asset, &pass_config)
            .into_iter()
            .next()
            .expect("mock asset should have an expected path");
        tokio::fs::create_dir_all(expected_path.path.parent().expect("path has parent"))
            .await
            .expect("create parent dir");
        tokio::fs::write(&expected_path.path, vec![0u8; 1024])
            .await
            .expect("seed existing file");
        seed_downloaded_state_for_expected_path(&mut config, &pass_config, &asset, &expected_path)
            .await;

        let result = download_photos_full_with_token(
            &Client::new(),
            &passes,
            &Arc::new(config),
            DownloadControls::download_hidden(),
            CancellationToken::new(),
        )
        .await
        .expect("recent full sync should complete");

        assert!(
            matches!(result.outcome, DownloadOutcome::Success),
            "a sparse recent window must not be treated as pagination undercount"
        );
        assert_eq!(
            result.stats.enumeration_errors, 0,
            "recent-limited count shortfalls are not exact enumeration errors"
        );
        assert_eq!(
            result.stats.api_total_at_start, None,
            "recent-limited runs must not seed comparable inventory totals"
        );
        assert_eq!(
            result.sync_token.as_deref(),
            Some("zone-token"),
            "non-binding recent cap should still produce a complete-zone token"
        );
        assert!(!result.stats.sync_token_blocked);
        assert_eq!(result.stats.sync_token_expected_receivers, Some(1));
    }

    #[tokio::test]
    async fn full_sync_recent_download_drains_multiple_reduced_pages() {
        let ids = recent_ids("recent-prod", 100);
        let session = DynamicRecentPhotosSession::from_ids(ids.clone())
            .with_filename_prefix("recent-prod")
            .with_token("zone-token");
        let passes = vec![AlbumPass {
            kind: PassKind::Album,
            album: album_with_session("PrimarySync", "Vacation", Box::new(session.clone())),
            exclude_ids: Arc::new(FxHashSet::default()),
        }];

        let mut config = test_config();
        let dir = TempDir::new().expect("temp dir");
        config.directory = Arc::from(dir.path());
        config.concurrent_downloads = 10;
        config.recent = Some(100);
        seed_existing_recent_files(&mut config, &passes[0], "PrimarySync", &ids, "recent-prod")
            .await;

        let result = download_photos_full_with_token(
            &Client::new(),
            &passes,
            &Arc::new(config),
            DownloadControls::download_hidden(),
            CancellationToken::new(),
        )
        .await
        .expect("recent full sync should complete");

        assert!(matches!(result.outcome, DownloadOutcome::Success));
        assert_eq!(result.stats.assets_seen, 100);
        assert_eq!(result.stats.enumeration_errors, 0);
        assert_eq!(
            result.sync_token.as_deref(),
            Some("zone-token"),
            "a recent run that still drains the selected stream to EOF should advance the zone token"
        );
        assert!(!result.stats.sync_token_blocked);
        assert!(
            session.offsets().len() >= 5,
            "write-mode full sync should drain every reduced download page"
        );
    }

    async fn run_recent_mode_for_ids(
        mode: DownloadRunMode,
        ids: &[String],
        filename_prefix: &str,
    ) -> (SyncResult, Vec<String>) {
        let session = DynamicRecentPhotosSession::from_ids(ids.to_vec())
            .with_filename_prefix(filename_prefix)
            .with_token("zone-token");
        let passes = vec![AlbumPass {
            kind: PassKind::Album,
            album: album_with_session("PrimarySync", "Vacation", Box::new(session.clone())),
            exclude_ids: Arc::new(FxHashSet::default()),
        }];

        let mut config = test_config();
        let dir = TempDir::new().expect("temp dir");
        config.directory = Arc::from(dir.path());
        config.concurrent_downloads = 1;
        config.recent = Some(ids.len().try_into().expect("test id count fits u32"));
        if matches!(mode, DownloadRunMode::Download) {
            seed_existing_recent_files(
                &mut config,
                &passes[0],
                "PrimarySync",
                ids,
                filename_prefix,
            )
            .await;
        }

        let result = download_photos_full_with_token(
            &Client::new(),
            &passes,
            &Arc::new(config),
            DownloadControls::new(mode, DownloadReporting::hidden()),
            CancellationToken::new(),
        )
        .await
        .expect("recent mode sync should complete");

        (result, unique_ids_in_order(session.emitted_ids()))
    }

    #[tokio::test]
    async fn full_sync_recent_run_modes_enumerate_same_asset_ids() {
        let ids = recent_ids("mode-parity", 6);

        let (print_result, print_ids) =
            run_recent_mode_for_ids(DownloadRunMode::PrintFilenames, &ids, "mode-parity").await;
        let (dry_result, dry_ids) =
            run_recent_mode_for_ids(DownloadRunMode::DryRun, &ids, "mode-parity").await;
        let (download_result, download_ids) =
            run_recent_mode_for_ids(DownloadRunMode::Download, &ids, "mode-parity").await;

        assert!(matches!(print_result.outcome, DownloadOutcome::Success));
        assert!(matches!(dry_result.outcome, DownloadOutcome::Success));
        assert!(matches!(download_result.outcome, DownloadOutcome::Success));
        assert_eq!(print_ids, ids);
        assert_eq!(dry_ids, ids);
        assert_eq!(download_ids, ids);
        assert_eq!(print_ids, dry_ids);
        assert_eq!(dry_ids, download_ids);
        assert_eq!(download_result.stats.assets_seen, ids.len() as u64);
        assert_eq!(print_result.sync_token, None);
        assert_eq!(dry_result.sync_token, None);
        assert_eq!(
            download_result.sync_token.as_deref(),
            Some("zone-token"),
            "download mode may advance when the recent cap is exactly the selected stream"
        );
    }

    #[tokio::test]
    async fn full_sync_recent_deferred_unfiled_filters_album_members_after_multi_page_stream() {
        let album_ids = recent_ids("album-member", 40);
        let unfiled_only_ids = recent_ids("unfiled-only", 20);
        let mut unfiled_ids = album_ids.clone();
        unfiled_ids.extend(unfiled_only_ids.clone());

        let album_session = DynamicRecentPhotosSession::from_ids(album_ids.clone())
            .with_filename_prefix("album-member")
            .with_token("zone-token");
        let unfiled_session = DynamicRecentPhotosSession::from_ids(unfiled_ids.clone())
            .with_filename_prefix("unfiled-mixed")
            .with_token("zone-token");
        let passes = vec![
            AlbumPass {
                kind: PassKind::Album,
                album: album_with_session("PrimarySync", "Vacation", Box::new(album_session)),
                exclude_ids: Arc::new(FxHashSet::default()),
            },
            AlbumPass {
                kind: PassKind::Unfiled,
                album: album_with_session("PrimarySync", "", Box::new(unfiled_session.clone())),
                exclude_ids: Arc::new(FxHashSet::default()),
            },
        ];

        let mut config = test_config();
        let dir = TempDir::new().expect("temp dir");
        config.directory = Arc::from(dir.path());
        config.concurrent_downloads = 10;
        config.recent = Some(60);
        seed_existing_recent_files(
            &mut config,
            &passes[0],
            "PrimarySync",
            &album_ids,
            "album-member",
        )
        .await;
        seed_existing_recent_files(
            &mut config,
            &passes[1],
            "PrimarySync",
            &unfiled_ids,
            "unfiled-mixed",
        )
        .await;

        let result = download_photos_full_with_token(
            &Client::new(),
            &passes,
            &Arc::new(config),
            DownloadControls::download_hidden(),
            CancellationToken::new(),
        )
        .await
        .expect("recent album plus unfiled sync should complete");

        assert!(matches!(result.outcome, DownloadOutcome::Success));
        assert_eq!(
            result.stats.assets_seen, 60,
            "40 album assets plus 20 non-album unfiled assets should be counted"
        );
        assert_eq!(result.stats.enumeration_errors, 0);
        assert_eq!(
            result.sync_token.as_deref(),
            Some("zone-token"),
            "recent deferred-unfiled sync may advance when the global frontier does not truncate"
        );
        assert_eq!(
            unfiled_session.emitted_ids().len(),
            120,
            "recent deferred unfiled should build the global frontier and then re-open a fresh stream for download URLs"
        );
    }

    #[tokio::test]
    async fn full_sync_recent_smart_folder_drains_multiple_reduced_pages() {
        let ids = recent_ids("smart-recent", 60);
        let session = DynamicRecentPhotosSession::from_ids(ids.clone())
            .with_filename_prefix("smart-recent")
            .with_token("zone-token");
        let passes = vec![AlbumPass {
            kind: PassKind::SmartFolder,
            album: album_with_session("PrimarySync", "Favorites", Box::new(session.clone())),
            exclude_ids: Arc::new(FxHashSet::default()),
        }];

        let mut config = test_config();
        let dir = TempDir::new().expect("temp dir");
        config.directory = Arc::from(dir.path());
        config.concurrent_downloads = 10;
        config.recent = Some(60);
        seed_existing_recent_files(&mut config, &passes[0], "PrimarySync", &ids, "smart-recent")
            .await;

        let result = download_photos_full_with_token(
            &Client::new(),
            &passes,
            &Arc::new(config),
            DownloadControls::download_hidden(),
            CancellationToken::new(),
        )
        .await
        .expect("recent smart-folder sync should complete");

        assert!(matches!(result.outcome, DownloadOutcome::Success));
        assert_eq!(result.stats.assets_seen, 60);
        assert_eq!(result.stats.enumeration_errors, 0);
        assert_eq!(
            result.sync_token.as_deref(),
            Some("zone-token"),
            "smart-folder recent sync may advance when the cap does not truncate"
        );
        assert!(
            session.offsets().len() >= 3,
            "smart-folder recent sync should drain every reduced page"
        );
    }

    #[tokio::test]
    async fn full_sync_deferred_unfiled_waits_when_album_enumeration_errors() {
        let album_ids = recent_ids("album-error", 40);
        let unfiled_ids = recent_ids("unfiled-after-error", 20);
        let mut library_ids = album_ids.clone();
        library_ids.extend(unfiled_ids.clone());
        let album_session = DynamicRecentPhotosSession::from_ids(album_ids.clone())
            .with_filename_prefix("album-error")
            .with_error_at_offset(20);
        let unfiled_session = DynamicRecentPhotosSession::from_ids(library_ids)
            .with_filename_prefix("unfiled-after-error");
        let passes = vec![
            AlbumPass {
                kind: PassKind::Album,
                album: album_with_session("PrimarySync", "Vacation", Box::new(album_session)),
                exclude_ids: Arc::new(FxHashSet::default()),
            },
            AlbumPass {
                kind: PassKind::Unfiled,
                album: album_with_session("PrimarySync", "", Box::new(unfiled_session.clone())),
                exclude_ids: Arc::new(FxHashSet::default()),
            },
        ];

        let mut config = test_config();
        let dir = TempDir::new().expect("temp dir");
        config.directory = Arc::from(dir.path());
        config.concurrent_downloads = 10;
        config.recent = Some(40);
        seed_existing_recent_files(
            &mut config,
            &passes[0],
            "PrimarySync",
            &album_ids,
            "album-error",
        )
        .await;
        seed_existing_recent_files(
            &mut config,
            &passes[1],
            "PrimarySync",
            &unfiled_ids,
            "unfiled-after-error",
        )
        .await;

        let result = download_photos_full_with_token(
            &Client::new(),
            &passes,
            &Arc::new(config),
            DownloadControls::download_hidden(),
            CancellationToken::new(),
        )
        .await
        .expect("album enumeration error should be reported as partial result");

        assert!(matches!(
            result.outcome,
            DownloadOutcome::PartialFailure { .. }
        ));
        assert_eq!(result.stats.enumeration_errors, 1);
        assert_eq!(
            result.stats.assets_seen, 20,
            "unfiled assets must not be processed when album exclusions are incomplete"
        );
        assert_eq!(result.sync_token, None);
    }

    #[test]
    fn zone_token_evidence_is_complete_when_all_passes_agree() {
        let observations = [PassKind::Album, PassKind::Unfiled]
            .into_iter()
            .enumerate()
            .map(|(index, kind)| PassTokenObservation {
                pass: PassKey {
                    index,
                    kind,
                    label: format!("pass-{index}"),
                },
                result: PassTokenResult::Present("zone-token".to_string()),
            })
            .collect::<Vec<_>>();

        assert_eq!(
            classify_zone_token_evidence(&observations),
            ZoneTokenEvidence::Complete {
                token: "zone-token".to_string()
            }
        );
    }

    #[test]
    fn zone_token_evidence_retries_all_passes_after_mismatch() {
        let observations = ["zone-token-a", "zone-token-b"]
            .into_iter()
            .enumerate()
            .map(|(index, token)| PassTokenObservation {
                pass: PassKey {
                    index,
                    kind: PassKind::Album,
                    label: format!("pass-{index}"),
                },
                result: PassTokenResult::Present(token.to_string()),
            })
            .collect::<Vec<_>>();

        let ZoneTokenEvidence::Recoverable { passes, reason } =
            classify_zone_token_evidence(&observations)
        else {
            panic!("mismatched pass tokens must be recoverable once");
        };
        assert_eq!(reason, TokenGap::Mismatch);
        assert_eq!(passes.len(), 2);
    }

    #[test]
    fn zone_token_evidence_retries_only_passes_with_gaps() {
        for (gap, expected_reason) in [
            (PassTokenResult::Missing, TokenGap::Missing),
            (PassTokenResult::Blank, TokenGap::Blank),
            (PassTokenResult::ReceiverDropped, TokenGap::ReceiverDropped),
            (
                PassTokenResult::EnumerationIncomplete,
                TokenGap::EnumerationIncomplete,
            ),
        ] {
            let observations = vec![
                PassTokenObservation {
                    pass: PassKey {
                        index: 0,
                        kind: PassKind::Album,
                        label: "complete".to_string(),
                    },
                    result: PassTokenResult::Present("zone-token".to_string()),
                },
                PassTokenObservation {
                    pass: PassKey {
                        index: 1,
                        kind: PassKind::Unfiled,
                        label: "gap".to_string(),
                    },
                    result: gap,
                },
            ];

            let ZoneTokenEvidence::Recoverable { passes, reason } =
                classify_zone_token_evidence(&observations)
            else {
                panic!("pass gap must receive one bounded recovery round");
            };
            assert_eq!(reason, expected_reason);
            assert_eq!(passes, vec![observations[1].pass.clone()]);
        }
    }

    #[test]
    fn contract_source_checkpoint_requires_durable_recovery_token_evidence_exhaustive() {
        const RESULT_KINDS: usize = 6;
        for len in 0..=3u32 {
            for encoded in 0..RESULT_KINDS.pow(len) {
                let mut value = encoded;
                let mut observations = Vec::with_capacity(len as usize);
                for index in 0..len as usize {
                    let result = match value % RESULT_KINDS {
                        0 => PassTokenResult::Present("token-a".to_string()),
                        1 => PassTokenResult::Present("token-b".to_string()),
                        2 => PassTokenResult::Missing,
                        3 => PassTokenResult::Blank,
                        4 => PassTokenResult::ReceiverDropped,
                        _ => PassTokenResult::EnumerationIncomplete,
                    };
                    value /= RESULT_KINDS;
                    observations.push(PassTokenObservation {
                        pass: PassKey {
                            index,
                            kind: PassKind::Album,
                            label: format!("pass-{index}"),
                        },
                        result,
                    });
                }

                let first_token = observations.first().and_then(|observation| {
                    if let PassTokenResult::Present(token) = &observation.result {
                        Some(token.as_str())
                    } else {
                        None
                    }
                });
                let complete_expected = first_token.is_some()
                    && observations.iter().all(|observation| {
                        matches!(
                            &observation.result,
                            PassTokenResult::Present(token)
                                if Some(token.as_str()) == first_token
                        )
                    });
                let evidence = classify_zone_token_evidence(&observations);
                assert_eq!(
                    matches!(evidence, ZoneTokenEvidence::Complete { .. }),
                    complete_expected,
                    "unexpected complete classification: {observations:?}"
                );
                assert_eq!(
                    matches!(evidence, ZoneTokenEvidence::Incomplete { .. }),
                    observations.is_empty(),
                    "only an empty observation set may be incomplete"
                );
            }
        }
    }

    /// Per-pass mode AND-folds `enumeration_complete` across passes.
    /// The first pass that aborts must drop the cycle's flag to false. The
    /// `&&=` semantics are subtle (especially around the empty-passes case)
    /// so this test pins the truth table.
    #[test]
    fn enum_progress_marker_per_pass_and_fold_semantics() {
        // All passes complete → cycle complete.
        let mut combined = true;
        for pass_complete in [true, true, true] {
            combined = combined && pass_complete;
        }
        assert!(combined, "all passes complete → marker clears");

        // One pass aborted mid-stream → cycle incomplete.
        let mut combined = true;
        for pass_complete in [true, false, true] {
            combined = combined && pass_complete;
        }
        assert!(
            !combined,
            "one pass aborted → marker must stay set even if siblings finished"
        );

        // Empty passes: combined stays at the initializer (false in the
        // production code so a no-pass cycle doesn't accidentally clear
        // the marker for a zone the cycle didn't actually enumerate).
        let combined: bool = []
            .iter()
            .fold(false, |acc, pass_complete: &bool| acc && *pass_complete);
        assert!(!combined, "no passes → marker stays set");
    }

    /// Pagination undercount classifier — exact match returns Match.
    /// Token must advance silently when the producer saw at least as many
    /// assets as the API reported.
    #[test]
    fn classify_pagination_shortfall_exact_match_is_silent() {
        let decision = classify_pagination_shortfall(1000, 1000, 0);
        assert_eq!(decision, PaginationShortfall::Match);
    }

    /// Duplicate API asset IDs can explain a count gap because they are
    /// included in the pre-enumeration count but suppressed by the producer
    /// before `assets_seen` advances.
    #[test]
    fn classify_pagination_shortfall_duplicate_compensated_allows_token() {
        let decision = classify_pagination_shortfall(23555, 23549, 6);
        assert_eq!(
            decision,
            PaginationShortfall::DuplicateCompensated { shortfall: 6 }
        );
    }

    /// A 1% undercount is still reported when duplicate asset IDs do not
    /// explain it.
    #[test]
    fn classify_pagination_shortfall_one_percent_below_reports_shortfall() {
        let decision = classify_pagination_shortfall(1000, 990, 0);
        assert_eq!(
            decision,
            PaginationShortfall::Shortfall { shortfall: 10 },
            "unexplained shortfalls should remain visible"
        );
    }

    /// Regression fixture for issue #498: expected=1578, seen=1533
    /// (shortfall=45, ~2.85%). This remains visible as an endpoint-drift
    /// diagnostic.
    #[test]
    fn classify_pagination_shortfall_issue_498_fixture_reports_shortfall() {
        let decision = classify_pagination_shortfall(1578, 1533, 0);
        assert_eq!(decision, PaginationShortfall::Shortfall { shortfall: 45 });
    }

    /// Regression fixture from downstream k8s-gitops mitigation:
    /// expected=31_000, seen=30_959 (shortfall=41, ~0.13%).
    #[test]
    fn classify_pagination_shortfall_billimek_sharedsync_fixture_reports_shortfall() {
        let decision = classify_pagination_shortfall(31_000, 30_959, 0);
        assert_eq!(decision, PaginationShortfall::Shortfall { shortfall: 41 });
    }

    #[test]
    fn owned_orphan_cleanup_removes_only_the_exact_recorded_path() {
        let dir = tempfile::tempdir().unwrap();
        let owned_path = dir.path().join("owned.kei-tmp");
        let bystander_path = dir.path().join("bystander.kei-tmp");
        std::fs::write(&owned_path, b"owned").unwrap();
        std::fs::write(&bystander_path, b"bystander").unwrap();
        let owned = [crate::state::OwnedTempFile {
            path: owned_path.clone(),
            claimed_at: 1,
        }];

        let cleanup = remove_owned_orphan_parts(dir.path(), &owned, i64::MAX / 2, i64::MAX / 2, 0);

        assert_eq!(cleanup.removed, 1);
        assert_eq!(cleanup.retire.as_slice(), std::slice::from_ref(&owned_path));
        assert!(!owned_path.exists());
        assert_eq!(std::fs::read(&bystander_path).unwrap(), b"bystander");
    }

    #[cfg(unix)]
    #[test]
    fn owned_orphan_cleanup_never_follows_a_directory_symlink() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let outside_path = outside.path().join("external.kei-tmp");
        std::fs::write(&outside_path, b"external").unwrap();
        let link = root.path().join("linked");
        symlink(outside.path(), &link).unwrap();
        let linked_path = link.join("external.kei-tmp");
        let owned = [crate::state::OwnedTempFile {
            path: linked_path.clone(),
            claimed_at: 1,
        }];

        let cleanup = remove_owned_orphan_parts(root.path(), &owned, i64::MAX / 2, i64::MAX / 2, 0);

        assert_eq!(cleanup.removed, 0);
        assert_eq!(cleanup.retire, [linked_path]);
        assert_eq!(std::fs::read(outside_path).unwrap(), b"external");
    }

    #[cfg(unix)]
    #[test]
    fn owned_orphan_cleanup_resists_ancestor_symlink_swap_before_remove() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let parent = root.path().join("parent");
        let moved_parent = root.path().join("moved-parent");
        std::fs::create_dir(&parent).unwrap();
        let owned_path = parent.join("owned.kei-tmp");
        let outside_path = outside.path().join("owned.kei-tmp");
        std::fs::write(&owned_path, b"owned").unwrap();
        std::fs::write(&outside_path, b"external").unwrap();
        let owned = [crate::state::OwnedTempFile {
            path: owned_path.clone(),
            claimed_at: 1,
        }];

        let cleanup = remove_owned_orphan_parts_with(
            root.path(),
            &owned,
            i64::MAX / 2,
            i64::MAX / 2,
            0,
            |_| {
                std::fs::rename(&parent, &moved_parent).unwrap();
                symlink(outside.path(), &parent).unwrap();
            },
        );

        assert_eq!(cleanup.removed, 1);
        assert_eq!(cleanup.retire.as_slice(), std::slice::from_ref(&owned_path));
        assert!(!moved_parent.join("owned.kei-tmp").exists());
        assert_eq!(std::fs::read(outside_path).unwrap(), b"external");
    }

    #[test]
    fn owned_orphan_cleanup_spares_recently_touched_files() {
        let dir = tempfile::tempdir().unwrap();
        let part_path = dir.path().join("active.kei-tmp");
        std::fs::write(&part_path, b"active").unwrap();
        let owned = [crate::state::OwnedTempFile {
            path: part_path.clone(),
            claimed_at: 1,
        }];
        let real_now = chrono::Utc::now().timestamp();

        let cleanup = remove_owned_orphan_parts(
            dir.path(),
            &owned,
            real_now + 1_800,
            real_now + 3_600,
            90 * 60,
        );

        assert_eq!(cleanup.removed, 0);
        assert!(cleanup.retire.is_empty());
        assert_eq!(std::fs::read(part_path).unwrap(), b"active");
    }

    #[test]
    fn owned_orphan_cleanup_retires_a_claim_when_the_path_changed_after_cutoff() {
        let dir = tempfile::tempdir().unwrap();
        let part_path = dir.path().join("changed.kei-tmp");
        std::fs::write(&part_path, b"replacement").unwrap();
        let owned = [crate::state::OwnedTempFile {
            path: part_path.clone(),
            claimed_at: -1,
        }];

        let cleanup = remove_owned_orphan_parts(dir.path(), &owned, 0, 0, 0);

        assert_eq!(cleanup.removed, 0);
        assert_eq!(cleanup.retire.as_slice(), std::slice::from_ref(&part_path));
        assert_eq!(std::fs::read(part_path).unwrap(), b"replacement");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn contract_temp_file_delete_requires_durable_ownership() {
        use std::fs::{File, FileTimes};
        use std::os::unix::fs::symlink;
        use std::time::{Duration as StdDuration, UNIX_EPOCH};

        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let owned_path = root.path().join("owned.kei-tmp");
        let bystander_path = root.path().join("bystander.kei-tmp");
        let outside_path = outside.path().join("external.kei-tmp");
        std::fs::write(&owned_path, b"owned").unwrap();
        std::fs::write(&bystander_path, b"bystander").unwrap();
        std::fs::write(&outside_path, b"external").unwrap();
        symlink(outside.path(), root.path().join("linked")).unwrap();
        let old_time = UNIX_EPOCH + StdDuration::from_secs(1);
        for path in [&owned_path, &bystander_path, &outside_path] {
            File::options()
                .write(true)
                .open(path)
                .unwrap()
                .set_times(FileTimes::new().set_modified(old_time))
                .unwrap();
        }

        let db = Arc::new(SqliteStateDb::open_in_memory().unwrap());
        db.claim_temp_file(&owned_path).await.unwrap();
        {
            let conn = db.acquire_lock("seed orphan cleanup").unwrap();
            conn.execute("UPDATE owned_temp_files SET claimed_at = 1", [])
                .unwrap();
        }
        let run_id = db.start_sync_run().await.unwrap();
        db.complete_sync_run(run_id, &crate::state::SyncRunStats::default())
            .await
            .unwrap();

        let mut config = test_config();
        config.directory = Arc::from(root.path());
        config.state_db = Some(db.clone());
        let result = download_photos_with_sync(
            &Client::new(),
            &[],
            Arc::new(config),
            DownloadControls::download_hidden(),
            CancellationToken::new(),
        )
        .await
        .unwrap();

        assert!(matches!(result.outcome, DownloadOutcome::Success));
        assert!(!owned_path.exists(), "owned stale path must be removed");
        assert_eq!(std::fs::read(&bystander_path).unwrap(), b"bystander");
        assert_eq!(std::fs::read(&outside_path).unwrap(), b"external");
        assert!(
            db.get_owned_temp_files_before(i64::MAX)
                .await
                .unwrap()
                .is_empty(),
            "cleanup must retire consumed ownership"
        );
    }

    /// Companion: accumulating into an empty `SyncStats` is a faithful copy
    /// (the operation is the additive identity for the empty case).
    #[test]
    fn sync_stats_accumulate_into_empty_is_copy() {
        let src = SyncStats {
            assets_seen: 5,
            downloaded: 2,
            failed: 1,
            skipped: SkipBreakdown {
                duplicates: 7,
                ..SkipBreakdown::default()
            },
            rate_limited: 4,
            interrupted: true,
            ..SyncStats::default()
        };
        let mut dst = SyncStats::default();
        dst.accumulate(&src);
        assert_eq!(dst.assets_seen, 5);
        assert_eq!(dst.downloaded, 2);
        assert_eq!(dst.failed, 1);
        assert_eq!(dst.skipped.duplicates, 7);
        assert_eq!(dst.rate_limited, 4);
        assert!(dst.interrupted);
    }
}
