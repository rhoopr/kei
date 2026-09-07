#![allow(
    clippy::print_stdout,
    reason = "CLI subcommand whose primary purpose is to print import-existing progress to stdout"
)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Context;

use crate::auth;
use crate::cli;
use crate::config;
use crate::download;
use crate::download::filter::{
    ExpectedAssetPath, PathDerivationConfig, PathDerivationSource, expected_paths_for,
    is_asset_filtered,
};
use crate::download::paths::{DirCache, normalize_ampm};
use crate::icloud::photos::PhotoAsset;
use crate::retry;
use crate::state;
use crate::state::{ImportStateStore, VersionSizeKey};
use crate::systemd::SystemdNotifier;
use crate::types::{FileMatchPolicy, LivePhotoMovFilenamePolicy};

use super::service::{
    build_collection_context, collection_libraries, init_photos_service, pass_scope_for_zone,
    resolve_cross_zone_libraries_for_album_hydration, resolve_libraries, resolve_passes_for_scope,
    zone_name_set,
};

/// Value of the `stage` field on the one-shot tracing event emitted by
/// [`import_assets`] when the first asset is dequeued. Operators (and the
/// live SIGINT idempotency test) sync on this token to know real scan
/// work has started, distinct from process-spawn or auth completion.
pub(crate) const SCAN_STARTED_STAGE: &str = "scan_started";

/// Value of the `stage` field on the periodic heartbeat tracing event
/// emitted by the import scan task. Operators tailing logs use this to
/// distinguish heartbeat lines (running counters, last-seen asset id)
/// from per-asset debug/warn lines.
const HEARTBEAT_STAGE: &str = "heartbeat";

/// Per-library counters returned by [`import_assets`].
///
/// Counters span asset- and expected-path levels so that divergent
/// totals in the summary surface silently dropped work instead of
/// masquerading as a clean run. `total` and `filtered` tick per asset;
/// `matched`, `unmatched`, `hash_errors`, and `skipped_already_imported`
/// tick per expected path (one asset can yield multiple paths, e.g.
/// live photos). `skipped_already_imported` is a subset of `matched`:
/// the file matched, but the DB had a prior adopt with the same size +
/// mtime so the SHA-256 re-read was skipped.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ImportStats {
    pub total: u64,
    pub matched: u64,
    pub unmatched: u64,
    pub filtered: u64,
    pub strict_refused: u64,
    pub hash_errors: u64,
    pub skipped_already_imported: u64,
}

impl std::ops::AddAssign for ImportStats {
    fn add_assign(&mut self, rhs: Self) {
        self.total += rhs.total;
        self.matched += rhs.matched;
        self.unmatched += rhs.unmatched;
        self.filtered += rhs.filtered;
        self.strict_refused += rhs.strict_refused;
        self.hash_errors += rhs.hash_errors;
        self.skipped_already_imported += rhs.skipped_already_imported;
    }
}

fn record_strict_refusal(stats: &mut ImportStats, heartbeat_state: &HeartbeatState) {
    stats.strict_refused += 1;
    heartbeat_state
        .strict_refused
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

/// Default cadence for the `import-existing` heartbeat log line.
const HEARTBEAT_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);
const STRICT_IMPORT_PREFIX_BYTES: u64 = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StrictImportDecision {
    Accepted,
    Refused,
}

#[async_trait::async_trait]
pub(crate) trait StrictImportVerifier: Send + Sync {
    async fn verify(
        &self,
        local_path: &Path,
        cloud_url: &str,
        expected_size: u64,
    ) -> anyhow::Result<StrictImportDecision>;
}

#[derive(Clone, Copy, Default)]
pub(crate) struct ImportRunOptions<'a> {
    pub(crate) dry_run: bool,
    pub(crate) show_progress: bool,
    pub(crate) strict_verifier: Option<&'a dyn StrictImportVerifier>,
    pub(crate) shutdown_token: Option<&'a tokio_util::sync::CancellationToken>,
}

impl std::fmt::Debug for ImportRunOptions<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ImportRunOptions")
            .field("dry_run", &self.dry_run)
            .field("show_progress", &self.show_progress)
            .field("strict", &self.strict_verifier.is_some())
            .field("shutdown", &self.shutdown_token.is_some())
            .finish()
    }
}

#[derive(Debug, Clone)]
struct HttpStrictImportVerifier {
    client: reqwest::Client,
}

impl HttpStrictImportVerifier {
    fn new() -> Self {
        Self {
            client: reqwest::Client::new(),
        }
    }
}

#[async_trait::async_trait]
impl StrictImportVerifier for HttpStrictImportVerifier {
    async fn verify(
        &self,
        local_path: &Path,
        cloud_url: &str,
        expected_size: u64,
    ) -> anyhow::Result<StrictImportDecision> {
        verify_strict_prefix(&self.client, local_path, cloud_url, expected_size).await
    }
}

async fn verify_strict_prefix(
    client: &reqwest::Client,
    local_path: &Path,
    cloud_url: &str,
    expected_size: u64,
) -> anyhow::Result<StrictImportDecision> {
    use futures_util::StreamExt;
    use tokio::io::AsyncReadExt;

    let prefix_len_u64 = expected_size.min(STRICT_IMPORT_PREFIX_BYTES);
    let prefix_len = usize::try_from(prefix_len_u64)
        .context("Strict import prefix length is too large for this system")?;
    if prefix_len == 0 {
        return Ok(StrictImportDecision::Accepted);
    }

    let mut local_prefix = vec![0_u8; prefix_len];
    let mut local = tokio::fs::File::open(local_path)
        .await
        .with_context(|| format!("Could not open {} for strict import", local_path.display()))?;
    local.read_exact(&mut local_prefix).await.with_context(|| {
        format!(
            "Could not read strict import prefix from {}",
            local_path.display()
        )
    })?;

    let range_end = prefix_len_u64.saturating_sub(1);
    let response = client
        .get(cloud_url)
        .header(reqwest::header::RANGE, format!("bytes=0-{range_end}"))
        .send()
        .await
        .with_context(|| format!("Could not fetch strict import prefix from {cloud_url}"))?;
    let status = response.status();
    if !(status == reqwest::StatusCode::PARTIAL_CONTENT || status == reqwest::StatusCode::OK) {
        anyhow::bail!("Strict import prefix check returned HTTP {status}.");
    }

    let mut cloud_prefix = Vec::with_capacity(prefix_len);
    let mut stream = response.bytes_stream();
    while cloud_prefix.len() < prefix_len {
        let Some(chunk) = stream.next().await else {
            break;
        };
        let chunk = chunk.context("Could not read strict import prefix response")?;
        let remaining = prefix_len - cloud_prefix.len();
        let take = chunk.len().min(remaining);
        let Some(prefix) = chunk.get(..take) else {
            anyhow::bail!("Strict import prefix response ended sooner than expected.");
        };
        cloud_prefix.extend_from_slice(prefix);
    }
    if cloud_prefix.len() < prefix_len {
        anyhow::bail!(
            "Strict import prefix check returned {} bytes, expected {prefix_len}.",
            cloud_prefix.len()
        );
    }

    if cloud_prefix == local_prefix {
        Ok(StrictImportDecision::Accepted)
    } else {
        Ok(StrictImportDecision::Refused)
    }
}

/// Live counters shared between the scan loop and the heartbeat task.
///
/// Atomics are cheaper than wrapping `ImportStats` in a lock because the
/// scan loop bumps these on every asset; the heartbeat task only ever
/// reads them. `last_seen_id` is kept under a `std::sync::Mutex` -- it's
/// touched once per asset and read every 30s, so contention is irrelevant
/// and a `Mutex<Option<String>>` avoids the ABA dance an atomic pointer
/// would require.
#[derive(Debug, Default)]
struct HeartbeatState {
    total: std::sync::atomic::AtomicU64,
    matched: std::sync::atomic::AtomicU64,
    unmatched: std::sync::atomic::AtomicU64,
    filtered: std::sync::atomic::AtomicU64,
    strict_refused: std::sync::atomic::AtomicU64,
    hash_errors: std::sync::atomic::AtomicU64,
    skipped_already_imported: std::sync::atomic::AtomicU64,
    last_seen_id: std::sync::Mutex<Option<String>>,
}

impl HeartbeatState {
    fn snapshot(&self) -> HeartbeatSnapshot {
        use std::sync::atomic::Ordering;
        HeartbeatSnapshot {
            total: self.total.load(Ordering::Relaxed),
            matched: self.matched.load(Ordering::Relaxed),
            unmatched: self.unmatched.load(Ordering::Relaxed),
            filtered: self.filtered.load(Ordering::Relaxed),
            strict_refused: self.strict_refused.load(Ordering::Relaxed),
            hash_errors: self.hash_errors.load(Ordering::Relaxed),
            skipped_already_imported: self.skipped_already_imported.load(Ordering::Relaxed),
            last_seen_id: self.last_seen_id.lock().ok().and_then(|g| g.clone()),
        }
    }
}

#[derive(Debug)]
struct HeartbeatSnapshot {
    total: u64,
    matched: u64,
    unmatched: u64,
    filtered: u64,
    strict_refused: u64,
    hash_errors: u64,
    skipped_already_imported: u64,
    last_seen_id: Option<String>,
}

/// RAII handle for the heartbeat task spawned by [`import_assets`].
///
/// On drop, cancels the cancellation token so the task exits even if the
/// scan loop bails early. The task itself is fire-and-forget; we don't
/// `await` its `JoinHandle` in Drop because Drop is sync and the task
/// will race-to-exit promptly once the token flips.
struct HeartbeatGuard {
    token: tokio_util::sync::CancellationToken,
}

impl HeartbeatGuard {
    fn spawn(
        state: Arc<HeartbeatState>,
        library_label: String,
        interval: std::time::Duration,
    ) -> Self {
        let token = tokio_util::sync::CancellationToken::new();
        let task_token = token.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            // Skip the immediate-fire so the first heartbeat lands ~interval
            // after spawn rather than at t=0 (avoids a noisy log line before
            // any work has happened).
            ticker.tick().await;
            loop {
                tokio::select! {
                    () = task_token.cancelled() => break,
                    _ = ticker.tick() => {
                        let snap = state.snapshot();
                        tracing::info!(
                            library = %library_label,
                            stage = HEARTBEAT_STAGE,
                            total = snap.total,
                            matched = snap.matched,
                            unmatched = snap.unmatched,
                            filtered = snap.filtered,
                            strict_refused = snap.strict_refused,
                            hash_errors = snap.hash_errors,
                            skipped_already_imported = snap.skipped_already_imported,
                            last_seen_id = snap.last_seen_id.as_deref().unwrap_or("(none)"),
                            "import scan heartbeat",
                        );
                    }
                }
            }
        });
        Self { token }
    }
}

impl Drop for HeartbeatGuard {
    fn drop(&mut self) {
        self.token.cancel();
    }
}

/// Convert a [`std::fs::Metadata`] modified time into an epoch-second `i64`.
///
/// Returns `None` when the host filesystem doesn't expose a usable mtime
/// or when the value falls outside `i64` range; callers treat `None` as
/// "no mtime, can't trust skip-rehash" rather than as an error.
fn file_mtime_epoch(metadata: &std::fs::Metadata) -> Option<i64> {
    metadata
        .modified()
        .ok()
        .and_then(|st| st.duration_since(std::time::UNIX_EPOCH).ok())
        .and_then(|d| i64::try_from(d.as_secs()).ok())
}

/// Print the every-100-matches progress milestone if `show_progress` is set.
/// Hoisted out of the scan loop so the skip-rehash arm and the hash-and-adopt
/// arm can't drift in their formatting or cadence.
fn log_progress_milestone(library_label: &str, matched: u64, show_progress: bool) {
    if show_progress && matched.is_multiple_of(100) {
        println!("  [{library_label}] Matched {matched} files so far...");
    }
}

/// Find the on-disk path that satisfies the expected size: primary, then
/// collision-family shapes under `NameSizeDedupWithSuffix`, then AM/PM
/// whitespace siblings. macOS screenshots use NARROW NO-BREAK SPACE
/// (`\u{202F}`) before AM/PM; trees synced through other tools may have
/// normalized to a regular space (or vice versa).
async fn resolve_match_path(
    candidate: &ImportPathCandidate<'_>,
    all_expected: &[ExpectedAssetPath],
    asset_id: &str,
    path_config: &(impl PathDerivationSource + ?Sized),
    dir_cache: &mut DirCache,
) -> Option<(PathBuf, std::fs::Metadata)> {
    let mut candidates = import_match_candidates(candidate, all_expected, asset_id, path_config);
    candidates.sort();
    candidates.dedup();

    for path in &candidates {
        if let Some(found) =
            match_exact_or_ampm_variant(path, candidate.expected_size, dir_cache).await
        {
            return Some(found);
        }
    }

    if path_config.file_match_policy() == FileMatchPolicy::NameSizeDedupWithSuffix {
        for path in candidates {
            if let Some(found) =
                find_identity_ordinal_sibling(&path, asset_id, candidate.expected_size).await
            {
                return Some(found);
            }
        }
    }

    None
}

struct ImportPathCandidate<'a> {
    path: &'a Path,
    expected_size: u64,
    version_size: VersionSizeKey,
}

fn import_match_candidates(
    candidate: &ImportPathCandidate<'_>,
    all_expected: &[ExpectedAssetPath],
    asset_id: &str,
    path_config: &(impl PathDerivationSource + ?Sized),
) -> Vec<PathBuf> {
    let mut candidates = vec![candidate.path.to_path_buf()];
    if path_config.file_match_policy() != FileMatchPolicy::NameSizeDedupWithSuffix {
        return candidates;
    }

    let Some(parent) = candidate.path.parent() else {
        return candidates;
    };
    let Some(fname) = candidate.path.file_name().and_then(|f| f.to_str()) else {
        tracing::debug!(
            path = %candidate.path.display(),
            "Skipping collision-family fallback: filename is not valid UTF-8",
        );
        return candidates;
    };

    candidates.push(parent.join(download::paths::add_dedup_suffix(
        fname,
        candidate.expected_size,
    )));
    candidates.push(parent.join(download::paths::insert_asset_identity_suffix(
        fname, asset_id,
    )));

    if candidate.version_size.is_live_photo_motion()
        && let Some(primary) = primary_expected_path(all_expected)
        && let Some(primary_fname) = primary.path.file_name().and_then(|f| f.to_str())
    {
        let primary_collision_filenames = [
            download::paths::add_dedup_suffix(primary_fname, primary.size),
            download::paths::insert_asset_identity_suffix(primary_fname, asset_id),
        ];
        for primary_filename in primary_collision_filenames {
            let mov_filename = match path_config.live_photo_mov_filename_policy() {
                LivePhotoMovFilenamePolicy::Suffix => {
                    download::paths::live_photo_mov_path_suffix(&primary_filename)
                }
                LivePhotoMovFilenamePolicy::Original => {
                    download::paths::live_photo_mov_path_original(&primary_filename)
                }
            };
            candidates.push(parent.join(&mov_filename));
            candidates.push(parent.join(download::paths::insert_asset_identity_suffix(
                &mov_filename,
                asset_id,
            )));
        }
    }

    candidates
}

fn primary_expected_path(expected: &[ExpectedAssetPath]) -> Option<&ExpectedAssetPath> {
    expected
        .iter()
        .find(|path| path.version_size.is_primary_media())
}

async fn match_exact_or_ampm_variant(
    path: &Path,
    expected_size: u64,
    dir_cache: &mut DirCache,
) -> Option<(PathBuf, std::fs::Metadata)> {
    if let Ok(m) = tokio::fs::metadata(path).await
        && m.len() == expected_size
    {
        return Some((path.to_path_buf(), m));
    }

    let needs_probe = path
        .file_name()
        .and_then(|f| f.to_str())
        .is_some_and(|f| normalize_ampm(f) != f);
    if !needs_probe {
        return None;
    }
    let parent = path.parent()?;
    dir_cache.ensure_dir_async(parent).await;
    let variant = dir_cache.find_ampm_variant(path)?;
    if dir_cache.file_size(&variant) != Some(expected_size) {
        return None;
    }
    let m = tokio::fs::metadata(&variant).await.ok()?;
    tracing::info!(
        primary = %path.display(),
        variant = %variant.display(),
        "Matched AM/PM whitespace variant on disk",
    );
    Some((variant, m))
}

async fn find_identity_ordinal_sibling(
    base_path: &Path,
    asset_id: &str,
    expected_size: u64,
) -> Option<(PathBuf, std::fs::Metadata)> {
    let parent = base_path.parent()?;
    let base = base_path.file_name()?.to_str()?;
    let mut entries = tokio::fs::read_dir(parent).await.ok()?;
    while let Some(entry) = entries.next_entry().await.ok()? {
        let candidate_name = entry.file_name();
        let Some(candidate_name) = candidate_name.to_str() else {
            continue;
        };
        if !download::paths::filename_matches_identity_collision(base, asset_id, candidate_name) {
            continue;
        }
        let metadata = entry.metadata().await.ok()?;
        if metadata.len() == expected_size {
            return Some((entry.path(), metadata));
        }
    }
    None
}

