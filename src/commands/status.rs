#![allow(
    clippy::print_stdout,
    reason = "CLI subcommand whose primary purpose is to print a status report to stdout"
)]

use crate::cli;
use crate::config;
use crate::service::status::{render_oneline, service_state};
use crate::sync_core::state::{Completeness, FileState};

use super::{LISTING_CAP, print_truncation_tail};

/// Run the status command.
pub(crate) async fn run_status(
    args: cli::StatusArgs,
    globals: &config::GlobalArgs,
    toml: Option<&config::TomlConfig>,
) -> anyhow::Result<()> {
    print_service_section().await;

    let (location, state) = super::core_state(globals, toml).await?;
    let Some(state) = state else {
        println!("No state database found at {}", location.current.display());
        println!("Run a sync first to create the database.");
        return Ok(());
    };
    let completeness = state.completeness().await?;
    let carried_over = state.list_imported().await?;

    println!("State Database: {}", location.current.display());
    println!();
    println!("Backup:");
    println!("  Saved:            {}", completeness.done);
    println!("  Owed:             {}", completeness.pending);
    println!("  Failed:           {}", completeness.failed);
    println!(
        "  Locally modified: {} (kept; kei never overwrites a file you changed)",
        completeness.modified
    );
    if !carried_over.is_empty() {
        println!(
            "  Carried over:     {} (from the previous state; taken over by the next sync)",
            carried_over.len()
        );
    }
    let zones = state.zones().await?;
    if !zones.is_empty() {
        println!();
        println!("Libraries:");
        for zone in &zones {
            let position = if zone.listed && zone.has_position {
                "caught up; the next sync reads only changes"
            } else if zone.has_position {
                "listing in progress; the next sync resumes it"
            } else {
                "the next sync lists the whole library"
            };
            println!("  {}: {position}", zone.zone);
        }
    }
    println!();
    let last = state.last_run().await?;
    if let Some(run) = &last {
        let time = |seconds: i64| {
            chrono::DateTime::from_timestamp(seconds, 0).map_or_else(
                || "unknown".to_owned(),
                |time| time.format("%Y-%m-%d %H:%M:%S UTC").to_string(),
            )
        };
        match run.finished_at {
            None => println!("Sync in progress:   started {}", time(run.started_at)),
            Some(finished) => {
                println!("Last sync started:   {}", time(run.started_at));
                println!(
                    "Last sync finished:  {} ({}; {} downloaded, {} failed)",
                    time(finished),
                    run.outcome.as_deref().unwrap_or("unknown"),
                    run.downloaded,
                    run.failed
                );
            }
        }
    }
    println!("{}", backup_line(last.as_ref(), completeness));

    let sections: [(bool, &str, &[FileState]); 3] = [
        (args.failed, "Failed assets:", &[FileState::Failed]),
        (
            args.pending,
            "Pending assets:",
            &[FileState::Pending, FileState::Adopt],
        ),
        (args.downloaded, "Downloaded assets:", &[FileState::Done]),
    ];
    for (wanted, heading, states) in sections {
        if !wanted {
            continue;
        }
        let mut files = state.list_files(states).await?;
        files.retain(|file| file.wanted);
        let mut lines: Vec<String> = files
            .iter()
            .map(|file| match &file.last_error {
                Some(error) if states.contains(&FileState::Failed) => {
                    format!("  {} - {error}", file.path)
                }
                _ => format!("  {}", file.path),
            })
            .collect();
        if states.contains(&FileState::Done) {
            lines.extend(
                carried_over
                    .iter()
                    .map(|copy| format!("  {} (carried over)", copy.path)),
            );
        }
        if lines.is_empty() {
            continue;
        }
        println!();
        println!("{heading}");
        for line in lines.iter().take(LISTING_CAP) {
            println!("{line}");
        }
        print_truncation_tail(lines.len(), lines.len().min(LISTING_CAP));
    }
    Ok(())
}

/// One line saying whether the recorded backup is complete.
fn backup_line(
    last: Option<&crate::sync_core::state::RunRow>,
    completeness: Completeness,
) -> String {
    let Some(last) = last else {
        return backup_record("no sync has completed yet");
    };
    if last.finished_at.is_none() {
        return backup_record("sync is currently in progress");
    }
    let mut reasons = Vec::new();
    match last.outcome.as_deref() {
        Some("complete") => {}
        Some("interrupted") => reasons.push("last sync was interrupted".to_owned()),
        Some(other) => reasons.push(format!("last sync status is {other}")),
        None => reasons.push("last sync did not record completion".to_owned()),
    }
    if completeness.failed > 0 {
        reasons.push(format!(
            "{} {}",
            count_phrase(completeness.failed, "failed copy", "failed copies"),
            remain_verb(completeness.failed)
        ));
    }
    if completeness.pending > 0 {
        reasons.push(format!(
            "{} {}",
            count_phrase(completeness.pending, "owed copy", "owed copies"),
            remain_verb(completeness.pending)
        ));
    }
    if reasons.is_empty() {
        backup_record("last sync completed and no owed or failed copies are recorded")
    } else {
        backup_record(&reasons.join("; "))
    }
}

const BACKUP_RECORD_SCOPE: &str = "Scope: recorded local state only; this does not establish iCloud completeness or verify local media checksums.";

fn backup_record(headline: &str) -> String {
    format!("Backup record: {headline}\n  {BACKUP_RECORD_SCOPE}")
}

fn count_phrase(count: u64, singular: &str, plural: &str) -> String {
    if count == 1 {
        format!("1 {singular}")
    } else {
        format!("{count} {plural}")
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
