//! Watch cadence, metadata follow-ups, and one-shot recovery policy.

use crate::{config, download};

/// Default watch interval applied when `kei service run` enters with no TOML
/// value set. 24 hours, matching the Docker image's always-on service shape.
pub(crate) const SERVICE_MODE_DEFAULT_WATCH_INTERVAL: u64 = 86400;

/// Maximum idle delay between clean metadata-capture repair batches.
pub(super) const METADATA_CAPTURE_FOLLOW_UP_INTERVAL_SECS: u64 = 60;

#[must_use]
pub(super) fn metadata_capture_watch_interval(
    configured_interval: Option<u64>,
    stats: &download::SyncStats,
    cycle_failed_count: usize,
) -> Option<u64> {
    configured_interval.map(|interval| {
        if cycle_failed_count == 0
            && stats.metadata_capture_remaining > 0
            && stats.metadata_capture_progressed
            && !stats.has_rate_limit_pressure()
        {
            interval.min(METADATA_CAPTURE_FOLLOW_UP_INTERVAL_SECS)
        } else {
            interval
        }
    })
}

/// Decide whether to apply the service-mode watch-interval fallback.
///
/// Returns `Some(interval)` only when `service_mode` is true AND the
/// existing layered resolution (CLI > TOML > env) produced no value,
/// signaling that the daemon would otherwise run once and exit.
pub(crate) fn service_mode_default_interval(
    current: Option<u64>,
    service_mode: bool,
) -> Option<u64> {
    if service_mode && current.is_none() {
        Some(super::SERVICE_MODE_DEFAULT_WATCH_INTERVAL)
    } else {
        None
    }
}

/// Precondition and watch adjustment for `--refresh-metadata`. It is a one-shot
/// recovery pass, so it is rejected under `service run` and with any narrowing
/// filter that would strand out-of-scope rows. Returns `true` when the caller
/// must force a single one-shot cycle (clear the watch interval).
pub(super) fn refresh_metadata_forces_one_shot(
    refresh_metadata: bool,
    service_mode: bool,
    narrows_enumeration: bool,
) -> anyhow::Result<bool> {
    if !refresh_metadata {
        return Ok(false);
    }
    anyhow::ensure!(
        !service_mode,
        "--refresh-metadata is a one-shot repair and is not supported under `kei service run`. Run `kei sync --refresh-metadata` instead."
    );
    anyhow::ensure!(
        !narrows_enumeration,
        "--refresh-metadata repairs every downloaded asset in the selected libraries and requires a complete library sweep. Enable the unfiled pass and remove album, smart-folder, media, or date/recent filters before retrying."
    );
    Ok(true)
}

pub(super) fn validate_capture_timestamp_repair(
    repair: download::CaptureTimestampRepair,
    metadata: &config::MetadataConfig,
) -> anyhow::Result<download::CaptureTimestampRepair> {
    if matches!(repair, download::CaptureTimestampRepair::Preserve) {
        return Ok(repair);
    }
    anyhow::ensure!(
        metadata.set_exif_datetime,
        "--repair-capture-timestamps requires `metadata.set_exif_datetime = true` so the embedded datetime writer is enabled"
    );
    Ok(repair)
}

/// Keep explicit truncated-file replacement out of long-running service mode.
/// The operator must authorize each repair run from a foreground command.
pub(super) fn repair_truncated_forces_one_shot(
    repair_truncated: bool,
    service_mode: bool,
) -> anyhow::Result<bool> {
    if !repair_truncated {
        return Ok(false);
    }
    anyhow::ensure!(
        !service_mode,
        "--repair-truncated is a one-shot repair and is not supported under `kei service run`. Run `kei sync --repair-truncated` instead."
    );
    Ok(true)
}

#[cfg(test)]
mod tests;