/// Run the import-existing matching loop over a stream of `PhotoAsset`s.
///
/// Splitting this out from [`run_import_existing`] lets tests (wiremock-based)
/// drive the loop without standing up auth + library resolution. Production
/// callers feed in `album.photo_stream(...)`; tests feed in a stream backed
/// by a `MockServer`-pointed `PhotoAlbum`.
///
/// `library_label` is used in tracing + progress prints so multi-library
/// imports stay distinguishable. `panic_rx` is the receiver returned by
/// `photo_stream` -- after the stream is drained, we check it and bail
/// loudly if any fetcher task panicked, since a panicked fetcher closes
/// the stream early and would otherwise read as a clean enumeration.
pub(crate) async fn import_assets<S, D>(
    stream: S,
    panic_rx: tokio::sync::oneshot::Receiver<bool>,
    db: &D,
    path_config: &(impl PathDerivationSource + ?Sized),
    library_label: &str,
    dir_cache: &mut DirCache,
    options: ImportRunOptions<'_>,
) -> anyhow::Result<ImportStats>
where
    S: futures_util::Stream<Item = anyhow::Result<PhotoAsset>>,
    D: ImportStateStore + ?Sized,
{
    use futures_util::StreamExt;
    use std::sync::atomic::Ordering;

    tokio::pin!(stream);
    let mut stats = ImportStats::default();
    let mut scan_started_emitted = false;

    let heartbeat_state = Arc::new(HeartbeatState::default());
    let _heartbeat = HeartbeatGuard::spawn(
        Arc::clone(&heartbeat_state),
        library_label.to_owned(),
        HEARTBEAT_INTERVAL,
    );

    // Bulk-load every prior import-time snapshot for this library once so
    // the per-asset path is an O(1) HashMap probe instead of a DB round-trip
    // per match candidate. On a fresh DB the map is empty and the lookup
    // path falls straight through to the real hash, so this costs one
    // single-row no-op query for first-time imports and saves N queries
    // (and N SHA-256 reads) on every subsequent pass.
    let imported_index = match db.get_all_imported_records(library_label).await {
        Ok(map) => map,
        Err(e) => {
            tracing::warn!(
                library = %library_label,
                error = %e,
                "get_all_imported_records failed; skip-rehash optimization disabled this pass",
            );
            std::collections::HashMap::new()
        }
    };

    loop {
        let result = if let Some(shutdown_token) = options.shutdown_token {
            tokio::select! {
                () = shutdown_token.cancelled() => {
                    anyhow::bail!(
                        "Import was interrupted while scanning library `{library_label}`."
                    );
                }
                result = stream.next() => result,
            }
        } else {
            stream.next().await
        };

        let Some(result) = result else {
            break;
        };

        // Emit a one-shot marker as soon as the first asset is dequeued so
        // tests (and operators tailing logs) can synchronise on real work
        // having started, rather than racing on a wall-clock sleep against
        // auth + library resolution + first-page latency.
        if !scan_started_emitted {
            tracing::info!(
                stage = SCAN_STARTED_STAGE,
                library = %library_label,
                "import scan dequeued first asset",
            );
            scan_started_emitted = true;
        }
        let asset: PhotoAsset = match result {
            Ok(a) => a,
            Err(e) => {
                // Continuing past a fetcher Err would let a partial scan
                // report as clean, leaving unmatched files to re-download
                // on the next sync.
                anyhow::bail!(
                    "Import scan stopped for library `{library_label}` because iCloud returned an error: {e}"
                );
            }
        };

        stats.total += 1;
        heartbeat_state.total.fetch_add(1, Ordering::Relaxed);
        if let Ok(mut last) = heartbeat_state.last_seen_id.lock() {
            *last = Some(asset.state_id().to_string());
        }

        if asset.versions().is_empty() {
            tracing::debug!(id = %asset.id(), "Skipping asset with no versions");
            stats.filtered += 1;
            heartbeat_state.filtered.fetch_add(1, Ordering::Relaxed);
            continue;
        }

        // `expected_paths_for` documents the precondition that callers run
        // `is_asset_filtered` first to apply content/date filters. Import
        // carries the same path/filter fields as sync without constructing a
        // fake download-pipeline config. Honor the contract uniformly so the
        // gate stays in one place.
        if let Some(reason) = is_asset_filtered(&asset, path_config) {
            tracing::debug!(id = %asset.id(), ?reason, "Skipping (is_asset_filtered)");
            stats.filtered += 1;
            heartbeat_state.filtered.fetch_add(1, Ordering::Relaxed);
            continue;
        }

        let expected = expected_paths_for(&asset, path_config);
        if expected.is_empty() {
            // WARN, not debug: an asset silently dropped here is invisible
            // to operators reconciling the on-disk tree against the report.
            tracing::warn!(
                id = %asset.id(),
                live_photo_mode = ?path_config.live_photo_mode(),
                force_resolution = path_config.force_resolution(),
                "Skipping asset with no expected paths from path-derivation",
            );
            stats.filtered += 1;
            heartbeat_state.filtered.fetch_add(1, Ordering::Relaxed);
            continue;
        }

        for expected_path in &expected {
            let ExpectedAssetPath {
                path: primary_path,
                size: expected_size,
                checksum,
                url,
                version_size,
            } = expected_path;
            let expected_size = *expected_size;
            let version_size = *version_size;
            // For `NameSizeDedupWithSuffix`, when two iCloud assets share
            // a filename, icloudpd renames the second's download to
            // `<stem>-<size><ext>` (it stat's the existing file at
            // download time, sees the wrong size, falls back). kei's
            // `expected_paths_for` is single-asset and emits only the
            // bare path, so the size-suffixed file would read as
            // unmatched on import even though it's what kei would also
            // have written under the same collision. Try the suffix
            // shape as a fallback.
            let candidate = ImportPathCandidate {
                path: primary_path,
                expected_size,
                version_size,
            };
            let (expected_path, metadata) = match resolve_match_path(
                &candidate,
                &expected,
                asset.state_id(),
                path_config,
                dir_cache,
            )
            .await
            {
                Some(found) => found,
                None => {
                    stats.unmatched += 1;
                    heartbeat_state.unmatched.fetch_add(1, Ordering::Relaxed);
                    continue;
                }
            };

            if let Some(verifier) = options.strict_verifier {
                match verifier
                    .verify(&expected_path, url.as_ref(), expected_size)
                    .await
                {
                    Ok(StrictImportDecision::Accepted) => {}
                    Ok(StrictImportDecision::Refused) => {
                        tracing::warn!(
                            asset_id = %asset.id(),
                            version = ?version_size,
                            path = %expected_path.display(),
                            "Strict import refused same-name, same-size file: local prefix differs from cloud prefix",
                        );
                        record_strict_refusal(&mut stats, &heartbeat_state);
                        continue;
                    }
                    Err(e) => {
                        tracing::warn!(
                            asset_id = %asset.id(),
                            version = ?version_size,
                            path = %expected_path.display(),
                            error = %e,
                            "Strict import refused same-name, same-size file: prefix verification failed",
                        );
                        record_strict_refusal(&mut stats, &heartbeat_state);
                        continue;
                    }
                }
            }

            if !options.dry_run {
                // Skip-rehash short-circuit: if a prior adopt left a row
                // for this (library, id, version_size) at the same path
                // with the same on-disk size + mtime, the file hasn't
                // changed and re-hashing buys nothing. Skipping is what
                // makes restart-after-interrupt tolerable on slow storage
                // (e.g. /photos on HDD).
                let mtime_epoch = file_mtime_epoch(&metadata);
                let already_imported = imported_index
                    .get(&(
                        asset.state_id().to_string(),
                        version_size.as_str().to_string(),
                    ))
                    .filter(|rec| {
                        rec.local_path == expected_path
                            && rec.imported_size == Some(metadata.len())
                            && rec.imported_mtime.is_some()
                            && rec.imported_mtime == mtime_epoch
                    });

                if let Some(rec) = already_imported {
                    tracing::debug!(
                        asset_id = %asset.id(),
                        version = ?version_size,
                        path = %expected_path.display(),
                        prior_checksum = %rec.local_checksum,
                        "Skipping re-hash: file unchanged since last import",
                    );
                    stats.matched += 1;
                    stats.skipped_already_imported += 1;
                    heartbeat_state.matched.fetch_add(1, Ordering::Relaxed);
                    heartbeat_state
                        .skipped_already_imported
                        .fetch_add(1, Ordering::Relaxed);
                    log_progress_milestone(library_label, stats.matched, options.show_progress);
                    continue;
                }

                // Hash the file BEFORE creating the pending row. If the read
                // fails (permissions, vanished file, I/O error), bailing
                // here leaves no orphan `pending` row that a future sync
                // would wrongly skip.
                let local_checksum = match download::file::compute_sha256(&expected_path).await {
                    Ok(hash) => hash,
                    Err(e) => {
                        tracing::warn!(path = %expected_path.display(), error = %e, "Failed to hash file");
                        stats.hash_errors += 1;
                        heartbeat_state.hash_errors.fetch_add(1, Ordering::Relaxed);
                        continue;
                    }
                };

                let media_type = download::determine_media_type(version_size, &asset);
                let filename = expected_path
                    .file_name()
                    .and_then(|f| f.to_str())
                    .unwrap_or_else(|| {
                        tracing::warn!(
                            asset_id = %asset.id(),
                            version = ?version_size,
                            path = %expected_path.display(),
                            "Recording empty filename: expected path has no UTF-8 file_name component"
                        );
                        ""
                    })
                    .to_string();
                let record = state::AssetRecord::new_pending(
                    Arc::from(library_label),
                    asset.state_id().to_string(),
                    version_size,
                    checksum.to_string(),
                    filename,
                    asset.created(),
                    Some(asset.added_date()),
                    expected_size,
                    media_type,
                );
                if let Err(e) = db
                    .import_adopt(
                        &record,
                        &expected_path,
                        &local_checksum,
                        metadata.len(),
                        mtime_epoch,
                    )
                    .await
                {
                    tracing::warn!(asset_id = %asset.id(), version = ?version_size, error = %e, "Failed to adopt asset");
                    continue;
                }
            }

            stats.matched += 1;
            heartbeat_state.matched.fetch_add(1, Ordering::Relaxed);
            log_progress_milestone(library_label, stats.matched, options.show_progress);
        }
    }

    // Enumeration drained -- but if a fetcher panicked, the stream just
    // closed short, leaving `total` understated. Bail so the scan is
    // obviously aborted (not a silently partial report).
    //
    // `unwrap_or(false)`: a recv error means the sender was dropped without
    // sending, i.e. the prefetch task exited cleanly. The only writer to
    // this channel is the panic guard in `photo_stream`, which sends `true`
    // on panic; absence of a send is the clean-exit signal.
    if panic_rx.await.unwrap_or(false) {
        anyhow::bail!(
            "Import scan stopped for library `{library_label}` because a fetcher task crashed. Results are incomplete; see the earlier error log."
        );
    }

    Ok(stats)
}

/// This imports existing local files into the state database by:
/// 1. Building a path-derivation config from CLI > env > TOML > default,
///    matching the resolution sync uses, so the path-derivation step (filename
///    mapping, name-id7 suffix, size suffix, MOV companions, ...) reproduces
///    exactly what sync would have written.
/// 2. Enumerating each library's all-photos album.
/// 3. For each asset, asking [`expected_paths_for`] which file(s) sync would
///    have produced and checking each against the local filesystem.
/// 4. Recording matches in the state DB so the next sync skips them.
pub(crate) async fn run_import_existing(
    args: cli::ImportArgs,
    globals: &config::GlobalArgs,
    toml: Option<&config::TomlConfig>,
    input_mode: crate::InputMode,
) -> anyhow::Result<()> {
    let shutdown_token = crate::shutdown::install_signal_handler(
        SystemdNotifier::new(false),
        crate::personality::Mode::Off,
    )?;
    let db_path = super::super::get_db_path(globals, toml)?;
    let path_config = build_import_path_config(toml)?;
    let directory = Arc::clone(&path_config.directory);
    let strict_import = resolve_import_strict(&args, toml);
    let strict_verifier = strict_import.then(HttpStrictImportVerifier::new);

    let recent_count: Option<u32> = match args.recent {
        None => None,
        Some(crate::cli::RecentLimit::Count(n)) => Some(n),
        Some(crate::cli::RecentLimit::Days(n)) => {
            anyhow::bail!(
                "`--recent {n}d` is not supported for import-existing because it scans existing files instead of filtering by iCloud date. Use a count like `--recent 1000` instead."
            );
        }
    };

    // Bail clearly on missing/non-dir; `dir_cache::read_dir_entries`
    // swallows read errors and would otherwise report "0 files matched".
    match tokio::fs::metadata(&directory).await {
        Ok(m) if m.is_dir() => {}
        Ok(_) => anyhow::bail!("Download path is not a directory: {}", directory.display()),
        Err(e) => anyhow::bail!(
            "Could not read download directory {}: {e}",
            directory.display()
        ),
    }

    let db = Arc::new(state::SqliteStateDb::open(&db_path).await?);
    tracing::debug!(path = %db_path.display(), "State database opened");

    let (username, password, domain, cookie_directory) =
        config::resolve_auth(globals, &args.password, toml);

    let password_provider = super::super::make_provider_from_auth(
        &args.password,
        password,
        &username,
        &cookie_directory,
        toml,
        input_mode,
    );

    let auth_result = auth::authenticate_in_input_mode(
        &cookie_directory,
        &username,
        &password_provider,
        domain.as_str(),
        None,
        None,
        None,
        input_mode,
    )
    .await?;

    // `kei import-existing` runs without the friendly download bar; off-mode
    // keeps the existing diagnostic warn line for journals.
    let (_shared_session, mut photos_service) = init_photos_service(
        auth_result,
        retry::RetryConfig::default(),
        crate::personality::Mode::Off,
        None,
    )
    .await?;

    // Resolve library selection (CLI > TOML > default `primary`)
    let toml_filters = toml.and_then(|t| t.filters.as_ref());
    let selector = config::resolve_library_selector(args.libraries.clone(), toml_filters)?;
    let libraries = resolve_libraries(&selector, &mut photos_service).await?;

    // Album / smart-folder / unfiled selection comes from TOML for
    // import-existing (which has no per-pass CLI flags of its own). The
    // resulting `Selection` drives `resolve_passes` below so import iterates
    // the same passes sync would, and each pass uses its own
    // `folder_structure_*` template when deriving expected paths.
    let selection = build_import_selection(toml_filters, &selector)?;
    let all_libraries = photos_service.all_libraries().await?;
    let cross_zone_libraries =
        resolve_cross_zone_libraries_for_album_hydration(&selection, async {
            Ok::<_, anyhow::Error>(all_libraries.clone())
        })
        .await?;
    let collection_libraries = collection_libraries(&selection, &libraries, &all_libraries);
    let collection_context = build_collection_context(&selection, collection_libraries).await?;
    let selected_zones = zone_name_set(&libraries);
    let collection_zones = zone_name_set(collection_libraries);

    let prior_db_total = db.get_summary().await?.total_assets;
    if prior_db_total > 0 && !args.force_empty {
        use futures_util::{StreamExt, TryStreamExt};
        let counts: Vec<u64> = futures_util::stream::iter(libraries.iter())
            .map(|library| async move {
                library.all().len().await.with_context(|| {
                    format!("Could not count assets in library {}", library.zone_name())
                })
            })
            .buffered(libraries.len().max(1))
            .try_collect()
            .await?;
        let empty_zones: Vec<&str> = libraries
            .iter()
            .zip(&counts)
            .filter(|&(_, &count)| count == 0)
            .map(|(library, _)| library.zone_name())
            .collect();
        validate_non_empty_libraries(&empty_zones, prior_db_total)?;
    }

    if !args.no_progress_bar {
        println!("Scanning iCloud assets and matching with local files...");
    }

    let mut totals = ImportStats::default();
    // Hoisted across passes: a multi-album asset's parent dir is read_dir'd
    // once per library scan, not once per pass.
    let mut dir_cache = DirCache::new();

    for library in &all_libraries {
        let zone = library.zone_name();
        let pass_scope = pass_scope_for_zone(&selection, zone, &selected_zones, &collection_zones);
        if pass_scope.is_empty() {
            continue;
        }
        tracing::debug!(zone = %zone, "Scanning library");
        let library_config = path_config.with_library(zone);

        let plan = resolve_passes_for_scope(
            library,
            &selection,
            pass_scope,
            &collection_context,
            &cross_zone_libraries,
        )
        .await?;
        if plan.passes.is_empty() {
            tracing::debug!(zone = %zone, "No passes resolved; nothing to import");
            continue;
        }

        for pass in &plan.passes {
            let pass_config = library_config.with_pass(pass);
            tracing::debug!(
                zone = %zone,
                kind = ?pass.kind,
                template = %pass_config.folder_structure,
                "Importing pass"
            );
            let (stream, panic_rx) = pass.album.photo_stream(recent_count, None, 1);
            let stats = import_assets(
                stream,
                panic_rx,
                db.as_ref(),
                &pass_config,
                zone,
                &mut dir_cache,
                ImportRunOptions {
                    dry_run: args.dry_run,
                    show_progress: !args.no_progress_bar,
                    strict_verifier: strict_verifier
                        .as_ref()
                        .map(|v| v as &dyn StrictImportVerifier),
                    shutdown_token: Some(&shutdown_token),
                },
            )
            .await?;
            totals += stats;
        }
    }

    println!();
    if args.dry_run {
        println!("Import complete (DRY RUN - no changes written to state DB):");
    } else {
        println!("Import complete:");
    }
    println!("  Total assets scanned: {}", totals.total);
    println!("  Files matched:        {}", totals.matched);
    println!(
        "  ... skipped re-hash:  {}",
        totals.skipped_already_imported
    );
    println!("  Unmatched versions:   {}", totals.unmatched);
    println!("  Filtered (no path):   {}", totals.filtered);
    println!("  Strict refusals:      {}", totals.strict_refused);
    println!("  Hash errors:          {}", totals.hash_errors);

    Ok(())
}

/// Build the [`Selection`] that `resolve_passes` consumes for import.
///
/// `import-existing` has no `--album` / `--smart-folder` / `--unfiled` CLI
/// flags of its own, so the selection comes from current `[filters]` TOML:
/// `albums`, `smart_folders`, and `unfiled`. The `LibrarySelector` was
/// already resolved upstream and is threaded in directly so
/// `Selection.libraries` matches what `resolve_libraries` walked.
fn build_import_selection(
    toml_filters: Option<&config::TomlFilters>,
    libraries: &crate::selection::LibrarySelector,
) -> anyhow::Result<crate::selection::Selection> {
    use crate::selection::{Selection, parse_album_selector, parse_smart_folder_selector};

    let raw_albums: Vec<String> = toml_filters
        .and_then(|f| f.albums.as_ref())
        .cloned()
        .unwrap_or_default();
    let raw_smart_folders: Vec<String> = toml_filters
        .and_then(|f| f.smart_folders.as_ref())
        .cloned()
        .unwrap_or_default();
    let unfiled = toml_filters.and_then(|f| f.unfiled).unwrap_or(true);

    Ok(Selection {
        albums: parse_album_selector(&raw_albums, true)?,
        albums_explicit: !raw_albums.is_empty(),
        smart_folders: parse_smart_folder_selector(&raw_smart_folders)?,
        smart_folders_explicit: !raw_smart_folders.is_empty(),
        libraries: libraries.clone(),
        unfiled,
    })
}

