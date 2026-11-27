//! Download owed copies from the work queue.
//!
//! Download URLs come from a record lookup made just before each batch, so
//! nothing depends on URLs the catalog saw hours ago. A URL that still
//! expires gets one fresh lookup in the same cycle. Every outcome is written
//! back to the copy's row; nothing here touches a zone's position.

use std::collections::HashMap;
use std::path::Path;

use futures_util::StreamExt;
use reqwest::Client;
use tokio_util::sync::CancellationToken;

use crate::download::core_io::{self, ExpectedFile, Fetch, FetchError, Recorded};
use crate::icloud::photos::PhotoLibrary;
use crate::icloud::photos::feed::{LOOKUP_BATCH, current_asset};

use super::CoreConfig;
use super::state::{FileRow, FileState, State};

/// Distinct assets per lookup: each needs its asset and master record.
const ASSETS_PER_BATCH: usize = LOOKUP_BATCH / 2;
const ROWS_PER_QUERY: usize = 200;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DrainStats {
    pub(crate) downloaded: u64,
    /// Bytes transferred by this cycle's downloads.
    pub(crate) bytes: u64,
    pub(crate) adopted: u64,
    pub(crate) failed: u64,
    pub(crate) modified: u64,
    /// Copies whose photo or version iCloud did not return; still owed.
    pub(crate) waiting: u64,
}

pub(crate) async fn drain_zone(
    state: &State,
    library: &PhotoLibrary,
    config: &CoreConfig,
    client: &Client,
    cancel: &CancellationToken,
) -> anyhow::Result<DrainStats> {
    let zone = library.zone_name().to_owned();
    let mut stats = Tally::start(&zone, state.owed(&zone).await?);
    let mut after = 0;
    loop {
        if cancel.is_cancelled() {
            break;
        }
        let rows = state.work(&zone, after, ROWS_PER_QUERY).await?;
        let Some(last) = rows.last() else {
            break;
        };
        after = last.id;
        let mut downloads = Vec::new();
        for row in rows {
            if row.state == FileState::Adopt {
                adopt(state, config, &row, &mut stats).await?;
            } else if config.transfer == super::Transfer::All {
                downloads.push(row);
            }
        }
        for batch in by_asset(downloads) {
            if cancel.is_cancelled() {
                break;
            }
            let expired =
                download_batch(state, library, config, client, cancel, batch, &mut stats).await?;
            if !expired.is_empty() && !cancel.is_cancelled() {
                let still_expired =
                    download_batch(state, library, config, client, cancel, expired, &mut stats)
                        .await?;
                for row in still_expired {
                    state
                        .mark_failed(row.id, "iCloud refused the download URL twice")
                        .await?;
                    stats.failed += 1;
                }
            }
        }
    }
    stats.finish();
    Ok(stats.stats)
}

/// Drain counters plus periodic progress lines for one zone.
struct Tally {
    stats: DrainStats,
    zone: String,
    /// Copies owed when the step started.
    owed: u64,
    progress: super::progress::Progress,
}

impl Tally {
    fn start(zone: &str, owed: u64) -> Self {
        if owed > 0 {
            tracing::info!(zone, owed, "Saving owed copies");
        }
        Self {
            stats: DrainStats::default(),
            zone: zone.to_owned(),
            owed,
            progress: super::progress::Progress::start(),
        }
    }

    /// Report progress when a line is due.
    fn tick(&mut self) {
        if self.progress.due() {
            self.report("Saving copies");
        }
    }

    fn finish(&self) {
        if self.owed > 0 {
            self.report("Saved copies");
        }
    }

    fn report(&self, message: &'static str) {
        let s = self.stats;
        let settled = s.downloaded + s.adopted + s.failed + s.modified;
        tracing::info!(
            zone = %self.zone,
            downloaded = s.downloaded,
            adopted = s.adopted,
            failed = s.failed,
            locally_modified = s.modified,
            bytes = s.bytes,
            remaining = self.owed.saturating_sub(settled),
            "{message}"
        );
    }
}

impl std::ops::Deref for Tally {
    type Target = DrainStats;
    fn deref(&self) -> &DrainStats {
        &self.stats
    }
}

impl std::ops::DerefMut for Tally {
    fn deref_mut(&mut self) -> &mut DrainStats {
        &mut self.stats
    }
}

