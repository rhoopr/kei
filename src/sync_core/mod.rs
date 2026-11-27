//! The v0.30 sync engine: observe, plan, download, report.
//!
//! A zone's position advances with the records it covers and never waits for
//! downloads. Owed copies are rows in `files` that later cycles retry. See
//! `docs/sync-core.md`.

pub(crate) mod access;
pub(crate) mod drain;
mod indexing;
pub(crate) mod metadata;
pub(crate) mod plan;
mod progress;
pub(crate) mod runner;
pub(crate) mod state;
mod support;
pub(crate) mod upgrade;

pub(crate) use support::support_summary;

use std::path::PathBuf;

use reqwest::Client;
use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;

use crate::download::filter::PathDerivationConfig;
use crate::icloud::photos::PhotoLibrary;
use crate::icloud::photos::feed::FeedError;
use crate::retry::RetryConfig;
use crate::selection::{AlbumSelector, SmartFolderSelector};

pub(crate) use state::State;

/// Pages read before a zone yields to planning and downloads.
const MAX_PAGES_PER_CYCLE: usize = 100_000;
const PLAN_CONFIG_KEY: &str = "plan_config";
/// Kv key holding the download folder the saved copies were checked against.
const DIRECTORY_KEY: &str = "download_directory";

/// Settings the sync core reads.
#[derive(Debug, Clone)]
pub(crate) struct CoreConfig {
    pub(crate) directory: PathBuf,
    /// Base path rules. Its `folder_structure` is the unfiled template.
    pub(crate) paths: PathDerivationConfig,
    pub(crate) albums: AlbumSelector,
    pub(crate) smart_folders: SmartFolderSelector,
    pub(crate) unfiled: bool,
    pub(crate) temp_suffix: String,
    pub(crate) concurrency: usize,
    pub(crate) retry: RetryConfig,
    pub(crate) transfer: Transfer,
    /// One shared byte budget for every download, when a limit is set.
    pub(crate) bandwidth: Option<crate::download::BandwidthLimiter>,
    /// Metadata to write after downloads; `None` when none is enabled.
    pub(crate) metadata: Option<crate::download::metadata_pass::Settings>,
    /// Plan every saved copy's metadata again this cycle.
    pub(crate) refresh_metadata: bool,
}

/// Which owed copies a cycle works on.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum Transfer {
    /// Download owed copies and adopt matching local files.
    #[default]
    All,
    /// Only adopt local files already at planned paths; download nothing.
    AdoptOnly,
    /// Observe and plan only.
    None,
}

impl CoreConfig {
    /// Changes to any of these re-plan every asset. Paths already chosen
    /// stay; only which copies are wanted, and where new ones go, changes.
    fn plan_fingerprint(&self) -> String {
        let text = format!(
            "{:?}|{:?}|{:?}|{:?}|{}",
            self.paths,
            self.albums,
            self.smart_folders,
            self.unfiled,
            self.directory.display()
        );
        format!("{:x}", Sha256::digest(text.as_bytes()))
    }
}

/// What one cycle did, per zone and overall.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct CycleReport {
    pub(crate) zones: Vec<ZoneReport>,
    pub(crate) completeness: state::Completeness,
    /// Photos moved to a Shared Library that is not selected. They are
    /// not saved; selecting that library saves them.
    pub(crate) unselected_shared: u64,
    /// iCloud refused the session. Sign in again and repeat the cycle;
    /// everything already observed stays recorded.
    pub(crate) session_expired: bool,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct ZoneReport {
    pub(crate) zone: String,
    pub(crate) pages: u64,
    /// The zone's feed reached the current end of its changes.
    pub(crate) caught_up: bool,
    /// iCloud rejected the saved position, so this cycle started a full listing.
    pub(crate) position_reset: bool,
    /// The zone had no saved position, so this cycle listed it from the start.
    pub(crate) full_listing: bool,
    /// Malformed records left out of the catalog.
    pub(crate) skipped_records: u64,
    /// iCloud has not finished indexing the zone, or its listing taken while
    /// it was has not been repeated yet. Queried smart folders wait.
    pub(crate) indexing: bool,
    pub(crate) plan: plan::PlanStats,
    pub(crate) drain: drain::DrainStats,
    pub(crate) metadata: metadata::MetadataStats,
    /// Why observation stopped early, if it did.
    pub(crate) error: Option<String>,
}