/// Refuse to scan when one or more selected libraries returned zero assets
/// while the state DB has prior asset rows. `prior_db_total` is global, not
/// per-zone (the assets table has no zone column), so a brand-new SharedSync
/// joining an account with a populated PrimarySync also trips this guard --
/// `--force-empty` is the documented escape hatch for that case. Caller
/// short-circuits when `prior_db_total == 0` or `--force-empty` is set, so
/// this function only runs when a guard is wanted.
fn validate_non_empty_libraries(empty_zones: &[&str], prior_db_total: u64) -> anyhow::Result<()> {
    let [head, tail @ ..] = empty_zones else {
        return Ok(());
    };
    let zones = if tail.is_empty() {
        format!("library {head}")
    } else {
        format!("libraries {}", empty_zones.join(", "))
    };
    anyhow::bail!(
        "{zones} returned 0 assets, but the state database already has {prior_db_total} asset rows. Stopping to avoid a misleading `matched: 0` import. This is usually a temporary iCloud permission or stale-login problem. Check with `kei list libraries` or the iCloud web UI, then retry. Use `--force-empty` (or KEI_FORCE_EMPTY=true) only if that library is intentionally empty."
    );
}

fn resolve_import_strict(args: &cli::ImportArgs, toml: Option<&config::TomlConfig>) -> bool {
    args.strict
        || toml
            .and_then(|t| t.import.as_ref())
            .and_then(|i| i.strict)
            .unwrap_or(false)
}

/// Resolve import-existing path/filter settings from TOML.
///
/// Persistent path/media/photo settings are shared with sync through the TOML
/// config. Pipeline-only download settings stay out of this type entirely.
fn build_import_path_config(
    toml: Option<&config::TomlConfig>,
) -> anyhow::Result<PathDerivationConfig> {
    let toml_dl = toml.and_then(|t| t.download.as_ref());

    let directory_str = toml_dl
        .and_then(|d| d.directory.clone())
        .unwrap_or_default();
    if directory_str.is_empty() {
        anyhow::bail!(crate::upgrade_hints::with_stale_env_hint(String::from(
            "Set [download].directory in the config file before running import-existing.",
        )));
    }
    let directory_path = config::expand_tilde(&directory_str);
    config::validate_download_dir(&directory_path)?;
    let directory: Arc<Path> = Arc::from(directory_path.as_path());

    let path_fields = config::resolve_path_derivation_fields(
        config::PathDerivationCliArgs {
            folder_structure: None,
            folder_structure_albums: None,
            folder_structure_smart_folders: None,
            resolution: None,
            live_photo_mode: None,
            live_resolution: None,
            live_photo_mov_filename_policy: None,
            edited: None,
            alternative: None,
            raw_policy: None,
            file_match_policy: None,
            force_resolution: None,
            keep_unicode_in_filenames: None,
        },
        toml,
    )?;
    let media = config::resolve_media_selection(toml.and_then(|t| t.filters.as_ref()), None, None)?;

    let config = PathDerivationConfig::from_path_fields(directory, path_fields, media);
    Ok(config)
}

#[cfg(test)]
mod build_selection_tests {
    //! Sync vs. import-existing parity for the current selector resolution.
    //! Both commands must produce the same `Selection` from the same TOML.
    use super::build_import_selection;
    use crate::commands::{
        collection_libraries, pass_scope_for_zone,
        resolve_cross_zone_libraries_for_album_hydration, zone_name_set,
    };
    use crate::config::TomlFilters;
    use crate::selection::{AlbumSelector, LibrarySelector, Selection, SmartFolderSelector};
    use crate::test_helpers::MockPhotosSession;
    use std::collections::BTreeSet;

    fn primary() -> LibrarySelector {
        LibrarySelector::default()
    }

    /// Current-shape input uses the array `albums` selector.
    #[test]
    fn new_albums_array_still_works() {
        let filters = TomlFilters {
            albums: Some(vec!["Vacation".to_string(), "!Family".to_string()]),
            ..TomlFilters::default()
        };
        let selection = build_import_selection(Some(&filters), &primary()).expect("ok");
        match selection.albums {
            AlbumSelector::Named { included, excluded } => {
                assert_eq!(included, BTreeSet::from(["Vacation".to_string()]));
                assert_eq!(excluded, BTreeSet::from(["Family".to_string()]));
            }
            other => panic!("expected AlbumSelector::Named, got {other:?}"),
        }
    }

    /// Empty TOML filters → defaults from `Selection::default()`. This is the
    /// no-config-file case and must match `kei sync` with no flags.
    #[test]
    fn no_filters_yields_default_selection() {
        let selection = build_import_selection(None, &primary()).expect("ok");
        assert_eq!(selection.albums, AlbumSelector::default());
        assert_eq!(selection.smart_folders, SmartFolderSelector::None);
        assert!(
            selection.unfiled,
            "unfiled defaults to true to match v0.13 sync semantics"
        );
        assert_eq!(
            selection,
            Selection {
                albums: AlbumSelector::default(),
                albums_explicit: false,
                smart_folders: SmartFolderSelector::None,
                smart_folders_explicit: false,
                libraries: primary(),
                unfiled: true,
            }
        );
    }

    #[tokio::test]
    async fn default_import_album_selection_skips_cross_zone_resolution() {
        let selection = build_import_selection(None, &primary()).expect("ok");

        let libraries = resolve_cross_zone_libraries_for_album_hydration(&selection, async {
            panic!("implicit default album scope must not request all libraries")
        })
        .await
        .unwrap();
        assert!(libraries.is_empty());
    }

    fn test_library(zone_name: &str) -> crate::icloud::photos::PhotoLibrary {
        crate::icloud::photos::PhotoLibrary::new_stub_with_zone(
            Box::new(MockPhotosSession::new()),
            zone_name,
        )
    }

    fn smart_folder_filters_with_unfiled(unfiled: bool) -> TomlFilters {
        TomlFilters {
            smart_folders: Some(vec!["Hidden".to_string()]),
            unfiled: Some(unfiled),
            ..TomlFilters::default()
        }
    }

    #[test]
    fn import_scope_planning_shared_only_smart_folder_widens_zone_scope() {
        let selector = crate::selection::parse_library_selector(&["shared".to_string()]).unwrap();
        let selection =
            build_import_selection(Some(&smart_folder_filters_with_unfiled(false)), &selector)
                .expect("ok");
        let primary = test_library("PrimarySync");
        let shared = test_library("SharedSync-ABCD1234");
        let selected_libraries = vec![shared.clone()];
        let all_libraries = vec![primary.clone(), shared.clone()];

        let collection = collection_libraries(&selection, &selected_libraries, &all_libraries);
        let selected_zones = zone_name_set(&selected_libraries);
        let collection_zones = zone_name_set(collection);

        let primary_scope = pass_scope_for_zone(
            &selection,
            primary.zone_name(),
            &selected_zones,
            &collection_zones,
        );
        let shared_scope = pass_scope_for_zone(
            &selection,
            shared.zone_name(),
            &selected_zones,
            &collection_zones,
        );

        assert!(
            primary_scope.include_smart_folders,
            "explicit smart-folder selection should widen import pass planning beyond the library selector"
        );
        assert!(
            shared_scope.include_smart_folders,
            "import-existing planning should schedule smart-folder passes for selected shared zone"
        );
    }

    #[test]
    fn import_scope_planning_primary_only_still_filters_unfiled() {
        let selector = crate::selection::parse_library_selector(&["primary".to_string()]).unwrap();
        let selection =
            build_import_selection(Some(&smart_folder_filters_with_unfiled(true)), &selector)
                .expect("ok");
        let primary = test_library("PrimarySync");
        let shared = test_library("SharedSync-ABCD1234");
        let selected_libraries = vec![primary.clone()];
        let all_libraries = vec![primary.clone(), shared.clone()];

        let collection = collection_libraries(&selection, &selected_libraries, &all_libraries);
        let selected_zones = zone_name_set(&selected_libraries);
        let collection_zones = zone_name_set(collection);

        let primary_scope = pass_scope_for_zone(
            &selection,
            primary.zone_name(),
            &selected_zones,
            &collection_zones,
        );
        let shared_scope = pass_scope_for_zone(
            &selection,
            shared.zone_name(),
            &selected_zones,
            &collection_zones,
        );

        assert!(
            primary_scope.include_smart_folders,
            "import-existing planning should keep smart-folder passes in the selected primary zone"
        );
        assert!(
            shared_scope.include_smart_folders,
            "explicit smart-folder selection should widen import scope to shared zones too"
        );
        assert!(
            primary_scope.include_unfiled,
            "selected primary zone should keep unfiled pass when unfiled=true"
        );
        assert!(
            !shared_scope.include_unfiled,
            "library selector should still filter unfiled passes to selected zones"
        );
    }
}

#[cfg(test)]
mod wiremock_tests {
    //! End-to-end tests for `import-existing` driven through a wiremock
    //! `MockServer` stubbing the CloudKit `/records/query` endpoint. Each
    //! test stands up a mock CloudKit, a real `PhotoAlbum` pointed at it,
    //! a real `SqliteStateDb`, and stages local files to match (or not
    //! match) what `expected_paths_for` derives. Then drives `import_assets`
    //! and asserts on the returned `ImportStats` plus DB rows.
    //!
    //! Coverage matrix lives in this one place rather than a sprawling
    //! integration-test directory because:
    //! - `MockPhotosSession`, `PhotoAlbum::new`, and `SqliteStateDb` are
    //!   `pub(crate)` / `pub(crate)`-by-default, so an integration test
    //!   under `tests/` couldn't reach them without exposing internals.
    //! - The matching logic is a pure function of (asset metadata,
    //!   `DownloadConfig`, on-disk files) -- a unit test exercises that
    //!   surface area faithfully.
    //!
    //! The live test in `tests/import_existing_live.rs` covers the full
    //! binary entry point against real Apple, complementing this file.
    use std::collections::HashMap;
    use std::path::Path as StdPath;
    use std::sync::Arc;

