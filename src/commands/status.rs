#![allow(
    clippy::print_stdout,
    reason = "CLI subcommand whose primary purpose is to print a status report to stdout"
)]

use crate::cli;
use crate::config;
use crate::state;
use crate::state::{AssetRecord, StateDb};

use super::{print_truncation_tail, LISTING_CAP};

/// Run the status command.
pub(crate) async fn run_status(
    args: cli::StatusArgs,
    globals: &config::GlobalArgs,
    toml: Option<&config::TomlConfig>,
) -> anyhow::Result<()> {
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
    println!("  Failed:     {}", summary.failed);
    println!();

    if let Some(started) = &summary.last_sync_started {
        println!(
            "Last sync started:   {}",
            started.format("%Y-%m-%d %H:%M:%S UTC")
        );
    }
    if let Some(completed) = &summary.last_sync_completed {
        println!(
            "Last sync completed: {}",
            completed.format("%Y-%m-%d %H:%M:%S UTC")
        );
    }

    if args.failed && summary.failed > 0 {
        println!();
        println!("Failed assets:");
        let failed = db.get_failed().await?;
        let shown = failed.len().min(LISTING_CAP);
        for asset in failed.iter().take(LISTING_CAP) {
            print_failed(asset);
        }
        print_truncation_tail(failed.len(), shown);
    }

    if args.pending && summary.pending > 0 {
        println!();
        println!("Pending assets:");
        let pending = db.get_pending().await?;
        let shown = pending.len().min(LISTING_CAP);
        for asset in pending.iter().take(LISTING_CAP) {
            print_pending(asset);
        }
        print_truncation_tail(pending.len(), shown);
    }

    if args.downloaded && summary.downloaded > 0 {
        println!();
        println!("Downloaded assets:");
        // page_size is smaller than LISTING_CAP so pagination is still
        // exercised before the cap kicks in — the post-cap rows are
        // skipped via an early break, not by narrowing the SQL query.
        let page_size: u32 = 100;
        let mut offset: u64 = 0;
        let mut printed: usize = 0;
        'outer: loop {
            let page = db.get_downloaded_page(offset, page_size).await?;
            if page.is_empty() {
                break;
            }
            for asset in &page {
                if printed >= LISTING_CAP {
                    break 'outer;
                }
                print_downloaded(asset);
                printed += 1;
            }
            offset += page.len() as u64;
        }
        // summary.downloaded is a u64 from the state DB count query; the
        // cap is well under u32::MAX, so `as usize` is lossless on any
        // 32-bit-or-wider target kei runs on.
        #[allow(
            clippy::cast_possible_truncation,
            reason = "downloaded count from SQLite; cap-to-usize is safe on supported targets"
        )]
        let total = summary.downloaded as usize;
        print_truncation_tail(total, printed);
    }

    Ok(())
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
