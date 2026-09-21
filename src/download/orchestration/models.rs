//! Download controls, outcomes, reporting, and checkpoint reason vocabulary.

use crate::download::pipeline::StreamingResult;
use crate::download::{filter, recap};
use crate::icloud::photos::ProviderRecordId;
use crate::icloud::photos::asset::MALFORMED_REQUIRED_ASSET_FIELDS_REASON;
use crate::state::{
    DownloadContextStateStore, DownloadStateStore, MembershipStore, MetadataRewriteStore,
    ReconciliationStateStore, ReportStateStore, SyncTokenStore, TempFileOwnershipStore,
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
    + ReconciliationStateStore
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
        + ReconciliationStateStore
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
    /// Unresolved asset hydration, independent of intentional checkpoint holds.
    #[serde(skip)]
    pub(crate) identity_incomplete: bool,
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
        self.identity_incomplete |= other.identity_incomplete;
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

pub(super) const ALBUM_RELATION_HYDRATION_INCOMPLETE_REASON: &str =
    "album_relation_hydration_incomplete";

pub(super) const DATE_BOUNDED_FULL_ENUMERATION_REASON: &str = "date_bounded_full_enumeration";

pub(super) const RECENT_LIMITED_FULL_ENUMERATION_REASON: &str = "recent_limited_full_enumeration";

pub(super) const UNPARSABLE_RELATION_DELTA_REASON: &str = "unparsable_relation_delta";

pub(super) const UNKNOWN_ALBUM_RELATION_CONTAINER_REASON: &str = "unknown_album_relation_container";

pub(super) const UNKNOWN_ALBUM_RELATION_ASSET_REASON: &str = "unknown_album_relation_asset";

pub(super) const ALBUM_DELTA_STATE_WRITE_FAILED_REASON: &str = "album_delta_state_write_failed";

pub(super) const ASSET_MASTER_MAPPING_STATE_WRITE_FAILED_REASON: &str =
    "asset_master_mapping_state_write_failed";

pub(super) const ASSET_DELTA_HYDRATION_INCOMPLETE_REASON: &str = "asset_delta_hydration_incomplete";

pub(super) const PROVIDER_METADATA_STATE_WRITE_FAILED_REASON: &str =
    "provider_metadata_state_write_failed";

pub(super) const METADATA_CAPTURE_REPAIR_FAILED_REASON: &str = "metadata_capture_repair_failed";

pub(super) const INCREMENTAL_DELETE_STATE_WRITE_FAILED_REASON: &str =
    "incremental_delete_state_write_failed";

const INCREMENTAL_DELETE_ZERO_ROWS_REASON: &str = "incremental_delete_no_matching_state";

pub(super) const INCREMENTAL_HIDDEN_STATE_WRITE_FAILED_REASON: &str =
    "incremental_hidden_state_write_failed";

const INCREMENTAL_HIDDEN_ZERO_ROWS_REASON: &str = "incremental_hidden_no_matching_state";

pub(super) const SMART_FOLDER_REFRESH_FAILED_REASON: &str = "smart_folder_refresh_failed";

pub(super) const TARGETED_ALBUM_BACKFILL_FAILED_REASON: &str = "targeted_album_backfill_failed";

pub(in crate::download) const PENDING_RETRY_UNMATCHED_REASON: &str = "pending_retry_unmatched";

const PAGINATION_SHORTFALL_REASON: &str = "pagination_shortfall";

pub(super) const ICLOUD_ALBUM_COUNT_ERROR_REASON: &str = "icloud_album_count_error";

pub(crate) const PRODUCER_ENUMERATION_INCOMPLETE_REASON: &str = "producer_enumeration_incomplete";

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

pub(super) fn merge_download_outcomes(
    left: &DownloadOutcome,
    right: &DownloadOutcome,
) -> DownloadOutcome {
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

    pub(super) fn retry_passes(&self) -> &[PassKey] {
        match self {
            Self::RetryPasses(passes) => passes,
            _ => &[],
        }
    }
}

pub(super) fn merge_streaming_result(combined: &mut StreamingResult, result: StreamingResult) {
    combined.downloaded += result.downloaded;
    combined.exif_failures += result.exif_failures;
    combined.failed.extend(result.failed);
    combined.auth_errors += result.auth_errors;
    combined.provider_auth_errors += result.provider_auth_errors;
    combined.state_write_failures += result.state_write_failures;
    combined.enumeration_errors += result.enumeration_errors;
    combined.assets_seen += result.assets_seen;
    combined.skip_summary += result.skip_summary;
    // AND-fold across passes so a single pass aborting (e.g.
    // producer-channel close, panic) leaves the marker set.
    combined.enumeration_complete = combined.enumeration_complete && result.enumeration_complete;
}

pub(super) fn merge_token_recovery_result(combined: &mut StreamingResult, result: StreamingResult) {
    // A token-recovery pass replays an already observed pass through the real
    // planner and state pipeline. Preserve the original inventory/progress
    // counts rather than double-counting every replayed asset, but retain all
    // transfer, durability, and error outcomes produced by the repair.
    combined.downloaded += result.downloaded;
    combined.exif_failures += result.exif_failures;
    combined.failed.extend(result.failed);
    combined.auth_errors += result.auth_errors;
    combined.provider_auth_errors += result.provider_auth_errors;
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

pub(super) fn set_full_enumeration_reason(result: &mut SyncResult, reason: FullEnumerationReason) {
    if result.full_enumeration_ran && result.stats.full_enumeration_reason.is_none() {
        result.stats.full_enumeration_reason = Some(reason);
    }
}

pub(crate) fn block_sync_token_for_unresolved_identity(stats: &mut SyncStats) {
    stats.identity_incomplete = true;
    block_sync_token_for_incremental_delta(stats, ASSET_DELTA_HYDRATION_INCOMPLETE_REASON);
}

pub(super) fn block_sync_token_for_incremental_delta(stats: &mut SyncStats, reason: &'static str) {
    if stats.sync_token_blocked_reason.is_none() {
        stats.sync_token_blocked_reason = Some(reason);
        stats.sync_token_blocked_source = Some(sync_token_blocked_source(reason));
        stats.sync_token_blocked_explanation = Some(sync_token_blocked_explanation(reason));
    }
    stats.sync_token_blocked = true;
}

pub(super) fn clear_full_query_token_block_stats(stats: &mut SyncStats) {
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

#[cfg(test)]
mod tests;
