//! Per-cycle counters for metrics, notifications, the JSON report and
//! support history. The sync core fills the fields that apply to it; the
//! rest stay zero.

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FullEnumerationReason {
    /// A library had no saved sync position, so it was listed from the start.
    NoStoredToken,
    /// iCloud rejected the saved position, so the library was listed again.
    PositionRejected,
}

impl FullEnumerationReason {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::NoStoredToken => "no_stored_token",
            Self::PositionRejected => "position_rejected",
        }
    }
}

/// Accumulated statistics from a sync run, used for JSON reports and notifications.
#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct SyncStats {
    pub assets_seen: u64,
    /// Additive count observations across invocations, not a comparable inventory.
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
    pub metadata_capture_unresolved: u64,
    pub metadata_capture_deferred: u64,
    pub unattributed_legacy_assets: u64,
    /// Historical rows lacking a stored certificate, not pending sync work.
    pub unattributed_legacy_pending: u64,
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
}

impl SyncStats {
    // Per-zone composition folds safety fields only in CheckpointEvidence.
    // Cycle reports separately aggregate their projected counters above.
}

const fn is_false(value: &bool) -> bool {
    !*value
}

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
}
