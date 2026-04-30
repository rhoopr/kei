#![allow(
    clippy::print_stdout,
    reason = "CLI subcommand whose primary purpose is to print import-existing progress to stdout"
)]

use std::path::Path;
use std::sync::Arc;

use crate::auth;
use crate::cli;
use crate::config;
use crate::download;
use crate::download::filter::{expected_paths_for, is_asset_filtered, ExpectedAssetPath};
use crate::download::paths::{normalize_ampm, DirCache};
use crate::icloud::photos::PhotoAsset;
use crate::retry;
use crate::state;
use crate::state::{StateDb, StateError};
use crate::types::{
    AssetVersionSize, FileMatchPolicy, LivePhotoMode, LivePhotoMovFilenamePolicy, LivePhotoSize,
    RawTreatmentPolicy, VersionSize,
};

use super::service::{init_photos_service, resolve_libraries};

/// Per-library counters returned by [`import_assets`].
///
/// Counters span asset- and expected-path levels so that divergent
/// totals in the summary surface silently dropped work instead of
/// masquerading as a clean run. `total` and `filtered` tick per asset;
/// `matched`, `unmatched`, and `hash_errors` tick per expected path
/// (one asset can yield multiple paths, e.g. live photos).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ImportStats {
    pub total: u64,
    pub matched: u64,
    pub unmatched: u64,
    pub filtered: u64,
    pub hash_errors: u64,
}

impl std::ops::AddAssign for ImportStats {
    fn add_assign(&mut self, rhs: Self) {
        self.total += rhs.total;
        self.matched += rhs.matched;
        self.unmatched += rhs.unmatched;
        self.filtered += rhs.filtered;
        self.hash_errors += rhs.hash_errors;
    }
}

