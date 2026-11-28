//! The opt-in metadata pass: after downloads, bring saved copies' metadata
//! up to date with the catalog.
//!
//! Only copies kei saved and that still hold the bytes kei recorded are
//! written. An embedded rewrite changes the file, so its new SHA-256 is
//! recorded as kei's own bytes, with a receipt written first so a restart
//! can tell kei's write from a local edit. Nothing here reads or moves a
//! zone's sync position, and a failure leaves only that copy to retry.

use futures_util::StreamExt;
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

/// Run the pass over one zone's copies, up to `concurrency` at a time.
/// Each copy's work touches only its own file and row.
pub(crate) async fn run_zone(
    state: &State,
    zone: &str,
    directory: &std::path::Path,
    settings: &Settings,
    concurrency: usize,
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
        let mut visits = futures_util::stream::iter(rows)
            .take_while(|_| std::future::ready(!cancel.is_cancelled()))
            .map(|row| visit(state, directory, settings, row))
            .buffer_unordered(concurrency.max(1));
        while let Some(outcome) = visits.next().await {
            match outcome? {
                Visit::Written => stats.written += 1,
                Visit::Unchanged => stats.unchanged += 1,
                Visit::Modified => stats.modified += 1,
                Visit::Failed => stats.failed += 1,
                Visit::Skipped => {}
            }
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

/// What visiting one copy did.
enum Visit {
    Written,
    Unchanged,
    /// Changed locally; left as it is.
    Modified,
    Failed,
    /// Nothing to do yet, such as a missing file or master record.
    Skipped,
}

async fn visit(
    state: &State,
    directory: &std::path::Path,
    settings: &Settings,
    mut row: MetadataRow,
) -> anyhow::Result<Visit> {
    let path = directory.join(&row.path);
    // Settle an embedded rewrite a crash interrupted before reading bytes.
    core_io::recover_replacement(&path).await?;
    let Some((size, sha256)) = core_io::hash_file(&path).await? else {
        // A missing copy is for `kei reconcile`; check it again later.
        return Ok(Visit::Skipped);
    };
    if row.pending_sha.as_deref() == Some(sha256.as_str()) {
        // kei published these bytes and stopped before recording them.
        state.accept_pending(row.id).await?;
        row.sha256 = row.pending_sha.take();
        row.meta_fields = state_fields(state, row.id).await?;
    }
    if row.sha256.as_deref() != Some(sha256.as_str()) {
        state.mark_modified(row.id).await?;
        return Ok(Visit::Modified);
    }
    let Some(master) = &row.master_body else {
        return Ok(Visit::Skipped);
    };
    let asset = match stored_asset(master, &row.asset_body) {
        Ok(asset) => asset,
        Err(error) => {
            tracing::warn!(error = %format!("{error:#}"), "Could not read a stored photo record");
            return Ok(Visit::Failed);
        }
    };
    let owned: EmbeddedMetadataFields = match row.meta_fields.as_deref() {
        Some(json) => serde_json::from_str(json).unwrap_or_default(),
        None => EmbeddedMetadataFields::default(),
    };
    let payload = writers::payload(settings, &asset, &row.rendition, &row.albums, owned)?;
    if row.meta_hash.as_deref() == Some(payload.hash.as_str()) {
        state.metadata_written(row.id, &payload.hash, None).await?;
        return Ok(Visit::Unchanged);
    }
    let refused = refusal_marker(&payload.hash);
    if row.meta_hash.as_deref() == Some(refused.as_str()) {
        // The writer refused these bytes with these values; the same answer
        // would come back. Still due and counted, but not redone until the
        // file, the values, the settings or kei change.
        tracing::debug!(path = %row.path, "Embedded metadata write was refused for this file; not retrying until it or its metadata changes");
        return Ok(Visit::Failed);
    }

    let mut written = None;
    let mut embed_refused = false;
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
                        return Ok(Visit::Modified);
                    }
                    Outcome::Unchanged | Outcome::Failed | Outcome::Refused => {
                        return Ok(Visit::Failed);
                    }
                }
            }
            Outcome::Unchanged => {}
            Outcome::Changed => {
                state.mark_modified(row.id).await?;
                return Ok(Visit::Modified);
            }
            Outcome::Failed => {
                return Ok(Visit::Failed);
            }
            // The sidecar does not depend on the media rewrite; still write it.
            Outcome::Refused => embed_refused = true,
        }
    }
    let mut sidecar = false;
    if settings_writes_sidecar(settings) {
        match writers::write_sidecar(settings, &path, row.source_sha.as_deref(), &payload).await {
            Outcome::Written(()) => sidecar = true,
            Outcome::Unchanged => {}
            Outcome::Changed | Outcome::Failed | Outcome::Refused => {
                // Keep any embedded write that already landed.
                if let Some((installed, fields)) = written {
                    state.set_pending(row.id, &installed, Some(fields)).await?;
                    state.accept_pending(row.id).await?;
                }
                return Ok(Visit::Failed);
            }
        }
    }
    if embed_refused {
        state.metadata_refused(row.id, &refused).await?;
        return Ok(Visit::Failed);
    }
    let embedded = written.is_some();
    state
        .metadata_written(row.id, &payload.hash, written)
        .await?;
    Ok(if embedded || sidecar {
        Visit::Written
    } else {
        Visit::Unchanged
    })
}

/// What `meta_hash` holds after the writer refused `payload_hash` for a
/// file's layout. Naming the kei version retries the write after an upgrade.
fn refusal_marker(payload_hash: &str) -> String {
    format!("refused:{}:{payload_hash}", env!("CARGO_PKG_VERSION"))
}

fn settings_writes_sidecar(settings: &Settings) -> bool {
    settings.flags.contains(writers::MetadataFlags::XMP_SIDECAR)
}

async fn state_fields(state: &State, id: i64) -> anyhow::Result<Option<String>> {
    state.meta_fields(id).await
}
