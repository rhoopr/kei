//! Download duration, rate-limit warnings, and summary formatting.

use std::time::Duration;

/// Emit a warn! if rate_limit_observations exceeded 10% of attempts. Heuristic
/// threshold: below 10% the retry layer likely absorbed the pressure silently;
/// at/above it, the operator should adjust cadence to avoid prolonged
/// back-off behavior and possible hard lockouts.
pub(super) fn maybe_warn_rate_limit_pressure(stats: &crate::download::SyncStats) {
    if !stats.has_rate_limit_pressure() {
        return;
    }
    if stats.assets_seen == 0 {
        // No enumeration anchor — surface the raw count so operators still
        // see the signal, but skip the (meaningless) percentage.
        tracing::warn!(target: "kei::download::pipeline",
            rate_limit_observations = stats.rate_limited,
            "Observed HTTP 429/503 rate-limiting before any assets were enumerated — \
             consider raising [watch] interval or lowering [download] threads"
        );
        return;
    }
    let pct = stats.rate_limited as u64 * 100 / stats.assets_seen;
    if pct >= 10 {
        tracing::warn!(target: "kei::download::pipeline",
            rate_limit_observations = stats.rate_limited,
            assets_seen = stats.assets_seen,
            percent = pct,
            "Observed HTTP 429/503 rate-limiting on >=10% of sync attempts — \
             consider raising [watch] interval or lowering [download] threads \
             to reduce sustained pressure on iCloud"
        );
    }
}

pub(in crate::download) fn format_duration(d: Duration) -> String {
    let total_secs = d.as_secs();
    let hours = total_secs / 3600;
    let mins = (total_secs % 3600) / 60;
    let secs = total_secs % 60;

    if hours > 0 {
        format!("{hours}h {mins:02}m {secs:02}s")
    } else if mins > 0 {
        format!("{mins}m {secs:02}s")
    } else {
        format!("{secs}s")
    }
}

#[allow(
    clippy::cast_precision_loss,
    reason = "display-only byte-size formatting; precision loss at exabyte scale is fine for a human-readable string"
)]
fn format_bytes(bytes: u64) -> String {
    if bytes >= 1_073_741_824 {
        format!("{:.1} GiB", bytes as f64 / 1_073_741_824.0)
    } else if bytes >= 1_048_576 {
        format!("{:.1} MiB", bytes as f64 / 1_048_576.0)
    } else if bytes >= 1024 {
        format!("{:.1} KiB", bytes as f64 / 1024.0)
    } else {
        format!("{bytes} B")
    }
}

/// Log a formatted summary of sync statistics.
pub(in crate::download) fn log_sync_summary(title: &str, stats: &crate::download::SyncStats) {
    tracing::info!(target: "kei::download::pipeline", title = %title, "Sync summary");

    // Line 1: core counts
    let skipped = stats.skipped.total() - stats.skipped.duplicates;
    let total = stats.downloaded + stats.failed + skipped;
    if skipped > 0 {
        tracing::info!(target: "kei::download::pipeline",
            "  {downloaded} downloaded, {skipped} skipped, {failed} failed ({total} total)",
            downloaded = stats.downloaded,
            failed = stats.failed
        );
    } else {
        tracing::info!(target: "kei::download::pipeline",
            "  {downloaded} downloaded, {failed} failed ({total} total)",
            downloaded = stats.downloaded,
            failed = stats.failed
        );
    }

    // Line 2: error details (only if any). `enumeration_errors` can
    // gate `PartialFailure` on its own, so an operator chasing exit
    // code 2 with no other failure counts needs to see it here.
    if stats.exif_failures > 0 || stats.state_write_failures > 0 || stats.enumeration_errors > 0 {
        tracing::info!(target: "kei::download::pipeline",
            "  {} EXIF write failure(s), {} state write failure(s), {} enumeration error(s)",
            stats.exif_failures,
            stats.state_write_failures,
            stats.enumeration_errors
        );
    }
    if stats.enumeration_incomplete {
        tracing::info!(target: "kei::download::pipeline",
            "  Enumeration incomplete; sync token blocked and next cycle will replay changes"
        );
    }

    // Line 3: skip breakdown (only if skips > 0)
    if skipped > 0 {
        let mut reasons = Vec::new();
        if stats.skipped.by_state > 0 {
            reasons.push(format!("{} already downloaded", stats.skipped.by_state));
        }
        if stats.skipped.on_disk > 0 {
            reasons.push(format!("{} on disk", stats.skipped.on_disk));
        }
        if stats.skipped.by_media_type > 0 {
            reasons.push(format!(
                "{} filtered by media type",
                stats.skipped.by_media_type
            ));
        }
        if stats.skipped.by_date_range > 0 {
            reasons.push(format!(
                "{} filtered by date range",
                stats.skipped.by_date_range
            ));
        }
        if stats.skipped.by_live_photo > 0 {
            reasons.push(format!(
                "{} filtered (live photo)",
                stats.skipped.by_live_photo
            ));
        }
        if stats.skipped.by_filename > 0 {
            reasons.push(format!(
                "{} filtered by filename",
                stats.skipped.by_filename
            ));
        }
        if stats.skipped.by_excluded_album > 0 {
            reasons.push(format!(
                "{} excluded by album",
                stats.skipped.by_excluded_album
            ));
        }
        if stats.skipped.ampm_variant > 0 {
            reasons.push(format!("{} AM/PM variants", stats.skipped.ampm_variant));
        }
        if stats.skipped.retry_exhausted > 0 {
            reasons.push(format!(
                "{} retries exhausted",
                stats.skipped.retry_exhausted
            ));
        }
        if stats.skipped.retry_only > 0 {
            reasons.push(format!(
                "{} not failed (retry mode)",
                stats.skipped.retry_only
            ));
        }
        if !reasons.is_empty() {
            tracing::info!(target: "kei::download::pipeline", "  Skipped: {}", reasons.join(", "));
        }
    }

    // Line 4: transfer stats (only if bytes downloaded)
    if stats.bytes_downloaded > 0 {
        if stats.bytes_downloaded == stats.disk_bytes_written {
            tracing::info!(target: "kei::download::pipeline", "  Transferred {}", format_bytes(stats.bytes_downloaded));
        } else {
            tracing::info!(target: "kei::download::pipeline",
                "  Transferred {}, {} written to disk",
                format_bytes(stats.bytes_downloaded),
                format_bytes(stats.disk_bytes_written)
            );
        }
    }

    // Line 5: elapsed
    tracing::info!(target: "kei::download::pipeline",
        "  Completed in {}",
        format_duration(Duration::from_secs_f64(stats.elapsed_secs))
    );
}

#[cfg(test)]
mod tests;
