//! Completed-delta membership departures through the existing layout journal.

use super::{LayoutMember, LayoutOperation, LayoutOutcome, finish, preservation_path, sidecar};
use crate::commands::AlbumPass;
use crate::download::DownloadConfig;
use crate::download::file::primary_layout::snapshot;
use crate::download::orchestration::delta::MembershipDelta;
use crate::icloud::photos::inbox::ShadowCapture;
use crate::state::db::primary_layout::{LayoutBinding, LayoutFile, MEMBERSHIP_RETIREMENT_PREFIX};
use crate::state::db::provider_generations::{ActiveGeneration, MembershipLayout};
use crate::state::db::provider_selection::SelectionPath;
use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;

/// No inference from a missing discovery row, snapshot count or missing pass.
/// Current selected publications precede all retirement, even across pages.
pub(in crate::download::orchestration) async fn reconcile(
    passes: &[AlbumPass],
    config: &DownloadConfig,
    capture: &ShadowCapture,
    root: &ActiveGeneration,
    delta: &MembershipDelta,
    recovery_source: Option<&crate::icloud::photos::PhotoAlbum>,
    cancel: &CancellationToken,
) -> Result<bool> {
    let Some(mut layouts) = capture
        .db
        .completed_membership_layouts(capture.owner.clone(), root.id.clone())
        .await?
    else {
        return Ok(false);
    };
    let pending = capture
        .db
        .primary_layout_operations(config.library.to_string())
        .await?;
    let mut complete = true;
    let mut after = None;
    loop {
        if cancel.is_cancelled() {
            return Ok(false);
        }
        let (receipts, next) = capture
            .db
            .membership_destinations(capture.owner.clone(), root.id.clone(), after)
            .await?;
        for receipt in receipts {
            if delta
                .relations
                .get(&(receipt.container.clone(), receipt.child.clone()))
                != Some(&true)
                && delta.containers.get(&receipt.container) != Some(&true)
            {
                continue;
            }
            if delta.deleted_photos.contains(&receipt.child)
                || delta.deleted_photos.contains(&receipt.state_id)
            {
                continue;
            }
            let Some(mut fingerprint) =
                snapshot(&config.directory, &receipt.destination.path.to_path()).await?
            else {
                continue;
            };
            // Native receipts own media only. An adjacent sidecar preserves the copy.
            fingerprint.sha256 = data_encoding::HEXLOWER
                .decode(receipt.local.as_bytes())?
                .try_into()
                .map_err(|_invalid| anyhow::anyhow!("Invalid native membership fingerprint"))?;
            let family = format!(
                "native-membership:{}",
                data_encoding::HEXLOWER.encode(&Sha256::digest(serde_json::to_vec(&(
                    &receipt.generation,
                    &receipt.pass,
                    &receipt.child,
                    &receipt.destination.path
                ))?))
            );
            let decision = format!("{MEMBERSHIP_RETIREMENT_PREFIX}{}", root.id);
            let binding = LayoutBinding {
                family: family.clone(),
                library: receipt.library.clone(),
                source_library: config.library.to_string(),
                child: receipt.state_id.clone(),
                generation: 1,
                decision: decision.clone(),
                policy: "suffix".into(),
                qualifier: receipt.generation.clone(),
                files: vec![LayoutFile {
                    path: receipt.destination.path,
                    version: receipt.destination.version_size,
                    provider_checksum: receipt.destination.checksum,
                    fingerprint,
                    source_checksum: Some(receipt.source),
                    sidecar: None,
                    archive_original: false,
                    metadata_decision: None,
                    local_preserved: false,
                    local_modified: None,
                    embedded_fields: Default::default(),
                }],
            };
            let previous = LayoutOperation {
                operation: String::new(),
                family,
                library: receipt.library,
                source_library: config.library.to_string(),
                child: receipt.state_id,
                asset_record_name: receipt.child,
                pass: receipt.pass,
                metadata_flags: 0,
                refresh_metadata: false,
                root: SelectionPath::from_path(&config.directory),
                generation: 1,
                decision,
                policy: "suffix".into(),
                qualifier: receipt.generation,
                phase: "planned".into(),
                members: Vec::new(),
            };
            layouts.push(MembershipLayout {
                native_seed: true,
                binding,
                previous,
                container: receipt.container,
                selected: receipt.selected,
            });
        }
        for layout in layouts.drain(..) {
            let prior = &layout.previous;
            let departed = delta
                .relations
                .get(&(layout.container.clone(), prior.asset_record_name.clone()))
                == Some(&true)
                || delta.containers.get(&layout.container) == Some(&true);
            if !departed
                || delta.deleted_photos.contains(&prior.asset_record_name)
                || delta.deleted_photos.contains(&prior.child)
            {
                continue;
            }
            if cancel.is_cancelled() {
                return Ok(false);
            }
            let source = passes
                .iter()
                .find(|pass| config.pass_key(pass) == prior.pass)
                .or_else(|| passes.first())
                .map(|pass| &pass.album)
                .or(recovery_source)
                .context("Membership departure has no current source")?;
            // Container disappearance is not photo liveness, including zero-pass recovery.
            let current = match source
                .confirm_layout_asset(&prior.asset_record_name, &prior.library)
                .await
            {
                Ok(current) => current.asset,
                Err(error) => {
                    if crate::icloud::photos::session::is_session_error(&error) {
                        return Err(error);
                    }
                    complete = false;
                    continue;
                }
            };
            if delta.deleted_photos.contains(current.id())
                || current
                    .metadata_arc(crate::state::VersionSizeKey::Original)
                    .is_deleted
            {
                continue;
            }
            if current.asset_record_name() != prior.asset_record_name
                || (layout.binding.child != current.asset_record_name()
                    && layout.binding.child != current.id())
                || layout
                    .selected
                    .iter()
                    .any(|selected| !crate::icloud::photos::same_selected_facts(selected, &current))
            {
                complete = false;
                continue;
            }
            let result = async {
                let mut operation = if let Some(operation) = pending
                    .iter()
                    .find(|operation| operation.family == layout.binding.family)
                {
                    anyhow::ensure!(
                        operation.is_membership_retirement(),
                        "Another layout transition owns the departed copy"
                    );
                    operation.clone()
                } else {
                    let operation_id = uuid::Uuid::new_v4().to_string();
                    let mut members = Vec::new();
                    for old in &layout.binding.files {
                        let path = old.path.to_path();
                        // Preservation is a disposition, even when recorded fingerprints match.
                        // Changed files and foreign sidecars detach in place at journal commit.
                        if old.local_preserved
                            || snapshot(&config.directory, &path).await?.as_ref()
                                != Some(&old.fingerprint)
                            || snapshot(&config.directory, &sidecar(&path)).await?.as_ref()
                                != old.sidecar.as_ref()
                        {
                            continue;
                        }
                        let preserved = preservation_path(old, &layout.binding.family);
                        let retired = preserved
                            .to_path()
                            .with_file_name(format!(".retired-{operation_id}-{}", members.len()));
                        members.push(LayoutMember {
                            record: None,
                            destination: None,
                            path_key: None,
                            stage: None,
                            old: Some(old.clone()),
                            missing: None,
                            archive_source: None,
                            preserved_path: Some(preserved),
                            retirement: Some(SelectionPath::from_path(&retired)),
                            prepared: None,
                            preserved: None,
                            installed: None,
                        });
                    }
                    let operation = LayoutOperation {
                        operation: operation_id,
                        generation: layout.binding.generation,
                        decision: format!("{MEMBERSHIP_RETIREMENT_PREFIX}{}", root.id),
                        phase: "planned".into(),
                        members,
                        ..prior.clone()
                    };
                    if layout.native_seed {
                        if operation.members.is_empty() {
                            return capture
                                .db
                                .detach_native_membership(operation, layout.binding.clone())
                                .await
                                .map_err(Into::into);
                        }
                        capture
                            .db
                            .begin_native_membership(operation.clone(), layout.binding.clone())
                            .await?;
                    } else {
                        capture.db.begin_primary_layout(operation.clone()).await?;
                    }
                    operation
                };
                if operation.phase == "planned" {
                    operation.phase = "prepared".into();
                    capture.db.save_primary_layout(operation.clone()).await?;
                }
                finish(
                    capture.db.as_ref(),
                    &mut operation,
                    cancel,
                    &mut LayoutOutcome::default(),
                )
                .await
            }
            .await;
            if result.is_err() {
                tracing::warn!(
                    "Membership layout remains unresolved; owned and user files are retained"
                );
                complete = false;
            }
        }
        if next.is_none() {
            return Ok(complete);
        }
        after = next;
    }
}