/// Find the on-disk path that satisfies the expected size: primary, then
/// the dedup-collision shape under `NameSizeDedupWithSuffix`, then an
/// AM/PM whitespace sibling. macOS screenshots use NARROW NO-BREAK SPACE
/// (`\u{202F}`) before AM/PM; trees synced through other tools may have
/// normalized to a regular space (or vice versa).
async fn resolve_match_path(
    primary: &Path,
    expected_size: u64,
    policy: FileMatchPolicy,
    dir_cache: &mut DirCache,
) -> Option<(std::path::PathBuf, std::fs::Metadata)> {
    if let Ok(m) = tokio::fs::metadata(primary).await {
        if m.len() == expected_size {
            return Some((primary.to_path_buf(), m));
        }
    }
    if policy == FileMatchPolicy::NameSizeDedupWithSuffix {
        let parent = primary.parent().unwrap_or(Path::new(""));
        let Some(fname) = primary.file_name().and_then(|f| f.to_str()) else {
            tracing::debug!(
                path = %primary.display(),
                "Skipping dedup-suffix fallback: filename is not valid UTF-8",
            );
            return None;
        };
        let suffixed_fname = download::paths::add_dedup_suffix(fname, expected_size);
        let suffixed = parent.join(suffixed_fname);
        if let Ok(m) = tokio::fs::metadata(&suffixed).await {
            if m.len() == expected_size {
                return Some((suffixed, m));
            }
        }
    }
    // Cheap pre-check: only pay for the dir read if the filename actually
    // has an AM/PM whitespace token to vary. `normalize_ampm` is idempotent
    // on filenames that have no such token, so the inequality below is the
    // exact condition under which `find_ampm_variant` could return Some.
    let needs_probe = primary
        .file_name()
        .and_then(|f| f.to_str())
        .is_some_and(|f| normalize_ampm(f) != f);
    if !needs_probe {
        return None;
    }
    let parent = primary.parent()?;
    dir_cache.ensure_dir_async(parent).await;
    let variant = dir_cache.find_ampm_variant(primary)?;
    if dir_cache.file_size(&variant) != Some(expected_size) {
        return None;
    }
    let m = tokio::fs::metadata(&variant).await.ok()?;
    tracing::info!(
        primary = %primary.display(),
        variant = %variant.display(),
        "Matched AM/PM whitespace variant on disk",
    );
    Some((variant, m))
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
pub(crate) async fn import_assets<S>(
    stream: S,
    panic_rx: tokio::sync::oneshot::Receiver<bool>,
    db: &dyn StateDb,
    download_config: &download::DownloadConfig,
    library_label: &str,
    dry_run: bool,
    show_progress: bool,
) -> anyhow::Result<ImportStats>
where
    S: futures_util::Stream<Item = anyhow::Result<PhotoAsset>>,
{
    use futures_util::StreamExt;

    tokio::pin!(stream);
    let mut stats = ImportStats::default();
    // One cache per library scan: each parent directory is read_dir'd at
    // most once across all sibling probes.
    let mut dir_cache = DirCache::new();

    while let Some(result) = stream.next().await {
        let asset: PhotoAsset = match result {
            Ok(a) => a,
            Err(e) => {
                // Continuing past a fetcher Err would let a partial scan
                // report as clean, leaving unmatched files to re-download
                // on the next sync.
                anyhow::bail!(
                    "import scan aborted for library '{library_label}': fetcher returned error: {e}"
                );
            }
        };

        stats.total += 1;

        if asset.versions().is_empty() {
            tracing::debug!(id = %asset.id(), "Skipping asset with no versions");
            stats.filtered += 1;
            continue;
        }

        // `expected_paths_for` documents the precondition that callers run
        // `is_asset_filtered` first to apply content/date filters. Most filter
        // inputs are inert here (build_import_download_config zeros them out),
        // but live_photo_mode IS user-configurable, and other filter sources
        // (TOML defaults, future flag additions) could leak through. Honor the
        // contract uniformly so the gate stays in one place.
        if let Some(reason) = is_asset_filtered(&asset, download_config) {
            tracing::debug!(id = %asset.id(), ?reason, "Skipping (is_asset_filtered)");
            stats.filtered += 1;
            continue;
        }

        let expected = expected_paths_for(&asset, download_config);
        if expected.is_empty() {
            // WARN, not debug: an asset silently dropped here is invisible
            // to operators reconciling the on-disk tree against the report.
            tracing::warn!(
                id = %asset.id(),
                live_photo_mode = ?download_config.live_photo_mode,
                force_size = download_config.force_size,
                "Skipping asset with no expected paths from path-derivation",
            );
            stats.filtered += 1;
            continue;
        }

        for ExpectedAssetPath {
            path: primary_path,
            size: expected_size,
            checksum,
            version_size,
        } in expected
        {
            // For `NameSizeDedupWithSuffix`, when two iCloud assets share
            // a filename, icloudpd renames the second's download to
            // `<stem>-<size><ext>` (it stat's the existing file at
            // download time, sees the wrong size, falls back). kei's
            // `expected_paths_for` is single-asset and emits only the
            // bare path, so the size-suffixed file would read as
            // unmatched on import even though it's what kei would also
            // have written under the same collision. Try the suffix
            // shape as a fallback.
            let (expected_path, _metadata) = match resolve_match_path(
                &primary_path,
                expected_size,
                download_config.file_match_policy,
                &mut dir_cache,
            )
            .await
            {
                Some(found) => found,
                None => {
                    stats.unmatched += 1;
                    continue;
                }
            };

            if !dry_run {
                // Hash the file BEFORE creating the pending row. If the read
                // fails (permissions, vanished file, I/O error), bailing
                // here leaves no orphan `pending` row that a future sync
                // would wrongly skip.
                let local_checksum = match download::file::compute_sha256(&expected_path).await {
                    Ok(hash) => hash,
                    Err(e) => {
                        tracing::warn!(path = %expected_path.display(), error = %e, "Failed to hash file");
                        stats.hash_errors += 1;
                        continue;
                    }
                };

                let media_type = download::determine_media_type(version_size, &asset);
                let record = state::AssetRecord::new_pending(
                    asset.id().to_string(),
                    version_size,
                    checksum.to_string(),
                    expected_path
                        .file_name()
                        .and_then(|f| f.to_str())
                        .unwrap_or("")
                        .to_string(),
                    asset.created(),
                    Some(asset.added_date()),
                    expected_size,
                    media_type,
                );
                if let Err(e) = db.upsert_seen(&record).await {
                    tracing::warn!(asset_id = %asset.id(), version = ?version_size, error = %e, "Failed to record asset");
                    continue;
                }

                match db
                    .mark_downloaded(
                        asset.id(),
                        version_size.as_str(),
                        &expected_path,
                        &local_checksum,
                        None,
                    )
                    .await
                {
                    Ok(()) => {}
                    Err(e @ StateError::AssetRowMissing { .. }) => {
                        // Row vanished between upsert_seen and
                        // mark_downloaded — retry can't fix this. Bail
                        // before more assets are silently dropped.
                        anyhow::bail!("import scan aborted for library '{library_label}': {e}");
                    }
                    Err(e) => {
                        tracing::warn!(asset_id = %asset.id(), version = ?version_size, error = %e, "Failed to mark as downloaded");
                        continue;
                    }
                }
            }

            stats.matched += 1;
            if show_progress && stats.matched.is_multiple_of(100) {
                println!(
                    "  [{label}] Matched {matched} files so far...",
                    label = library_label,
                    matched = stats.matched,
                );
            }
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
            "import scan aborted for library '{library_label}': a fetcher task panicked; \
             results are incomplete, see earlier error log"
        );
    }

    Ok(stats)
}

/// This imports existing local files into the state database by:
/// 1. Building a [`download::DownloadConfig`] from CLI > env > TOML > default,
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
) -> anyhow::Result<()> {
    let db_path = super::super::get_db_path(globals, toml)?;
    let download_config = build_import_download_config(&args, toml)?;
    let directory = Arc::clone(&download_config.directory);

    let recent_count: Option<u32> = match args.recent {
        None => None,
        Some(crate::cli::RecentLimit::Count(n)) => Some(n),
        Some(crate::cli::RecentLimit::Days(n)) => {
            anyhow::bail!(
                "`--recent {n}d` isn't supported for import-existing (which scans \
                 existing files rather than filtering by iCloud date). Use a plain \
                 count like `--recent 1000` instead."
            );
        }
    };

    if !directory.exists() {
        anyhow::bail!("Directory does not exist: {}", directory.display());
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
    );

    let auth_result = auth::authenticate(
        &cookie_directory,
        &username,
        &password_provider,
        domain.as_str(),
        None,
        None,
        None,
    )
    .await?;

    let (_shared_session, mut photos_service) =
        init_photos_service(auth_result, retry::RetryConfig::default()).await?;

    let toml_filters = toml.and_then(|t| t.filters.as_ref());
    let selection = config::resolve_library_selection(args.library.clone(), toml_filters);
    let libraries = resolve_libraries(&selection, &mut photos_service).await?;

    if !args.no_progress_bar {
        println!("Scanning iCloud assets and matching with local files...");
    }

    let mut totals = ImportStats::default();

    for library in &libraries {
        let zone = library.zone_name();
        tracing::debug!(zone = %zone, "Scanning library");
        let all_album = library.all();
        let (stream, panic_rx) = all_album.photo_stream(recent_count, None, 1);

        let stats = import_assets(
            stream,
            panic_rx,
            db.as_ref(),
            &download_config,
            zone,
            args.dry_run,
            !args.no_progress_bar,
        )
        .await?;
        totals += stats;
    }

    println!();
    if args.dry_run {
        println!("Import complete (DRY RUN - no changes written to state DB):");
    } else {
        println!("Import complete:");
    }
    println!("  Total assets scanned: {}", totals.total);
    println!("  Files matched:        {}", totals.matched);
    println!("  Unmatched versions:   {}", totals.unmatched);
    println!("  Filtered (no path):   {}", totals.filtered);
    println!("  Hash errors:          {}", totals.hash_errors);

    Ok(())
}

