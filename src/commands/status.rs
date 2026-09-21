#![allow(
    clippy::print_stdout,
    reason = "CLI subcommand whose primary purpose is to print a status report to stdout"
)]

use crate::cli;
use crate::config;
use crate::service::status::{render_oneline, service_state};
use crate::state;
use crate::state::AssetRecord;

use super::{LISTING_CAP, print_truncation_tail};

/// Run the status command.
pub(crate) async fn run_status(
    args: cli::StatusArgs,
    globals: &config::GlobalArgs,
    toml: Option<&config::TomlConfig>,
) -> anyhow::Result<()> {
    print_service_section().await;

    let db_path = super::super::get_db_path(globals, toml)?;

    if !db_path.exists() {
        println!("No state database found at {}", db_path.display());
        println!("Run a sync first to create the database.");
        return Ok(());
    }

    let db = state::SqliteStateDb::open(&db_path).await?;
    let summary = db.get_summary().await?;

    println!("State Database: {}", db_path.display());
    println!();
    println!("Assets:");
    println!("  Total:      {}", summary.total_assets);
    println!("  Downloaded: {}", summary.downloaded);
    println!("  Pending:    {}", summary.pending);
    println!("  Policy excluded: {}", summary.policy_excluded);
    println!("  Failed:     {}", summary.failed);
    println!("  Source deleted: {}", summary.source_deleted);
    println!(
        "  Awaiting provider verification: {}",
        summary.awaiting_provider_verification
    );
    println!();
    println!(
        "Provider checkpoint: {}",
        summary
            .provider_checkpoint_status
            .as_deref()
            .unwrap_or("unavailable")
    );
    if let Some(reason) = &summary.last_full_enumeration_reason {
        println!("Last full-enumeration reason: {reason}");
    }
    if let Some(action) = summary
        .last_recovery_action
        .as_deref()
        .filter(|action| *action != "none")
    {
        println!("Last recovery action: {action}");
    }
    if !summary.metadata_capture.is_empty() {
        println!("Metadata capture:");
        for capture in &summary.metadata_capture {
            if let Some(pending) = capture.pending_revision {
                println!(
                    "  {}: revision {} -> {}; {} processed, {} failed, {} remaining",
                    capture.library,
                    capture.active_revision,
                    pending,
                    capture.processed_assets,
                    capture.failed_assets,
                    capture.remaining_assets
                );
                if let Some(error) = &capture.last_error {
                    println!("    Last error: {error}");
                }
            } else {
                println!(
                    "  {}: revision {} current",
                    capture.library, capture.active_revision
                );
            }
        }
    }
    println!("{}", backup_status_line(&summary));
    println!();

    if let Some(started) = &summary.active_sync_started {
        println!(
            "Sync in progress:   started {}",
            started.format("%Y-%m-%d %H:%M:%S UTC")
        );
    } else if let Some(started) = &summary.last_sync_started {
        println!(
            "Last sync started:   {}",
            started.format("%Y-%m-%d %H:%M:%S UTC")
        );
    }
    if summary.active_sync_started.is_none()
        && let Some(completed) = &summary.last_sync_completed
    {
        println!(
            "Last sync completed: {}",
            completed.format("%Y-%m-%d %H:%M:%S UTC")
        );
    }
    if !summary.active_enumeration_zones.is_empty() {
        println!(
            "Full enumeration in progress: {}",
            summary.active_enumeration_zones.join(", ")
        );
    }
    if let Some(api_total) = summary.last_api_total_at_start {
        if summary.last_api_total_at_start_partial {
            println!("Last API total at start: partial, {api_total}");
        } else {
            println!("Last API total at start: {api_total}");
        }
    }
    if summary.last_inventory_drop_detected {
        let library = summary
            .last_inventory_drop_library
            .as_deref()
            .unwrap_or("unknown library");
        if let (Some(previous), Some(current)) = (
            summary.last_inventory_drop_previous_total,
            summary.last_inventory_drop_current_total,
        ) {
            let drop = previous.saturating_sub(current);
            println!(
                "Inventory warning: {library} dropped {drop} assets since the previous comparable full run ({previous} -> {current})"
            );
        } else {
            println!("Inventory warning: {library} dropped below the previous comparable full run");
        }
    }

    if args.failed && summary.failed > 0 {
        println!();
        println!("Failed assets:");
        let printed = paginate_print(&db, Section::Failed).await?;
        print_truncation_tail(summary_count_to_usize(summary.failed), printed);
    }

    if args.pending && summary.pending > 0 {
        println!();
        println!("Pending assets:");
        let printed = paginate_print(&db, Section::Pending).await?;
        print_truncation_tail(summary_count_to_usize(summary.pending), printed);
    }

    if args.downloaded && summary.downloaded > 0 {
        println!();
        println!("Downloaded assets:");
        let printed = paginate_print(&db, Section::Downloaded).await?;
        print_truncation_tail(summary_count_to_usize(summary.downloaded), printed);
    }

    Ok(())
}

