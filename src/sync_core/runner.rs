//! Drive sync core cycles for the `sync` command and watch mode.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::Context;
use tokio_util::sync::CancellationToken;

use crate::auth::SharedSession;
use crate::config::Config;
use crate::download::filter::PathDerivationConfig;
use crate::icloud::photos::{PRIMARY_ZONE_NAME, PhotoLibrary};
use crate::retry::RetryConfig;
use crate::state::db::account::AccountOwner;

use super::{CoreConfig, CycleReport, State, run_cycle};

const ACCOUNT_KEY: &str = "account_provider";

pub(crate) struct Run<'a> {
    pub(crate) config: &'a Config,
    pub(crate) libraries: Vec<PhotoLibrary>,
    pub(crate) session: SharedSession,
    pub(crate) owner: &'a AccountOwner,
    pub(crate) retry: RetryConfig,
    pub(crate) shutdown: &'a CancellationToken,
}

/// Run one cycle, or cycles at the watch interval until shutdown.
pub(crate) async fn run(run: Run<'_>) -> anyhow::Result<()> {
    let config = run.config;
    let legacy = crate::state::db::account::state_path(
        &config.auth.cookie_directory,
        &config.auth.username,
        config.auth.domain.as_str(),
    )
    .await?;
    let state = open_owned(&State::path_for(&legacy), run.owner).await?;
    let core = core_config(config, run.retry);
    let libraries = supported_libraries(run.libraries);
    anyhow::ensure!(
        !libraries.is_empty(),
        "No supported library is selected. This build syncs the primary library only."
    );

    loop {
        let client = run.session.read().await.download_client().clone();
        let report = run_cycle(&state, &libraries, &core, &client, run.shutdown).await?;
        log_report(&report);
        let Some(interval) = config.watch.interval else {
            if run.shutdown.is_cancelled() {
                return Ok(());
            }
            anyhow::ensure!(
                report.completeness.failed == 0
                    && report.zones.iter().all(|zone| zone.error.is_none()),
                "Sync finished with {} failed downloads; the next run retries them",
                report.completeness.failed
            );
            return Ok(());
        };
        tokio::select! {
            () = run.shutdown.cancelled() => return Ok(()),
            () = tokio::time::sleep(Duration::from_secs(interval)) => {}
        }
    }
}

/// Open the sync state and bind it to the authenticated account.
async fn open_owned(path: &std::path::Path, owner: &AccountOwner) -> anyhow::Result<State> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let state = State::open(path).await?;
    let provider = owner
        .provider_key()
        .context("The iCloud account identity is unavailable")?;
    match state.get_kv(ACCOUNT_KEY).await? {
        Some(bound) => anyhow::ensure!(
            bound == provider,
            "This sync state belongs to a different iCloud account"
        ),
        None => state.set_kv(ACCOUNT_KEY, provider).await?,
    }
    Ok(state)
}

fn supported_libraries(libraries: Vec<PhotoLibrary>) -> Vec<PhotoLibrary> {
    libraries
        .into_iter()
        .filter(|library| {
            let supported = library.zone_name() == PRIMARY_ZONE_NAME;
            if !supported {
                tracing::warn!(
                    zone = library.zone_name(),
                    "Shared libraries are not synced by this build yet; skipping"
                );
            }
            supported
        })
        .collect()
}

fn core_config(config: &Config, retry: RetryConfig) -> CoreConfig {
    let directory: PathBuf = config.download.directory.clone();
    let paths = PathDerivationConfig {
        directory: std::sync::Arc::from(directory.as_path()),
        folder_structure: config.download.folder_structure.clone(),
        folder_structure_albums: config.download.folder_structure_albums.as_str().into(),
        folder_structure_smart_folders: config
            .download
            .folder_structure_smart_folders
            .as_str()
            .into(),
        media: config.filters.media,
        skip_created_before: config.filters.skip_created_before,
        skip_created_after: config.filters.skip_created_after,
        live_photo_mode: config.photos.live_photo_mode,
        live_photo_mov_filename_policy: config.photos.live_photo_mov_filename_policy,
        edited_naming: config.photos.edited_naming,
        raw_policy: config.photos.raw_policy,
        file_match_policy: config.photos.file_match_policy,
        keep_unicode_in_filenames: config.photos.keep_unicode_in_filenames,
        album_name: None,
        library: PRIMARY_ZONE_NAME.into(),
        exclude_asset_ids: std::sync::Arc::default(),
    };
    CoreConfig {
        directory,
        paths,
        albums: config.filters.selection.albums.clone(),
        unfiled: config.filters.selection.unfiled,
        temp_suffix: config.download.temp_suffix.clone(),
        concurrency: usize::from(config.download.threads_num.max(1)),
        retry,
    }
}

fn log_report(report: &CycleReport) {
    for zone in &report.zones {
        tracing::info!(
            zone = %zone.zone,
            pages = zone.pages,
            caught_up = zone.caught_up,
            listed_again = zone.position_reset,
            planned = zone.plan.planned,
            downloaded = zone.drain.downloaded,
            adopted = zone.drain.adopted,
            failed = zone.drain.failed,
            locally_modified = zone.drain.modified,
            "Library synced"
        );
    }
    let c = report.completeness;
    tracing::info!(
        saved = c.done,
        owed = c.pending,
        failed = c.failed,
        locally_modified = c.modified,
        complete = report.complete(),
        "Backup status"
    );
}