/// Hash a file already at a new copy's path and take ownership of it.
async fn adopt(
    state: &State,
    config: &CoreConfig,
    row: &FileRow,
    stats: &mut Tally,
) -> anyhow::Result<()> {
    match core_io::hash_file(&config.directory.join(&row.path)).await {
        Ok(Some((size, sha256))) if size == row.size => {
            state.mark_done(row.id, &sha256).await?;
            stats.adopted += 1;
            stats.tick();
        }
        Ok(Some(_)) => {
            state
                .mark_failed(row.id, "A different file now occupies this path")
                .await?;
            stats.failed += 1;
        }
        Ok(None) => {
            // The file went away; download it instead.
            state.mark_pending(row.id).await?;
        }
        Err(error) => {
            state.mark_failed(row.id, &format!("{error:#}")).await?;
            stats.failed += 1;
        }
    }
    Ok(())
}

/// Group rows so each batch covers at most [`ASSETS_PER_BATCH`] assets.
fn by_asset(rows: Vec<FileRow>) -> Vec<Vec<FileRow>> {
    let mut batches: Vec<Vec<FileRow>> = Vec::new();
    let mut assets = 0;
    let mut previous: Option<String> = None;
    for row in rows {
        if previous.as_deref() != Some(row.asset.as_str()) {
            if assets == ASSETS_PER_BATCH || batches.is_empty() {
                batches.push(Vec::new());
                assets = 0;
            }
            assets += 1;
            previous = Some(row.asset.clone());
        }
        if let Some(batch) = batches.last_mut() {
            batch.push(row);
        }
    }
    batches
}

/// Look up current records for a batch and download its copies. Returns the
/// rows whose URL expired.
async fn download_batch(
    state: &State,
    library: &PhotoLibrary,
    config: &CoreConfig,
    client: &Client,
    cancel: &CancellationToken,
    rows: Vec<FileRow>,
    stats: &mut Tally,
) -> anyhow::Result<Vec<FileRow>> {
    let zone = library.zone_name();
    let mut assets: Vec<String> = rows.iter().map(|row| row.asset.clone()).collect();
    assets.dedup();
    let masters = state.masters_of(zone, assets.clone()).await?;
    let mut names = assets.clone();
    names.extend(masters.values().cloned());
    names.sort();
    names.dedup();
    let records: HashMap<String, _> = library
        .lookup_records(&names)
        .await?
        .into_iter()
        .map(|record| (record.record_name.clone(), record))
        .collect();

    let mut current: HashMap<String, Vec<ExpectedFile>> = HashMap::new();
    for asset in &assets {
        let (Some(asset_record), Some(master_record)) = (
            records.get(asset),
            masters.get(asset).and_then(|master| records.get(master)),
        ) else {
            continue;
        };
        let photo = current_asset(master_record.clone(), asset_record);
        current.insert(
            asset.clone(),
            core_io::expected_files(&photo, &config.paths),
        );
    }

    let mut jobs = Vec::new();
    for row in rows {
        let Some(files) = current.get(&row.asset) else {
            state
                .note_waiting(row.id, "iCloud did not return this photo")
                .await?;
            stats.waiting += 1;
            continue;
        };
        let Some(file) = files.iter().find(|file| file.rendition == row.rendition) else {
            state
                .note_waiting(row.id, "iCloud no longer offers this version")
                .await?;
            stats.waiting += 1;
            continue;
        };
        let mut row = row;
        if file.checksum != row.checksum || file.size != row.size {
            state
                .refresh_version(row.id, file.size, &file.checksum)
                .await?;
            row.checksum.clone_from(&file.checksum);
            row.size = file.size;
        }
        let path = config.directory.join(&row.path);
        if row.sha256.is_some() {
            // Settle a replacement a crash interrupted before comparing bytes.
            core_io::recover_replacement(&path).await?;
        }
        let replace = match &row.sha256 {
            None => None,
            Some(recorded) => match untouched(&path, recorded, row.pending_sha.as_deref()).await? {
                Untouched::Missing => None,
                Untouched::Same(recorded) => Some(recorded),
                Untouched::AlreadyReplaced => {
                    // The replacement finished before kei last stopped.
                    state.accept_pending(row.id).await?;
                    stats.downloaded += 1;
                    continue;
                }
                Untouched::Changed => {
                    state.mark_modified(row.id).await?;
                    stats.modified += 1;
                    continue;
                }
            },
        };
        jobs.push((row, file.url.clone(), replace));
    }

    let modified = |row: &FileRow| -> i64 {
        records
            .get(&row.asset)
            .and_then(|record| capture_seconds(&record.fields))
            .unwrap_or(0)
    };
    let jobs = jobs
        .into_iter()
        .map(|(row, url, replace)| {
            let seconds = modified(&row);
            async move {
                let path = config.directory.join(&row.path);
                if let Some(parent) = path.parent()
                    && let Err(error) = tokio::fs::create_dir_all(parent).await
                {
                    return (row, Err(FetchError::Failed(error.into())));
                }
                let result = core_io::fetch(
                    Fetch {
                        client,
                        url: &url,
                        path: &path,
                        checksum: &row.checksum,
                        size: row.size,
                        temp_suffix: &config.temp_suffix,
                        retry: &config.retry,
                        replace,
                        modified: seconds,
                        cancel,
                        bandwidth: config.bandwidth.as_ref(),
                    },
                    async |sha256: &str| state.set_pending(row.id, sha256, None).await,
                )
                .await;
                (row, result)
            }
        })
        .collect::<Vec<_>>();
    let mut results = futures_util::stream::iter(jobs).buffer_unordered(config.concurrency.max(1));
    let mut expired = Vec::new();
    while let Some((row, result)) = results.next().await {
        match result {
            Ok(fetched) => {
                state.mark_done(row.id, &fetched.sha256).await?;
                stats.downloaded += 1;
                stats.bytes += fetched.bytes;
                stats.tick();
            }
            Err(FetchError::Expired) => expired.push(row),
            Err(FetchError::Interrupted) => {}
            Err(FetchError::Failed(error)) => {
                tracing::warn!(path = %row.path, error = %format!("{error:#}"), "Download failed; it will be retried");
                state.mark_failed(row.id, &format!("{error:#}")).await?;
                stats.failed += 1;
                stats.tick();
            }
        }
    }
    Ok(expired)
}