impl CycleReport {
    /// Why this cycle counts against service health, if it does.
    ///
    /// Health tracks whether kei can still read iCloud and save photos. A
    /// copy that keeps failing the same way is reported in the run report
    /// and metrics, but does not count again every cycle. New failures with
    /// nothing saved alongside them do, since nothing is getting through.
    pub(crate) fn health_problem(&self) -> Option<String> {
        if let Some(error) = self.zones.iter().find_map(|zone| zone.error.clone()) {
            return Some(error);
        }
        let newly_failed: u64 = self.zones.iter().map(|zone| zone.drain.newly_failed).sum();
        let saved: u64 = self
            .zones
            .iter()
            .map(|zone| zone.drain.downloaded + zone.drain.adopted)
            .sum();
        (newly_failed > 0 && saved == 0)
            .then(|| format!("{newly_failed} downloads failed and none succeeded"))
    }

    /// Every zone caught up and no owed copy is left.
    pub(crate) fn complete(&self) -> bool {
        self.zones
            .iter()
            .all(|zone| zone.caught_up && zone.error.is_none() && !zone.indexing)
            && self.completeness.pending == 0
            && self.completeness.failed == 0
    }
}

/// Run one sync cycle over `libraries`.
pub(crate) async fn run_cycle(
    state: &State,
    libraries: &[PhotoLibrary],
    config: &CoreConfig,
    client: &Client,
    cancel: &CancellationToken,
) -> anyhow::Result<CycleReport> {
    let run = state.start_run(now()).await?;
    follow_directory_change(state, &config.directory).await?;
    let fingerprint = config.plan_fingerprint();
    if state.get_kv(PLAN_CONFIG_KEY).await?.as_deref() != Some(fingerprint.as_str()) {
        state.mark_all_dirty().await?;
        state.set_kv(PLAN_CONFIG_KEY, &fingerprint).await?;
    }

    // Observe every zone before planning any: a photo in one zone can be
    // placed by albums in another.
    let mut report = CycleReport::default();
    for library in libraries {
        if cancel.is_cancelled() {
            break;
        }
        let zone = library.zone_name().to_owned();
        let mut zone_report = ZoneReport {
            zone: zone.clone(),
            ..ZoneReport::default()
        };
        repair_album_names(state, &zone).await?;
        let indexing = indexing::Indexing::check(library).await;
        indexing::before_observe(state, &zone, indexing).await?;
        if let Err(error) = observe(state, library, cancel, &mut zone_report).await {
            report.session_expired |= crate::icloud::photos::session::is_session_error(&error);
            tracing::warn!(zone = %zone, error = %format!("{error:#}"), "Could not read iCloud changes; owed downloads continue");
            zone_report.error = Some(format!("{error:#}"));
        }
        indexing::after_observe(
            state,
            &zone,
            indexing,
            zone_report.caught_up && zone_report.error.is_none(),
        )
        .await?;
        zone_report.indexing = indexing::relist_owed(state, &zone).await?;
        // These folders come from iCloud's search index, which may be
        // partial while indexing runs. Their stored members stay.
        let queried = if matches!(
            indexing,
            indexing::Indexing::Finished | indexing::Indexing::Unavailable(_)
        ) {
            plan::queried_smart_folders(&config.smart_folders)
        } else {
            Vec::new()
        };
        for folder in queried {
            if cancel.is_cancelled() {
                break;
            }
            match library.smart_folder_members(&folder).await {
                Ok(members) => {
                    state.replace_smart_members(&zone, &folder, members).await?;
                    state.set_kv(&smart_read_key(&zone, &folder), "1").await?;
                }
                Err(error) => {
                    report.session_expired |=
                        crate::icloud::photos::session::is_session_error(&error);
                    tracing::warn!(zone = %zone, folder = %folder, error = %format!("{error:#}"), "Could not read a smart folder; its copies wait");
                    zone_report
                        .error
                        .get_or_insert_with(|| format!("{error:#}"));
                }
            }
        }
        report.zones.push(zone_report);
    }

    let mut all_listed = true;
    for library in libraries {
        all_listed &= state.listed(library.zone_name()).await?;
    }
    if all_listed && !cancel.is_cancelled() {
        // Every album is known now, so a named album that matches none is a
        // mistake in the selection, not one still to arrive.
        let missing = plan::missing_named_albums(state, &config.albums).await?;
        if !missing.is_empty() {
            let pending = state.completeness().await?.pending;
            state
                .finish_run(run, now(), "failed", 0, 0, pending)
                .await?;
            let names = missing
                .iter()
                .map(|name| format!("\"{name}\""))
                .collect::<Vec<_>>()
                .join(", ");
            anyhow::bail!(
                "{} {names} not found in the selected libraries. Check [filters].albums: a name \
                 matches an album's own name, or its full path such as \"Travel/Paris\". \
                 Nothing was downloaded.",
                if missing.len() == 1 {
                    "Album"
                } else {
                    "Albums"
                }
            );
        }
        for (library, zone_report) in libraries.iter().zip(report.zones.iter_mut()) {
            let zone = library.zone_name();
            if !smart_folders_read(state, zone, config).await? {
                // Placing a photo before its queried folder is known would
                // file it as unfiled, and that copy would stay.
                tracing::warn!(
                    zone,
                    "New photos wait until iCloud's smart folders such as Screenshots can be read"
                );
                continue;
            }
            zone_report.plan = plan::plan_zone(state, zone, config).await?;
        }
    }

    for (library, zone_report) in libraries.iter().zip(report.zones.iter_mut()) {
        if cancel.is_cancelled() || config.transfer == Transfer::None {
            break;
        }
        match drain::drain_zone(state, library, config, client, cancel).await {
            Ok(stats) => zone_report.drain = stats,
            Err(error) => {
                report.session_expired |= crate::icloud::photos::session::is_session_error(&error);
                tracing::warn!(zone = %zone_report.zone, error = %format!("{error:#}"), "Downloads stopped early; owed copies are retried next cycle");
                zone_report
                    .error
                    .get_or_insert_with(|| format!("{error:#}"));
            }
        }
    }

    // Metadata comes after every download and never touches a position.
    if let Some(settings) = &config.metadata
        && config.transfer == Transfer::All
        && !cancel.is_cancelled()
    {
        metadata::prepare(state, settings, config.refresh_metadata).await?;
        for (library, zone_report) in libraries.iter().zip(report.zones.iter_mut()) {
            if cancel.is_cancelled() {
                break;
            }
            match metadata::run_zone(
                state,
                library.zone_name(),
                &config.directory,
                settings,
                config.concurrency,
                cancel,
            )
            .await
            {
                Ok(stats) => zone_report.metadata = stats,
                Err(error) => {
                    tracing::warn!(zone = %zone_report.zone, error = %format!("{error:#}"), "Metadata writes stopped early; they are retried next cycle");
                    zone_report.metadata.failed += 1;
                }
            }
        }
    }

    report.completeness = state.completeness().await?;
    report.unselected_shared = state
        .uncovered_stand_ins(
            libraries
                .iter()
                .map(|library| library.zone_name().to_owned())
                .collect(),
        )
        .await?;
    let downloaded = report.zones.iter().map(|zone| zone.drain.downloaded).sum();
    let failed = report.completeness.failed;
    let outcome = if cancel.is_cancelled() {
        "interrupted"
    } else if report.complete() {
        "complete"
    } else {
        "incomplete"
    };
    state
        .finish_run(
            run,
            now(),
            outcome,
            downloaded,
            failed,
            report.completeness.pending,
        )
        .await?;
    Ok(report)
}

