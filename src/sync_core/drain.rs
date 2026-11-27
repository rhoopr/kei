//! Download owed copies from the work queue.
//!
//! Download URLs come from a record lookup made just before each batch, so
//! nothing depends on URLs the catalog saw hours ago. Downloads run in one
//! queue with a fixed number of slots, and the next batch is looked up while
//! earlier ones download, so no slot waits for a lookup or a slow file. A URL that still
//! expires gets one fresh lookup in the same cycle. Every outcome is written
//! back to the copy's row; nothing here touches a zone's position.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::Path;

use futures_util::StreamExt;
use futures_util::stream::FuturesUnordered;
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
    /// Failures of copies that had not failed before this cycle. A copy
    /// that fails the same way every cycle does not count again.
    pub(crate) newly_failed: u64,
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
    let ctx = Ctx {
        state,
        library,
        config,
        client,
        cancel,
    };
    let slots = config.concurrency.max(1);
    let mut source = Source::default();
    let mut retry = Vec::new();
    let mut expired_once = HashSet::new();
    // Downloads and adoptions ready to start, in work order.
    let mut ready: VecDeque<Work> = VecDeque::new();
    let mut running = FuturesUnordered::new();
    let mut transfers = 0;
    let mut preparing = false;
    let mut failure: Option<anyhow::Error> = None;
    loop {
        if !cancel.is_cancelled() && failure.is_none() {
            // Look up the next batch while earlier downloads run, so the
            // slots never wait for a lookup or for a batch's slowest file.
            // At most one batch waits ahead of the downloads, so its URLs
            // stay fresh.
            if !preparing && ready.len() < slots.max(ASSETS_PER_BATCH) {
                match next_batch(
                    state,
                    &zone,
                    config.transfer,
                    &mut source,
                    &mut retry,
                    &mut ready,
                )
                .await
                {
                    Ok(Some(batch)) => {
                        running.push(step(Work::Prepare(batch), &ctx));
                        preparing = true;
                    }
                    Ok(None) => {}
                    Err(error) => {
                        failure = Some(error);
                    }
                }
            }
            while transfers < slots
                && failure.is_none()
                && let Some(work) = ready.pop_front()
            {
                running.push(step(work, &ctx));
                transfers += 1;
            }
        }
        // Nothing running means nothing is left to start, or work stopped.
        let Some(done) = running.next().await else {
            break;
        };
        match done {
            Done::Prepared(result) => {
                preparing = false;
                match result {
                    Ok(prepared) => {
                        stats.waiting += prepared.waiting;
                        stats.modified += prepared.modified;
                        stats.downloaded += prepared.accepted;
                        ready.extend(prepared.jobs.into_iter().map(Work::Download));
                    }
                    Err(error) => {
                        failure.get_or_insert(error);
                    }
                }
            }
            Done::Downloaded(row, result) => {
                transfers -= 1;
                match result {
                    Ok(Ok(bytes)) => {
                        stats.downloaded += 1;
                        stats.bytes += bytes;
                        stats.tick();
                    }
                    Ok(Err(FetchError::Expired)) => {
                        if expired_once.insert(row.id) {
                            retry.push(row);
                        } else if let Err(error) = state
                            .mark_failed(row.id, "iCloud refused the download URL twice")
                            .await
                        {
                            failure.get_or_insert(error);
                        } else {
                            stats.fail(&row);
                        }
                    }
                    Ok(Err(FetchError::Interrupted)) => {}
                    Ok(Err(FetchError::Failed(_))) => {
                        stats.fail(&row);
                        stats.tick();
                    }
                    Err(error) => {
                        failure.get_or_insert(error);
                    }
                }
            }
            Done::Adopted(row, result) => {
                transfers -= 1;
                match result {
                    Ok(Adopted::Done) => {
                        stats.adopted += 1;
                        stats.tick();
                    }
                    Ok(Adopted::Failed) => stats.fail(&row),
                    Ok(Adopted::Missing) => {}
                    Err(error) => {
                        failure.get_or_insert(error);
                    }
                }
            }
        }
    }
    if let Some(error) = failure {
        return Err(error);
    }
    stats.finish();
    Ok(stats.stats)
}