    use rustc_hash::FxHashSet;
    use serde_json::{Value, json};
    use tempfile::TempDir;
    use wiremock::matchers::{header, method as wm_method, path as wm_path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::{
        ImportRunOptions, ImportStats, StrictImportDecision, StrictImportVerifier, import_assets,
        verify_strict_prefix,
    };
    use crate::download::filter::expected_paths_for;
    use crate::download::paths::DirCache;
    use crate::download::{AssetGroupings, DownloadConfig, SyncMode};
    use crate::icloud::photos::session::PhotosSession;
    use crate::icloud::photos::{PhotoAlbum, PhotoAlbumConfig, PhotoAsset};
    use crate::retry::RetryConfig;
    use crate::state::{
        AssetStatus, ImportStateStore, ReportStateStore, SqliteStateDb, VersionSizeKey,
    };
    use crate::types::{
        AssetVersionSize, FileMatchPolicy, LivePhotoMode, LivePhotoMovFilenamePolicy, RawPolicy,
    };

    // ── Synthetic asset / wire JSON helpers ──────────────────────────

    /// One synthetic asset that knows both how to build a `PhotoAsset`
    /// (so tests can pre-compute `expected_paths_for`) and how to emit
    /// wire-format CPLMaster + CPLAsset records (so wiremock can serve
    /// them on `/records/query`).
    #[derive(Clone)]
    struct WiremockAsset {
        record_name: String,
        filename: String,
        item_type: String,
        orig_size: u64,
        orig_checksum: String,
        orig_file_type: String,
        asset_date: f64,
        /// `(size, checksum)` for the live-photo MOV companion.
        live_mov: Option<(u64, String)>,
        /// `(size, checksum, file_type)` for the alternative version
        /// (used for RAW+JPEG pairs).
        alt: Option<(u64, String, String)>,
    }

    impl WiremockAsset {
        fn new(record_name: &str, filename: &str, item_type: &str) -> Self {
            Self {
                record_name: record_name.to_string(),
                filename: filename.to_string(),
                item_type: item_type.to_string(),
                orig_size: 1024,
                orig_checksum: format!("checksum_{record_name}"),
                orig_file_type: item_type.to_string(),
                asset_date: 1_736_899_200_000.0,
                live_mov: None,
                alt: None,
            }
        }

        fn orig(mut self, size: u64, checksum: &str, file_type: &str) -> Self {
            self.orig_size = size;
            self.orig_checksum = checksum.to_string();
            self.orig_file_type = file_type.to_string();
            self
        }

        fn live_mov(mut self, size: u64, checksum: &str) -> Self {
            self.live_mov = Some((size, checksum.to_string()));
            self
        }

        fn alt(mut self, size: u64, checksum: &str, file_type: &str) -> Self {
            self.alt = Some((size, checksum.to_string(), file_type.to_string()));
            self
        }

        fn master_fields(&self) -> Value {
            let mut fields = json!({
                "filenameEnc": {"value": &self.filename, "type": "STRING"},
                "itemType": {"value": &self.item_type},
                "resOriginalFileType": {"value": &self.orig_file_type},
                "resOriginalRes": {"value": {
                    "size": self.orig_size,
                    "downloadURL": "https://p01.icloud-content.com/test/orig",
                    "fileChecksum": &self.orig_checksum,
                }},
            });
            if let Some((size, checksum)) = &self.live_mov {
                fields["resOriginalVidComplRes"] = json!({"value": {
                    "size": *size,
                    "downloadURL": "https://p01.icloud-content.com/test/mov",
                    "fileChecksum": checksum,
                }});
                fields["resOriginalVidComplFileType"] =
                    json!({"value": "com.apple.quicktime-movie"});
            }
            if let Some((size, checksum, ftype)) = &self.alt {
                fields["resOriginalAltRes"] = json!({"value": {
                    "size": *size,
                    "downloadURL": "https://p01.icloud-content.com/test/alt",
                    "fileChecksum": checksum,
                }});
                fields["resOriginalAltFileType"] = json!({"value": ftype});
            }
            fields
        }

        /// Build the in-memory `PhotoAsset` for staging-path calculation.
        fn to_photo_asset(&self) -> PhotoAsset {
            let master = json!({
                "recordName": &self.record_name,
                "fields": self.master_fields(),
            });
            let asset = json!({
                "fields": {
                    "assetDate": {"value": self.asset_date},
                    "addedDate": {"value": self.asset_date},
                },
            });
            PhotoAsset::new(master, asset)
        }

        /// Emit `[CPLMaster, CPLAsset]` records as they appear on the
        /// `/records/query` wire response. The pairing uses a `masterRef`
        /// pointing at the master's `recordName`.
        fn to_cloudkit_records(&self) -> [Value; 2] {
            let master = json!({
                "recordName": &self.record_name,
                "recordType": "CPLMaster",
                "fields": self.master_fields(),
            });
            let asset = json!({
                "recordName": format!("{}_asset", self.record_name),
                "recordType": "CPLAsset",
                "fields": {
                    "masterRef": {"value": {"recordName": &self.record_name}},
                    "assetDate": {"value": self.asset_date},
                    "addedDate": {"value": self.asset_date},
                },
            });
            [master, asset]
        }
    }

    /// Stateful responder: serves a records-page on the FIRST matching
    /// request and an empty page on every subsequent request. Lets one
    /// mounted Mock cover the full enumeration so we don't trip over
    /// wiremock stub-priority when multiple stubs match the same path.
    struct OneShotPage {
        full_body: String,
        empty_body: String,
        served: std::sync::atomic::AtomicBool,
    }

    impl wiremock::Respond for OneShotPage {
        fn respond(&self, _req: &wiremock::Request) -> ResponseTemplate {
            if self.served.swap(true, std::sync::atomic::Ordering::SeqCst) {
                ResponseTemplate::new(200).set_body_string(self.empty_body.clone())
            } else {
                ResponseTemplate::new(200).set_body_string(self.full_body.clone())
            }
        }
    }

    /// Build a `/records/query` response body wrapping the given assets'
    /// CloudKit records. Used both by `stub_records_query` (single-page
    /// stubs) and by tests that script a sequence of pages.
    fn cloudkit_records_body(assets: &[&WiremockAsset]) -> String {
        let mut records = Vec::with_capacity(assets.len() * 2);
        for a in assets {
            for rec in a.to_cloudkit_records() {
                records.push(rec);
            }
        }
        serde_json::to_string(&json!({
            "records": records,
            "syncToken": "stub-token",
        }))
        .expect("serialize body")
    }

    /// Mount a single mock on `server` that returns the assets on the
    /// first `/records/query` POST and empty pages on all later requests.
    async fn stub_records_query(server: &MockServer, assets: &[WiremockAsset]) {
        let asset_refs: Vec<&WiremockAsset> = assets.iter().collect();
        let full_body = cloudkit_records_body(&asset_refs);
        let empty_body = cloudkit_records_body(&[]);

        Mock::given(wm_method("POST"))
            .and(wm_path("/records/query"))
            .respond_with(OneShotPage {
                full_body,
                empty_body,
                served: std::sync::atomic::AtomicBool::new(false),
            })
            .mount(server)
            .await;
    }

    /// Build a `PhotoAlbum` whose `service_endpoint` points at the
    /// wiremock server. Uses a real `reqwest::Client` so the full HTTP
    /// stack runs.
    fn album_pointed_at(server: &MockServer) -> PhotoAlbum {
        let session: Box<dyn PhotosSession> = Box::new(reqwest::Client::new());
        PhotoAlbum::new(
            PhotoAlbumConfig {
                params: Arc::new(HashMap::new()),
                service_endpoint: Arc::from(server.uri()),
                name: Arc::from("test-all"),
                list_type: Arc::from("CPLAssetAndMasterByAssetDateWithoutHiddenOrDeleted"),
                obj_type: Arc::from("CPLAssetByAssetDateWithoutHiddenOrDeleted"),
                query_filter: None,
                page_size: 100,
                zone_id: Arc::new(json!({"zoneName": "PrimarySync"})),
                retry_config: RetryConfig {
                    max_retries: 0,
                    base_delay_secs: 0,
                    max_delay_secs: 0,
                },
                container_id: None,
                cross_zone_sources: Vec::new(),
            },
            session,
        )
    }

    async fn open_db(tmp: &TempDir) -> Arc<SqliteStateDb> {
        let path = tmp.path().join("state.db");
        Arc::new(SqliteStateDb::open(&path).await.expect("open state db"))
    }

    /// Convenience: fetch every downloaded row.
    async fn all_downloaded(db: &dyn ReportStateStore) -> Vec<crate::state::AssetRecord> {
        db.get_downloaded_page(0, 1024)
            .await
            .expect("get_downloaded_page")
    }

    /// Build a `DownloadConfig` with `directory` set and everything else
    /// at its production-default value. Tests then mutate just the field
    /// they're exercising.
    fn base_config(directory: &StdPath) -> DownloadConfig {
        let dir_arc: Arc<StdPath> = Arc::from(directory);
        DownloadConfig {
            directory: dir_arc,
            folder_structure: "%Y/%m/%d".to_string(),
            folder_structure_albums: Arc::from("{album}"),
            folder_structure_smart_folders: Arc::from("{smart-folder}"),
            library: Arc::from(crate::icloud::photos::PRIMARY_ZONE_NAME),
            resolution: crate::types::PhotoResolution::Original,
            media: crate::config::MediaSelection::all(),
            skip_created_before: None,
            skip_created_after: None,
            metadata: crate::config::MetadataConfig::default(),
            refresh_metadata: false,
            capture_timestamp_repair: crate::download::CaptureTimestampRepair::Preserve,
            repair_truncated: false,
            concurrent_downloads: 1,
            recent: None,
            recent_scope: crate::cli::RecentScope::Global,
            retry: RetryConfig::default(),
            live_photo_mode: LivePhotoMode::Both,
            live_resolution: AssetVersionSize::LiveOriginal,
            live_photo_mov_filename_policy: LivePhotoMovFilenamePolicy::Suffix,
            edited: false,
            alternative: false,
            raw_policy: RawPolicy::AsIs,
            file_match_policy: FileMatchPolicy::NameSizeDedupWithSuffix,
            force_resolution: false,
            keep_unicode_in_filenames: false,
            filename_exclude: Arc::from(Vec::<glob::Pattern>::new()),
            temp_suffix: Arc::from(".kei-tmp"),
            state_db: None,
            retry_only: false,
            max_download_attempts: 0,
            sync_mode: SyncMode::Full,
            enum_config_hash: None,
            album_name: None,
            exclude_asset_ids: Arc::new(FxHashSet::default()),
            asset_groupings: Arc::new(AssetGroupings::default()),
            bandwidth_limiter: None,
        }
    }

    /// Stage a zero-filled file at `path` of exactly `size` bytes.
    /// `import-existing` matches on `metadata.len() == expected_size`,
    /// then SHA-256s the file (which can be zero-bytes content -- the
    /// hash is recorded as `local_checksum`, not compared to the iCloud
    /// `checksum` at this stage).
    fn stage_file(path: &StdPath, size: u64) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create parent");
        }
        let f = std::fs::File::create(path).expect("create file");
        f.set_len(size).expect("set_len");
    }

    fn stage_file_with_bytes(path: &StdPath, bytes: &[u8]) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create parent");
        }
        std::fs::write(path, bytes).expect("write file");
    }

    #[cfg(unix)]
    fn set_mtime_for_test(path: &StdPath, secs: i64) {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;

        let c_path = CString::new(path.as_os_str().as_bytes()).unwrap();
        let times = [
            libc::timespec {
                tv_sec: secs,
                tv_nsec: 0,
            },
            libc::timespec {
                tv_sec: secs,
                tv_nsec: 0,
            },
        ];
        // SAFETY: `c_path` is a valid, NUL-terminated copy of `path`, and
        // `times` points to two initialized timespec values for the duration
        // of the call.
        let rc = unsafe { libc::utimensat(libc::AT_FDCWD, c_path.as_ptr(), times.as_ptr(), 0) };
        assert_eq!(rc, 0, "set mtime for {}", path.display());
    }

    #[cfg(not(unix))]
    fn set_mtime_for_test(path: &StdPath, _secs: i64) {
        std::thread::sleep(std::time::Duration::from_millis(1100));
        let f = std::fs::OpenOptions::new()
            .write(true)
            .open(path)
            .expect("open for mtime bump");
        f.set_len(f.metadata().expect("metadata").len())
            .expect("set_len same size");
    }

    /// Stage every file `expected_paths_for` would emit for `asset` so
    /// they all match. Returns the list of staged paths.
    fn stage_expected(asset: &PhotoAsset, config: &DownloadConfig) -> Vec<std::path::PathBuf> {
        let expected = expected_paths_for(asset, config);
        let mut staged = Vec::new();
        for ep in expected {
            stage_file(&ep.path, ep.size);
            staged.push(ep.path.clone());
        }
        staged
    }

    /// Drive `import_assets` once with the given config + assets, returning
    /// the resulting stats. Sets up the mock server, the album, and the
    /// stream. Caller is responsible for staging files first.
    async fn run_import(
        server: &MockServer,
        assets: &[WiremockAsset],
        db: &dyn ImportStateStore,
        config: &DownloadConfig,
        dry_run: bool,
    ) -> ImportStats {
        run_import_with_strict(server, assets, db, config, dry_run, None).await
    }

    async fn run_import_with_strict(
        server: &MockServer,
        assets: &[WiremockAsset],
        db: &dyn ImportStateStore,
        config: &DownloadConfig,
        dry_run: bool,
        strict_verifier: Option<&dyn StrictImportVerifier>,
    ) -> ImportStats {
        stub_records_query(server, assets).await;
        let album = album_pointed_at(server);
        let (stream, panic_rx) = album.photo_stream(None, None, 1);
        let mut dir_cache = DirCache::new();
        import_assets(
            stream,
            panic_rx,
            db,
            config,
            "test-all",
            &mut dir_cache,
            ImportRunOptions {
                dry_run,
                strict_verifier,
                ..Default::default()
            },
        )
        .await
        .expect("import_assets")
    }

    #[derive(Debug)]
    struct TestStrictVerifier {
        cloud_prefix: Vec<u8>,
    }

    #[async_trait::async_trait]
    impl StrictImportVerifier for TestStrictVerifier {
        async fn verify(
            &self,
            local_path: &StdPath,
            _cloud_url: &str,
            expected_size: u64,
        ) -> anyhow::Result<StrictImportDecision> {
            use tokio::io::AsyncReadExt;

            let prefix_len = usize::try_from(expected_size)
                .unwrap_or(usize::MAX)
                .min(self.cloud_prefix.len());
            let mut local_prefix = vec![0_u8; prefix_len];
            let mut file = tokio::fs::File::open(local_path).await?;
            file.read_exact(&mut local_prefix).await?;
            if local_prefix == self.cloud_prefix[..prefix_len] {
                Ok(StrictImportDecision::Accepted)
            } else {
                Ok(StrictImportDecision::Refused)
            }
        }
    }

    // ── Tests: default flow ───────────────────────────────────────────

    /// Diagnostic: prove the wire round-trip produces a matching PhotoAsset
    /// and that the stream emits exactly one item.
    #[tokio::test]
    async fn diagnostic_stream_round_trip() {
        use futures_util::StreamExt;
        let server = crate::start_wiremock_or_skip!();
        let asset = WiremockAsset::new("D1", "IMG_DIAG.JPG", "public.jpeg").orig(
            1234,
            "ck_d1",
            "public.jpeg",
        );
        let test_asset = asset.to_photo_asset();
        let test_versions: Vec<_> = test_asset.versions().iter().map(|(k, _)| *k).collect();
        let test_filename = test_asset.filename().map(String::from);
        assert!(
            !test_versions.is_empty(),
            "test_helpers asset must have versions"
        );

        stub_records_query(&server, &[asset]).await;
        let album = album_pointed_at(&server);
        let (stream, _panic) = album.photo_stream(None, None, 1);
        let collected: Vec<_> = stream.collect().await;
        assert_eq!(
            collected.len(),
            1,
            "stream must emit exactly one PhotoAsset, got {}",
            collected.len()
        );
        let first = collected.into_iter().next().unwrap().expect("stream ok");
        let stream_versions: Vec<_> = first.versions().iter().map(|(k, _)| *k).collect();
        assert_eq!(first.filename().map(String::from), test_filename);
        assert_eq!(stream_versions, test_versions);
    }

    #[tokio::test]
    async fn matches_single_jpeg_with_default_policy() {
        let server = crate::start_wiremock_or_skip!();
        let asset = WiremockAsset::new("A1", "IMG_0001.JPG", "public.jpeg").orig(
            1234,
            "ck_a1",
            "public.jpeg",
        );
        let tmp = TempDir::new().unwrap();
        let dl = tmp.path().join("photos");
        std::fs::create_dir_all(&dl).unwrap();
        let config = base_config(&dl);
        stage_expected(&asset.to_photo_asset(), &config);

        let db = open_db(&tmp).await;
        let stats = run_import(&server, &[asset], db.as_ref(), &config, false).await;

        assert_eq!(stats.total, 1, "one asset enumerated");
        assert_eq!(stats.matched, 1, "one version matched");
        assert_eq!(stats.unmatched, 0);

        let rows = all_downloaded(db.as_ref()).await;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].status, AssetStatus::Downloaded);
        assert_eq!(&*rows[0].id, "A1");
    }

    #[tokio::test]
    async fn unmatched_when_size_differs() {
        let server = crate::start_wiremock_or_skip!();
        let asset = WiremockAsset::new("A2", "IMG_0002.JPG", "public.jpeg").orig(
            5000,
            "ck_a2",
            "public.jpeg",
        );
        let tmp = TempDir::new().unwrap();
        let dl = tmp.path().join("photos");
        std::fs::create_dir_all(&dl).unwrap();
        let config = base_config(&dl);

        // Stage a file with the right path but wrong size.
        let expected = expected_paths_for(&asset.to_photo_asset(), &config);
        assert_eq!(expected.len(), 1);
        stage_file(&expected[0].path, expected[0].size + 1);

        let db = open_db(&tmp).await;
        let stats = run_import(&server, &[asset], db.as_ref(), &config, false).await;

        assert_eq!(stats.total, 1);
        assert_eq!(stats.matched, 0);
        assert_eq!(stats.unmatched, 1);
        assert!(all_downloaded(db.as_ref()).await.is_empty());
    }

    #[tokio::test]
    async fn unmatched_when_file_missing() {
        let server = crate::start_wiremock_or_skip!();
        let asset = WiremockAsset::new("A3", "IMG_0003.JPG", "public.jpeg");
        let tmp = TempDir::new().unwrap();
        let dl = tmp.path().join("photos");
        std::fs::create_dir_all(&dl).unwrap();
        let config = base_config(&dl);

        let db = open_db(&tmp).await;
        let stats = run_import(&server, &[asset], db.as_ref(), &config, false).await;

        assert_eq!(stats.total, 1);
        assert_eq!(stats.matched, 0);
        assert_eq!(stats.unmatched, 1);
    }

    // ── Tests: name-id7 (the PR #294 fix path) ────────────────────────

    #[tokio::test]
    async fn name_id7_filename_is_matched() {
        let server = crate::start_wiremock_or_skip!();
        let asset = WiremockAsset::new("REC123", "IMG_4726.HEIC", "public.heic").orig(
            2345,
            "ck_rec123",
            "public.heic",
        );
        let tmp = TempDir::new().unwrap();
        let dl = tmp.path().join("photos");
        std::fs::create_dir_all(&dl).unwrap();
        let mut config = base_config(&dl);
        config.file_match_policy = FileMatchPolicy::NameId7;

        // expected_paths_for must produce a name-id7-suffixed filename.
        let expected = expected_paths_for(&asset.to_photo_asset(), &config);
        assert_eq!(expected.len(), 1);
        let fname = expected[0].path.file_name().unwrap().to_str().unwrap();
        assert!(
            fname.contains('_') && fname != "IMG_4726.HEIC",
            "name-id7 must inject a record-derived suffix, got: {fname}"
        );
        stage_file(&expected[0].path, expected[0].size);

        let db = open_db(&tmp).await;
        let stats = run_import(&server, &[asset], db.as_ref(), &config, false).await;

        assert_eq!(stats.matched, 1);
        let rows = all_downloaded(db.as_ref()).await;
        assert_eq!(rows.len(), 1);
        assert_eq!(&*rows[0].filename, fname);
    }

    /// If `file_match_policy` defaults are used (NameSizeDedupWithSuffix),
    /// a name-id7-suffixed file on disk should NOT match -- guards against
    /// the inverse of PR #294 (silently matching the wrong layout).
    #[tokio::test]
    async fn default_policy_does_not_match_name_id7_layout() {
        let server = crate::start_wiremock_or_skip!();
        let asset = WiremockAsset::new("REC456", "IMG_5000.HEIC", "public.heic").orig(
            2000,
            "ck_rec456",
            "public.heic",
        );
        let tmp = TempDir::new().unwrap();
        let dl = tmp.path().join("photos");
        std::fs::create_dir_all(&dl).unwrap();
        let config = base_config(&dl); // default = NameSizeDedupWithSuffix

        // Compute the name-id7 path via a parallel config and stage there.
        let mut id7_config = base_config(&dl);
        id7_config.file_match_policy = FileMatchPolicy::NameId7;
        let id7_paths = expected_paths_for(&asset.to_photo_asset(), &id7_config);
        for ep in &id7_paths {
            stage_file(&ep.path, ep.size);
        }

        let db = open_db(&tmp).await;
        let stats = run_import(&server, &[asset], db.as_ref(), &config, false).await;

        // Default policy looks for `IMG_5000.HEIC` directly, which we did
        // NOT stage; it must come up unmatched, not silently match the
        // _<id7>.HEIC file we did stage.
        assert_eq!(stats.matched, 0, "default policy must not match id7 layout");
        assert_eq!(stats.unmatched, 1);
    }

    // ── Tests: live photos ────────────────────────────────────────────

    #[tokio::test]
    async fn live_photo_both_matches_image_and_mov() {
        let server = crate::start_wiremock_or_skip!();
        let asset = WiremockAsset::new("LIVE1", "IMG_0100.HEIC", "public.heic")
            .orig(3000, "ck_live1", "public.heic")
            .live_mov(2000, "ck_live1_mov");
        let tmp = TempDir::new().unwrap();
        let dl = tmp.path().join("photos");
        std::fs::create_dir_all(&dl).unwrap();
        let config = base_config(&dl); // default LivePhotoMode::Both

        stage_expected(&asset.to_photo_asset(), &config);

        let db = open_db(&tmp).await;
        let stats = run_import(&server, &[asset], db.as_ref(), &config, false).await;

        // Live photo with mode=Both should produce 2 versions: HEIC + MOV.
        assert_eq!(stats.matched, 2, "image + MOV both match");

        let rows = all_downloaded(db.as_ref()).await;
        assert_eq!(rows.len(), 2);
        // One row has the HEIC filename, the other has the MOV filename.
        let filenames: Vec<&str> = rows.iter().map(|r| &*r.filename).collect();
        assert!(filenames.iter().any(|f| f.ends_with(".HEIC")));
        assert!(filenames.iter().any(|f| f.ends_with(".MOV")));
    }

    /// `LivePhotoMode::Skip` is "skip live photos entirely (both image and
    /// MOV)" per the type doc. The asset is still enumerated (so `total`
    /// ticks) but `expected_paths_for` returns empty, so neither matched
    /// nor unmatched moves and no DB rows are written.
    #[tokio::test]
    async fn live_photo_skip_drops_image_and_mov() {
        let server = crate::start_wiremock_or_skip!();
        let asset = WiremockAsset::new("LIVE2", "IMG_0200.HEIC", "public.heic")
            .orig(3000, "ck_live2", "public.heic")
            .live_mov(2000, "ck_live2_mov");
        let tmp = TempDir::new().unwrap();
        let dl = tmp.path().join("photos");
        std::fs::create_dir_all(&dl).unwrap();
        let mut config = base_config(&dl);
        config.live_photo_mode = LivePhotoMode::Skip;

        // Deliberately stage NOTHING: Skip means sync wouldn't have written
        // either file. If import-existing later starts emitting paths under
        // Skip again, this test would still pass (nothing on disk → both
        // counters stay 0), so we additionally verify no DB rows.

        let db = open_db(&tmp).await;
        let stats = run_import(&server, &[asset], db.as_ref(), &config, false).await;

        assert_eq!(stats.total, 1, "asset still enumerated");
        assert_eq!(stats.matched, 0, "Skip drops both image and MOV");
        assert_eq!(
            stats.unmatched, 0,
            "no path is attempted under Skip, so unmatched stays 0",
        );
        assert_eq!(
            stats.filtered, 1,
            "empty expected paths must tick the filtered counter",
        );
        assert_eq!(stats.hash_errors, 0);
        assert!(
            all_downloaded(db.as_ref()).await.is_empty(),
            "no DB rows for skipped live photo"
        );
    }

    #[tokio::test]
    async fn live_photo_video_only_drops_image() {
        let server = crate::start_wiremock_or_skip!();
        let asset = WiremockAsset::new("LIVE3", "IMG_0300.HEIC", "public.heic")
            .orig(3000, "ck_live3", "public.heic")
            .live_mov(2000, "ck_live3_mov");
        let tmp = TempDir::new().unwrap();
        let dl = tmp.path().join("photos");
        std::fs::create_dir_all(&dl).unwrap();
        let mut config = base_config(&dl);
        config.live_photo_mode = LivePhotoMode::VideoOnly;

        stage_expected(&asset.to_photo_asset(), &config);

        let db = open_db(&tmp).await;
        let stats = run_import(&server, &[asset], db.as_ref(), &config, false).await;

        assert_eq!(stats.matched, 1);
        let rows = all_downloaded(db.as_ref()).await;
        assert!(rows[0].filename.ends_with(".MOV"));
    }

    #[tokio::test]
    async fn import_matches_live_photo_motion_from_size_suffixed_primary_stem() {
        let server = crate::start_wiremock_or_skip!();
        let asset = WiremockAsset::new("LIVE6", "IMG_0600.HEIC", "public.heic")
            .orig(3000, "ck_live6", "public.heic")
            .live_mov(2000, "ck_live6_mov");
        let tmp = TempDir::new().unwrap();
        let dl = tmp.path().join("photos");
        std::fs::create_dir_all(&dl).unwrap();
        let config = base_config(&dl);

        let expected = expected_paths_for(&asset.to_photo_asset(), &config);
        let primary = expected
            .iter()
            .find(|e| e.version_size == VersionSizeKey::Original)
            .expect("primary path");
        let mov = expected
            .iter()
            .find(|e| e.version_size == VersionSizeKey::LiveOriginal)
            .expect("MOV path");
        let primary_name = primary.path.file_name().and_then(|f| f.to_str()).unwrap();
        let primary_collision =
            primary
                .path
                .with_file_name(crate::download::paths::add_dedup_suffix(
                    primary_name,
                    primary.size,
                ));
        let mov_collision_base = crate::download::paths::live_photo_mov_path_suffix(
            primary_collision
                .file_name()
                .and_then(|f| f.to_str())
                .unwrap(),
        );
        let mov_collision = mov
            .path
            .with_file_name(crate::download::paths::insert_suffix(
                &mov_collision_base,
                "LIVE6-2",
            ));
        stage_file(&primary_collision, primary.size);
        stage_file(&mov_collision, mov.size);

        let db = open_db(&tmp).await;
        let stats = run_import(&server, &[asset], db.as_ref(), &config, false).await;

        assert_eq!(stats.matched, 2);
        let rows = all_downloaded(db.as_ref()).await;
        assert!(
            rows.iter()
                .any(|r| r.filename.as_ref() == "IMG_0600-3000.HEIC")
        );
        assert!(
            rows.iter()
                .any(|r| r.filename.as_ref() == "IMG_0600-3000_HEVC-LIVE6-2.MOV")
        );
    }

    #[tokio::test]
    async fn import_matches_live_photo_motion_from_sanitized_identity_suffixed_primary_stem() {
        let server = crate::start_wiremock_or_skip!();
        let asset = WiremockAsset::new("LIVE/SLASH6", "IMG_0610.HEIC", "public.heic")
            .orig(3000, "ck_live_slash6", "public.heic")
            .live_mov(2000, "ck_live_slash6_mov");
        let tmp = TempDir::new().unwrap();
        let dl = tmp.path().join("photos");
        std::fs::create_dir_all(&dl).unwrap();
        let config = base_config(&dl);

        let expected = expected_paths_for(&asset.to_photo_asset(), &config);
        let primary = expected
            .iter()
            .find(|e| e.version_size == VersionSizeKey::Original)
            .expect("primary path");
        let mov = expected
            .iter()
            .find(|e| e.version_size == VersionSizeKey::LiveOriginal)
            .expect("MOV path");
        let primary_name = primary.path.file_name().and_then(|f| f.to_str()).unwrap();
        let primary_collision =
            primary
                .path
                .with_file_name(crate::download::paths::insert_asset_identity_suffix(
                    primary_name,
                    "LIVE/SLASH6",
                ));
        let mov_collision_base = crate::download::paths::live_photo_mov_path_suffix(
            primary_collision
                .file_name()
                .and_then(|f| f.to_str())
                .unwrap(),
        );
        let mov_collision = mov.path.with_file_name(
            crate::download::paths::insert_asset_identity_ordinal_suffix(
                &mov_collision_base,
                "LIVE/SLASH6",
                22,
            ),
        );
        stage_file(&primary_collision, primary.size);
        stage_file(&mov_collision, mov.size);

        let db = open_db(&tmp).await;
        let stats = run_import(&server, &[asset], db.as_ref(), &config, false).await;

        assert_eq!(stats.matched, 2);
        let rows = all_downloaded(db.as_ref()).await;
        assert!(
            rows.iter()
                .any(|r| r.filename.as_ref() == "IMG_0610-LIVE_SLASH6.HEIC")
        );
        assert!(
            rows.iter()
                .any(|r| r.filename.as_ref() == "IMG_0610-LIVE_SLASH6_HEVC-LIVE_SLASH6-22.MOV")
        );
    }

    #[tokio::test]
    async fn live_photo_mov_filename_policy_original_preserves_base_name() {
        let server = crate::start_wiremock_or_skip!();
        let asset = WiremockAsset::new("LIVE4", "IMG_0400.HEIC", "public.heic")
            .orig(3000, "ck_live4", "public.heic")
            .live_mov(2000, "ck_live4_mov");
        let tmp = TempDir::new().unwrap();
        let dl = tmp.path().join("photos");
        std::fs::create_dir_all(&dl).unwrap();
        let mut config = base_config(&dl);
        config.live_photo_mov_filename_policy = LivePhotoMovFilenamePolicy::Original;

        let expected = expected_paths_for(&asset.to_photo_asset(), &config);
        let mov_path = expected
            .iter()
            .find(|e| e.path.extension().and_then(|s| s.to_str()) == Some("MOV"))
            .expect("MOV path");
        let mov_filename = mov_path.path.file_name().unwrap().to_str().unwrap();
        assert_eq!(
            mov_filename, "IMG_0400.MOV",
            "Original policy keeps the base filename (no _HEVC suffix)"
        );

        stage_expected(&asset.to_photo_asset(), &config);
        let db = open_db(&tmp).await;
        let stats = run_import(&server, &[asset], db.as_ref(), &config, false).await;
        assert_eq!(stats.matched, 2);
    }

    #[tokio::test]
    async fn live_photo_mov_filename_policy_suffix_appends_hevc() {
        let server = crate::start_wiremock_or_skip!();
        let asset = WiremockAsset::new("LIVE5", "IMG_0500.HEIC", "public.heic")
            .orig(3000, "ck_live5", "public.heic")
            .live_mov(2000, "ck_live5_mov");
        let tmp = TempDir::new().unwrap();
        let dl = tmp.path().join("photos");
        std::fs::create_dir_all(&dl).unwrap();
        let config = base_config(&dl); // default Suffix policy

        let expected = expected_paths_for(&asset.to_photo_asset(), &config);
        let mov_path = expected
            .iter()
            .find(|e| e.path.extension().and_then(|s| s.to_str()) == Some("MOV"))
            .expect("MOV path");
        let mov_filename = mov_path.path.file_name().unwrap().to_str().unwrap();
        assert!(
            mov_filename.contains("_HEVC"),
            "Suffix policy adds _HEVC, got: {mov_filename}"
        );
        // Stage + run end-to-end to confirm the matching loop also lands on
        // the _HEVC.MOV file and writes a Live row.
        stage_expected(&asset.to_photo_asset(), &config);
        let db = open_db(&tmp).await;
        let stats = run_import(&server, &[asset], db.as_ref(), &config, false).await;
        assert_eq!(stats.matched, 2);
        let rows = all_downloaded(db.as_ref()).await;
        assert!(rows.iter().any(|r| r.filename.contains("_HEVC")));
    }

    // ── Tests: dry-run ────────────────────────────────────────────────

    #[tokio::test]
    async fn dry_run_counts_matches_without_writing_db() {
        let server = crate::start_wiremock_or_skip!();
        let asset = WiremockAsset::new("DRY1", "IMG_0001.JPG", "public.jpeg").orig(
            1000,
            "ck_dry1",
            "public.jpeg",
        );
        let tmp = TempDir::new().unwrap();
        let dl = tmp.path().join("photos");
        std::fs::create_dir_all(&dl).unwrap();
        let config = base_config(&dl);
        stage_expected(&asset.to_photo_asset(), &config);

        let db = open_db(&tmp).await;
        let stats = run_import(&server, &[asset], db.as_ref(), &config, true).await;

        assert_eq!(stats.matched, 1, "match counter ticks even in dry-run");
        assert!(
            all_downloaded(db.as_ref()).await.is_empty(),
            "dry-run must not write rows"
        );
    }

    #[tokio::test]
    async fn same_name_same_size_different_content_adopts_without_strict() {
        let server = crate::start_wiremock_or_skip!();
        let asset = WiremockAsset::new("STRICT1", "IMG_0001.JPG", "public.jpeg").orig(
            4,
            "ck_s1",
            "public.jpeg",
        );
        let tmp = TempDir::new().unwrap();
        let dl = tmp.path().join("photos");
        std::fs::create_dir_all(&dl).unwrap();
        let config = base_config(&dl);
        let expected = expected_paths_for(&asset.to_photo_asset(), &config);
        assert_eq!(expected.len(), 1);
        stage_file_with_bytes(&expected[0].path, b"bbbb");

        let db = open_db(&tmp).await;
        let stats = run_import(&server, &[asset], db.as_ref(), &config, false).await;

        assert_eq!(stats.matched, 1);
        assert_eq!(stats.strict_refused, 0);
        assert_eq!(all_downloaded(db.as_ref()).await.len(), 1);
    }

    #[tokio::test]
    async fn same_name_same_size_different_content_refuses_with_strict() {
        let server = crate::start_wiremock_or_skip!();
        let asset = WiremockAsset::new("STRICT2", "IMG_0002.JPG", "public.jpeg").orig(
            4,
            "ck_s2",
            "public.jpeg",
        );
        let tmp = TempDir::new().unwrap();
        let dl = tmp.path().join("photos");
        std::fs::create_dir_all(&dl).unwrap();
        let config = base_config(&dl);
        let expected = expected_paths_for(&asset.to_photo_asset(), &config);
        assert_eq!(expected.len(), 1);
        stage_file_with_bytes(&expected[0].path, b"bbbb");
        let verifier = TestStrictVerifier {
            cloud_prefix: b"aaaa".to_vec(),
        };

        let db = open_db(&tmp).await;
        let stats = run_import_with_strict(
            &server,
            &[asset],
            db.as_ref(),
            &config,
            false,
            Some(&verifier),
        )
        .await;

        assert_eq!(stats.matched, 0);
        assert_eq!(stats.strict_refused, 1);
        assert!(all_downloaded(db.as_ref()).await.is_empty());
    }

    #[tokio::test]
    async fn dry_run_strict_reports_refusal_without_writing_db() {
        let server = crate::start_wiremock_or_skip!();
        let asset = WiremockAsset::new("STRICT3", "IMG_0003.JPG", "public.jpeg").orig(
            4,
            "ck_s3",
            "public.jpeg",
        );
        let tmp = TempDir::new().unwrap();
        let dl = tmp.path().join("photos");
        std::fs::create_dir_all(&dl).unwrap();
        let config = base_config(&dl);
        let expected = expected_paths_for(&asset.to_photo_asset(), &config);
        assert_eq!(expected.len(), 1);
        stage_file_with_bytes(&expected[0].path, b"bbbb");
        let verifier = TestStrictVerifier {
            cloud_prefix: b"aaaa".to_vec(),
        };

        let db = open_db(&tmp).await;
        let stats = run_import_with_strict(
            &server,
            &[asset],
            db.as_ref(),
            &config,
            true,
            Some(&verifier),
        )
        .await;

        assert_eq!(stats.matched, 0);
        assert_eq!(stats.strict_refused, 1);
        assert!(all_downloaded(db.as_ref()).await.is_empty());
    }

    #[tokio::test]
    async fn http_strict_prefix_verifier_fetches_range_and_accepts_match() {
        let server = crate::start_wiremock_or_skip!();
        Mock::given(wm_method("GET"))
            .and(wm_path("/asset"))
            .and(header("Range", "bytes=0-3"))
            .respond_with(ResponseTemplate::new(206).set_body_bytes(b"abcd".to_vec()))
            .mount(&server)
            .await;

        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("IMG_0004.JPG");
        stage_file_with_bytes(&path, b"abcd");

        let decision = verify_strict_prefix(
            &reqwest::Client::new(),
            &path,
            &format!("{}/asset", server.uri()),
            4,
        )
        .await
        .expect("strict prefix verify");

        assert_eq!(decision, StrictImportDecision::Accepted);
    }

    // ── Tests: idempotency ────────────────────────────────────────────

    #[tokio::test]
    async fn idempotent_re_run_keeps_db_consistent() {
        let server1 = crate::start_wiremock_or_skip!();
        let server2 = crate::start_wiremock_or_skip!();
        let asset = WiremockAsset::new("IDEM1", "IMG_0001.JPG", "public.jpeg").orig(
            1000,
            "ck_idem1",
            "public.jpeg",
        );
        let tmp = TempDir::new().unwrap();
        let dl = tmp.path().join("photos");
        std::fs::create_dir_all(&dl).unwrap();
        let config = base_config(&dl);
        stage_expected(&asset.to_photo_asset(), &config);

        let db = open_db(&tmp).await;
        let stats1 = run_import(
            &server1,
            std::slice::from_ref(&asset),
            db.as_ref(),
            &config,
            false,
        )
        .await;
        assert_eq!(stats1.matched, 1);

        let stats2 = run_import(&server2, &[asset], db.as_ref(), &config, false).await;
        // Second run finds the same asset on disk, re-counts matched.
        // The DB row is upserted (no duplicate row).
        assert_eq!(stats2.matched, 1);

        let rows = all_downloaded(db.as_ref()).await;
        assert_eq!(rows.len(), 1, "no duplicate rows");
    }

    // ── Tests: size selection ─────────────────────────────────────────

    #[tokio::test]
    async fn force_resolution_unchecked_falls_back_when_size_missing() {
        // Asset has only Original; user requests Medium with force_resolution=false
        // (the default fallback policy: pick what exists).
        let server = crate::start_wiremock_or_skip!();
        let asset = WiremockAsset::new("FS1", "IMG_0001.JPG", "public.jpeg").orig(
            1000,
            "ck_fs1",
            "public.jpeg",
        );
        let tmp = TempDir::new().unwrap();
        let dl = tmp.path().join("photos");
        std::fs::create_dir_all(&dl).unwrap();
        let mut config = base_config(&dl);
        config.resolution = crate::types::PhotoResolution::Medium;
        config.force_resolution = false;
        stage_expected(&asset.to_photo_asset(), &config);

        let db = open_db(&tmp).await;
        let stats = run_import(&server, &[asset], db.as_ref(), &config, false).await;
        assert_eq!(stats.matched, 1);
        let rows = all_downloaded(db.as_ref()).await;
        // Fell back to Original since Medium wasn't published.
        assert_eq!(rows[0].version_size, VersionSizeKey::Original);
    }

    #[tokio::test]
    async fn force_resolution_strict_skips_when_size_missing() {
        let server = crate::start_wiremock_or_skip!();
        let asset = WiremockAsset::new("FS2", "IMG_0002.JPG", "public.jpeg").orig(
            1000,
            "ck_fs2",
            "public.jpeg",
        );
        let tmp = TempDir::new().unwrap();
        let dl = tmp.path().join("photos");
        std::fs::create_dir_all(&dl).unwrap();
        let mut config = base_config(&dl);
        config.resolution = crate::types::PhotoResolution::Medium;
        config.force_resolution = true;

        let db = open_db(&tmp).await;
        let stats = run_import(&server, &[asset], db.as_ref(), &config, false).await;
        assert_eq!(stats.total, 1);
        assert_eq!(stats.matched, 0, "force_resolution strict must skip");
        assert_eq!(stats.unmatched, 0);
        assert_eq!(
            stats.filtered, 1,
            "force_resolution + missing size produces empty expected paths -> filtered",
        );
    }

    // ── Tests: pagination + EOF ───────────────────────────────────────

    #[tokio::test]
    async fn matches_multiple_assets_in_one_page() {
        let server = crate::start_wiremock_or_skip!();
        let assets: Vec<WiremockAsset> = (0_u64..5)
            .map(|i| {
                let rec = format!("M{i}");
                let fname = format!("IMG_{i:04}.JPG");
                let ck = format!("ck_m{i}");
                WiremockAsset::new(&rec, &fname, "public.jpeg").orig(1000 + i, &ck, "public.jpeg")
            })
            .collect();

        let tmp = TempDir::new().unwrap();
        let dl = tmp.path().join("photos");
        std::fs::create_dir_all(&dl).unwrap();
        let config = base_config(&dl);
        for a in &assets {
            stage_expected(&a.to_photo_asset(), &config);
        }

        let db = open_db(&tmp).await;
        let stats = run_import(&server, &assets, db.as_ref(), &config, false).await;
        assert_eq!(stats.total, 5);
        assert_eq!(stats.matched, 5);
        assert_eq!(all_downloaded(db.as_ref()).await.len(), 5);
    }

    // ── Tests: folder structure ───────────────────────────────────────

    #[tokio::test]
    async fn flat_folder_structure_no_date_subdirs() {
        let server = crate::start_wiremock_or_skip!();
        let asset = WiremockAsset::new("FLAT1", "IMG_FLAT.JPG", "public.jpeg").orig(
            500,
            "ck_flat1",
            "public.jpeg",
        );
        let tmp = TempDir::new().unwrap();
        let dl = tmp.path().join("photos");
        std::fs::create_dir_all(&dl).unwrap();
        let mut config = base_config(&dl);
        config.folder_structure = "none".to_string();

        let expected = expected_paths_for(&asset.to_photo_asset(), &config);
        assert_eq!(expected.len(), 1);
        // With folder_structure=none, the file lives directly under
        // the download dir (no Y/m/d subdirs).
        let parent = expected[0].path.parent().unwrap();
        assert_eq!(parent, dl.as_path(), "flat layout: file in download dir");
        stage_file(&expected[0].path, expected[0].size);

        let db = open_db(&tmp).await;
        let stats = run_import(&server, &[asset], db.as_ref(), &config, false).await;
        assert_eq!(stats.matched, 1);
    }

    // ── Tests: RAW alignment ──────────────────────────────────────────

    /// Apple's typical RAW arrangement: Original=JPEG (processed), Alt=RAW.
    /// `raw_policy=PreferRaw` swaps so the RAW Alt becomes the primary,
    /// matching what a user who wants "the actual original RAW" expects.
    #[tokio::test]
    async fn raw_policy_prefer_raw_swaps_to_raw() {
        let server = crate::start_wiremock_or_skip!();
        let asset = WiremockAsset::new("RAW1", "IMG_RAW.JPG", "public.jpeg")
            .orig(2000, "ck_raw1_jpg", "public.jpeg")
            .alt(8000, "ck_raw1_dng", "com.adobe.raw-image");
        let tmp = TempDir::new().unwrap();
        let dl = tmp.path().join("photos");
        std::fs::create_dir_all(&dl).unwrap();
        let mut config = base_config(&dl);
        config.raw_policy = RawPolicy::PreferRaw;

        // Stage every path the policy chose. With PreferRaw swapping
        // RAW↔JPEG, we expect a non-.JPG filename for at least one row.
        stage_expected(&asset.to_photo_asset(), &config);

        let db = open_db(&tmp).await;
        let stats = run_import(&server, &[asset], db.as_ref(), &config, false).await;
        assert!(stats.matched >= 1, "RAW path matched");
        let rows = all_downloaded(db.as_ref()).await;
        assert!(
            rows.iter().any(|r| !r.filename.ends_with(".JPG")),
            "PreferRaw: at least one row should use a non-JPG (RAW) extension, got {:?}",
            rows.iter().map(|r| &r.filename).collect::<Vec<_>>()
        );
    }

    /// Same fixture with `raw_policy=AsIs` (default) keeps the
    /// JPEG as primary even though a RAW alternative exists.
    #[tokio::test]
    async fn raw_policy_unchanged_keeps_jpeg_primary() {
        let server = crate::start_wiremock_or_skip!();
        let asset = WiremockAsset::new("RAW2", "IMG_RAW2.JPG", "public.jpeg")
            .orig(2000, "ck_raw2_jpg", "public.jpeg")
            .alt(8000, "ck_raw2_dng", "com.adobe.raw-image");
        let tmp = TempDir::new().unwrap();
        let dl = tmp.path().join("photos");
        std::fs::create_dir_all(&dl).unwrap();
        let config = base_config(&dl); // default: AsIs

        stage_expected(&asset.to_photo_asset(), &config);
        let db = open_db(&tmp).await;
        let stats = run_import(&server, &[asset], db.as_ref(), &config, false).await;
        assert!(stats.matched >= 1);
        let rows = all_downloaded(db.as_ref()).await;
        assert!(
            rows.iter().any(|r| r.filename.ends_with(".JPG")),
            "AsIs: JPEG primary, got {:?}",
            rows.iter().map(|r| &r.filename).collect::<Vec<_>>()
        );
    }

    // ── Tests: keep_unicode_in_filenames ──────────────────────────────

    #[tokio::test]
    async fn keep_unicode_preserves_non_ascii_filename() {
        let server = crate::start_wiremock_or_skip!();
        let asset = WiremockAsset::new("UNI1", "Café_München.JPG", "public.jpeg").orig(
            800,
            "ck_uni1",
            "public.jpeg",
        );
        let tmp = TempDir::new().unwrap();
        let dl = tmp.path().join("photos");
        std::fs::create_dir_all(&dl).unwrap();
        let mut config = base_config(&dl);
        config.keep_unicode_in_filenames = true;

        let expected = expected_paths_for(&asset.to_photo_asset(), &config);
        let fname = expected[0].path.file_name().unwrap().to_str().unwrap();
        assert!(
            fname.contains("Café") || fname.contains("München"),
            "unicode preserved with keep_unicode=true, got {fname}"
        );
        stage_file(&expected[0].path, expected[0].size);

        let db = open_db(&tmp).await;
        let stats = run_import(&server, &[asset], db.as_ref(), &config, false).await;
        assert_eq!(stats.matched, 1);
    }

    #[tokio::test]
    async fn strip_unicode_drops_non_ascii_filename() {
        let server = crate::start_wiremock_or_skip!();
        let asset = WiremockAsset::new("UNI2", "Café_München.JPG", "public.jpeg").orig(
            800,
            "ck_uni2",
            "public.jpeg",
        );
        let tmp = TempDir::new().unwrap();
        let dl = tmp.path().join("photos");
        std::fs::create_dir_all(&dl).unwrap();
        let config = base_config(&dl); // default keep_unicode=false

        let expected = expected_paths_for(&asset.to_photo_asset(), &config);
        let fname = expected[0].path.file_name().unwrap().to_str().unwrap();
        assert!(
            !fname.contains("Café") && !fname.contains("München"),
            "non-ASCII chars stripped with keep_unicode=false, got {fname}"
        );
        stage_file(&expected[0].path, expected[0].size);

        let db = open_db(&tmp).await;
        let stats = run_import(&server, &[asset], db.as_ref(), &config, false).await;
        assert_eq!(stats.matched, 1);
    }

    // ── Tests: media filters ──────────────────────────────────────────

    /// Media filtering happens via `is_asset_filtered`, which
    /// `import_assets` now invokes upstream of `expected_paths_for`. The
    /// previous incarnation of this test asserted `matched == 0` without
    /// staging a file, so it would have passed even if the gate were
    /// missing entirely (no file → no match for an unrelated reason). Stage
    /// the file the matcher would land on AND verify `is_asset_filtered`
    /// classifies it as a media skip; if the gate disappears, `matched`
    /// becomes 1 and `unmatched` stays 0, failing this test loudly.
    #[tokio::test]
    async fn skip_videos_excludes_movie_assets() {
        let server = crate::start_wiremock_or_skip!();
        let tmp = TempDir::new().unwrap();
        let dl = tmp.path().join("photos");
        std::fs::create_dir_all(&dl).unwrap();
        let mut config = base_config(&dl);
        config.media.videos = false;

        let asset = WiremockAsset::new("VID1", "MOV_0001.MOV", "com.apple.quicktime-movie").orig(
            5000,
            "ck_vid1",
            "com.apple.quicktime-movie",
        );

        // Stage the file at the path expected_paths_for *would* emit if the
        // gate were missing. With all media enabled, this would match.
        let probe_config = base_config(&dl);
        let expected_if_unfiltered = expected_paths_for(&asset.to_photo_asset(), &probe_config);
        assert_eq!(
            expected_if_unfiltered.len(),
            1,
            "probe: video should have one expected path when media is unrestricted",
        );
        stage_file(
            &expected_if_unfiltered[0].path,
            expected_if_unfiltered[0].size,
        );

        // is_asset_filtered must classify a movie excluded by media as filtered.
        assert!(
            crate::download::filter::is_asset_filtered(&asset.to_photo_asset(), &config).is_some(),
            "media filter must drop movie assets via is_asset_filtered",
        );

        let db = open_db(&tmp).await;
        let stats = run_import(&server, &[asset], db.as_ref(), &config, false).await;

        assert_eq!(stats.total, 1, "asset still enumerated");
        assert_eq!(stats.matched, 0, "media filter must drop the movie");
        assert_eq!(
            stats.unmatched, 0,
            "filter happens before path derivation; unmatched stays 0",
        );
        assert_eq!(stats.filtered, 1);
    }

    // ── Gap-coverage tests ────────────────────────────────────────────
    //
    // Three scenarios surfaced during PR review that the broader suite
    // didn't pin down:
    //   1. compute_sha256 failure leaving an orphan pending row in the DB
    //   2. LivePhotoMode::Skip emitting no path end-to-end
    //   3. is_asset_filtered actually being honored by import_assets

    /// Pre-fix, `import_assets` did `upsert_seen` (creates a `pending` row)
    /// BEFORE `compute_sha256`, so an unreadable file left an orphan
    /// pending row that future syncs would silently skip. The hash is now
    /// computed first; this test pins the no-orphan invariant.
    #[cfg(unix)]
    #[tokio::test]
    async fn no_orphan_pending_row_when_compute_sha256_fails() {
        use std::os::unix::fs::PermissionsExt;

        let server = crate::start_wiremock_or_skip!();
        let asset = WiremockAsset::new("HASHFAIL", "IMG_HF.JPG", "public.jpeg").orig(
            1234,
            "ck_hf",
            "public.jpeg",
        );
        let tmp = TempDir::new().unwrap();
        let dl = tmp.path().join("photos");
        std::fs::create_dir_all(&dl).unwrap();
        let config = base_config(&dl);

        // Stage the file the matcher will land on, then strip read perms
        // so File::open in compute_sha256 fails with EACCES.
        let expected = expected_paths_for(&asset.to_photo_asset(), &config);
        assert_eq!(expected.len(), 1);
        stage_file(&expected[0].path, expected[0].size);
        std::fs::set_permissions(&expected[0].path, std::fs::Permissions::from_mode(0o000))
            .expect("chmod 000");

        let db = open_db(&tmp).await;
        let stats = run_import(&server, &[asset], db.as_ref(), &config, false).await;

        // Restore perms so TempDir cleanup can drop the file.
        let _ = std::fs::set_permissions(&expected[0].path, std::fs::Permissions::from_mode(0o644));

        // metadata.len matched, so resolve_match_path returned Some. But
        // compute_sha256 failed, so we bailed before upsert_seen.
        assert_eq!(stats.total, 1);
        assert_eq!(
            stats.matched, 0,
            "hash failure means we did not record a match"
        );
        assert_eq!(
            stats.unmatched, 0,
            "the file *was* found, just not hashable"
        );
        assert_eq!(stats.filtered, 0);
        assert_eq!(
            stats.hash_errors, 1,
            "compute_sha256 failure must tick the hash_errors counter",
        );
        assert!(
            all_downloaded(db.as_ref()).await.is_empty(),
            "no downloaded rows",
        );
        let summary = db.get_summary().await.expect("summary");
        assert_eq!(
            summary.total_assets, 0,
            "no orphan pending row left behind by failed hash; summary={summary:?}",
        );
    }

    /// End-to-end pin for the LivePhotoMode::Skip semantic across the
    /// whole `import_assets` pipeline: enumerated, but neither matched
    /// nor unmatched, with no DB writes -- distinct from
    /// `live_photo_skip_drops_image_and_mov` above which is the same
    /// shape but explicit about why.
    #[tokio::test]
    async fn live_photo_skip_emits_no_path_end_to_end() {
        let server = crate::start_wiremock_or_skip!();
        let live = WiremockAsset::new("SKIPLIVE", "IMG_SL.HEIC", "public.heic")
            .orig(1000, "ck_sl", "public.heic")
            .live_mov(2000, "ck_sl_mov");
        let still = WiremockAsset::new("SKIPSTILL", "IMG_SS.JPG", "public.jpeg").orig(
            500,
            "ck_ss",
            "public.jpeg",
        );

        let tmp = TempDir::new().unwrap();
        let dl = tmp.path().join("photos");
        std::fs::create_dir_all(&dl).unwrap();
        let mut config = base_config(&dl);
        config.live_photo_mode = LivePhotoMode::Skip;

        // Stage the still photo's expected path (sync would have written
        // it under Skip; live photo files are NOT staged).
        stage_expected(&still.to_photo_asset(), &config);

        let db = open_db(&tmp).await;
        let stats = run_import(&server, &[live, still], db.as_ref(), &config, false).await;

        assert_eq!(stats.total, 2, "both assets enumerated");
        assert_eq!(stats.matched, 1, "still matched, live dropped entirely");
        assert_eq!(stats.unmatched, 0);
        assert_eq!(
            stats.filtered, 1,
            "live photo with empty expected paths must tick filtered",
        );
        assert_eq!(stats.hash_errors, 0);
        let rows = all_downloaded(db.as_ref()).await;
        assert_eq!(rows.len(), 1);
        assert!(
            rows[0].filename.ends_with(".JPG"),
            "the still, not the HEIC"
        );
    }

    /// Verifies `import_assets` actually invokes `is_asset_filtered`. We
    /// can't easily flip media selection through `build_import_path_config`,
    /// but `exclude_asset_ids` is honored by
    /// `is_asset_filtered` and we can set it directly on the test
    /// `PathDerivationSource`. If `import_assets` ever stops calling the gate,
    /// this test fails with `matched=1` instead of `matched=0`.
    #[tokio::test]
    async fn is_asset_filtered_blocks_excluded_asset_id() {
        let server = crate::start_wiremock_or_skip!();
        let asset = WiremockAsset::new("EXCL1", "IMG_EX.JPG", "public.jpeg").orig(
            1000,
            "ck_excl1",
            "public.jpeg",
        );
        let tmp = TempDir::new().unwrap();
        let dl = tmp.path().join("photos");
        std::fs::create_dir_all(&dl).unwrap();
        let mut config = base_config(&dl);

        // File is on disk and would match without the filter.
        stage_expected(&asset.to_photo_asset(), &config);

        // Add the asset's id to the exclude set.
        let mut excluded: FxHashSet<String> = FxHashSet::default();
        excluded.insert("EXCL1".to_string());
        config.exclude_asset_ids = Arc::new(excluded);

        let db = open_db(&tmp).await;
        let stats = run_import(&server, &[asset], db.as_ref(), &config, false).await;

        assert_eq!(stats.total, 1, "asset still enumerated");
        assert_eq!(
            stats.matched, 0,
            "is_asset_filtered must block the excluded id before path derivation"
        );
        assert_eq!(
            stats.unmatched, 0,
            "filter runs before resolve_match_path; unmatched stays 0",
        );
        assert_eq!(
            stats.filtered, 1,
            "is_asset_filtered must tick the filtered counter",
        );
        assert!(all_downloaded(db.as_ref()).await.is_empty());
    }

    // ── filter-then-no-hash: broader coverage ─────────────────────────
    //
    // `skip_videos_excludes_movie_assets` and
    // `is_asset_filtered_blocks_excluded_asset_id` already pin the gate
    // for two filter sources. The branch's commit "honor is_asset_filtered"
    // (05356d2) ties the import path to ALL filter sources, not just two.
    // These pin media / date / filename-exclude so a future change
    // that loses one of them surfaces here.
    //
    // Each test stages the file the matcher would land on without the
    // filter and asserts matched=0 + no DB rows -- proving the filter
    // ran *before* the path derivation + hash + upsert chain.

    #[tokio::test]
    async fn skip_photos_excludes_still_assets() {
        let server = crate::start_wiremock_or_skip!();
        let asset = WiremockAsset::new("STILL1", "IMG_S1.JPG", "public.jpeg").orig(
            1000,
            "ck_still1",
            "public.jpeg",
        );
        let tmp = TempDir::new().unwrap();
        let dl = tmp.path().join("photos");
        std::fs::create_dir_all(&dl).unwrap();
        let mut config = base_config(&dl);
        config.media.photos = false;
        stage_expected(&asset.to_photo_asset(), &base_config(&dl));

        let db = open_db(&tmp).await;
        let stats = run_import(&server, &[asset], db.as_ref(), &config, false).await;
        assert_eq!(stats.total, 1);
        assert_eq!(stats.matched, 0, "media filter must drop the still");
        assert_eq!(stats.unmatched, 0, "filter fires before resolve_match_path");
        assert_eq!(stats.filtered, 1);
        assert!(all_downloaded(db.as_ref()).await.is_empty());
    }

    #[tokio::test]
    async fn date_filter_excludes_assets_outside_window() {
        // skip_created_before set to mid-2025 drops anything older.
        // The default WiremockAsset asset_date is Jan 14 2025, before
        // the cutoff.
        let server = crate::start_wiremock_or_skip!();
        let asset = WiremockAsset::new("OLD1", "IMG_OLD.JPG", "public.jpeg").orig(
            1000,
            "ck_old1",
            "public.jpeg",
        );
        let tmp = TempDir::new().unwrap();
        let dl = tmp.path().join("photos");
        std::fs::create_dir_all(&dl).unwrap();
        let mut config = base_config(&dl);
        // 2025-06-01 cutoff; asset is dated 2025-01-14, so it must be filtered.
        config.skip_created_before = chrono::DateTime::from_timestamp(1_748_736_000, 0)
            .map(crate::config::CreatedDateFilter::Instant);
        stage_expected(&asset.to_photo_asset(), &base_config(&dl));

        let db = open_db(&tmp).await;
        let stats = run_import(&server, &[asset], db.as_ref(), &config, false).await;
        assert_eq!(stats.total, 1);
        assert_eq!(
            stats.matched, 0,
            "date filter must drop the asset before hash"
        );
        assert_eq!(stats.unmatched, 0);
        assert_eq!(stats.filtered, 1);
        assert!(all_downloaded(db.as_ref()).await.is_empty());
    }

    #[tokio::test]
    async fn filename_exclude_glob_drops_matching_assets() {
        let server = crate::start_wiremock_or_skip!();
        let asset = WiremockAsset::new("BLOCK1", "IMG_BLOCK.JPG", "public.jpeg").orig(
            1000,
            "ck_block1",
            "public.jpeg",
        );
        let tmp = TempDir::new().unwrap();
        let dl = tmp.path().join("photos");
        std::fs::create_dir_all(&dl).unwrap();
        let mut config = base_config(&dl);
        let pattern = glob::Pattern::new("IMG_BLOCK.*").expect("compile glob");
        config.filename_exclude = Arc::from(vec![pattern]);
        stage_expected(&asset.to_photo_asset(), &base_config(&dl));

        let db = open_db(&tmp).await;
        let stats = run_import(&server, &[asset], db.as_ref(), &config, false).await;
        assert_eq!(stats.total, 1);
        assert_eq!(
            stats.matched, 0,
            "filename_exclude must drop the asset before hash"
        );
        assert_eq!(stats.unmatched, 0);
        assert_eq!(stats.filtered, 1);
        assert!(all_downloaded(db.as_ref()).await.is_empty());
    }

    // ── filesystem-edge coverage ──────────────────────────────────────
    //
    // The matcher reads `metadata.len()` and (on size match) `compute_sha256`.
    // A user library can contain symlinks, hardlinks, broken symlinks,
    // permission-denied directories, and unusual file types. These pin
    // kei's behavior on each edge so a future scan-tree refactor doesn't
    // silently change semantics on real-world libraries.
    //
    // We don't test races (file mutating mid-scan) because reproducing
    // them deterministically requires fault injection we don't have, and
    // a flaky test for a real correctness bug is worse than no test.

    /// A symlink to a same-size file at the expected path matches like the
    /// real file -- `metadata.len()` follows symlinks by default. Pins the
    /// "user reorganized via symlink" migration story.
    #[cfg(unix)]
    #[tokio::test]
    async fn symlink_to_real_file_at_expected_path_matches() {
        let server = crate::start_wiremock_or_skip!();
        let asset = WiremockAsset::new("SYM1", "IMG_SYM.JPG", "public.jpeg").orig(
            1234,
            "ck_sym1",
            "public.jpeg",
        );
        let tmp = TempDir::new().unwrap();
        let dl = tmp.path().join("photos");
        std::fs::create_dir_all(&dl).unwrap();
        let config = base_config(&dl);

        let real_dir = tmp.path().join("real");
        std::fs::create_dir_all(&real_dir).unwrap();
        let real = real_dir.join("IMG_SYM.JPG");
        stage_file(&real, 1234);

        let expected = expected_paths_for(&asset.to_photo_asset(), &config);
        assert_eq!(expected.len(), 1);
        if let Some(parent) = expected[0].path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::os::unix::fs::symlink(&real, &expected[0].path).expect("symlink");

        let db = open_db(&tmp).await;
        let stats = run_import(&server, &[asset], db.as_ref(), &config, false).await;
        assert_eq!(stats.total, 1);
        assert_eq!(
            stats.matched, 1,
            "symlink to a same-size file must read as a match"
        );
        assert_eq!(stats.unmatched, 0);
        assert_eq!(all_downloaded(db.as_ref()).await.len(), 1);
    }

    /// A broken (dangling) symlink at the expected path must NOT match,
    /// must NOT panic, and must NOT leave an orphan pending row in the
    /// state DB. Counts as one "unmatched" version since the resolve step
    /// fails to stat.
    #[cfg(unix)]
    #[tokio::test]
    async fn broken_symlink_at_expected_path_does_not_match() {
        let server = crate::start_wiremock_or_skip!();
        let asset = WiremockAsset::new("BROKEN1", "IMG_BR.JPG", "public.jpeg").orig(
            500,
            "ck_br1",
            "public.jpeg",
        );
        let tmp = TempDir::new().unwrap();
        let dl = tmp.path().join("photos");
        std::fs::create_dir_all(&dl).unwrap();
        let config = base_config(&dl);

        let expected = expected_paths_for(&asset.to_photo_asset(), &config);
        assert_eq!(expected.len(), 1);
        if let Some(parent) = expected[0].path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        let target = tmp.path().join("does_not_exist");
        std::os::unix::fs::symlink(&target, &expected[0].path).expect("dangling symlink");

        let db = open_db(&tmp).await;
        let stats = run_import(&server, &[asset], db.as_ref(), &config, false).await;
        assert_eq!(stats.total, 1);
        assert_eq!(
            stats.matched, 0,
            "broken symlink must not match (stat fails)"
        );
        assert_eq!(stats.unmatched, 1);
        assert!(
            all_downloaded(db.as_ref()).await.is_empty(),
            "no DB row for the broken symlink"
        );
    }

    /// Two hardlinks to the same content count as two distinct files for
    /// matching purposes -- import treats each `expected_paths_for` entry
    /// as its own match attempt, so a library with `IMG.JPG` linked from
    /// two albums should still produce two matches (one per asset_id +
    /// album combination). This pins the simpler "single-asset hardlinked
    /// at the expected path" case: the file matches normally regardless
    /// of link count.
    #[cfg(unix)]
    #[tokio::test]
    async fn hardlinked_file_at_expected_path_matches() {
        let server = crate::start_wiremock_or_skip!();
        let asset = WiremockAsset::new("HARD1", "IMG_H1.JPG", "public.jpeg").orig(
            900,
            "ck_h1",
            "public.jpeg",
        );
        let tmp = TempDir::new().unwrap();
        let dl = tmp.path().join("photos");
        std::fs::create_dir_all(&dl).unwrap();
        let config = base_config(&dl);

        // metadata.len follows the inode, so a hardlink is
        // indistinguishable from a regular file at this layer.
        let real_dir = tmp.path().join("orig");
        std::fs::create_dir_all(&real_dir).unwrap();
        let real = real_dir.join("IMG_H1.JPG");
        stage_file(&real, 900);

        let expected = expected_paths_for(&asset.to_photo_asset(), &config);
        assert_eq!(expected.len(), 1);
        if let Some(parent) = expected[0].path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::hard_link(&real, &expected[0].path).expect("hard_link");

        let db = open_db(&tmp).await;
        let stats = run_import(&server, &[asset], db.as_ref(), &config, false).await;
        assert_eq!(stats.matched, 1, "hardlinked file matches like a regular");
        assert_eq!(stats.unmatched, 0);
    }

    /// A file inside a permission-denied directory cannot be stat'd, so
    /// resolve_match_path returns None and the asset reads as unmatched
    /// (not a hard error). Pins that import-existing keeps going past
    /// EACCES rather than aborting the whole scan.
    #[cfg(unix)]
    #[tokio::test]
    async fn permission_denied_subdir_does_not_abort_scan() {
        use std::os::unix::fs::PermissionsExt;

        let server = crate::start_wiremock_or_skip!();
        let blocked = WiremockAsset::new("DENIED1", "IMG_D.JPG", "public.jpeg").orig(
            1000,
            "ck_denied1",
            "public.jpeg",
        );
        let tmp = TempDir::new().unwrap();
        let dl = tmp.path().join("photos");
        std::fs::create_dir_all(&dl).unwrap();
        let config = base_config(&dl);

        let blocked_paths = stage_expected(&blocked.to_photo_asset(), &config);
        let blocked_parent = blocked_paths[0].parent().expect("parent").to_path_buf();
        std::fs::set_permissions(&blocked_parent, std::fs::Permissions::from_mode(0o000))
            .expect("chmod 000");

        let db = open_db(&tmp).await;
        let stats = run_import(&server, &[blocked], db.as_ref(), &config, false).await;

        let _ = std::fs::set_permissions(&blocked_parent, std::fs::Permissions::from_mode(0o755));

        assert_eq!(stats.total, 1);
        // The invariant: the asset reaches a terminal state (no panic,
        // no abort) -- either matched or unmatched.
        assert_eq!(stats.matched + stats.unmatched, 1, "got {stats:?}");
        assert_eq!(
            all_downloaded(db.as_ref()).await.len(),
            stats.matched as usize,
        );
    }

    // ── cancellation / fetcher-panic propagation ──────────────────────
    //
    // import_assets accepts `panic_rx`, the receiver from
    // `photo_stream`'s panic guard. After draining, if the channel
    // signals true, the function bails -- otherwise a fetcher panic
    // would close the stream early and we'd report a partial scan as
    // clean. The wiremock end-to-end tests above never trigger this
    // path. These tests construct the channel manually.

    /// The happy path: the panic_rx sender is dropped without sending,
    /// matching `photo_stream`'s clean-exit signal. import_assets must
    /// return Ok with whatever stats accumulated.
    #[tokio::test]
    async fn import_assets_returns_ok_when_panic_rx_sender_dropped() {
        let tmp = TempDir::new().unwrap();
        let dl = tmp.path().join("photos");
        std::fs::create_dir_all(&dl).unwrap();
        let config = base_config(&dl);
        let db = open_db(&tmp).await;

        // Empty stream + dropped-sender panic_rx.
        let stream = futures_util::stream::empty::<anyhow::Result<PhotoAsset>>();
        let (tx, rx) = tokio::sync::oneshot::channel::<bool>();
        drop(tx);

        let mut dir_cache = DirCache::new();
        let stats = import_assets(
            stream,
            rx,
            db.as_ref(),
            &config,
            "test-all",
            &mut dir_cache,
            ImportRunOptions::default(),
        )
        .await
        .expect("clean exit must be Ok");
        assert_eq!(stats.total, 0);
        assert_eq!(stats.matched, 0);
        assert_eq!(stats.unmatched, 0);
    }

    /// Cancellation must make `import-existing` leave the scan loop instead
    /// of relying on default SIGINT process termination. That protects the
    /// SQLite state DB from being killed mid-write and lets a rerun recover
    /// from the last committed import rows.
    #[tokio::test]
    async fn import_assets_bails_promptly_when_shutdown_token_cancels() {
        let tmp = TempDir::new().unwrap();
        let dl = tmp.path().join("photos");
        std::fs::create_dir_all(&dl).unwrap();
        let config = base_config(&dl);
        let db = open_db(&tmp).await;

        let stream = futures_util::stream::pending::<anyhow::Result<PhotoAsset>>();
        let (_tx, rx) = tokio::sync::oneshot::channel::<bool>();
        let shutdown_token = tokio_util::sync::CancellationToken::new();
        shutdown_token.cancel();

        let mut dir_cache = DirCache::new();
        let err = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            import_assets(
                stream,
                rx,
                db.as_ref(),
                &config,
                "test-all",
                &mut dir_cache,
                ImportRunOptions {
                    shutdown_token: Some(&shutdown_token),
                    ..Default::default()
                },
            ),
        )
        .await
        .expect("shutdown cancellation must not hang")
        .expect_err("shutdown cancellation must abort the scan");

        let msg = format!("{err}");
        assert!(
            msg.contains("Import was interrupted while scanning library")
                && msg.contains("test-all"),
            "error message must name the shutdown and library label, got: {msg}",
        );
    }

    /// Fetcher panic: the panic_rx delivers `true`. import_assets must
    /// surface this as Err so callers don't report a partial scan as a
    /// clean enumeration -- the invariant the contract was added for.
    #[tokio::test]
    async fn import_assets_bails_when_panic_rx_signals_true() {
        let tmp = TempDir::new().unwrap();
        let dl = tmp.path().join("photos");
        std::fs::create_dir_all(&dl).unwrap();
        let config = base_config(&dl);
        let db = open_db(&tmp).await;

        let stream = futures_util::stream::empty::<anyhow::Result<PhotoAsset>>();
        let (tx, rx) = tokio::sync::oneshot::channel::<bool>();
        tx.send(true).expect("send panic signal");

        let mut dir_cache = DirCache::new();
        let err = import_assets(
            stream,
            rx,
            db.as_ref(),
            &config,
            "test-all",
            &mut dir_cache,
            ImportRunOptions::default(),
        )
        .await
        .expect_err("must bail on fetcher panic");
        let msg = format!("{err}");
        assert!(
            msg.contains("Import scan stopped") && msg.contains("test-all"),
            "error message must name the abort + label, got: {msg}"
        );
    }

    /// Stateful responder: serves a sequence of canned bodies, then HTTP
    /// 500 forever after. Lets a single-fetcher test simulate a partial
    /// enumeration that hits an upstream failure mid-scan.
    struct ScriptedPagesThenError {
        bodies: Vec<String>,
        counter: std::sync::atomic::AtomicUsize,
    }

    impl wiremock::Respond for ScriptedPagesThenError {
        fn respond(&self, _req: &wiremock::Request) -> ResponseTemplate {
            let n = self
                .counter
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            match self.bodies.get(n) {
                Some(body) => ResponseTemplate::new(200).set_body_string(body),
                None => ResponseTemplate::new(500).set_body_string("upstream error"),
            }
        }
    }

    /// A fetcher Err mid-enumeration must abort `import_assets` instead of
    /// being downgraded to a warning. Two successful pages followed by a
    /// 500 surface as `Err` after page 2; the bail must name the library
    /// and the fetcher error so operators can re-run after upstream clears.
    #[tokio::test]
    async fn import_assets_bails_when_fetcher_errs_mid_enumeration() {
        let tmp = TempDir::new().unwrap();
        let dl = tmp.path().join("photos");
        std::fs::create_dir_all(&dl).unwrap();
        let config = base_config(&dl);
        let db = open_db(&tmp).await;

        let asset1 = WiremockAsset::new("rec_a", "IMG_0001.JPG", "public.jpeg").orig(
            1024,
            "ck_a",
            "public.jpeg",
        );
        let asset2 = WiremockAsset::new("rec_b", "IMG_0002.JPG", "public.jpeg").orig(
            2048,
            "ck_b",
            "public.jpeg",
        );

        let server = crate::start_wiremock_or_skip!();
        Mock::given(wm_method("POST"))
            .and(wm_path("/records/query"))
            .respond_with(ScriptedPagesThenError {
                bodies: vec![
                    cloudkit_records_body(&[&asset1]),
                    cloudkit_records_body(&[&asset2]),
                ],
                counter: std::sync::atomic::AtomicUsize::new(0),
            })
            .mount(&server)
            .await;

        let album = album_pointed_at(&server);
        let (stream, panic_rx) = album.photo_stream(None, None, 1);
        let mut dir_cache = DirCache::new();
        let err = import_assets(
            stream,
            panic_rx,
            db.as_ref(),
            &config,
            "test-all",
            &mut dir_cache,
            ImportRunOptions {
                dry_run: true,
                ..Default::default()
            },
        )
        .await
        .expect_err("must bail on fetcher Err");
        let msg = format!("{err}");
        assert!(
            msg.contains("Import scan stopped") && msg.contains("test-all"),
            "error message must name the abort + library label, got: {msg}",
        );
        assert!(
            msg.contains("iCloud returned an error"),
            "error message must name the fetcher source, got: {msg}",
        );

        // Bail must short-circuit before any DB write so a re-run starts
        // clean.
        let downloaded = all_downloaded(db.as_ref()).await;
        assert!(
            downloaded.is_empty(),
            "no downloaded rows expected after bail, got {n}",
            n = downloaded.len(),
        );
    }

    // ── AM/PM whitespace variant probe ────────────────────────────────
    //
    // macOS uses NARROW NO-BREAK SPACE (U+202F) before AM/PM in default
    // screenshot filenames since macOS 13. A user whose tree was synced
    // via a third-party tool that normalized the filename to a regular
    // space (or the other way around) would otherwise see every such
    // photo come up unmatched on import. The DirCache-backed AM/PM probe
    // bridges both directions.

    /// Run an end-to-end import where the iCloud filename is `icloud`
    /// and the file actually staged is named `on_disk_filename` in the
    /// same parent directory. `keep_unicode_in_filenames=true` keeps
    /// the U+202F in the expected path; otherwise `remove_unicode_chars`
    /// would strip it before the AM/PM probe could fire.
    async fn assert_ampm_variant_adopted(icloud: &str, on_disk_filename: &str) {
        let server = crate::start_wiremock_or_skip!();
        let asset = WiremockAsset::new("AMPM_TEST", icloud, "public.png").orig(
            4096,
            "ck_ampm_test",
            "public.png",
        );

        let tmp = TempDir::new().unwrap();
        let dl = tmp.path().join("photos");
        std::fs::create_dir_all(&dl).unwrap();
        let mut config = base_config(&dl);
        config.keep_unicode_in_filenames = true;

        let expected = expected_paths_for(&asset.to_photo_asset(), &config);
        assert_eq!(
            expected.len(),
            1,
            "single-version asset must yield one path"
        );
        let parent = expected[0]
            .path
            .parent()
            .expect("expected path always has a parent");
        let on_disk = parent.join(on_disk_filename);
        stage_file(&on_disk, expected[0].size);

        let db = open_db(&tmp).await;
        let stats = run_import(&server, &[asset], db.as_ref(), &config, false).await;

        assert_eq!(stats.total, 1);
        assert_eq!(
            stats.matched, 1,
            "AM/PM probe must adopt the on-disk variant",
        );
        let rows = all_downloaded(db.as_ref()).await;
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].local_path.as_deref().map(StdPath::new),
            Some(on_disk.as_path()),
            "DB must record the variant path that was actually adopted",
        );
    }

    #[tokio::test]
    async fn ampm_variant_with_regular_space_on_disk_matches_nbsp_from_icloud() {
        assert_ampm_variant_adopted(
            "Screenshot 2025-01-14 at 1.40.01\u{202F}PM.PNG",
            "Screenshot 2025-01-14 at 1.40.01 PM.PNG",
        )
        .await;
    }

    #[tokio::test]
    async fn ampm_variant_with_nbsp_on_disk_matches_regular_space_from_icloud() {
        assert_ampm_variant_adopted(
            "Screenshot 2025-01-14 at 1.40.02 PM.PNG",
            "Screenshot 2025-01-14 at 1.40.02\u{202F}PM.PNG",
        )
        .await;
    }

    /// The `stage="scan_started"` marker is the synchronisation point
    /// the live SIGINT test polls for, so a regression that drops it
    /// (e.g. wrapping `import_assets` in an early-return) would silently
    /// break that test by reintroducing a sleep race. Pin its emission
    /// via `tracing_test`.
    #[tokio::test]
    async fn import_emits_scan_started_marker_with_library_label() {
        let (capture, _guard) = crate::test_helpers::TracingCapture::install();
        let tmp = TempDir::new().expect("tempdir");
        let server = crate::start_wiremock_or_skip!();
        let asset = WiremockAsset::new("S1", "IMG_SCAN.JPG", "public.jpeg").orig(
            512,
            "ck_s1",
            "public.jpeg",
        );
        stub_records_query(&server, &[asset]).await;
        let album = album_pointed_at(&server);
        let (stream, panic_rx) = album.photo_stream(None, None, 1);
        let db = open_db(&tmp).await;
        let config = base_config(tmp.path());
        let mut dir_cache = DirCache::new();
        import_assets(
            stream,
            panic_rx,
            db.as_ref(),
            &config,
            "PrimarySync",
            &mut dir_cache,
            ImportRunOptions {
                dry_run: true,
                ..Default::default()
            },
        )
        .await
        .expect("import_assets");

        let events = capture.events();
        let started = events
            .iter()
            .find(|event| event.field("stage") == Some(super::SCAN_STARTED_STAGE))
            .unwrap_or_else(|| panic!("missing scan_started event: {events:?}"));
        assert_eq!(
            started.field("library"),
            Some("PrimarySync"),
            "scan_started marker must carry the library_label for multi-library disambiguation",
        );
        assert_eq!(
            started.message(),
            Some("import scan dequeued first asset"),
            "scan_started event should keep the operator breadcrumb message",
        );
    }

    /// Empty stream -> no marker. Operators tailing logs shouldn't see
    /// `scan_started` for a library that produced zero assets (would
    /// be misleading; the existing empty-library guard surfaces that
    /// case at a higher level).
    #[tokio::test]
    #[tracing_test::traced_test]
    async fn import_skips_scan_started_marker_when_stream_empty() {
        let tmp = TempDir::new().expect("tempdir");
        let server = crate::start_wiremock_or_skip!();
        stub_records_query(&server, &[]).await;
        let album = album_pointed_at(&server);
        let (stream, panic_rx) = album.photo_stream(None, None, 1);
        let db = open_db(&tmp).await;
        let config = base_config(tmp.path());
        let mut dir_cache = DirCache::new();
        import_assets(
            stream,
            panic_rx,
            db.as_ref(),
            &config,
            "PrimarySync",
            &mut dir_cache,
            ImportRunOptions {
                dry_run: true,
                ..Default::default()
            },
        )
        .await
        .expect("import_assets");

        assert!(
            !logs_contain(&format!("stage=\"{}\"", super::SCAN_STARTED_STAGE)),
            "marker must not fire when no assets are dequeued",
        );
    }

    // ── Skip-rehash optimization (issue #347) ─────────────────────────

    /// Skip-rehash optimization: a second `import_assets` pass over the
    /// same on-disk tree must increment `skipped_already_imported` for
    /// every match. The counter sits in the same arm of the loop where
    /// the SHA-256 read and `import_adopt` call are skipped, so a tick
    /// proves the optimization engaged. Without this, every restart
    /// re-pays the SHA-256 cost on every already-imported file (#347).
    #[tokio::test]
    async fn second_pass_skips_rehash_on_unchanged_files() {
        let server = crate::start_wiremock_or_skip!();
        let asset = WiremockAsset::new("REHASH1", "IMG_REH1.JPG", "public.jpeg").orig(
            2048,
            "ck_reh1",
            "public.jpeg",
        );
        let tmp = TempDir::new().unwrap();
        let dl = tmp.path().join("photos");
        std::fs::create_dir_all(&dl).unwrap();
        let config = base_config(&dl);
        stage_expected(&asset.to_photo_asset(), &config);

        let db = open_db(&tmp).await;

        let stats1 = run_import(
            &server,
            std::slice::from_ref(&asset),
            db.as_ref(),
            &config,
            false,
        )
        .await;
        assert_eq!(stats1.matched, 1);
        assert_eq!(
            stats1.skipped_already_imported, 0,
            "fresh DB has nothing to skip",
        );

        // wiremock keeps every mount; the first pass's stub has served its
        // single page and would now return empty if we just re-mounted on
        // top. Reset clears the server so the new stub serves fresh.
        server.reset().await;
        let stats2 = run_import(&server, &[asset], db.as_ref(), &config, false).await;
        assert_eq!(stats2.matched, 1, "match still tallies on skip path");
        assert_eq!(
            stats2.skipped_already_imported, 1,
            "skip-rehash counter must tick on the second pass",
        );
    }

    /// If the on-disk file's mtime jumps between passes, the skip path
    /// must NOT engage — we re-hash and re-adopt. Otherwise a silent
    /// content change goes undetected forever once the row is adopted.
    #[tokio::test]
    async fn second_pass_rehashes_when_file_mtime_changes() {
        let server = crate::start_wiremock_or_skip!();
        let asset = WiremockAsset::new("REHASH2", "IMG_REH2.JPG", "public.jpeg").orig(
            2048,
            "ck_reh2",
            "public.jpeg",
        );
        let tmp = TempDir::new().unwrap();
        let dl = tmp.path().join("photos");
        std::fs::create_dir_all(&dl).unwrap();
        let config = base_config(&dl);
        let staged = stage_expected(&asset.to_photo_asset(), &config);

        let db = open_db(&tmp).await;

        let _ = run_import(
            &server,
            std::slice::from_ref(&asset),
            db.as_ref(),
            &config,
            false,
        )
        .await;

        // Bump mtime on every staged file without relying on wall-clock
        // sleeps. Keep size identical so the path still matches and only
        // the rehash guard changes.
        for path in &staged {
            set_mtime_for_test(path, 1_800_000_000);
        }

        // See sibling test for why reset() is necessary between passes.
        server.reset().await;
        let stats2 = run_import(&server, &[asset], db.as_ref(), &config, false).await;
        assert_eq!(stats2.matched, 1);
        assert_eq!(
            stats2.skipped_already_imported, 0,
            "mtime change must invalidate the skip path",
        );
    }

    // ── icloudpd compat baseline ──────────────────────────────────────
    //
    // Scenario-driven tests that stage on-disk layouts using icloudpd's
    // path rules (taken from icloud_photos_downloader's own test suite
    // fixture data) and verify kei's import-existing matches. Acts as a
    // baseline against accidental layout divergence.
    mod icloudpd_compat;
}