pub(crate) fn backup_status_line(summary: &state::types::SyncSummary) -> String {
    if summary.active_sync_started.is_some() {
        return "Backup status: unsafe - sync is currently in progress".to_string();
    }

    if summary.last_sync_started.is_none() {
        return "Backup status: unsafe - no sync has completed yet".to_string();
    }

    let mut reasons = Vec::new();
    if summary.unresolved_identity_zones > 0 {
        reasons.push(format!(
            "unresolved asset identity in {}; checkpoint replay is required",
            count_phrase(summary.unresolved_identity_zones, "provider zone")
        ));
    }

    if summary.last_sync_interrupted
        || summary
            .last_sync_status
            .as_deref()
            .is_some_and(|status| status == "interrupted")
    {
        reasons.push("last sync was interrupted".to_string());
    } else if summary
        .last_sync_status
        .as_deref()
        .is_some_and(|status| status != "complete")
        && let Some(status) = &summary.last_sync_status
    {
        reasons.push(format!("last sync status is {status}"));
    }
    if summary.last_sync_completed.is_none() {
        reasons.push("last sync did not record completion".to_string());
    }
    if summary.last_sync_assets_failed > 0 {
        reasons.push(format!(
            "{} failed in the last sync",
            count_phrase(summary.last_sync_assets_failed, "asset")
        ));
    }
    if summary.last_sync_enumeration_errors > 0 {
        reasons.push(format!(
            "{} occurred in the last sync",
            count_phrase(summary.last_sync_enumeration_errors, "enumeration error")
        ));
    }
    if summary.failed > 0 {
        reasons.push(format!(
            "{} {}",
            count_phrase(summary.failed, "failed asset"),
            remain_verb(summary.failed)
        ));
    }
    if summary.pending > 0 {
        reasons.push(format!(
            "{} {}",
            count_phrase(summary.pending, "pending asset"),
            remain_verb(summary.pending)
        ));
    }
    if summary.awaiting_provider_verification > 0 {
        reasons.push(format!(
            "{} awaiting provider verification",
            count_phrase(summary.awaiting_provider_verification, "asset")
        ));
    }
    let metadata_capture_remaining: u64 = summary
        .metadata_capture
        .iter()
        .map(|capture| capture.remaining_assets)
        .sum();
    if metadata_capture_remaining > 0 {
        reasons.push(format!(
            "{} metadata capture {}",
            count_phrase(metadata_capture_remaining, "asset"),
            remain_verb(metadata_capture_remaining)
        ));
    }
    let metadata_capture_failures: u64 = summary
        .metadata_capture
        .iter()
        .map(|capture| capture.failed_assets)
        .sum();
    if metadata_capture_failures > 0 {
        reasons.push(format!(
            "{} metadata capture failure{} recorded",
            metadata_capture_failures,
            if metadata_capture_failures == 1 {
                ""
            } else {
                "s"
            }
        ));
    }
    if summary.last_inventory_drop_detected {
        reasons.push("inventory warning is active".to_string());
    }

    if reasons.is_empty() {
        "Backup status: safe - last sync completed and no pending or failed assets are recorded"
            .to_string()
    } else {
        format!("Backup status: unsafe - {}", reasons.join("; "))
    }
}