/// Saved, wanted copies whose file is missing from `directory`, as relative
/// paths.
pub(crate) async fn missing_saved_copies(
    state: &State,
    directory: &std::path::Path,
) -> anyhow::Result<Vec<String>> {
    let mut missing = Vec::new();
    for file in state.list_files(&[state::FileState::Done]).await? {
        if !file.wanted {
            continue;
        }
        match tokio::fs::symlink_metadata(directory.join(&file.path)).await {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => missing.push(file.path),
            Err(error) => anyhow::bail!("Could not check {}: {error}", file.path),
            Ok(_) => {}
        }
    }
    Ok(missing)
}

/// Saved copies are recorded relative to the download folder. When the
/// folder changes, copies that were not moved along are owed again, so the
/// new folder ends up complete instead of only receiving new photos.
async fn follow_directory_change(state: &State, directory: &std::path::Path) -> anyhow::Result<()> {
    let current = directory.to_string_lossy().into_owned();
    let previous = state.get_kv(DIRECTORY_KEY).await?;
    if previous.as_deref() == Some(current.as_str()) {
        return Ok(());
    }
    if previous.is_some() {
        let missing = missing_saved_copies(state, directory).await?;
        for path in &missing {
            if let Some(row) = state.file_at(path).await? {
                state.owe_again(row.id, None).await?;
            }
        }
        tracing::warn!(
            missing = missing.len(),
            "The download folder changed; saved copies not found in the new folder are downloaded again"
        );
    }
    state.set_kv(DIRECTORY_KEY, &current).await
}