/// What a drain step needs, shared by every step.
struct Ctx<'a> {
    state: &'a State,
    library: &'a PhotoLibrary,
    config: &'a CoreConfig,
    client: &'a Client,
    cancel: &'a CancellationToken,
}

enum Work {
    /// Look up current records for a batch and turn its rows into downloads.
    Prepare(Vec<FileRow>),
    Download(Job),
    Adopt(FileRow),
}

enum Done {
    Prepared(anyhow::Result<Prepared>),
    /// The outer error is a state write; the inner result is the transfer.
    Downloaded(FileRow, anyhow::Result<Result<u64, FetchError>>),
    Adopted(FileRow, anyhow::Result<Adopted>),
}

struct Job {
    row: FileRow,
    url: String,
    replace: Option<Recorded>,
    /// Modification time for the published file.
    modified: i64,
}

#[derive(Default)]
struct Prepared {
    jobs: Vec<Job>,
    waiting: u64,
    modified: u64,
    /// Replacements a crash interrupted after publishing; now recorded.
    accepted: u64,
}

enum Adopted {
    Done,
    Failed,
    /// The file went away; the copy is downloaded instead.
    Missing,
}

/// Owed rows, read a page at a time and grouped into lookup batches.
#[derive(Default)]
struct Source {
    after: i64,
    done: bool,
    batches: VecDeque<Vec<FileRow>>,
}

/// The next batch to look up. Adoptions found on the way go straight to
/// `ready`. Returns `None` when there is no batch to look up right now.
async fn next_batch(
    state: &State,
    zone: &str,
    transfer: super::Transfer,
    source: &mut Source,
    retry: &mut Vec<FileRow>,
    ready: &mut VecDeque<Work>,
) -> anyhow::Result<Option<Vec<FileRow>>> {
    // A URL that expired gets one more lookup in this cycle.
    if !retry.is_empty() {
        source.batches.extend(by_asset(std::mem::take(retry)));
    }
    loop {
        if let Some(batch) = source.batches.pop_front() {
            return Ok(Some(batch));
        }
        if source.done {
            return Ok(None);
        }
        let rows = state.work(zone, source.after, ROWS_PER_QUERY).await?;
        let Some(last) = rows.last() else {
            source.done = true;
            continue;
        };
        source.after = last.id;
        let mut downloads = Vec::new();
        for row in rows {
            if row.state == FileState::Adopt {
                ready.push_back(Work::Adopt(row));
            } else if transfer == super::Transfer::All {
                downloads.push(row);
            }
        }
        source.batches.extend(by_asset(downloads));
        if source.batches.is_empty() && !ready.is_empty() {
            // Let the adoptions run before reading further.
            return Ok(None);
        }
    }
}