#[cfg(test)]
mod build_config_tests {
    //! Unit tests for [`build_import_path_config`] -- the
    //! TOML > default resolution chain for import-existing's path-derivation
    //! settings. Each new TOML field consumed by import-existing needs a row
    //! in this test mod or the resolver is unverified.
    //!
    //! Construction of `ImportArgs` happens through `Cli::try_parse_from`
    //! (rather than struct literals) so a regression that reorders
    //! `or_else` arms or flips a default reaches us through the same
    //! parse path that production uses.
    use super::build_import_path_config;
    use crate::cli::{Cli, Command};
    use crate::config::{Config, GlobalArgs, TomlConfig};
    use crate::types::{
        AssetVersionSize, FileMatchPolicy, LivePhotoMode, LivePhotoMovFilenamePolicy, RawPolicy,
    };
    use clap::Parser;

    fn parse_import_args(extra: &[&str]) -> crate::cli::ImportArgs {
        let mut argv = vec!["kei", "import-existing"];
        argv.extend(extra.iter().copied());
        let cli = Cli::try_parse_from(argv).expect("clap parse");
        match cli.command {
            Command::ImportExisting(a) => a,
            _ => panic!("expected ImportExisting"),
        }
    }

    fn toml_with_download(body: &str) -> TomlConfig {
        toml::from_str(&format!(
            r#"
            [download]
            directory = "/photos"

            {body}
            "#
        ))
        .expect("parse TOML")
    }

