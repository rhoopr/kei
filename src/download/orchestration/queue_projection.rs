//! Current-config admission from retained provider sources into existing queues.

use anyhow::Result;
use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;

use super::config::{DownloadConfig, hash_download_config};
use super::models::DownloadControls;
use crate::commands::{AlbumPass, PassKind};
use crate::download::planner::{TaskPlanner, pending_record_for_task};
use crate::state::db::provider_selection::{
    MAX_SELECTION_BYTES, SelectionDecision, SelectionDestination, SelectionManifest,
    SelectionOutcome, SelectionSource,
};
use crate::state::db::provider_work::{MAX_WORK_BYTES, WorkPlan};

const MAX_WORK_ATTEMPTS_PER_CYCLE: usize = 64;

fn config_hash(config: &DownloadConfig, zone: &serde_json::Value) -> String {
    let value = serde_json::json!({"work_format":1,"metadata_capture_revision":crate::state::types::METADATA_CAPTURE_REVISION,"selection":"private-library-wide-without-exclusions",
        "zone":zone,"path_and_renditions":hash_download_config(config),
        "before":config.skip_created_before.map(crate::config::CreatedDateFilter::fingerprint),
        "after":config.skip_created_after.map(crate::config::CreatedDateFilter::fingerprint),
        "metadata_flags":crate::download::pipeline::MetadataFlags::from(config).bits(),
        "capture_timestamp_repair":config.capture_timestamp_repair==crate::download::CaptureTimestampRepair::ReplaceWithCaptureLocal,
        "repair_truncated":config.repair_truncated,
    });
    format!("{:x}", Sha256::digest(value.to_string().as_bytes()))
}

struct WorkContext {
    capture: crate::icloud::photos::inbox::ShadowCapture,
    scope: String,
    zone: serde_json::Value,
    effective: DownloadConfig,
    config_hash: String,
}

// Scheduling and admission share the same scope and selection boundary.
fn work_context(
    passes: &[AlbumPass],
    config: &DownloadConfig,
    controls: DownloadControls,
) -> Result<Option<WorkContext>> {
    // Selection that needs rank or membership evidence remains with its existing
    // owner. These observations are retained without an admission receipt.
    if !controls.run_mode.downloads_files()
        || config.recent.is_some()
        || config.retry_only
        || config.refresh_metadata
        || !config.exclude_asset_ids.is_empty()
    {
        return Ok(None);
    }
    let [pass] = passes else { return Ok(None) };
    if pass.kind != PassKind::Unfiled || !pass.exclude_ids.is_empty() {
        return Ok(None);
    }
    let Some((capture, scope, zone)) = pass.album.catalog_work_scope()? else {
        return Ok(None);
    };
    let Some(db) = &config.state_db else {
        return Ok(None);
    };
    let capture_store =
        std::sync::Arc::clone(&capture.db) as std::sync::Arc<dyn crate::download::DownloadStore>;
    anyhow::ensure!(
        std::sync::Arc::ptr_eq(db, &capture_store),
        "Catalog work database context mismatch"
    );
    if pass.album.zone_name() != config.library.as_ref() {
        anyhow::bail!("Catalog work library scope mismatch");
    }
    let effective = config.with_pass(pass);
    let config_hash = config_hash(&effective, &zone);
    Ok(Some(WorkContext {
        capture,
        scope,
        zone,
        effective,
        config_hash,
    }))
}

pub(crate) async fn has_due_retained_work(
    passes: &[AlbumPass],
    config: &DownloadConfig,
    controls: DownloadControls,
) -> Result<bool> {
    let Some(context) = work_context(passes, config, controls)? else {
        return Ok(false);
    };
    Ok(context
        .capture
        .db
        .has_due_provider_work(context.capture.owner, context.scope, context.config_hash)
        .await?)
}