/// Resolve a [`download::DownloadConfig`] from import-existing CLI args + TOML.
///
/// The resolution mirrors sync's CLI/env > TOML > default chain for every
/// field that affects path derivation. Fields that don't affect path
/// derivation (state DB handle, retry config, concurrency, sync mode, ...)
/// are populated with inert defaults: `import-existing` never instantiates a
/// download pipeline, so those values are unused.
fn build_import_download_config(
    args: &cli::ImportArgs,
    toml: Option<&config::TomlConfig>,
) -> anyhow::Result<download::DownloadConfig> {
    use rustc_hash::FxHashSet;

    let toml_dl = toml.and_then(|t| t.download.as_ref());
    let toml_photos = toml.and_then(|t| t.photos.as_ref());

    anyhow::ensure!(
        !(args.download_dir.is_some() && args.directory.is_some()),
        "both `--download-dir` and `--directory` are set; `--directory` is \
         deprecated and will be removed in v0.20.0 -- pick one"
    );
    let directory_cli = if let Some(d) = args.download_dir.clone() {
        Some(d)
    } else if let Some(d) = args.directory.clone() {
        tracing::warn!(
            "`--directory` / `KEI_DIRECTORY` is deprecated and will be removed in v0.20.0, \
             use `--download-dir` / `KEI_DOWNLOAD_DIR` instead"
        );
        Some(d)
    } else {
        None
    };
    let directory_str = directory_cli
        .or_else(|| toml_dl.and_then(|d| d.directory.clone()))
        .unwrap_or_default();
    if directory_str.is_empty() {
        anyhow::bail!("--download-dir is required for import-existing");
    }
    let directory: Arc<Path> = Arc::from(config::expand_tilde(&directory_str).as_path());

    let folder_structure = args
        .folder_structure
        .clone()
        .or_else(|| toml_dl.and_then(|d| d.folder_structure.clone()))
        .unwrap_or_else(|| "%Y/%m/%d".to_string());

    let keep_unicode_in_filenames = args
        .keep_unicode_in_filenames
        .or_else(|| toml_photos.and_then(|p| p.keep_unicode_in_filenames))
        .unwrap_or(false);

    let file_match_policy = args
        .file_match_policy
        .or_else(|| toml_photos.and_then(|p| p.file_match_policy))
        .unwrap_or(FileMatchPolicy::NameSizeDedupWithSuffix);

    let size: AssetVersionSize = args
        .size
        .or_else(|| toml_photos.and_then(|p| p.size))
        .unwrap_or(VersionSize::Original)
        .into();

    let live_photo_mode = args
        .live_photo_mode
        .or_else(|| toml_photos.and_then(|p| p.live_photo_mode))
        .unwrap_or(LivePhotoMode::Both);

    let live_photo_size: AssetVersionSize = args
        .live_photo_size
        .or_else(|| toml_photos.and_then(|p| p.live_photo_size))
        .unwrap_or(LivePhotoSize::Original)
        .to_asset_version_size();

    let live_photo_mov_filename_policy = args
        .live_photo_mov_filename_policy
        .or_else(|| toml_photos.and_then(|p| p.live_photo_mov_filename_policy))
        .unwrap_or(LivePhotoMovFilenamePolicy::Suffix);

    let align_raw = args
        .align_raw
        .or_else(|| toml_photos.and_then(|p| p.align_raw))
        .unwrap_or(RawTreatmentPolicy::Unchanged);

    let force_size = args
        .force_size
        .or_else(|| toml_photos.and_then(|p| p.force_size))
        .unwrap_or(false);

    Ok(download::DownloadConfig {
        directory,
        folder_structure,
        size,
        skip_videos: false,
        skip_photos: false,
        skip_created_before: None,
        skip_created_after: None,
        #[cfg(feature = "xmp")]
        set_exif_datetime: false,
        #[cfg(feature = "xmp")]
        set_exif_rating: false,
        #[cfg(feature = "xmp")]
        set_exif_gps: false,
        #[cfg(feature = "xmp")]
        set_exif_description: false,
        #[cfg(feature = "xmp")]
        embed_xmp: false,
        #[cfg(feature = "xmp")]
        xmp_sidecar: false,
        dry_run: args.dry_run,
        concurrent_downloads: 1,
        recent: None,
        retry: retry::RetryConfig::default(),
        live_photo_mode,
        live_photo_size,
        live_photo_mov_filename_policy,
        align_raw,
        no_progress_bar: args.no_progress_bar,
        only_print_filenames: false,
        file_match_policy,
        force_size,
        keep_unicode_in_filenames,
        filename_exclude: Arc::from(Vec::<glob::Pattern>::new()),
        temp_suffix: Arc::from(".kei-tmp"),
        state_db: None,
        retry_only: false,
        max_download_attempts: 0,
        sync_mode: download::SyncMode::Full,
        album_name: None,
        exclude_asset_ids: Arc::new(FxHashSet::default()),
        asset_groupings: Arc::new(download::AssetGroupings::default()),
        bandwidth_limiter: None,
    })
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
    use serde_json::{json, Value};
    use tempfile::TempDir;
    use wiremock::matchers::{method as wm_method, path as wm_path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::{import_assets, ImportStats};
    use crate::download::filter::expected_paths_for;
    use crate::download::{AssetGroupings, DownloadConfig, SyncMode};
    use crate::icloud::photos::session::PhotosSession;
    use crate::icloud::photos::{PhotoAlbum, PhotoAlbumConfig, PhotoAsset};
    use crate::retry::RetryConfig;
    use crate::state::{AssetStatus, SqliteStateDb, StateDb, VersionSizeKey};
    use crate::types::{
        AssetVersionSize, FileMatchPolicy, LivePhotoMode, LivePhotoMovFilenamePolicy,
        RawTreatmentPolicy,
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
            },
            session,
        )
    }

    async fn open_db(tmp: &TempDir) -> Arc<SqliteStateDb> {
        let path = tmp.path().join("state.db");
        Arc::new(SqliteStateDb::open(&path).await.expect("open state db"))
    }

    /// Convenience: fetch every downloaded row.
    async fn all_downloaded(db: &dyn StateDb) -> Vec<crate::state::AssetRecord> {
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
            size: AssetVersionSize::Original,
            skip_videos: false,
            skip_photos: false,
            skip_created_before: None,
            skip_created_after: None,
            #[cfg(feature = "xmp")]
            set_exif_datetime: false,
            #[cfg(feature = "xmp")]
            set_exif_rating: false,
            #[cfg(feature = "xmp")]
            set_exif_gps: false,
            #[cfg(feature = "xmp")]
            set_exif_description: false,
            #[cfg(feature = "xmp")]
            embed_xmp: false,
            #[cfg(feature = "xmp")]
            xmp_sidecar: false,
            dry_run: false,
            concurrent_downloads: 1,
            recent: None,
            retry: RetryConfig::default(),
            live_photo_mode: LivePhotoMode::Both,
            live_photo_size: AssetVersionSize::LiveOriginal,
            live_photo_mov_filename_policy: LivePhotoMovFilenamePolicy::Suffix,
            align_raw: RawTreatmentPolicy::Unchanged,
            no_progress_bar: true,
            only_print_filenames: false,
            file_match_policy: FileMatchPolicy::NameSizeDedupWithSuffix,
            force_size: false,
            keep_unicode_in_filenames: false,
            filename_exclude: Arc::from(Vec::<glob::Pattern>::new()),
            temp_suffix: Arc::from(".kei-tmp"),
            state_db: None,
            retry_only: false,
            max_download_attempts: 0,
            sync_mode: SyncMode::Full,
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
        db: &dyn StateDb,
        config: &DownloadConfig,
        dry_run: bool,
    ) -> ImportStats {
        stub_records_query(server, assets).await;
        let album = album_pointed_at(server);
        let (stream, panic_rx) = album.photo_stream(None, None, 1);
        import_assets(stream, panic_rx, db, config, "test-all", dry_run, false)
            .await
            .expect("import_assets")
    }

    // ── Tests: default flow ───────────────────────────────────────────

    /// Diagnostic: prove the wire round-trip produces a matching PhotoAsset
    /// and that the stream emits exactly one item.
    #[tokio::test]
    async fn diagnostic_stream_round_trip() {
        use futures_util::StreamExt;
        let server = MockServer::start().await;
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
        let server = MockServer::start().await;
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
        let server = MockServer::start().await;
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
        let server = MockServer::start().await;
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
        let server = MockServer::start().await;
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
        let server = MockServer::start().await;
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
        let server = MockServer::start().await;
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
        let server = MockServer::start().await;
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
        let server = MockServer::start().await;
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
    async fn live_photo_mov_filename_policy_original_preserves_base_name() {
        let server = MockServer::start().await;
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
        let server = MockServer::start().await;
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
        let server = MockServer::start().await;
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

    // ── Tests: idempotency ────────────────────────────────────────────

    #[tokio::test]
    async fn idempotent_re_run_keeps_db_consistent() {
        let server1 = MockServer::start().await;
        let server2 = MockServer::start().await;
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
    async fn force_size_unchecked_falls_back_when_size_missing() {
        // Asset has only Original; user requests Medium with force_size=false
        // (the default fallback policy: pick what exists).
        let server = MockServer::start().await;
        let asset = WiremockAsset::new("FS1", "IMG_0001.JPG", "public.jpeg").orig(
            1000,
            "ck_fs1",
            "public.jpeg",
        );
        let tmp = TempDir::new().unwrap();
        let dl = tmp.path().join("photos");
        std::fs::create_dir_all(&dl).unwrap();
        let mut config = base_config(&dl);
        config.size = AssetVersionSize::Medium;
        config.force_size = false;
        stage_expected(&asset.to_photo_asset(), &config);

        let db = open_db(&tmp).await;
        let stats = run_import(&server, &[asset], db.as_ref(), &config, false).await;
        assert_eq!(stats.matched, 1);
        let rows = all_downloaded(db.as_ref()).await;
        // Fell back to Original since Medium wasn't published.
        assert_eq!(rows[0].version_size, VersionSizeKey::Original);
    }

    #[tokio::test]
    async fn force_size_strict_skips_when_size_missing() {
        let server = MockServer::start().await;
        let asset = WiremockAsset::new("FS2", "IMG_0002.JPG", "public.jpeg").orig(
            1000,
            "ck_fs2",
            "public.jpeg",
        );
        let tmp = TempDir::new().unwrap();
        let dl = tmp.path().join("photos");
        std::fs::create_dir_all(&dl).unwrap();
        let mut config = base_config(&dl);
        config.size = AssetVersionSize::Medium;
        config.force_size = true;

        let db = open_db(&tmp).await;
        let stats = run_import(&server, &[asset], db.as_ref(), &config, false).await;
        assert_eq!(stats.total, 1);
        assert_eq!(stats.matched, 0, "force_size strict must skip");
        assert_eq!(stats.unmatched, 0);
        assert_eq!(
            stats.filtered, 1,
            "force_size + missing size produces empty expected paths -> filtered",
        );
    }

    // ── Tests: pagination + EOF ───────────────────────────────────────

    #[tokio::test]
    async fn matches_multiple_assets_in_one_page() {
        let server = MockServer::start().await;
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
        let server = MockServer::start().await;
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
    /// `align_raw=PreferOriginal` swaps so the RAW Alt becomes the primary,
    /// matching what a user who wants "the actual original RAW" expects.
    #[tokio::test]
    async fn align_raw_prefer_original_swaps_to_raw() {
        let server = MockServer::start().await;
        let asset = WiremockAsset::new("RAW1", "IMG_RAW.JPG", "public.jpeg")
            .orig(2000, "ck_raw1_jpg", "public.jpeg")
            .alt(8000, "ck_raw1_dng", "com.adobe.raw-image");
        let tmp = TempDir::new().unwrap();
        let dl = tmp.path().join("photos");
        std::fs::create_dir_all(&dl).unwrap();
        let mut config = base_config(&dl);
        config.align_raw = RawTreatmentPolicy::PreferOriginal;

        // Stage every path the policy chose. With PreferOriginal swapping
        // RAW↔JPEG, we expect a non-.JPG filename for at least one row.
        stage_expected(&asset.to_photo_asset(), &config);

        let db = open_db(&tmp).await;
        let stats = run_import(&server, &[asset], db.as_ref(), &config, false).await;
        assert!(stats.matched >= 1, "RAW path matched");
        let rows = all_downloaded(db.as_ref()).await;
        assert!(
            rows.iter().any(|r| !r.filename.ends_with(".JPG")),
            "PreferOriginal: at least one row should use a non-JPG (RAW) extension, got {:?}",
            rows.iter().map(|r| &r.filename).collect::<Vec<_>>()
        );
    }

    /// Same fixture with `align_raw=Unchanged` (default) keeps the
    /// JPEG as primary even though a RAW alternative exists.
    #[tokio::test]
    async fn align_raw_unchanged_keeps_jpeg_primary() {
        let server = MockServer::start().await;
        let asset = WiremockAsset::new("RAW2", "IMG_RAW2.JPG", "public.jpeg")
            .orig(2000, "ck_raw2_jpg", "public.jpeg")
            .alt(8000, "ck_raw2_dng", "com.adobe.raw-image");
        let tmp = TempDir::new().unwrap();
        let dl = tmp.path().join("photos");
        std::fs::create_dir_all(&dl).unwrap();
        let config = base_config(&dl); // default: Unchanged

        stage_expected(&asset.to_photo_asset(), &config);
        let db = open_db(&tmp).await;
        let stats = run_import(&server, &[asset], db.as_ref(), &config, false).await;
        assert!(stats.matched >= 1);
        let rows = all_downloaded(db.as_ref()).await;
        assert!(
            rows.iter().any(|r| r.filename.ends_with(".JPG")),
            "Unchanged: JPEG primary, got {:?}",
            rows.iter().map(|r| &r.filename).collect::<Vec<_>>()
        );
    }

    // ── Tests: keep_unicode_in_filenames ──────────────────────────────

    #[tokio::test]
    async fn keep_unicode_preserves_non_ascii_filename() {
        let server = MockServer::start().await;
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
        let server = MockServer::start().await;
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

    // ── Tests: skip_videos / skip_photos ──────────────────────────────

    /// `skip_videos` filtering happens via `is_asset_filtered`, which
    /// `import_assets` now invokes upstream of `expected_paths_for`. The
    /// previous incarnation of this test asserted `matched == 0` without
    /// staging a file, so it would have passed even if the gate were
    /// missing entirely (no file → no match for an unrelated reason). Stage
    /// the file the matcher would land on AND verify `is_asset_filtered`
    /// classifies it as a video-skip; if the gate disappears, `matched`
    /// becomes 1 and `unmatched` stays 0, failing this test loudly.
    #[tokio::test]
    async fn skip_videos_excludes_movie_assets() {
        let server = MockServer::start().await;
        let tmp = TempDir::new().unwrap();
        let dl = tmp.path().join("photos");
        std::fs::create_dir_all(&dl).unwrap();
        let mut config = base_config(&dl);
        config.skip_videos = true;

        let asset = WiremockAsset::new("VID1", "MOV_0001.MOV", "com.apple.quicktime-movie").orig(
            5000,
            "ck_vid1",
            "com.apple.quicktime-movie",
        );

        // Stage the file at the path expected_paths_for *would* emit if the
        // gate were missing. Without skip_videos honored, this would match.
        let probe_config = base_config(&dl); // skip_videos defaults to false
        let expected_if_unfiltered = expected_paths_for(&asset.to_photo_asset(), &probe_config);
        assert_eq!(
            expected_if_unfiltered.len(),
            1,
            "probe: video should have one expected path when skip_videos=false",
        );
        stage_file(
            &expected_if_unfiltered[0].path,
            expected_if_unfiltered[0].size,
        );

        // is_asset_filtered must classify a movie under skip_videos as filtered.
        assert!(
            crate::download::filter::is_asset_filtered(&asset.to_photo_asset(), &config).is_some(),
            "skip_videos must filter movie assets via is_asset_filtered",
        );

        let db = open_db(&tmp).await;
        let stats = run_import(&server, &[asset], db.as_ref(), &config, false).await;

        assert_eq!(stats.total, 1, "asset still enumerated");
        assert_eq!(stats.matched, 0, "skip_videos must drop the movie");
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

        let server = MockServer::start().await;
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
        let server = MockServer::start().await;
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
    /// can't easily flip skip_videos through `build_import_download_config`
    /// (it hardcodes false), but `exclude_asset_ids` is honored by
    /// `is_asset_filtered` and we can set it directly on the test
    /// `DownloadConfig`. If `import_assets` ever stops calling the gate,
    /// this test fails with `matched=1` instead of `matched=0`.
    #[tokio::test]
    async fn is_asset_filtered_blocks_excluded_asset_id() {
        let server = MockServer::start().await;
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
    // These pin skip_photos / date / filename-exclude so a future change
    // that loses one of them surfaces here.
    //
    // Each test stages the file the matcher would land on without the
    // filter and asserts matched=0 + no DB rows -- proving the filter
    // ran *before* the path derivation + hash + upsert chain.

    #[tokio::test]
    async fn skip_photos_excludes_still_assets() {
        let server = MockServer::start().await;
        let asset = WiremockAsset::new("STILL1", "IMG_S1.JPG", "public.jpeg").orig(
            1000,
            "ck_still1",
            "public.jpeg",
        );
        let tmp = TempDir::new().unwrap();
        let dl = tmp.path().join("photos");
        std::fs::create_dir_all(&dl).unwrap();
        let mut config = base_config(&dl);
        config.skip_photos = true;
        stage_expected(&asset.to_photo_asset(), &base_config(&dl));

        let db = open_db(&tmp).await;
        let stats = run_import(&server, &[asset], db.as_ref(), &config, false).await;
        assert_eq!(stats.total, 1);
        assert_eq!(stats.matched, 0, "skip_photos must drop the still");
        assert_eq!(stats.unmatched, 0, "filter fires before resolve_match_path");
        assert_eq!(stats.filtered, 1);
        assert!(all_downloaded(db.as_ref()).await.is_empty());
    }

    #[tokio::test]
    async fn date_filter_excludes_assets_outside_window() {
        // skip_created_before set to mid-2025 drops anything older.
        // The default WiremockAsset asset_date is Jan 14 2025, before
        // the cutoff.
        let server = MockServer::start().await;
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
        config.skip_created_before = chrono::DateTime::from_timestamp(1_748_736_000, 0);
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
        let server = MockServer::start().await;
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
        let server = MockServer::start().await;
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
        let server = MockServer::start().await;
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
        let server = MockServer::start().await;
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

        let server = MockServer::start().await;
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

        let stats = import_assets(stream, rx, db.as_ref(), &config, "test-all", false, false)
            .await
            .expect("clean exit must be Ok");
        assert_eq!(stats.total, 0);
        assert_eq!(stats.matched, 0);
        assert_eq!(stats.unmatched, 0);
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

        let err = import_assets(stream, rx, db.as_ref(), &config, "test-all", false, false)
            .await
            .expect_err("must bail on fetcher panic");
        let msg = format!("{err}");
        assert!(
            msg.contains("import scan aborted") && msg.contains("test-all"),
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

        let server = MockServer::start().await;
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
        let err = import_assets(
            stream,
            panic_rx,
            db.as_ref(),
            &config,
            "test-all",
            true,
            false,
        )
        .await
        .expect_err("must bail on fetcher Err");
        let msg = format!("{err}");
        assert!(
            msg.contains("import scan aborted") && msg.contains("test-all"),
            "error message must name the abort + library label, got: {msg}",
        );
        assert!(
            msg.contains("fetcher returned error"),
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
        let server = MockServer::start().await;
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
    //! Unit tests for [`build_import_download_config`] -- the
    //! CLI > TOML > default resolution chain for import-existing's
    //! path-derivation flags. Each new flag added to import-existing
    //! needs a row in this test mod or the precedence is unverified.
    //!
    //! Construction of `ImportArgs` happens through `Cli::try_parse_from`
    //! (rather than struct literals) so a regression that reorders
    //! `or_else` arms or flips a default reaches us through the same
    //! parse path that production uses.
    use super::build_import_download_config;
    use crate::cli::{Cli, Command};
    use crate::config::{TomlConfig, TomlPhotos};
    use crate::types::{
        AssetVersionSize, FileMatchPolicy, LivePhotoMode, LivePhotoMovFilenamePolicy,
        LivePhotoSize, RawTreatmentPolicy, VersionSize,
    };
    use clap::Parser;

    fn parse_import_args(extra: &[&str]) -> crate::cli::ImportArgs {
        let mut argv = vec!["kei", "import-existing", "--download-dir", "/photos"];
        argv.extend(extra.iter().copied());
        let cli = Cli::try_parse_from(argv).expect("clap parse");
        match cli.command {
            Some(Command::ImportExisting(a)) => a,
            _ => panic!("expected ImportExisting"),
        }
    }

    fn empty_toml() -> TomlConfig {
        TomlConfig {
            data_dir: None,
            log_level: None,
            auth: None,
            download: None,
            filters: None,
            photos: None,
            watch: None,
            notifications: None,
            metrics: None,
            server: None,
            report: None,
        }
    }

    fn toml_with_photos(p: TomlPhotos) -> TomlConfig {
        let mut t = empty_toml();
        t.photos = Some(p);
        t
    }

    fn empty_photos() -> TomlPhotos {
        TomlPhotos {
            size: None,
            live_photo_size: None,
            live_photo_mode: None,
            live_photo_mov_filename_policy: None,
            align_raw: None,
            file_match_policy: None,
            force_size: None,
            keep_unicode_in_filenames: None,
        }
    }

    /// CG-1 / AT-3: CLI value beats TOML value across every photos field.
    /// Each row plants a different value at each layer; if the resolver
    /// reverses precedence on any field, exactly that row fails.
    #[test]
    fn build_import_download_config_cli_overrides_toml() {
        // size: CLI=Medium, TOML=Thumb -> Medium
        let args = parse_import_args(&["--size", "medium"]);
        let toml = toml_with_photos(TomlPhotos {
            size: Some(VersionSize::Thumb),
            ..empty_photos()
        });
        let cfg = build_import_download_config(&args, Some(&toml)).unwrap();
        assert_eq!(cfg.size, AssetVersionSize::Medium, "size: CLI must win");

        // file_match_policy: CLI=NameId7, TOML=NameSizeDedupWithSuffix -> NameId7
        let args = parse_import_args(&["--file-match-policy", "name-id7"]);
        let toml = toml_with_photos(TomlPhotos {
            file_match_policy: Some(FileMatchPolicy::NameSizeDedupWithSuffix),
            ..empty_photos()
        });
        let cfg = build_import_download_config(&args, Some(&toml)).unwrap();
        assert_eq!(
            cfg.file_match_policy,
            FileMatchPolicy::NameId7,
            "file_match_policy: CLI must win"
        );

        // live_photo_mode: CLI=VideoOnly, TOML=ImageOnly -> VideoOnly
        let args = parse_import_args(&["--live-photo-mode", "video-only"]);
        let toml = toml_with_photos(TomlPhotos {
            live_photo_mode: Some(LivePhotoMode::ImageOnly),
            ..empty_photos()
        });
        let cfg = build_import_download_config(&args, Some(&toml)).unwrap();
        assert_eq!(
            cfg.live_photo_mode,
            LivePhotoMode::VideoOnly,
            "live_photo_mode: CLI must win"
        );

        // live_photo_size: CLI=Medium, TOML=Thumb -> LiveMedium
        let args = parse_import_args(&["--live-photo-size", "medium"]);
        let toml = toml_with_photos(TomlPhotos {
            live_photo_size: Some(LivePhotoSize::Thumb),
            ..empty_photos()
        });
        let cfg = build_import_download_config(&args, Some(&toml)).unwrap();
        assert_eq!(
            cfg.live_photo_size,
            AssetVersionSize::LiveMedium,
            "live_photo_size: CLI must win"
        );

        // live_photo_mov_filename_policy: CLI=Original, TOML=Suffix -> Original
        let args = parse_import_args(&["--live-photo-mov-filename-policy", "original"]);
        let toml = toml_with_photos(TomlPhotos {
            live_photo_mov_filename_policy: Some(LivePhotoMovFilenamePolicy::Suffix),
            ..empty_photos()
        });
        let cfg = build_import_download_config(&args, Some(&toml)).unwrap();
        assert_eq!(
            cfg.live_photo_mov_filename_policy,
            LivePhotoMovFilenamePolicy::Original,
            "live_photo_mov_filename_policy: CLI must win"
        );

        // align_raw: CLI=PreferOriginal, TOML=PreferAlternative -> PreferOriginal
        let args = parse_import_args(&["--align-raw", "original"]);
        let toml = toml_with_photos(TomlPhotos {
            align_raw: Some(RawTreatmentPolicy::PreferAlternative),
            ..empty_photos()
        });
        let cfg = build_import_download_config(&args, Some(&toml)).unwrap();
        assert_eq!(
            cfg.align_raw,
            RawTreatmentPolicy::PreferOriginal,
            "align_raw: CLI must win"
        );

        // force_size: CLI=true, TOML=false -> true
        let args = parse_import_args(&["--force-size"]);
        let toml = toml_with_photos(TomlPhotos {
            force_size: Some(false),
            ..empty_photos()
        });
        let cfg = build_import_download_config(&args, Some(&toml)).unwrap();
        assert!(
            cfg.force_size,
            "force_size: CLI true must win over TOML false"
        );

        // keep_unicode_in_filenames: CLI=true, TOML=false -> true
        let args = parse_import_args(&["--keep-unicode-in-filenames"]);
        let toml = toml_with_photos(TomlPhotos {
            keep_unicode_in_filenames: Some(false),
            ..empty_photos()
        });
        let cfg = build_import_download_config(&args, Some(&toml)).unwrap();
        assert!(
            cfg.keep_unicode_in_filenames,
            "keep_unicode_in_filenames: CLI true must win over TOML false"
        );
    }

    /// CG-1: TOML value beats default when CLI absent.
    #[test]
    fn build_import_download_config_uses_toml_when_cli_absent() {
        let args = parse_import_args(&[]);
        let toml = toml_with_photos(TomlPhotos {
            size: Some(VersionSize::Medium),
            file_match_policy: Some(FileMatchPolicy::NameId7),
            live_photo_mode: Some(LivePhotoMode::VideoOnly),
            live_photo_size: Some(LivePhotoSize::Thumb),
            live_photo_mov_filename_policy: Some(LivePhotoMovFilenamePolicy::Original),
            align_raw: Some(RawTreatmentPolicy::PreferOriginal),
            force_size: Some(true),
            keep_unicode_in_filenames: Some(true),
        });
        let cfg = build_import_download_config(&args, Some(&toml)).unwrap();
        assert_eq!(cfg.size, AssetVersionSize::Medium);
        assert_eq!(cfg.file_match_policy, FileMatchPolicy::NameId7);
        assert_eq!(cfg.live_photo_mode, LivePhotoMode::VideoOnly);
        assert_eq!(cfg.live_photo_size, AssetVersionSize::LiveThumb);
        assert_eq!(
            cfg.live_photo_mov_filename_policy,
            LivePhotoMovFilenamePolicy::Original
        );
        assert_eq!(cfg.align_raw, RawTreatmentPolicy::PreferOriginal);
        assert!(cfg.force_size);
        assert!(cfg.keep_unicode_in_filenames);
    }

    /// CG-1: documented defaults when neither CLI nor TOML set the field.
    /// Pinning the *documented* default protects against silent default
    /// drift (per memory `feedback_default_value_change_test`).
    #[test]
    fn build_import_download_config_falls_through_to_default() {
        let args = parse_import_args(&[]);
        let cfg = build_import_download_config(&args, None).unwrap();
        assert_eq!(cfg.size, AssetVersionSize::Original);
        assert_eq!(
            cfg.file_match_policy,
            FileMatchPolicy::NameSizeDedupWithSuffix
        );
        assert_eq!(cfg.live_photo_mode, LivePhotoMode::Both);
        assert_eq!(cfg.live_photo_size, AssetVersionSize::LiveOriginal);
        assert_eq!(
            cfg.live_photo_mov_filename_policy,
            LivePhotoMovFilenamePolicy::Suffix
        );
        assert_eq!(cfg.align_raw, RawTreatmentPolicy::Unchanged);
        assert!(!cfg.force_size);
        assert!(!cfg.keep_unicode_in_filenames);
        assert_eq!(cfg.folder_structure, "%Y/%m/%d");
    }

    /// CG-9: empty `directory` after TOML resolve produces the documented
    /// "is required" error rather than a confusing downstream failure.
    #[test]
    fn build_import_download_config_empty_directory_bails() {
        // No --download-dir on the CLI, no TOML directory either: bails.
        let cli = Cli::try_parse_from([
            "kei",
            "import-existing",
            // explicitly absent --download-dir
        ])
        .unwrap();
        let args = match cli.command {
            Some(Command::ImportExisting(a)) => a,
            _ => panic!("expected ImportExisting"),
        };
        let err =
            build_import_download_config(&args, None).expect_err("empty directory should bail");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("--download-dir is required"),
            "error must name the missing flag, got: {msg}"
        );
    }

    /// CG-10: setting both `--directory` (deprecated) and `--download-dir`
    /// at once fails fast rather than letting one silently win. The
    /// deprecation warning users rely on requires this guard to stay
    /// strict.
    #[test]
    fn build_import_download_config_both_directory_flags_bails() {
        let args = parse_import_args(&["--directory", "/old/photos"]);
        // parse_import_args already set --download-dir=/photos, so both
        // are present.
        assert!(args.download_dir.is_some());
        assert!(args.directory.is_some());
        let err = build_import_download_config(&args, None)
            .expect_err("both directory flags should bail");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("--directory") && (msg.contains("deprecated") || msg.contains("pick one")),
            "error must name the deprecation conflict, got: {msg}"
        );
    }
}