    /// TOML is the only durable import path/photo source in v0.20. This pins
    /// every field that import-existing needs to stay aligned with sync.
    #[test]
    fn build_import_path_config_uses_toml_path_photo_and_media_settings() {
        let toml = toml_with_download(
            r#"
            folder_structure = "%Y/%m"

            [photos]
            resolution = "medium"
            file_match_policy = "name-id7"
            live_photo_mode = "video-only"
            live_resolution = "thumb"
            live_photo_mov_filename_policy = "original"
            raw_policy = "prefer-raw"
            force_resolution = true
            keep_unicode_in_filenames = true

            [filters]
            media = ["photos", "live-photos"]
            "#,
        );

        let cfg = build_import_path_config(Some(&toml)).unwrap();

        assert_eq!(cfg.folder_structure, "%Y/%m");
        assert_eq!(cfg.resolution, crate::types::PhotoResolution::Medium);
        assert_eq!(cfg.file_match_policy, FileMatchPolicy::NameId7);
        assert_eq!(cfg.live_photo_mode, LivePhotoMode::VideoOnly);
        assert_eq!(cfg.live_resolution, AssetVersionSize::LiveThumb);
        assert_eq!(
            cfg.live_photo_mov_filename_policy,
            LivePhotoMovFilenamePolicy::Original
        );
        assert_eq!(cfg.raw_policy, RawPolicy::PreferRaw);
        assert!(cfg.force_resolution);
        assert!(cfg.keep_unicode_in_filenames);
        assert!(cfg.media.photos);
        assert!(!cfg.media.videos);
        assert!(cfg.media.live_photos);
    }