/// Earlier builds did not ask the feed for album fields, so their catalogs
/// hold albums without names, and iCloud resends an album only when it
/// changes. List such a zone from the start once to fetch them. Planning
/// carries on meanwhile; photos in those albums wait.
async fn repair_album_names(state: &State, zone: &str) -> anyhow::Result<()> {
    let key = format!("album_names_relisted:{zone}");
    if state.get_kv(&key).await?.is_some() || state.unnamed_albums(zone).await? == 0 {
        return Ok(());
    }
    tracing::warn!(
        zone,
        "Some saved albums have no name; listing the library again once to fetch them"
    );
    state.relist(zone).await?;
    state.set_kv(&key, "1").await
}

fn smart_read_key(zone: &str, folder: &str) -> String {
    format!("smart_read:{zone}:{folder}")
}

/// Every selected queried smart folder's members were read at least once.
async fn smart_folders_read(
    state: &State,
    zone: &str,
    config: &CoreConfig,
) -> anyhow::Result<bool> {
    for folder in plan::queried_smart_folders(&config.smart_folders) {
        if state
            .get_kv(&smart_read_key(zone, &folder))
            .await?
            .is_none()
        {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Read the zone's feed to its current end, storing each page with its position.
async fn observe(
    state: &State,
    library: &PhotoLibrary,
    cancel: &CancellationToken,
    report: &mut ZoneReport,
) -> anyhow::Result<()> {
    let zone = library.zone_name();
    let mut position = state.position(zone).await?;
    report.full_listing = position.is_none();
    if report.full_listing {
        tracing::info!(
            zone,
            "Listing the whole library; a first listing of a large library can take a while"
        );
    }
    let mut reset = false;
    let mut records = 0_usize;
    let mut progress = progress::Progress::start();
    for _ in 0..MAX_PAGES_PER_CYCLE {
        if cancel.is_cancelled() {
            return Ok(());
        }
        let page = match library.feed_page(position.as_deref()).await {
            Ok(page) => page,
            Err(FeedError::PositionRejected(reason)) if !reset => {
                tracing::warn!(zone, reason = %reason, "iCloud rejected the saved sync position; listing the whole library again");
                state.reset_position(zone).await?;
                position = None;
                reset = true;
                report.position_reset = true;
                continue;
            }
            Err(FeedError::Other(error)) => return Err(error),
            Err(error) => return Err(error.into()),
        };
        anyhow::ensure!(
            !(page.more_coming && position.as_deref() == Some(page.sync_token.as_str())),
            "iCloud returned the same position with more changes pending"
        );
        let caught_up = !page.more_coming;
        let next = page.sync_token.clone();
        records += page.observations.len();
        report.skipped_records += page.skipped;
        state
            .commit_page(zone, page.observations, page.sync_token, caught_up)
            .await?;
        report.pages += 1;
        position = Some(next);
        if caught_up {
            report.caught_up = true;
            return Ok(());
        }
        if progress.due() {
            // Each page is saved before this line, so a restart resumes here.
            tracing::info!(
                zone,
                pages = report.pages,
                records,
                "Listing library: progress saved"
            );
        }
    }
    Ok(())
}

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

#[cfg(test)]
mod tests;
