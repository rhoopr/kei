//! The opt-in metadata pass: after downloads, bring saved copies' metadata
//! up to date with the catalog.
//!
//! Only copies kei saved and that still hold the bytes kei recorded are
//! written. An embedded rewrite changes the file, so its new SHA-256 is
//! recorded as kei's own bytes, with a receipt written first so a restart
//! can tell kei's write from a local edit. Nothing here reads or moves a
//! zone's sync position, and a failure leaves only that copy to retry.

use tokio_util::sync::CancellationToken;

use crate::download::core_io;
use crate::download::metadata_pass::{self as writers, Outcome, Settings};
use crate::icloud::photos::feed::stored_asset;
use crate::state::types::EmbeddedMetadataFields;

use super::state::{MetadataRow, State};

const ROWS_PER_QUERY: usize = 200;
/// Kv key holding the settings fingerprint the stored hashes were made with.
const METADATA_CONFIG_KEY: &str = "metadata_config";

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct MetadataStats {
    /// Copies whose media or sidecar kei wrote.
    pub(crate) written: u64,
    /// Copies already up to date.
    pub(crate) unchanged: u64,
    /// Copies changed locally; left as they are.
    pub(crate) modified: u64,
    /// Copies to retry next cycle.
    pub(crate) failed: u64,
}

/// Visit copies whose metadata may be stale when the settings changed since
/// the last pass; `refresh` also forgets what was written, so every copy is
/// planned again.
pub(crate) async fn prepare(
    state: &State,
    settings: &Settings,
    refresh: bool,
) -> anyhow::Result<()> {
    let fingerprint = settings.fingerprint();
    if refresh || state.get_kv(METADATA_CONFIG_KEY).await?.as_deref() != Some(&fingerprint) {
        state.mark_all_metadata_dirty(refresh).await?;
        state.set_kv(METADATA_CONFIG_KEY, &fingerprint).await?;
    }
    Ok(())
}

/// Run the pass over one zone's copies.
pub(crate) async fn run_zone(
    state: &State,
    zone: &str,
    directory: &std::path::Path,
    settings: &Settings,
    cancel: &CancellationToken,
) -> anyhow::Result<MetadataStats> {
    let mut stats = MetadataStats::default();
    let mut after = 0;
    let mut progress = super::progress::Progress::start();
    loop {
        if cancel.is_cancelled() {
            break;
        }
        let rows = state.metadata_work(zone, after, ROWS_PER_QUERY).await?;
        let Some(last) = rows.last() else {
            break;
        };
        after = last.id;
        for row in rows {
            if cancel.is_cancelled() {
                break;
            }
            visit(state, directory, settings, row, &mut stats).await?;
            if progress.due() {
                tracing::info!(
                    zone,
                    written = stats.written,
                    unchanged = stats.unchanged,
                    locally_modified = stats.modified,
                    failed = stats.failed,
                    "Updating metadata"
                );
            }
        }
    }
    Ok(stats)
}

async fn visit(
    state: &State,
    directory: &std::path::Path,
    settings: &Settings,
    mut row: MetadataRow,
    stats: &mut MetadataStats,
) -> anyhow::Result<()> {
    let path = directory.join(&row.path);
    // Settle an embedded rewrite a crash interrupted before reading bytes.
    core_io::recover_replacement(&path).await?;
    let Some((size, sha256)) = core_io::hash_file(&path).await? else {
        // A missing copy is for `kei reconcile`; check it again later.
        return Ok(());
    };
    if row.pending_sha.as_deref() == Some(sha256.as_str()) {
        // kei published these bytes and stopped before recording them.
        state.accept_pending(row.id).await?;
        row.sha256 = row.pending_sha.take();
        row.meta_fields = state_fields(state, row.id).await?;
    }
    if row.sha256.as_deref() != Some(sha256.as_str()) {
        state.mark_modified(row.id).await?;
        stats.modified += 1;
        return Ok(());
    }
    let Some(master) = &row.master_body else {
        return Ok(());
    };
    let asset = match stored_asset(master, &row.asset_body) {
        Ok(asset) => asset,
        Err(error) => {
            tracing::warn!(error = %format!("{error:#}"), "Could not read a stored photo record");
            stats.failed += 1;
            return Ok(());
        }
    };
    let owned: EmbeddedMetadataFields = match row.meta_fields.as_deref() {
        Some(json) => serde_json::from_str(json).unwrap_or_default(),
        None => EmbeddedMetadataFields::default(),
    };
    let payload = writers::payload(settings, &asset, &row.rendition, &row.albums, owned)?;
    if row.meta_hash.as_deref() == Some(payload.hash.as_str()) {
        state.metadata_written(row.id, &payload.hash, None).await?;
        stats.unchanged += 1;
        return Ok(());
    }

    let mut written = None;
    if writers::embeds_into(settings, &path) {
        let expected = (
            size,
            core_io::parse_sha256(&sha256)
                .ok_or_else(|| anyhow::anyhow!("Recorded SHA-256 is malformed"))?,
        );
        match writers::prepare_embed(
            settings,
            &path,
            expected,
            row.source_sha.as_deref(),
            &payload,
        )
        .await
        {
            Outcome::Written(prepared) => {
                let output = prepared.output_sha256();
                let fields = prepared.fields_json()?;
                state
                    .set_pending(row.id, &output, Some(fields.clone()))
                    .await?;
                match writers::publish_embed(&path, prepared).await {
                    Outcome::Written(installed) => written = Some((installed, fields)),
                    Outcome::Changed => {
                        state.mark_modified(row.id).await?;
                        stats.modified += 1;
                        return Ok(());
                    }
                    Outcome::Unchanged | Outcome::Failed => {
                        stats.failed += 1;
                        return Ok(());
                    }
                }
            }
            Outcome::Unchanged => {}
            Outcome::Changed => {
                state.mark_modified(row.id).await?;
                stats.modified += 1;
                return Ok(());
            }
            Outcome::Failed => {
                stats.failed += 1;
                return Ok(());
            }
        }
    }
    let mut sidecar = false;
    if settings_writes_sidecar(settings) {
        match writers::write_sidecar(settings, &path, row.source_sha.as_deref(), &payload).await {
            Outcome::Written(()) => sidecar = true,
            Outcome::Unchanged => {}
            Outcome::Changed | Outcome::Failed => {
                // Keep any embedded write that already landed.
                if let Some((installed, fields)) = written {
                    state.set_pending(row.id, &installed, Some(fields)).await?;
                    state.accept_pending(row.id).await?;
                }
                stats.failed += 1;
                return Ok(());
            }
        }
    }
    let embedded = written.is_some();
    state
        .metadata_written(row.id, &payload.hash, written)
        .await?;
    if embedded || sidecar {
        stats.written += 1;
    } else {
        stats.unchanged += 1;
    }
    Ok(())
}

fn settings_writes_sidecar(settings: &Settings) -> bool {
    settings.flags.contains(writers::MetadataFlags::XMP_SIDECAR)
}

async fn state_fields(state: &State, id: i64) -> anyhow::Result<Option<String>> {
    state.meta_fields(id).await
}
