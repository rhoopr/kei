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

    let db_path = super::super::get_db_path(globals, toml).await?;

    if !db_path.exists() {
        println!("No state database found at {}", db_path.display());
        println!("Run a sync first to create the database.");
        return Ok(());
    }

    let db =
        state::SqliteStateDb::open_owned(&db_path, &super::super::get_account_owner(globals, toml))
            .await?;
    let summary = db.get_summary().await?;

    println!("State Database: {}", db_path.display());
    println!();
    println!("Recorded catalog assets (includes historical state; not coverage totals):");
    println!("  Total:      {}", summary.total_assets);
    println!("  Downloaded: {}", summary.downloaded);
    println!("  Pending:    {}", summary.pending);
    println!("  Policy excluded: {}", summary.policy_excluded);
    println!("  Failed:     {}", summary.failed);
    println!("  Source deleted: {}", summary.source_deleted);
    let trusted = db.stored_trusted_legacy_file_count().await?;
    if trusted > 0 {
        println!(
            "  Stored Trusted Legacy File acceptances: {trusted} (provider verification unknown)"
        );
    }

    println!(
        "  Awaiting provider verification: {}",
        summary.awaiting_provider_verification
    );
    if summary.source_revalidation_eligible > 0 {
        println!(
            "  Source revalidation obligations: {}",
            summary.source_revalidation_eligible
        );
    }
    if summary.source_revalidation_retained > 0 {
        println!(
            "  Retained source obligations not yet eligible: {}",
            summary.source_revalidation_retained
        );
    }
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
        if action == "repair_retained_checkpoint_evidence" {
            println!(
                "  Recorded retained-history recovery found malformed or unsupported evidence. Preserve the state database and media. This recorded event does not establish a current sync hold."
            );
        } else if action == "retained_checkpoint_retry_exhausted" {
            println!(
                "  Recorded retained-checkpoint recovery exhausted its automatic attempts. Historical evidence remains preserved; this recorded event does not establish a current sync hold or missing selected content."
            );
        } else if action == "await_retained_checkpoint_evidence" {
            println!(
                "  Recorded retained cursor was rejected. Preserve the state database and media. Current baseline and work evidence are reported separately; this recorded event does not establish an active retry or current sync hold."
            );
        }
    }
    if summary.unresolved_sparse_records > 0 {
        println!();
        println!(
            "Recorded sparse observations: {} unresolved, {} with a saved retry deadline (provenance below; not an active retry promise)",
            summary.unresolved_sparse_records, summary.deferred_sparse_records
        );
    }
    if !summary.metadata_capture.is_empty() {
        println!("Metadata capture:");
        for capture in &summary.metadata_capture {
            if capture.unattributed_legacy_assets > 0 {
                println!(
                    "  {}: {} preserved records with unresolved historical attribution; {} recorded preservation associations",
                    capture.library,
                    capture.unattributed_legacy_assets,
                    capture.unattributed_legacy_pending
                );
            }
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
                if capture.unresolved_assets > 0 {
                    println!(
                        "    {} unresolved identities, {} deferred retries",
                        capture.unresolved_assets, capture.deferred_assets
                    );
                }
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
    if let Some(primary) = &summary.primary_layout {
        println!(
            "Managed primary: {} bound families; {} independently preserved files; {} pending operations ({} held)",
            primary.bound_families,
            primary.preserved_files,
            primary.pending_operations,
            primary.held_operations
        );
        println!("  Preserved files are local copies, separate from sync network downloads.");
    }
    println!("{}", backup_status_line(&summary));
    match db.observed_selection_status().await {
        Ok(observations) if !observations.is_empty() => {
            for observation in &observations {
                let disposition = if observation.settlement_unknown {
                    "fully observed; settlement unknown"
                } else if observation.pending_media
                    || observation.pending_metadata
                    || observation.unresolved
                {
                    "fully observed; requested work outstanding"
                } else {
                    "complete as of this recorded observation"
                };
                println!(
                    "Recorded {} selection {}: {disposition} (observation completed at {} Unix UTC).",
                    observation.selector_label, observation.plan_id, observation.observed_at
                );
                println!(
                    "  Outstanding recorded obligations: media={}; requested metadata={}; unresolved selection decisions={}.",
                    observation.pending_media, observation.pending_metadata, observation.unresolved
                );
            }
            println!(
                "  Other selected libraries or plans may lack proof. This covers the recorded selection through its saved provider boundary, not unseen later changes or a fresh check of today's file integrity. Read-only adoption preserves existing metadata; it does not claim metadata was rewritten."
            );
        }
        _ => println!(
            "As-of selected coverage: unknown; no matching validated observation/checkpoint proof."
        ),
    }
    match db.recorded_selection_work().await {
        Ok(work) => println!("{}", render_selection_work(&work)),
        Err(_) => println!(
            "Current selected coverage: unknown. Recorded work could not be read or validated; preserve state. No completeness claim is available."
        ),
    }
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

    if args.pending {
        let matches = db.pending_match_status(LISTING_CAP).await?;
        if !matches.is_empty() {
            println!("\nExisting-file matching attempts (local paths; review before sharing):");
            for pending in matches {
                println!("{}", render_match_attempt(&pending));
            }
            println!(
                "Details are bounded last-attempt evidence, not a complete pending-work inventory. Retry schedules require current selection, authentication and file revalidation; configuration-ineligible work is retained without a promise of active retry."
            );
        }
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
        return "Recorded run/database notes: sync is currently in progress".to_string();
    }

    if summary.last_sync_started.is_none() {
        return "Recorded run/database notes: no sync has completed yet".to_string();
    }

    let mut reasons = Vec::new();
    if summary
        .last_recovery_action
        .as_deref()
        .is_some_and(|action| {
            matches!(
                action,
                "await_retained_checkpoint_evidence"
                    | "retained_checkpoint_retry_exhausted"
                    | "repair_retained_checkpoint_evidence"
            )
        })
    {
        reasons.push(
            "retained provider history is unavailable; this is not a current-selection coverage assessment".to_owned(),
        );
    }
    if summary.unresolved_identity_zones > 0 {
        reasons.push(format!(
            "unresolved asset identity in {}; historical and current associations are reported separately below",
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
    if summary.source_revalidation_eligible > 0 {
        reasons.push(format!(
            "{} awaiting source verification",
            count_phrase(summary.source_revalidation_eligible, "source obligation")
        ));
    }
    if summary.source_revalidation_retained > 0 {
        reasons.push(format!(
            "{} retained for later source verification",
            count_phrase(summary.source_revalidation_retained, "source obligation")
        ));
    }
    let unattributed: u64 = summary
        .metadata_capture
        .iter()
        .map(|capture| capture.unattributed_legacy_assets)
        .sum();
    if unattributed > 0 {
        reasons.push(format!(
            "{unattributed} preserved legacy records have unresolved historical attribution"
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
        "Recorded run/database notes: last sync recorded completion and no pending or failed catalog rows remain; this does not prove coverage"
            .to_string()
    } else {
        format!("Recorded run/database notes: {}", reasons.join("; "))
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
    let diagnostic = asset
        .last_error
        .as_deref()
        .and_then(crate::state::db::match_diagnostics::MatchDiagnostics::decode);
    let error = diagnostic.as_ref().map(|report| report.last_transfer_error.as_deref().unwrap_or("Existing-file identity is unproven; use status --pending for local candidate details"))
        .unwrap_or_else(|| match asset.last_error.as_deref() {
            Some(error) if error.starts_with("existing_match_v1:") => "Unreadable matching diagnostic; preserve state and use normal sync to obtain fresh evidence",
            Some(error) => error,
            None => "unknown error",
        });
    println!(
        "  {} ({}) - {} (attempts: {}, last seen: {})",
        asset.filename, asset.id, error, asset.download_attempts, last_seen
    );
}

fn print_pending(asset: &AssetRecord) {
    let last_seen = asset.last_seen_at.format("%Y-%m-%d %H:%M:%S");
    println!(
        "  {} ({}) - attempts: {}, last seen: {}",
        asset.filename, asset.id, asset.download_attempts, last_seen
    );
    if let Some(reason) = asset
        .last_error
        .as_deref()
        .filter(|reason| !reason.starts_with("existing_match_v1:"))
    {
        println!("    Last recorded error: {reason:?}");
        println!(
            "    Retained work; retry eligibility depends on the current selection and provider access."
        );
    }
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

fn render_match_attempt(
    pending: &crate::state::db::match_diagnostics::PendingMatchStatus,
) -> String {
    let attempt = &pending.attempt;
    let mut lines = vec![format!(
        "  Destination: {:?}; last match attempt: {} (Unix UTC)",
        attempt.destination.to_path(),
        attempt.observed_at
    )];
    if pending.policy_excluded {
        lines.push("    Asset currently marked policy-excluded. Work is retained; review the selected libraries/albums/filters if this content is wanted. A saved deadline is not an active retry promise.".into());
    }
    match pending.schedule {
        Some((attempts, deadline)) => lines.push(format!("    Recorded retry attempts: {attempts}; next retry not before: {deadline} (Unix UTC). Current configuration eligibility has not been checked by status.")),
        None => lines.push("    No matching current unfinished destination/schedule: stale or unbound attempt. This is not a claim that automatic retry is active.".into()),
    }
    for refusal in &attempt.explanation.candidates {
        let path = refusal
            .path
            .as_ref()
            .map(|path| format!("{:?}", path.to_path()))
            .unwrap_or_else(|| "candidate discovery/scope".into());
        lines.push(format!("    {path}: {:?}", refusal.reason));
        let action = match refusal.reason.as_str() {
            "candidate_unreadable_unsafe_or_changed" | "candidate_discovery_incomplete" => {
                "Check archive mount/access and file stability. Restore access to the same paths, then run normal sync; symlinks or unsafe paths are not accepted."
            }
            "provider_signature_missing" | "scoped_receipt_or_reservation_validation_failed" => {
                "Current provider/receipt or destination evidence is unavailable or inconsistent. Normal sync can retry after access or evidence changes; do not edit state or bypass ownership checks."
            }
            _ => {
                "Content identity is unproven. Names and sizes are insufficient. Preserve these files and their state/receipts; supported provider proof or a valid bound receipt is required. There is no safe force-trust or uncertain-download action. Unchanged retries cannot manufacture missing proof."
            }
        };
        lines.push(format!("      {action}"));
    }
    if attempt.explanation.omitted > 0 || pending.omitted > 0 {
        lines.push(format!(
            "    Additional diagnostics omitted: {} candidates, {} destination attempts.",
            attempt.explanation.omitted, pending.omitted
        ));
    }
    lines.join("\n")
}

#[cfg(test)]
mod match_diagnostic_tests {
    use super::render_match_attempt;
    use crate::state::db::match_diagnostics::{MatchAttempt, MatchExplanation, PendingMatchStatus};
    use crate::state::db::provider_selection::SelectionPath;

    #[test]
    fn status_match_explanation_escapes_paths_and_never_promises_eligibility() {
        let path = std::path::Path::new("/archive/line\n\u{1b}[31m.JPG");
        let mut explanation = MatchExplanation::default();
        explanation.add(
            Some(path),
            "unsupported_provider_proof_without_valid_receipt",
        );
        let mut pending = PendingMatchStatus {
            policy_excluded: false,
            attempt: MatchAttempt {
                generation: None,
                pass: None,
                child: "private-child".into(),
                destination: SelectionPath::from_path(path),
                checksum: "private-signature".into(),
                observed_at: 123,
                explanation,
            },
            schedule: None,
            omitted: 0,
        };
        let stale = render_match_attempt(&pending);
        assert!(!stale.contains('\u{1b}'));
        assert!(!stale.contains("private-child"));
        assert!(!stale.contains("private-signature"));
        assert!(stale.contains("stale or unbound"));
        assert!(stale.contains("no safe force-trust"));
        pending.schedule = Some((3, 999));
        let scheduled = render_match_attempt(&pending);
        assert!(scheduled.contains("next retry not before: 999"));
        assert!(scheduled.contains("eligibility has not been checked"));
    }
}

fn render_selection_work(
    work: &crate::state::db::provider_generations::coverage::RecordedSelectionWork,
) -> String {
    let mut lines = vec!["Current selected coverage: unknown offline. Retained records are not a complete current provider inventory.".to_owned(),
        "Recorded selection work (each profile keeps its own scope/settings; none is assumed to be today's configuration):".into()];
    if work.profiles.is_empty() {
        lines.push("  No validated current-format retained profile. Zero retained work does not mean a complete backup.".into());
    }
    for (index, profile) in work.profiles.iter().enumerate() {
        lines.push(format!(
            "  Profile {}: {} retained observation windows; {} unsealed or held.",
            index + 1,
            profile.roots,
            profile.unsealed_or_held_roots
        ));
        lines.push(format!("    Destinations: {} verified/settled; {} awaiting media; {} awaiting requested metadata after media completion.",profile.verified_destinations,profile.pending_media,profile.pending_metadata));
        lines.push(format!(
            "    Decisions: {} unresolved selection/identity; {} recorded exclusions.",
            profile.deferred_decisions, profile.excluded_decisions
        ));
    }
    lines.push("  Verified means validated stored completion proof, not a fresh filesystem check. Counts describe resource/pass/destination obligations, not unique files. Repeated identical obligations within a profile are counted once; unfinished evidence takes precedence. Profiles and decision counts must not be added into a coverage total.".into());
    lines.push(format!("Stored completed-media receipts: {} validated; {} invalid/unreadable. These may overlap retained work and older selections; they do not establish current membership, requested metadata coverage, or today's file integrity.",work.stored_media_receipts,work.invalid_receipts));
    lines.push(format!("Unresolved source observations: {} current-origin; {} historical-origin; {} unclassified. These are observations, not inferred missing-file counts.",work.current_source_observations,work.historical_source_observations,work.unclassified_source_observations));
    lines.push(format!("Other retained evidence: {} historical selection windows; {} older/unsupported profiles; {} invalid/unreadable windows. Historical associations and optional backfill are not current missing-content totals.",work.historical_roots,work.unsupported_profiles,work.invalid_roots));
    lines.push("Current eligibility is rechecked by normal sync. Recorded exclusions or older profiles may no longer match your settings. A saved cursor or healthy service is not backup completeness. `verify` checks recorded files; it is not a provider-coverage census.".into());
    lines.join("\n")
}

#[cfg(test)]
mod selection_work_tests {
    use super::render_selection_work;
    use crate::state::db::provider_generations::coverage::{
        RecordedProfileWork, RecordedSelectionWork,
    };

    #[test]
    fn recorded_selection_output_never_infers_current_completeness_from_receipts_or_empty_work() {
        let mut work = RecordedSelectionWork::default();
        let empty = render_selection_work(&work);
        assert!(empty.contains("Current selected coverage: unknown offline"));
        assert!(empty.contains("Zero retained work does not mean a complete backup"));
        work.stored_media_receipts = 100;
        work.historical_source_observations = 22;
        work.unsupported_profiles = 1;
        work.invalid_roots = 1;
        work.profiles = vec![
            RecordedProfileWork {
                verified_destinations: 2,
                pending_media: 1,
                ..Default::default()
            },
            RecordedProfileWork {
                pending_metadata: 1,
                excluded_decisions: 3,
                ..Default::default()
            },
        ];
        let output = render_selection_work(&work);
        assert!(output.contains("Profile 1:"));
        assert!(output.contains("Profile 2:"));
        assert!(output.contains("1 awaiting media"));
        assert!(output.contains("1 awaiting requested metadata"));
        assert!(output.contains("22 historical-origin"));
        assert!(output.contains("1 invalid/unreadable windows"));
        assert!(
            output.contains("`verify` checks recorded files; it is not a provider-coverage census")
        );
        assert!(!output.contains("Backup status: safe"));
    }
}
