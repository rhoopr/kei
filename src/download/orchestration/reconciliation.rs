//! Recover recorded source journals without planning a new archive layout.

use std::sync::Arc;

use anyhow::Result;
use tokio_util::sync::CancellationToken;

use crate::download::file;

use super::config::DownloadConfig;
use super::models::{DownloadStore, SyncStats};

#[derive(Debug, Default)]
pub(crate) struct PathReconciliationResult {
    pub(crate) complete: bool,
    /// Local work is complete, but only a fresh smart-folder query can finish
    /// selection reconciliation. This never authorizes promotion on its own.
    pub(crate) complete_after_smart_query: bool,
    pub(crate) stats: SyncStats,
}

fn classify_reconciliation_io_error(error: &anyhow::Error) -> Option<std::io::ErrorKind> {
    error
        .downcast_ref::<std::io::Error>()
        .map(std::io::Error::kind)
}

async fn recover_source_journals(
    db: &dyn DownloadStore,
    records: &[crate::state::AssetRecord],
) -> Result<PathReconciliationResult> {
    // Root drift can leave interrupted media or sidecar transactions outside
    // the newly configured tree. Restore them before planning reads the source.
    for record in records {
        let Some(path) = &record.local_path else {
            continue;
        };
        match db.source_metadata_recovery(path).await {
            Ok(
                recovery @ (crate::state::db::SourceMetadataRecovery::ProtectedLegacy
                | crate::state::db::SourceMetadataRecovery::ReadOnly),
            ) => {
                tracing::warn!(
                    stage = "path_reconciliation",
                    reason = if recovery == crate::state::db::SourceMetadataRecovery::ReadOnly {
                        "readonly_source"
                    } else {
                        "protected_legacy_source"
                    },
                    "Deferring protected source recovery"
                );
                continue;
            }
            Err(_) => {
                tracing::warn!(
                    stage = "path_reconciliation",
                    reason = "source_writer_guard_failed",
                    "Could not authorize recorded source recovery; keeping catalogue paths and checkpoints unchanged"
                );
                return Ok(PathReconciliationResult {
                    stats: SyncStats {
                        failed: 1,
                        ..SyncStats::default()
                    },
                    ..PathReconciliationResult::default()
                });
            }
            Ok(crate::state::db::SourceMetadataRecovery::ManagedPrimary) => {
                // Committed layouts have no pending operation to recover old-root journals.
                let inspection = async {
                    let root = path
                        .parent()
                        .ok_or_else(|| anyhow::anyhow!("Recorded source has no parent"))?;
                    let mut name = path
                        .file_name()
                        .ok_or_else(|| anyhow::anyhow!("Recorded source has no filename"))?
                        .to_os_string();
                    name.push(".xmp");
                    let sidecar = path.with_file_name(name);
                    for target in [path, &sidecar] {
                        match file::has_replacement_journal(root, target).await {
                            Ok(true) => return Ok(true),
                            Ok(false) => {}
                            // A moved old parent leaves no journal at this recorded path.
                            Err(error)
                                if classify_reconciliation_io_error(&error)
                                    == Some(std::io::ErrorKind::NotFound) => {}
                            Err(error) => return Err(error),
                        }
                    }
                    anyhow::Ok(false)
                }
                .await;
                if !matches!(inspection, Ok(false)) {
                    tracing::warn!(
                        reason = "managed_source_replacement_pending",
                        "Managed source replacement needs its layout owner; keeping catalogue paths and checkpoints unchanged"
                    );
                    return Ok(PathReconciliationResult {
                        stats: SyncStats {
                            failed: 1,
                            ..SyncStats::default()
                        },
                        ..PathReconciliationResult::default()
                    });
                }
                continue;
            }
            Ok(crate::state::db::SourceMetadataRecovery::Ordinary) => {}
        }
        if let Err(error) = file::recover_metadata_replacements(path).await {
            tracing::warn!(error = %format!("{error:#}"), "Could not recover recorded source; keeping catalogue paths and checkpoints unchanged");
            return Ok(PathReconciliationResult {
                complete: false,
                complete_after_smart_query: false,
                stats: SyncStats {
                    failed: 1,
                    ..SyncStats::default()
                },
            });
        }
    }

    Ok(PathReconciliationResult {
        complete: true,
        ..PathReconciliationResult::default()
    })
}