enum Untouched {
    Missing,
    Same(Recorded),
    /// The file holds the bytes kei recorded just before replacing it.
    AlreadyReplaced,
    Changed,
}

/// Compare a local file with the hash kei recorded when it wrote it.
// CONTRACT: REPLACE_REQUIRES_RECORDED_BYTES
// A copy is replaced only while its bytes still hash to what kei recorded;
// anything else is kept and reported as locally modified.
async fn untouched(
    path: &Path,
    recorded: &str,
    pending: Option<&str>,
) -> anyhow::Result<Untouched> {
    let Some(expected) = core_io::parse_sha256(recorded) else {
        return Ok(Untouched::Changed);
    };
    Ok(match core_io::hash_file(path).await? {
        None => Untouched::Missing,
        Some((size, sha256)) if sha256 == recorded => Untouched::Same(Recorded {
            size,
            sha256: expected,
        }),
        Some((_, sha256)) if pending == Some(sha256.as_str()) => Untouched::AlreadyReplaced,
        Some(_) => Untouched::Changed,
    })
}

/// Capture time of a `CPLAsset` in whole seconds.
fn capture_seconds(fields: &serde_json::Value) -> Option<i64> {
    let millis = fields.get("assetDate")?.get("value")?.as_f64()?;
    #[expect(
        clippy::cast_possible_truncation,
        reason = "capture dates are far inside i64 seconds"
    )]
    Some((millis / 1000.0).floor() as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: i64, asset: &str) -> FileRow {
        FileRow {
            id,
            zone: "Z".into(),
            asset: asset.into(),
            rendition: "original".into(),
            album: String::new(),
            path: format!("{id}.jpg"),
            size: 1,
            checksum: "c".into(),
            sha256: None,
            state: FileState::Pending,
            attempts: 0,
            pending_sha: None,
        }
    }

    #[test]
    fn batches_keep_an_assets_copies_together() {
        let mut rows = Vec::new();
        for index in 0..(ASSETS_PER_BATCH + 1) {
            rows.push(row(index as i64 * 2, &format!("A{index:03}")));
            rows.push(row(index as i64 * 2 + 1, &format!("A{index:03}")));
        }
        let batches = by_asset(rows);
        assert_eq!(batches.len(), 2);
        assert_eq!(batches[0].len(), ASSETS_PER_BATCH * 2);
        assert_eq!(batches[1].len(), 2);
    }
}