fn count_phrase(count: u64, singular: &str) -> String {
    if count == 1 {
        format!("1 {singular}")
    } else {
        format!("{count} {singular}s")
    }
}

const fn remain_verb(count: u64) -> &'static str {
    if count == 1 { "remains" } else { "remain" }
}

/// Prints the `Service:` line at the top of `kei status`. Errors from
/// the per-platform probe (no systemd, headless macOS, locked-down SCM)
/// are absorbed so a probe failure never poisons the rest of the status
/// command -- the state DB summary is the load-bearing output.
async fn print_service_section() {
    let state = match service_state().await {
        Ok(state) => state,
        Err(e) => {
            tracing::debug!(error = %e, "service_state probe failed; rendering placeholder");
            println!("Service: status unavailable");
            println!();
            return;
        }
    };
    println!("{}", render_oneline(&state));
    println!();
}

/// Which `kei status` listing is being paginated. Picks the state-DB page
/// fetcher and the per-row print fn.
#[derive(Clone, Copy)]
enum Section {
    Failed,
    Pending,
    Downloaded,
}

/// Stream a status listing through the state-DB pagination primitive,
/// printing up to [`LISTING_CAP`] rows. Returns the number of rows printed
/// so the caller can decide whether to emit the "... and N more" tail.
///
/// The page size is smaller than `LISTING_CAP` so pagination is exercised
/// before the cap kicks in; post-cap rows are skipped via an early break,
/// not by narrowing the SQL query.
async fn paginate_print(db: &state::SqliteStateDb, section: Section) -> anyhow::Result<usize> {
    let page_size: u32 = 100;
    let mut offset: u64 = 0;
    let mut printed: usize = 0;
    'outer: loop {
        let page = match section {
            Section::Failed => db.get_failed_page(offset, page_size).await?,
            Section::Pending => db.get_pending_page(offset, page_size).await?,
            Section::Downloaded => db.get_downloaded_page(offset, page_size).await?,
        };
        if page.is_empty() {
            break;
        }
        for asset in &page {
            if printed >= LISTING_CAP {
                break 'outer;
            }
            match section {
                Section::Failed => print_failed(asset),
                Section::Pending => print_pending(asset),
                Section::Downloaded => print_downloaded(asset),
            }
            printed += 1;
        }
        offset += page.len() as u64;
    }
    Ok(printed)
}

/// Convert a `summary.{failed,pending,downloaded}` count (u64 from SQLite
/// `COUNT(*)`) to `usize` for `print_truncation_tail`. Counts are well under
/// `u32::MAX` on any supported target, so the cast is lossless.
#[allow(
    clippy::cast_possible_truncation,
    reason = "asset counts from SQLite; cap-to-usize is safe on supported targets"
)]
fn summary_count_to_usize(count: u64) -> usize {
    count as usize
}

fn print_failed(asset: &AssetRecord) {
    let last_seen = asset.last_seen_at.format("%Y-%m-%d %H:%M:%S");
    println!(
        "  {} ({}) - {} (attempts: {}, last seen: {})",
        asset.filename,
        asset.id,
        asset.last_error.as_deref().unwrap_or("unknown error"),
        asset.download_attempts,
        last_seen
    );
}

fn print_pending(asset: &AssetRecord) {
    let last_seen = asset.last_seen_at.format("%Y-%m-%d %H:%M:%S");
    println!(
        "  {} ({}) - attempts: {}, last seen: {}",
        asset.filename, asset.id, asset.download_attempts, last_seen
    );
}

fn print_downloaded(asset: &AssetRecord) {
    // status='downloaded' rows are written with local_path by mark_downloaded,
    // so a missing path here means a state-DB invariant violation (manual
    // edit, partial migration, upsert after mark_downloaded without path).
    // Surface it clearly rather than hiding it.
    let local = asset.local_path.as_ref().map_or_else(
        || "<MISSING local_path>".to_string(),
        |p| p.display().to_string(),
    );
    println!("  {} ({}) -> {}", asset.filename, asset.id, local);
}
