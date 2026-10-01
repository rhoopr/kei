//! Removal of stale temporary files with durable ownership evidence.

use std::path::{Path, PathBuf};

use super::config::DownloadConfig;

/// Download photos with syncToken support.
///
/// In `SyncMode::Full`: runs the existing full enumeration via
/// `photo_stream_with_token`, captures the syncToken after the stream is
/// consumed, and delegates download logic to the existing pipeline.
///
/// In `SyncMode::Incremental`: uses `changes_stream` for delta sync,
/// filters `ChangeEvent`s to downloadable assets, and feeds them through
/// the existing download pipeline. Falls back to `SyncMode::Full` if the
/// token is invalid or expired.
/// Minimum untouched age before cleanup removes a durably owned temporary
/// file. A second kei process can share the download root while using another
/// state database, so recent writes remain protected even when this database
/// has older completed-sync evidence.
const PART_FILE_RECENT_GRACE_SECS: i64 = 10 * 60;

#[derive(Debug, Default)]
struct OwnedTempCleanup {
    removed: usize,
    retire: Vec<PathBuf>,
}

/// Remove only stale exact paths from kei's durable temporary-file ownership
/// ledger. No suffix match can authorize deletion, and the removal stays
/// relative to a verified directory or file handle.
fn remove_owned_orphan_parts(
    root: &Path,
    owned: &[crate::state::OwnedTempFile],
    cutoff_secs: i64,
    now_secs: i64,
    recent_grace_secs: i64,
) -> OwnedTempCleanup {
    remove_owned_orphan_parts_with(
        root,
        owned,
        cutoff_secs,
        now_secs,
        recent_grace_secs,
        |_| {},
    )
}

fn remove_owned_orphan_parts_with<F>(
    root: &Path,
    owned: &[crate::state::OwnedTempFile],
    cutoff_secs: i64,
    now_secs: i64,
    recent_grace_secs: i64,
    mut before_remove: F,
) -> OwnedTempCleanup
where
    F: FnMut(&Path),
{
    use crate::fs_util::ConfinedFileOpen;

    let mut cleanup = OwnedTempCleanup::default();
    for record in owned {
        debug_assert!(record.claimed_at < cutoff_secs);
        let candidate = match crate::fs_util::open_confined_regular_file(root, &record.path) {
            Ok(candidate) => candidate,
            Err(error) => {
                tracing::warn!(
                    path = %record.path.display(),
                    error = %error,
                    "Could not safely inspect owned temporary file during cleanup"
                );
                continue;
            }
        };
        match candidate {
            ConfinedFileOpen::OutsideRoot => continue,
            ConfinedFileOpen::Retire => {
                cleanup.retire.push(record.path.clone());
            }
            ConfinedFileOpen::Regular(file) => {
                let mtime_secs = file.modified_secs();
                // A path modified after the completed-sync cutoff no longer
                // matches the stale claim. Retire the claim without touching
                // the file so it can never authorize a later deletion.
                if mtime_secs >= cutoff_secs {
                    cleanup.retire.push(record.path.clone());
                    continue;
                }
                let is_recently_touched = recent_grace_secs > 0
                    && mtime_secs > now_secs.saturating_sub(recent_grace_secs);
                if is_recently_touched {
                    continue;
                }
                before_remove(&record.path);
                match file.remove() {
                    Ok(()) => {
                        cleanup.removed += 1;
                        cleanup.retire.push(record.path.clone());
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        cleanup.retire.push(record.path.clone());
                    }
                    Err(error) => tracing::warn!(
                        path = %record.path.display(),
                        error = %error,
                        "Failed to remove owned orphan temporary file"
                    ),
                }
            }
        }
    }
    cleanup
}

/// Remove orphaned `.part` files from the download directory.
///
/// Loads stale exact-path ownership evidence from SQLite, rejects symlinked
/// paths, and removes only owned files older than the last completed sync.
/// CONTRACT: TEMP_FILE_DELETE_REQUIRES_DURABLE_OWNERSHIP
pub(super) async fn cleanup_orphan_part_files(config: &DownloadConfig) {
    let Some(db) = &config.state_db else { return };
    let cutoff = match db.get_summary().await {
        Ok(summary) => match summary.last_sync_completed {
            Some(ts) => ts,
            None => return, // No prior sync — nothing is orphaned
        },
        Err(e) => {
            tracing::debug!(error = %e, "Could not query last sync time for .part cleanup");
            return;
        }
    };

    let cutoff_secs = cutoff.timestamp();
    let owned = match db.get_owned_temp_files_before(cutoff_secs).await {
        Ok(owned) if owned.is_empty() => return,
        Ok(owned) => owned,
        Err(error) => {
            tracing::warn!(error = %error, "Could not load temporary-file ownership for cleanup");
            return;
        }
    };
    let dir = match crate::fs_util::absolute_lexical(&config.directory) {
        Ok(dir) => dir,
        Err(error) => {
            tracing::warn!(
                path = %config.directory.display(),
                error = %error,
                "Could not resolve download root for owned temporary-file cleanup"
            );
            return;
        }
    };
    let now_secs = chrono::Utc::now().timestamp();

    let cleanup = tokio::task::spawn_blocking(move || {
        remove_owned_orphan_parts(
            &dir,
            &owned,
            cutoff_secs,
            now_secs,
            PART_FILE_RECENT_GRACE_SECS,
        )
    })
    .await
    .unwrap_or_default();

    if let Err(error) = db.retire_temp_files(&cleanup.retire).await {
        tracing::warn!(error = %error, "Could not retire temporary-file ownership after cleanup");
    }

    if cleanup.removed > 0 {
        tracing::info!(
            count = cleanup.removed,
            "Cleaned up owned orphan temporary files"
        );
    }
}

#[cfg(test)]
mod tests;