    /// Documented defaults still apply when TOML only supplies the required
    /// download directory.
    #[test]
    fn build_import_path_config_falls_through_to_default() {
        let toml = toml_with_download("");
        let cfg = build_import_path_config(Some(&toml)).unwrap();
        assert_eq!(cfg.resolution, crate::types::PhotoResolution::Original);
        assert_eq!(
            cfg.file_match_policy,
            FileMatchPolicy::NameSizeDedupWithSuffix
        );
        assert_eq!(cfg.live_photo_mode, LivePhotoMode::Both);
        assert_eq!(cfg.live_resolution, AssetVersionSize::LiveOriginal);
        assert_eq!(
            cfg.live_photo_mov_filename_policy,
            LivePhotoMovFilenamePolicy::Suffix
        );
        assert_eq!(cfg.raw_policy, RawPolicy::AsIs);
        assert!(!cfg.force_resolution);
        assert!(!cfg.keep_unicode_in_filenames);
        assert_eq!(cfg.folder_structure, "%Y/%m/%d");
    }

    #[test]
    fn build_import_path_config_empty_directory_bails() {
        let err = build_import_path_config(None).expect_err("empty directory should bail");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("Set [download].directory"),
            "error must name the missing TOML field, got: {msg}"
        );
    }

    /// Sync's `Config::build` and import's `build_import_path_config`
    /// share `resolve_path_derivation_fields`. For the same TOML inputs the
    /// path-derivation knobs they emit must agree byte-for-byte.
    #[test]
    fn sync_and_import_agree_on_path_derivation_fields() {
        use crate::cli::SyncArgs;

        let toml_str = r#"
            [download]
            directory = "/photos"
            folder_structure = "%Y/%m"

            [photos]
            resolution = "original"
            edited = true
            file_match_policy = "name-id7"
            live_photo_mov_filename_policy = "original"
            raw_policy = "prefer-raw"
            force_resolution = true
            keep_unicode_in_filenames = true
        "#;
        let toml: TomlConfig = toml::from_str(toml_str).unwrap();

        let import_cfg = build_import_path_config(Some(&toml)).unwrap();

        let sync = SyncArgs::default();
        let globals = GlobalArgs {
            username: Some("u@example.com".to_string()),
            domain: None,
            data_dir: None,
        };
        let sync_cfg = Config::build(
            &globals,
            &crate::cli::PasswordArgs::default(),
            sync,
            Some(&toml),
        )
        .unwrap();

        assert_eq!(
            import_cfg.folder_structure,
            sync_cfg.download.folder_structure
        );
        assert_eq!(import_cfg.resolution, sync_cfg.photos.resolution);
        assert_eq!(import_cfg.live_photo_mode, sync_cfg.photos.live_photo_mode);
        assert_eq!(
            import_cfg.live_resolution,
            sync_cfg.photos.live_resolution.to_asset_version_size()
        );
        assert_eq!(
            import_cfg.live_photo_mov_filename_policy,
            sync_cfg.photos.live_photo_mov_filename_policy
        );
        assert_eq!(import_cfg.raw_policy, sync_cfg.photos.raw_policy);
        assert_eq!(
            import_cfg.file_match_policy,
            sync_cfg.photos.file_match_policy
        );
        assert_eq!(
            import_cfg.force_resolution,
            sync_cfg.photos.force_resolution
        );
        assert_eq!(
            import_cfg.keep_unicode_in_filenames,
            sync_cfg.photos.keep_unicode_in_filenames
        );
    }

    #[test]
    fn sync_and_import_reject_system_directory_with_same_message() {
        use crate::cli::SyncArgs;

        let toml: TomlConfig = toml::from_str(
            r#"
            [download]
            directory = "/etc"
            "#,
        )
        .unwrap();
        let import_err =
            build_import_path_config(Some(&toml)).expect_err("import: system dir must reject");

        let sync = SyncArgs::default();
        let globals = GlobalArgs {
            username: Some("u@example.com".to_string()),
            domain: None,
            data_dir: None,
        };
        let sync_err = Config::build(
            &globals,
            &crate::cli::PasswordArgs::default(),
            sync,
            Some(&toml),
        )
        .expect_err("sync: system dir must reject");

        let import_msg = format!("{import_err:#}");
        let sync_msg = format!("{sync_err:#}");
        for (label, msg) in [("import", &import_msg), ("sync", &sync_msg)] {
            assert!(
                msg.contains("Refusing to use system directory") && msg.contains("/etc"),
                "{label} must name the rejection and the path; got: {msg}"
            );
        }
    }

    #[test]
    fn resolve_import_strict_uses_toml() {
        let args = parse_import_args(&[]);
        let toml: TomlConfig = toml::from_str(
            r#"
            [import]
            strict = true
            "#,
        )
        .unwrap();

        assert!(super::resolve_import_strict(&args, Some(&toml)));
    }

    #[test]
    fn resolve_import_strict_cli_overrides_toml_false() {
        let args = parse_import_args(&["--strict"]);
        let toml: TomlConfig = toml::from_str(
            r#"
            [import]
            strict = false
            "#,
        )
        .unwrap();

        assert!(super::resolve_import_strict(&args, Some(&toml)));
    }

    #[test]
    fn validate_non_empty_libraries_passes_with_no_empty_zones() {
        super::validate_non_empty_libraries(&[], 1234)
            .expect("no empty zones must not bail regardless of prior count");
    }

    #[test]
    fn validate_non_empty_libraries_bails_naming_zone_and_count() {
        let err = super::validate_non_empty_libraries(&["PrimarySync"], 4321)
            .expect_err("empty zone with prior assets must bail");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("library PrimarySync"),
            "must use singular `library` and name the zone, got: {msg}"
        );
        assert!(
            msg.contains("4321"),
            "must name the prior count, got: {msg}"
        );
        assert!(
            msg.contains("--force-empty"),
            "must mention the override flag, got: {msg}"
        );
    }

    #[test]
    fn validate_non_empty_libraries_pluralizes_for_multiple_zones() {
        let err = super::validate_non_empty_libraries(&["PrimarySync", "SharedSync-abc"], 99)
            .expect_err("multiple empty zones must bail");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("libraries PrimarySync, SharedSync-abc"),
            "must use plural `libraries` and list both, got: {msg}"
        );
    }
}