pub(super) async fn admit_retained_work(
    passes: &[AlbumPass],
    config: &DownloadConfig,
    controls: DownloadControls,
    cancel: &CancellationToken,
) -> Result<()> {
    let Some(WorkContext {
        capture,
        scope,
        zone,
        effective,
        config_hash,
    }) = work_context(passes, config, controls)?
    else {
        return Ok(());
    };
    let [pass] = passes else { return Ok(()) };
    let mut planner = TaskPlanner::for_download(effective.state_db.as_deref()).await?;
    let mut admitted = 0usize;
    let mut deferred = 0usize;
    for _ in 0..MAX_WORK_ATTEMPTS_PER_CYCLE {
        if cancel.is_cancelled() {
            break;
        }
        let Some(source) = capture
            .db
            .next_work_source(capture.owner.clone(), scope.clone(), config_hash.clone())
            .await?
        else {
            break;
        };
        // Original source validation remains independent of current lookup.
        let validated_scope = scope.clone();
        let source = tokio::task::spawn_blocking(move || {
            let validated = crate::icloud::photos::catalog_observed_page(
                source.page.page.body.clone(),
                &validated_scope,
                &source.page.page.request_cursor,
            )?;
            anyhow::ensure!(
                validated == source.page.page,
                "Catalog work source provenance mismatch"
            );
            Ok::<_, anyhow::Error>(source)
        })
        .await??;
        let ordinal = usize::try_from(source.ordinal)?;
        let identity = source
            .page
            .page
            .identities
            .get(ordinal)
            .ok_or_else(|| anyhow::anyhow!("Catalog work ordinal mismatch"))?;
        let confirmation = tokio::select! {
            biased;
            ()=cancel.cancelled()=>break,
            confirmation=pass.album.confirm_catalog_asset(&identity.name)=>confirmation,
        };
        let (body, master, records, destinations, reason) = match confirmation {
            Ok(current) => {
                let plan = planner
                    .plan_download_asset(&current.asset, &effective)
                    .await?;
                let reason =
                    if current.asset.metadata().is_hidden || current.asset.metadata().is_deleted {
                        "current_library_ineligible"
                    } else if plan.malformed_resource.is_some() {
                        "malformed_current_resource"
                    } else if plan.filter_reason.is_some() {
                        "currently_filtered"
                    } else {
                        ""
                    };
                let records = if reason.is_empty() {
                    plan.tasks
                        .iter()
                        .map(|task| pending_record_for_task(&effective, &current.asset, task))
                        .collect()
                } else {
                    Vec::new()
                };
                let destinations = if reason.is_empty() {
                    plan.tasks
                        .iter()
                        .zip(&records)
                        .map(|(task, record): (_, &crate::state::AssetRecord)| {
                            Ok(SelectionDestination {
                                version_size: task.version_size.as_str().to_owned(),
                                path: std::path::absolute(&task.download_path)?
                                    .to_str()
                                    .ok_or_else(|| {
                                        anyhow::anyhow!("Selection destination is not UTF-8")
                                    })?
                                    .to_owned(),
                                checksum: task.checksum.to_string(),
                                size: task.size,
                                metadata_hash: record.metadata.metadata_hash.clone().ok_or_else(
                                    || {
                                        anyhow::anyhow!(
                                            "Selection metadata fingerprint is unavailable"
                                        )
                                    },
                                )?,
                            })
                        })
                        .collect::<Result<Vec<_>>>()?
                } else {
                    Vec::new()
                };
                (
                    Some(current.body),
                    Some(current.asset.id().to_owned()),
                    records,
                    destinations,
                    reason,
                )
            }
            Err(_unresolved) => (
                None,
                None,
                Vec::new(),
                Vec::new(),
                "current_lookup_unresolved",
            ),
        };
        if cancel.is_cancelled() {
            break;
        }
        let manifest = SelectionManifest {
            scope: scope.clone(),
            zone: zone.clone(),
            config_hash: config_hash.clone(),
            profile: serde_json::json!({"format":1,"coverage":"confirmed_sources_only",
                "selection":"private-library-wide-without-exclusions","pass":"unfiled"}),
            sources: vec![SelectionSource {
                page_id: source.page.id.0,
                ordinal: source.ordinal,
                body_hash: source.page.body_hash.clone(),
            }],
            decisions: vec![SelectionDecision {
                pass_key: "unfiled".to_owned(),
                child: identity.name.clone(),
                master: master.clone(),
                confirmation: body.clone(),
                destinations,
                outcome: if reason.is_empty() {
                    SelectionOutcome::Selected
                } else if matches!(reason, "currently_filtered" | "current_library_ineligible") {
                    SelectionOutcome::Excluded
                } else {
                    SelectionOutcome::Deferred
                },
                reason: reason.to_owned(),
            }],
        };
        let generation = capture
            .db
            .capture_selection_shadow(capture.owner.clone(), manifest.clone(), MAX_SELECTION_BYTES)
            .await?;
        anyhow::ensure!(
            capture
                .db
                .replay_selection_shadow(capture.owner.clone(), generation)
                .await?
                == manifest,
            "Selection shadow replay differs from current planning"
        );
        // Shadow parity cannot authorize queue admission or a source cursor.
        let admission = capture
            .db
            .project_provider_work(
                capture.owner.clone(),
                WorkPlan {
                    source,
                    scope: scope.clone(),
                    zone: zone.clone(),
                    config_hash: config_hash.clone(),
                    confirmation: body,
                    master,
                    records,
                    reason,
                },
                MAX_WORK_BYTES,
            )
            .await?;
        match admission {
            crate::state::db::provider_work::WorkAdmission::Admitted => admitted += 1,
            crate::state::db::provider_work::WorkAdmission::Deferred => deferred += 1,
        }
    }
    if admitted + deferred > 0 {
        tracing::debug!(
            admitted,
            deferred,
            "Finished bounded catalog work admission; completion remains with existing queues"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests;