async fn step(work: Work, ctx: &Ctx<'_>) -> Done {
    match work {
        Work::Prepare(rows) => Done::Prepared(prepare(ctx, rows).await),
        Work::Download(job) => {
            let row = job.row.clone();
            Done::Downloaded(row, download(ctx, job).await)
        }
        Work::Adopt(row) => {
            let result = adopt(ctx.state, ctx.config, &row).await;
            Done::Adopted(row, result)
        }
    }
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

    /// Count a copy that failed this cycle.
    fn fail(&mut self, row: &FileRow) {
        self.stats.failed += 1;
        if row.state != FileState::Failed {
            self.stats.newly_failed += 1;
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
async fn adopt(state: &State, config: &CoreConfig, row: &FileRow) -> anyhow::Result<Adopted> {
    Ok(
        match core_io::hash_file(&config.directory.join(&row.path)).await {
            Ok(Some((size, sha256))) if size == row.size => {
                state.mark_adopted(row.id, &sha256).await?;
                Adopted::Done
            }
            Ok(Some(_)) => {
                state
                    .mark_failed(row.id, "A different file now occupies this path")
                    .await?;
                Adopted::Failed
            }
            Ok(None) => {
                // The file went away; download it instead.
                state.mark_pending(row.id).await?;
                Adopted::Missing
            }
            Err(error) => {
                state.mark_failed(row.id, &format!("{error:#}")).await?;
                Adopted::Failed
            }
        },
    )
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

/// Look up current records for a batch and turn its rows into downloads.
async fn prepare(ctx: &Ctx<'_>, rows: Vec<FileRow>) -> anyhow::Result<Prepared> {
    let Ctx {
        state,
        library,
        config,
        ..
    } = *ctx;
    let zone = library.zone_name();
    let mut out = Prepared::default();
    let mut pending = Vec::new();
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

    for row in rows {
        let Some(files) = current.get(&row.asset) else {
            state
                .note_waiting(row.id, "iCloud did not return this photo")
                .await?;
            out.waiting += 1;
            continue;
        };
        let Some(file) = files.iter().find(|file| file.rendition == row.rendition) else {
            state
                .note_waiting(row.id, "iCloud no longer offers this version")
                .await?;
            out.waiting += 1;
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
                    out.accepted += 1;
                    continue;
                }
                Untouched::Changed => {
                    state.mark_modified(row.id).await?;
                    out.modified += 1;
                    continue;
                }
            },
        };
        let url = file.url.clone();
        pending.push((row, url, replace));
    }

    let modified = |row: &FileRow, replacing: bool| -> i64 {
        let fields = records.get(&row.asset).map(|record| &record.fields);
        // A primary-naming slot that changes hands gets the edit's time, or
        // the current time for a revert, so library scanners notice it.
        // Every other copy carries the capture time.
        if replacing && matches!(row.slot.as_str(), "still" | "motion") {
            let edited = row.rendition.contains("adjusted");
            return fields
                .filter(|_| edited)
                .and_then(crate::icloud::photos::enc::decode_adjustment_timestamp)
                .map_or_else(|| chrono::Utc::now().timestamp(), |time| time.timestamp());
        }
        fields.and_then(capture_seconds).unwrap_or(0)
    };
    out.jobs = pending
        .into_iter()
        .map(|(row, url, replace)| {
            let modified = modified(&row, replace.is_some());
            Job {
                row,
                url,
                replace,
                modified,
            }
        })
        .collect();
    Ok(out)
}

/// Download one copy and record the outcome on its row.
async fn download(ctx: &Ctx<'_>, job: Job) -> anyhow::Result<Result<u64, FetchError>> {
    let Ctx {
        state,
        config,
        client,
        cancel,
        ..
    } = *ctx;
    let Job {
        row,
        url,
        replace,
        modified,
    } = job;
    let path = config.directory.join(&row.path);
    let parent_ready = match path.parent() {
        Some(parent) => tokio::fs::create_dir_all(parent).await,
        None => Ok(()),
    };
    let result = match parent_ready {
        Err(error) => Err(FetchError::Failed(error.into())),
        Ok(()) => {
            core_io::fetch(
                Fetch {
                    client,
                    url: &url,
                    path: &path,
                    checksum: &row.checksum,
                    size: row.size,
                    temp_suffix: &config.temp_suffix,
                    retry: &config.retry,
                    replace,
                    modified,
                    cancel,
                    bandwidth: config.bandwidth.as_ref(),
                },
                async |sha256: &str| state.set_pending(row.id, sha256, None).await,
            )
            .await
        }
    };
    Ok(match result {
        Ok(fetched) => {
            state.mark_done(row.id, &fetched.sha256).await?;
            Ok(fetched.bytes)
        }
        Err(FetchError::Failed(error)) => {
            tracing::warn!(path = %row.path, error = %format!("{error:#}"), "Download failed; it will be retried");
            state.mark_failed(row.id, &format!("{error:#}")).await?;
            Err(FetchError::Failed(error))
        }
        Err(other) => Err(other),
    })
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
            slot: String::new(),
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
