//! Sync entry point and composition of full, incremental, and recovery passes.

use std::sync::Arc;

use anyhow::{Context, Result};
use reqwest::Client;
use tokio_util::sync::CancellationToken;

use crate::icloud::photos::session::is_session_error as is_provider_session_error;
use crate::icloud::photos::{SyncTokenError, error::saved_cursor_rejected};

use super::cleanup::cleanup_orphan_part_files;
use super::config::{DownloadConfig, InventoryCheckpointPolicy};
use super::context::backfill_asset_master_mappings_from_album_history;
use super::full::{download_photos_full_with_token, download_photos_full_with_token_policy};
use super::incremental::download_photos_incremental;
use super::maintenance::{observe_metadata_capture, run_metadata_capture_repair};
use super::models::{
    DownloadControls, DownloadOutcome, SyncMode, SyncResult, SyncStats, merge_download_outcomes,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IncrementalErrorClass {
    TokenFallback,
    SessionExpired,
    TransientFailure,
    StaticFallback,
    CaptureRefused,
}

/// Classify incremental-enumeration failures before deciding whether to fall
/// back to a full records/query pass.
///
/// Token errors that CloudKit explicitly marks unsafe fall back to full
/// enumeration. Auth and transport transients bubble up because a full pass
/// would likely hit the same service condition. Other static/decode errors
/// fall back so malformed token responses do not strand the user.
fn classify_incremental_error(error: &anyhow::Error) -> IncrementalErrorClass {
    if is_provider_session_error(error) {
        return IncrementalErrorClass::SessionExpired;
    }
    if error
        .downcast_ref::<crate::icloud::photos::error::ShadowPageError>()
        .is_some()
    {
        return IncrementalErrorClass::CaptureRefused;
    }
    if error
        .downcast_ref::<SyncTokenError>()
        .is_some_and(SyncTokenError::should_fallback_to_full)
    {
        return IncrementalErrorClass::TokenFallback;
    }
    if error
        .downcast_ref::<crate::auth::error::AuthError>()
        .is_some()
        || error
            .downcast_ref::<reqwest::Error>()
            .is_some_and(is_transient_reqwest_error)
    {
        return IncrementalErrorClass::TransientFailure;
    }
    IncrementalErrorClass::StaticFallback
}

/// An activation state refusal cannot start another rank inventory. All other
/// errors retain the existing session, token, transport and decode classification.
fn classify_selection_incremental_error(error: &anyhow::Error) -> IncrementalErrorClass {
    if error
        .downcast_ref::<crate::state::error::StateError>()
        .is_some()
    {
        IncrementalErrorClass::CaptureRefused
    } else {
        classify_incremental_error(error)
    }
}

fn is_transient_reqwest_error(error: &reqwest::Error) -> bool {
    error
        .status()
        .is_some_and(|status| status == 429 || status.as_u16() >= 500)
        || error.is_timeout()
        || error.is_connect()
}

fn replacement_inventory_config(
    config: &Arc<DownloadConfig>,
    error: &anyhow::Error,
) -> Option<Arc<DownloadConfig>> {
    (config.inventory_checkpoint == InventoryCheckpointPolicy::Baseline
        && saved_cursor_rejected(error))
    .then(|| {
        Arc::new(DownloadConfig {
            sync_mode: SyncMode::Full,
            ..config.as_ref().clone()
        })
    })
}

/// Apply one incremental failure decision and preserve its checkpoint evidence.
/// A refused selection holds its checkpoint. Rejected cursors can start inventory.
async fn finish_incremental_attempt(
    attempt: Result<SyncResult>,
    client: &Client,
    passes: &[crate::commands::AlbumPass],
    config: &Arc<DownloadConfig>,
    controls: DownloadControls,
    cancel: CancellationToken,
) -> Result<SyncResult> {
    let error = match attempt {
        Ok(result) => return Ok(result),
        Err(error) => error,
    };
    if config.inventory_checkpoint == InventoryCheckpointPolicy::RetainedBridge
        && saved_cursor_rejected(&error)
    {
        return Err(error);
    }
    let class = classify_selection_incremental_error(&error);
    match class {
        IncrementalErrorClass::SessionExpired
        | IncrementalErrorClass::TransientFailure
        | IncrementalErrorClass::CaptureRefused => return Err(error),
        IncrementalErrorClass::TokenFallback | IncrementalErrorClass::StaticFallback => {}
    }
    let replacement = replacement_inventory_config(config, &error);
    let mut result = Box::pin(super::generation::inventory(
        client,
        passes,
        replacement.as_ref().unwrap_or(config),
        controls,
        cancel,
    ))
    .await?;
    result.checkpoint.retained_token_rejected = class == IncrementalErrorClass::TokenFallback;
    Ok(result)
}

pub async fn download_photos_with_sync(
    download_client: &Client,
    passes: &[crate::commands::AlbumPass],
    config: Arc<DownloadConfig>,
    controls: DownloadControls,
    shutdown_token: CancellationToken,
) -> Result<SyncResult> {
    anyhow::ensure!(
        !controls.run_mode.downloads_files() || config.state_db.is_some(),
        "Writable sync requires a state database; use dry-run for a state-free preview"
    );
    if passes.is_empty() {
        return Ok(SyncResult::from_execution(
            DownloadOutcome::Success,
            None,
            SyncStats::default(),
        ));
    }
    if controls.run_mode.downloads_files() && !config.provisional {
        let context = super::generation::context(passes, &config, controls).await?.context(
            "Writable sync requires authenticated destination ownership for every selected query; refresh authenticated library discovery or use dry-run to inspect an unsupported selection",
        )?;
        // This read validates the database's authenticated pin and retained root
        // before recovery, cleanup, or any other dispatcher mutation.
        context.current().await?;
        super::album_directory::prepare(passes, &config, &shutdown_token).await?;
    }
    // Bound the complete cycle's new selection/replay state at its owner,
    // preserving one opaque public future and avoiding caller-specific boxing.
    Box::pin(download_photos_with_sync_inner(
        download_client,
        passes,
        Arc::clone(&config),
        controls,
        shutdown_token,
    ))
    .await
}

async fn download_photos_with_sync_inner(
    download_client: &Client,
    passes: &[crate::commands::AlbumPass],
    config: Arc<DownloadConfig>,
    controls: DownloadControls,
    shutdown_token: CancellationToken,
) -> Result<SyncResult> {
    if config.provisional {
        // Do not turn incomplete listings/lookups into deletion, membership,
        // generation activation, retry retirement, or incremental evidence.
        // Managed replacement needs independently certified current coverage.
        let managed = config.edited_naming == crate::types::EditedNaming::Primary
            || match &config.state_db {
                Some(db) => db.has_primary_layouts(&config.library).await?,
                None => false,
            };
        anyhow::ensure!(
            !managed || config.indexing_inventory_only,
            "Unqualified indexing recovery cannot certify managed primary replacement coverage"
        );
        return download_photos_full_with_token_policy(
            download_client,
            passes,
            &config,
            controls,
            shutdown_token,
            false,
        )
        .await;
    }
    let primary_layout_requested = config.edited_naming == crate::types::EditedNaming::Primary
        || match &config.state_db {
            Some(db) => db.has_primary_layouts(&config.library).await?,
            None => false,
        };
    let primary_layout_active = controls.run_mode.downloads_files() && primary_layout_requested;
    let primary_layout_preview = !controls.run_mode.downloads_files() && primary_layout_requested;
    let config = Arc::new(DownloadConfig {
        primary_layout_active,
        primary_layout_preview,
        ..config.as_ref().clone()
    });
    // Bind the validated selection before historical maintenance can schedule
    // work. The same context also owns enumeration and publication below.
    let config =
        if let Some(context) = super::generation::context(passes, &config, controls).await? {
            Arc::new(DownloadConfig {
                selection_context: Some(Arc::new(context)),
                ..config.as_ref().clone()
            })
        } else {
            config
        };
    super::generation::freeze_before_sync(passes, &config, controls).await?;
    if matches!(controls.run_mode, super::models::DownloadRunMode::Download) {
        let protected = crate::download::legacy_preservation::protected_replacement_paths(
            config.state_db.as_deref(),
        )
        .await?;
        crate::download::file::recover_conditional_replacements(&config.directory, &protected)
            .await?;
    }
    if primary_layout_active {
        super::primary_layout::recover(download_client, passes, &config, &shutdown_token).await?;
    }
    if controls.run_mode.downloads_files() {
        cleanup_orphan_part_files(&config).await;
    }
    if controls.run_mode.downloads_files()
        && matches!(config.sync_mode, SyncMode::Incremental { .. })
        && let Some(db) = &config.state_db
    {
        backfill_asset_master_mappings_from_album_history(db.as_ref()).await;
    }
    let metadata_capture_repair = if config.selection_context.is_some() {
        observe_metadata_capture(&config).await
    } else {
        run_metadata_capture_repair(passes, &config, controls, &shutdown_token).await
    };
    if metadata_capture_repair.auth_errors > 0 {
        return Ok(SyncResult {
            outcome: DownloadOutcome::SessionExpired {
                auth_error_count: metadata_capture_repair.auth_errors,
            },
            sync_token: None,
            stats: metadata_capture_repair.stats,
            checkpoint: metadata_capture_repair.checkpoint,
            full_enumeration_ran: false,
            inventory: None,
        });
    }

    let mut result = match &config.sync_mode {
        SyncMode::Full if config.selection_context.is_some() => {
            Box::pin(super::generation::inventory(
                download_client,
                passes,
                &config,
                controls,
                shutdown_token.clone(),
            ))
            .await
        }
        SyncMode::Incremental { zone_sync_token } if config.selection_context.is_some() => {
            let attempt = download_photos_incremental(
                download_client,
                passes,
                &config,
                zone_sync_token,
                controls,
                shutdown_token.clone(),
            )
            .await;
            finish_incremental_attempt(
                attempt,
                download_client,
                passes,
                &config,
                controls,
                shutdown_token.clone(),
            )
            .await
        }
        // Previews have no retained provider cursor or writable selection.
        // Enumerate the selected queries directly without consulting retry debt.
        _ => {
            download_photos_full_with_token(
                download_client,
                passes,
                &config,
                controls,
                shutdown_token.clone(),
            )
            .await
        }
    }
    .or_else(|error| {
        if !is_provider_session_error(&error) {
            return Err(error);
        }
        Ok(SyncResult {
            full_enumeration_ran: matches!(config.sync_mode, SyncMode::Full),
            ..SyncResult::from_execution(
                DownloadOutcome::SessionExpired {
                    auth_error_count: 1,
                },
                None,
                SyncStats {
                    enumeration_errors: 1,
                    enumeration_incomplete: true,
                    ..SyncStats::default()
                },
            )
        })
    })?;

    if matches!(result.outcome, DownloadOutcome::SessionExpired { .. })
        && controls.run_mode.downloads_files()
        && let Some(db) = &config.state_db
    {
        // Discovery and retained replay can return session expiry without
        // starting a streaming run. Persist the final invocation outcome even
        // when an earlier empty pipeline recorded a successful subpass.
        result.checkpoint.enumeration_errors = result.checkpoint.enumeration_errors.max(1);
        result.checkpoint.enumeration_incomplete = true;
        result.checkpoint.project(&mut result.stats);
        let recorded = async {
            let run = db.start_sync_run().await?;
            db.complete_sync_run(
                run,
                &crate::state::SyncRunStats {
                    assets_seen: result.stats.assets_seen,
                    assets_downloaded: u64::try_from(result.stats.downloaded).unwrap_or(u64::MAX),
                    assets_failed: u64::try_from(result.stats.failed).unwrap_or(u64::MAX),
                    enumeration_errors: u64::try_from(result.stats.enumeration_errors)
                        .unwrap_or(u64::MAX),
                    interrupted: true,
                    ..Default::default()
                },
            )
            .await
        }
        .await;
        if let Err(error) = recorded {
            tracing::warn!(%error, "Failed to retain provider session failure status");
            result.checkpoint.state_write_failures += 1;
            result.stats.state_write_failures += 1;
        }
    }

    let repair_outcome = if metadata_capture_repair.failures > 0
        || ((config.refresh_metadata || config.selection_context.is_none())
            && metadata_capture_repair.stats.metadata_capture_unresolved > 0)
    {
        DownloadOutcome::PartialFailure {
            failed_count: metadata_capture_repair.failures.max(1),
        }
    } else {
        DownloadOutcome::Success
    };
    let repair = SyncResult {
        outcome: repair_outcome,
        sync_token: None,
        stats: metadata_capture_repair.stats,
        checkpoint: metadata_capture_repair.checkpoint,
        full_enumeration_ran: false,
        inventory: None,
    };
    if matches!(repair.outcome, DownloadOutcome::PartialFailure { .. }) {
        result.outcome = merge_download_outcomes(&result.outcome, &repair.outcome);
    }
    result.accumulate(&repair);
    if repair.checkpoint.sync_token_blocked {
        result.sync_token = None;
    }

    super::generation::hold_retained_debt(passes, &config, controls, &mut result).await?;

    Ok(result)
}

#[cfg(test)]
mod tests;

#[cfg(test)]
#[path = "dispatch/retry_tests.rs"]
mod retry_tests;