/// Recover existing durable operations without planning a new archive layout.
/// Pending operations keep configuration reconciliation incomplete.
pub(crate) async fn recover_recorded_sources(
    client: &reqwest::Client,
    passes: &[crate::commands::AlbumPass],
    config: Arc<DownloadConfig>,
    shutdown_token: CancellationToken,
) -> Result<PathReconciliationResult> {
    let Some(db) = &config.state_db else {
        return Ok(PathReconciliationResult::default());
    };
    if passes.is_empty() {
        return Ok(PathReconciliationResult::default());
    };

    let config = Arc::new(config.check_ownership().await?);
    if !config.allows_existing_mutation() {
        tracing::warn!(
            "Recorded file operations remain pending because historical ownership cannot be indexed; current read-only reuse and new publication may proceed"
        );
        return Ok(PathReconciliationResult::default());
    }
    let protected_paths =
        crate::download::legacy_preservation::protected_replacement_paths(Some(db.as_ref()))
            .await?;
    match file::recover_conditional_replacements(&config.directory, &protected_paths).await {
        Ok(0) => {}
        Ok(_) => {
            tracing::warn!(
                "Protected replacement journals remain pending before source reconciliation"
            );
            return Ok(PathReconciliationResult::default());
        }
        Err(error) => {
            tracing::warn!(error = %format!("{error:#}"), "Could not recover replacements before path reconciliation; keeping catalogue paths and checkpoints unchanged");
            return Ok(PathReconciliationResult {
                complete: false,
                complete_after_smart_query: false,
                stats: SyncStats {
                    failed: 1,
                    ..SyncStats::default()
                },
            });
        }
    }

    let managed = config.edited_naming == crate::types::EditedNaming::Primary
        || db.has_primary_layouts(&config.library).await?;
    // A changed root or template identifies a new family. Finish recorded
    // handovers before reading catalogue paths or allowing that family to
    // commit, so an old operation cannot later restore stale rendition facts.
    // The layout owner also recovers replacements at each recorded old root.
    if managed
        && let Err(error) = Box::pin(super::primary_layout::recover(
            client,
            passes,
            &config,
            &shutdown_token,
        ))
        .await
    {
        tracing::warn!(error = %format!("{error:#}"), "Could not recover managed layouts before path reconciliation; keeping checkpoints unchanged");
        return Ok(PathReconciliationResult {
            stats: SyncStats {
                failed: 1,
                ..SyncStats::default()
            },
            ..PathReconciliationResult::default()
        });
    }

    let protected: std::collections::HashSet<String> = db
        .get_protected_legacy_ids()
        .await?
        .into_iter()
        .filter(|(library, _)| library == config.library.as_ref())
        .map(|(_, id)| id)
        .collect();
    let mut records = Vec::new();
    let mut offset = 0u64;
    const PAGE_SIZE: u32 = 1_000;
    loop {
        let page = db.get_downloaded_page(offset, PAGE_SIZE).await?;
        let page_len = page.len();
        records.extend(page.into_iter().filter(|record| {
            record.library.as_ref() == config.library.as_ref()
                && !record.metadata.is_deleted
                && !protected.contains(record.id.as_ref())
        }));
        if page_len < PAGE_SIZE as usize {
            break;
        }
        offset = offset.saturating_add(u64::from(PAGE_SIZE));
    }
    if records.is_empty() {
        return Ok(PathReconciliationResult {
            complete: db
                .primary_layout_operations(config.library.to_string())
                .await?
                .is_empty(),
            ..PathReconciliationResult::default()
        });
    }

    let recovered = recover_source_journals(db.as_ref(), &records).await?;
    if !recovered.complete {
        return Ok(recovered);
    }

    // Earlier phases retain their own durable files and claims. Recovery
    // returning successfully does not certify that their work has completed.
    Ok(PathReconciliationResult {
        complete: db
            .primary_layout_operations(config.library.to_string())
            .await?
            .is_empty(),
        ..recovered
    })
}

#[cfg(test)]
mod tests;