#[cfg(test)]
mod heartbeat_tests {
    //! Heartbeat-task unit tests. The skip-rehash + heartbeat-firing
    //! end-to-end tests live in `wiremock_tests` because they need the
    //! full wiremock + on-disk staging setup; this module covers the
    //! snapshot + guard behaviour in isolation.
    use std::sync::Arc;
    use std::sync::atomic::Ordering;

    /// `HeartbeatState::snapshot` must mirror the live atomics so the
    /// emitted log line reflects whatever the scan loop has counted up
    /// to that instant.
    #[tokio::test]
    async fn heartbeat_state_snapshot_reflects_loop_progress() {
        let state = Arc::new(super::HeartbeatState::default());
        state.total.fetch_add(7, Ordering::Relaxed);
        state.matched.fetch_add(3, Ordering::Relaxed);
        state.filtered.fetch_add(2, Ordering::Relaxed);
        state
            .skipped_already_imported
            .fetch_add(1, Ordering::Relaxed);
        if let Ok(mut last) = state.last_seen_id.lock() {
            *last = Some("ASSET-XYZ".to_string());
        }

        let snap = state.snapshot();
        assert_eq!(snap.total, 7);
        assert_eq!(snap.matched, 3);
        assert_eq!(snap.filtered, 2);
        assert_eq!(snap.skipped_already_imported, 1);
        assert_eq!(snap.last_seen_id.as_deref(), Some("ASSET-XYZ"));
    }

    /// `HeartbeatGuard::drop` must cancel the spawned task so a bailing
    /// scan loop doesn't leak a periodic logger.
    #[tokio::test]
    async fn heartbeat_guard_cancels_task_on_drop() {
        let state = Arc::new(super::HeartbeatState::default());
        let guard = super::HeartbeatGuard::spawn(
            Arc::clone(&state),
            "test-lib".to_string(),
            std::time::Duration::from_millis(50),
        );
        let token = guard.token.clone();
        assert!(!token.is_cancelled(), "fresh guard is not cancelled");
        drop(guard);
        assert!(
            token.is_cancelled(),
            "Drop must flip the cancellation token",
        );
    }

    /// Paused time makes the heartbeat progress causal rather than
    /// wall-clock dependent: a busy scan loop can keep logging, but the
    /// watchdog must still tick at its configured cadence.
    #[tokio::test(start_paused = true)]
    async fn heartbeat_fires_under_writer_load() {
        use std::sync::atomic::{AtomicBool, Ordering};

        let (capture, _guard) = crate::test_helpers::TracingCapture::install();

        let state = Arc::new(super::HeartbeatState::default());
        let hb = super::HeartbeatGuard::spawn(
            Arc::clone(&state),
            "test-lib".to_string(),
            std::time::Duration::from_millis(50),
        );

        let stop = Arc::new(AtomicBool::new(false));
        let stop_c = Arc::clone(&stop);
        let scan = tokio::spawn(async move {
            while !stop_c.load(Ordering::Relaxed) {
                tracing::info!(target: "kei::scan_test", "scan loop fake event");
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        });

        tokio::task::yield_now().await;
        // 300 ms covers five heartbeat intervals after the immediate tick
        // is skipped. Because time is paused, no host scheduling jitter is
        // involved in the count.
        tokio::time::advance(std::time::Duration::from_millis(300)).await;
        tokio::task::yield_now().await;
        stop.store(true, Ordering::Relaxed);
        scan.await.unwrap();

        drop(hb);

        let events = capture.events();
        let heartbeat_count = events
            .iter()
            .filter(|event| event.message() == Some("import scan heartbeat"))
            .count();
        assert!(
            heartbeat_count >= 5,
            "heartbeat fired {heartbeat_count} times in 300 ms under concurrent scan-loop traffic; events: {events:?}",
        );
    }
}
