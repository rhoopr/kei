//! Bounded local drift recovery and periodic read-only diagnostics.

use crate::{download, state};

/// Spend at most one provider request after current work across all selected
/// libraries. This tail owns no checkpoint, transfer or watch wake decision.
pub(super) async fn recover_historical_sources(
    db: &state::SqliteStateDb,
    libraries: &[crate::sync_cycle::LibraryState],
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<(), state::error::StateError> {
    let now = chrono::Utc::now().timestamp();
    if cancel.is_cancelled() {
        return Ok(());
    }
    let zones = libraries
        .iter()
        .filter(|l| !l.plan_is_stale && l.library.last_indexing_finished())
        .map(|l| l.zone_name.clone())
        .collect();
    if let Some((zone, ids)) = db.reserve_historical_sources(zones, now).await?
        && let Some(library) = libraries.iter().find(|l| l.zone_name == zone)
    {
        let confirmed = tokio::select! {
            biased;
            () = cancel.cancelled() => return Ok(()),
            result = tokio::time::timeout(std::time::Duration::from_secs(5),library.library.historical_sources(&ids)) => result.unwrap_or_default(),
        };
        if ids.len() != confirmed.len() {
            tracing::debug!(
                deferred = ids.len() - confirmed.len(),
                "Historical source checks deferred"
            );
        }
        for (id, source) in confirmed {
            if cancel.is_cancelled() {
                break;
            }
            db.finish_historical_source(&library.zone_name, id, now, source)
                .await?;
        }
    }
    Ok(())
}

/// Walk every `downloaded` row in the state DB and warn when the
/// recorded `local_path` is missing. Read-only - no
/// rows are mutated by this periodic full-catalog walk. Triggered on a fixed
/// cadence by the watch loop so operators can see drift outside the bounded
/// pre-cycle probe.
///
/// Errors from the DB scan are logged at `warn!` rather than propagated:
/// the periodic walk is a diagnostic, not a load-bearing correctness gate,
/// and a transient SQLite hiccup must not crash the watch daemon.
pub(super) async fn run_periodic_reconcile(db: &dyn state::ReportStateStore, cycle_index: u64) {
    use crate::commands::reconcile::{LocalDriftAsset, LocalDriftKind, scan_missing_local_files};
    tracing::info!(
        cycle_index,
        "Periodic reconciliation: scanning state DB for missing local files"
    );
    let mut sample_logged = 0usize;
    const SAMPLE_LOG_CAP: usize = 25;
    // Cap per-cycle log spam at SAMPLE_LOG_CAP missing entries; the
    // aggregate count is logged below regardless of how many fired.
    let report_drift = |m: &LocalDriftAsset| {
        if sample_logged < SAMPLE_LOG_CAP {
            match m.kind {
                LocalDriftKind::Missing => tracing::warn!(
                    asset_id = %m.id,
                    version_size = m.version_size.as_str(),
                    path = %m.local_path.display(),
                    "Reconcile: state row marks asset downloaded but local file is missing"
                ),
                LocalDriftKind::Truncated {
                    actual_size,
                    expected_size,
                } => tracing::warn!(
                    asset_id = %m.id,
                    version_size = m.version_size.as_str(),
                    path = %m.local_path.display(),
                    actual_size,
                    expected_size,
                    "Reconcile: state row marks asset downloaded but local file is smaller than expected"
                ),
            }
            sample_logged += 1;
        }
    };
    let report_no_path = |id: &str| {
        tracing::debug!(asset_id = %id, "Reconcile: downloaded row has no local_path recorded");
    };
    let scan = scan_missing_local_files(db, report_drift, report_no_path).await;
    match scan {
        Ok((counts, drifted)) => {
            if drifted.is_empty() && counts.no_path == 0 {
                tracing::info!(
                    present = counts.present,
                    "Periodic reconciliation: all downloaded files look present on disk"
                );
            } else {
                tracing::warn!(
                    present = counts.present,
                    missing = counts.missing,
                    damaged = counts.damaged,
                    no_path = counts.no_path,
                    sample_logged,
                    "Periodic reconciliation: drift detected; run `kei reconcile` to mark local drift for re-download"
                );
            }
        }
        Err(e) => {
            tracing::warn!(error = %e, "Periodic reconciliation scan failed; will retry on next interval");
        }
    }
}

const LOCAL_DRIFT_PROBE_CURSOR_KEY: &str = "local_drift_probe_offset_v1";

const LOCAL_DRIFT_PROBE_PAGE_SIZE: u32 = 128;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(super) struct LocalDriftProbeOutcome {
    scanned: u64,
    drifted: u64,
    pub(super) marked_failed: u64,
    mark_errors: u64,
}

/// Probe a bounded page of downloaded rows for missing files before each sync
/// cycle. Unlike the opt-in full reconciliation walk, this runs by default
/// and advances a cursor so watch mode eventually covers the catalog without
/// turning every quiet incremental cycle into a full filesystem crawl.
/// A present shorter file may be an intentional local edit. Only explicit
/// integrity reconciliation may turn that observation into repair work.
pub(super) async fn run_bounded_local_drift_probe(
    db: &dyn download::DownloadStore,
    cycle_index: u64,
) -> LocalDriftProbeOutcome {
    let summary = match db.get_summary().await {
        Ok(summary) => summary,
        Err(e) => {
            tracing::warn!(error = %e, "Local drift probe failed to read state summary");
            return LocalDriftProbeOutcome::default();
        }
    };
    if summary.downloaded == 0 {
        return LocalDriftProbeOutcome::default();
    }

    let start_offset = match db.get_metadata(LOCAL_DRIFT_PROBE_CURSOR_KEY).await {
        Ok(Some(raw)) => raw.parse::<u64>().unwrap_or(0).min(summary.downloaded),
        Ok(None) => 0,
        Err(e) => {
            tracing::warn!(error = %e, "Local drift probe failed to read cursor");
            0
        }
    };

    let mut page = match db
        .get_downloaded_page(start_offset, LOCAL_DRIFT_PROBE_PAGE_SIZE)
        .await
    {
        Ok(page) => page,
        Err(e) => {
            tracing::warn!(error = %e, "Local drift probe failed to load downloaded page");
            return LocalDriftProbeOutcome::default();
        }
    };
    let offset = if page.is_empty() && start_offset > 0 {
        match db.get_downloaded_page(0, LOCAL_DRIFT_PROBE_PAGE_SIZE).await {
            Ok(first_page) => {
                page = first_page;
                0
            }
            Err(e) => {
                tracing::warn!(error = %e, "Local drift probe failed to wrap cursor");
                return LocalDriftProbeOutcome::default();
            }
        }
    } else {
        start_offset
    };

    let scanned = u64::try_from(page.len()).unwrap_or(u64::MAX);
    let next_offset = if page.is_empty()
        || offset.saturating_add(scanned) >= summary.downloaded
        || scanned < u64::from(LOCAL_DRIFT_PROBE_PAGE_SIZE)
    {
        0
    } else {
        offset.saturating_add(scanned)
    };
    if let Err(e) = db
        .set_metadata(LOCAL_DRIFT_PROBE_CURSOR_KEY, &next_offset.to_string())
        .await
    {
        tracing::warn!(error = %e, "Local drift probe failed to persist cursor");
    }

    let mut outcome = LocalDriftProbeOutcome {
        scanned,
        ..LocalDriftProbeOutcome::default()
    };
    for asset in page {
        let Some(path) = &asset.local_path else {
            continue;
        };
        // Ordinary recovery checks absence only. The integrity classifier may
        // hash shorter metadata-rewritten files, even when its result is ignored.
        match tokio::fs::metadata(path).await {
            Ok(_) => continue,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                tracing::warn!(error = %e, "Local drift probe failed to inspect a downloaded row");
                continue;
            }
        }
        outcome.drifted = outcome.drifted.saturating_add(1);
        tracing::warn!(
            cycle_index,
            asset_id = %asset.id,
            version_size = asset.version_size.as_str(),
            path = %path.display(),
            "Local drift probe found a missing downloaded file"
        );
        match db
            .mark_failed(
                &asset.library,
                &asset.id,
                asset.version_size.as_str(),
                crate::commands::reconcile::FILE_MISSING_REASON,
            )
            .await
        {
            Ok(()) => outcome.marked_failed = outcome.marked_failed.saturating_add(1),
            Err(e) => {
                outcome.mark_errors = outcome.mark_errors.saturating_add(1);
                tracing::warn!(
                    error = %e,
                    asset_id = %asset.id,
                    version_size = asset.version_size.as_str(),
                    "Local drift probe could not mark drifted file failed"
                );
            }
        }
    }

    if outcome.drifted > 0 || outcome.mark_errors > 0 {
        tracing::warn!(
            cycle_index,
            scanned = outcome.scanned,
            drifted = outcome.drifted,
            marked_failed = outcome.marked_failed,
            mark_errors = outcome.mark_errors,
            next_offset,
            "Local drift probe completed with drift"
        );
    } else {
        tracing::debug!(
            cycle_index,
            scanned = outcome.scanned,
            next_offset,
            "Local drift probe completed"
        );
    }
    outcome
}

/// Should this watch cycle run a periodic local-vs-state reconciliation?
///
/// Returns `true` for the very first cycle whose 1-based index is a multiple
/// of `every_n` (e.g. `every_n = 24` fires on cycle 24, 48, ...). The first
/// firing is at cycle `every_n` rather than cycle 0 so a freshly-started
/// daemon doesn't burn its startup time walking the disk before a single
/// sync has run. Disabled (`None`) or `Some(0)` always returns `false`; the
/// `cycle_index` is also 1-based so the first cycle is `1`.
///
/// Pure function so the cadence is unit-testable without spinning up a real
/// watch loop or filesystem walk.
pub(crate) fn should_reconcile_this_cycle(cycle_index: u64, every_n: Option<u64>) -> bool {
    let n = match every_n {
        Some(n) if n > 0 => n,
        _ => return false,
    };
    cycle_index > 0 && cycle_index.is_multiple_of(n)
}

#[cfg(test)]
mod tests;
