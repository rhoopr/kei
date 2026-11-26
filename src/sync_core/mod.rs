//! The v0.30 sync engine: observe, plan, download, report.
//!
//! A zone's position advances with the records it covers and never waits for
//! downloads. Owed copies are rows in `files` that later cycles retry. See
//! `docs/sync-core.md`.

pub(crate) mod drain;
pub(crate) mod plan;
pub(crate) mod runner;
pub(crate) mod state;

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
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct ZoneReport {
    pub(crate) zone: String,
    pub(crate) pages: u64,
    /// The zone's feed reached the current end of its changes.
    pub(crate) caught_up: bool,
    /// iCloud rejected the saved position, so this cycle started a full listing.
    pub(crate) position_reset: bool,
    pub(crate) plan: plan::PlanStats,
    pub(crate) drain: drain::DrainStats,
    /// Why observation stopped early, if it did.
    pub(crate) error: Option<String>,
}

impl CycleReport {
    /// Every zone caught up and no owed copy is left.
    pub(crate) fn complete(&self) -> bool {
        self.zones
            .iter()
            .all(|zone| zone.caught_up && zone.error.is_none())
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
        if let Err(error) = observe(state, library, cancel, &mut zone_report).await {
            tracing::warn!(zone = %zone, error = %format!("{error:#}"), "Could not read iCloud changes; owed downloads continue");
            zone_report.error = Some(format!("{error:#}"));
        }
        for folder in plan::queried_smart_folders(&config.smart_folders) {
            if cancel.is_cancelled() {
                break;
            }
            match library.smart_folder_members(&folder).await {
                Ok(members) => state.replace_smart_members(&zone, &folder, members).await?,
                Err(error) => {
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
        for (library, zone_report) in libraries.iter().zip(report.zones.iter_mut()) {
            zone_report.plan = plan::plan_zone(state, library.zone_name(), config).await?;
        }
    }

    for (library, zone_report) in libraries.iter().zip(report.zones.iter_mut()) {
        if cancel.is_cancelled() {
            break;
        }
        match drain::drain_zone(state, library, config, client, cancel).await {
            Ok(stats) => zone_report.drain = stats,
            Err(error) => {
                tracing::warn!(zone = %zone_report.zone, error = %format!("{error:#}"), "Downloads stopped early; owed copies are retried next cycle");
                zone_report
                    .error
                    .get_or_insert_with(|| format!("{error:#}"));
            }
        }
    }

    report.completeness = state.completeness().await?;
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

/// Read the zone's feed to its current end, storing each page with its position.
async fn observe(
    state: &State,
    library: &PhotoLibrary,
    cancel: &CancellationToken,
    report: &mut ZoneReport,
) -> anyhow::Result<()> {
    let zone = library.zone_name();
    let mut position = state.position(zone).await?;
    let mut reset = false;
    for _ in 0..MAX_PAGES_PER_CYCLE {
        if cancel.is_cancelled() {
            return Ok(());
        }
        let page = match library.feed_page(position.as_deref()).await {
            Ok(page) => page,
            Err(FeedError::PositionRejected(reason)) if !reset => {
                tracing::warn!(zone, reason = %reason, "iCloud rejected the saved sync position; listing the library again");
                state.reset_position(zone).await?;
                position = None;
                reset = true;
                report.position_reset = true;
                continue;
            }
            Err(error) => return Err(error.into()),
        };
        anyhow::ensure!(
            !(page.more_coming && position.as_deref() == Some(page.sync_token.as_str())),
            "iCloud returned the same position with more changes pending"
        );
        let caught_up = !page.more_coming;
        let next = page.sync_token.clone();
        state
            .commit_page(zone, page.observations, page.sync_token, caught_up)
            .await?;
        report.pages += 1;
        position = Some(next);
        if caught_up {
            report.caught_up = true;
            return Ok(());
        }
    }
    Ok(())
}

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

#[cfg(test)]
mod tests;
