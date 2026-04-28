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
use crate::download::filter::{expected_paths_for, ExpectedAssetPath};
use crate::retry;
use crate::state;
use crate::state::StateDb;
use crate::types::{
    AssetVersionSize, FileMatchPolicy, LivePhotoMode, LivePhotoMovFilenamePolicy, LivePhotoSize,
    RawTreatmentPolicy, VersionSize,
};

use super::service::{init_photos_service, resolve_libraries};

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
    use futures_util::StreamExt;

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

    let mut matched = 0u64;
    let mut unmatched = 0u64;
    let mut total = 0u64;

    for library in &libraries {
        tracing::debug!(zone = %library.zone_name(), "Scanning library");
        let all_album = library.all();
        let (stream, panic_rx) = all_album.photo_stream(recent_count, None, 1);
        tokio::pin!(stream);

        while let Some(result) = stream.next().await {
            let asset: crate::icloud::photos::PhotoAsset = match result {
                Ok(a) => a,
                Err(e) => {
                    tracing::warn!(error = %e, "Error fetching asset");
                    continue;
                }
            };

            total += 1;

            if asset.versions().is_empty() {
                tracing::debug!(id = %asset.id(), "Skipping asset with no versions");
                continue;
            }

            let expected = expected_paths_for(&asset, &download_config);
            if expected.is_empty() {
                continue;
            }

            for ExpectedAssetPath {
                path: expected_path,
                size: expected_size,
                checksum,
                version_size,
            } in expected
            {
                let Ok(metadata) = tokio::fs::metadata(&expected_path).await else {
                    unmatched += 1;
                    continue;
                };
                if metadata.len() != expected_size {
                    unmatched += 1;
                    continue;
                }

                if !args.dry_run {
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

                    let local_checksum = match download::file::compute_sha256(&expected_path).await
                    {
                        Ok(hash) => hash,
                        Err(e) => {
                            tracing::warn!(path = %expected_path.display(), error = %e, "Failed to hash file");
                            continue;
                        }
                    };

                    if let Err(e) = db
                        .mark_downloaded(
                            asset.id(),
                            version_size.as_str(),
                            &expected_path,
                            &local_checksum,
                            None,
                        )
                        .await
                    {
                        tracing::warn!(asset_id = %asset.id(), version = ?version_size, error = %e, "Failed to mark as downloaded");
                        continue;
                    }
                }

                matched += 1;
                if !args.no_progress_bar && matched.is_multiple_of(100) {
                    println!("  Matched {matched} files so far...");
                }
            }
        }

        // Enumeration is complete -- but if a fetcher panicked the stream
        // just closed short, leaving `total` understated. Bail so the
        // scan is obviously aborted (not a silently partial report).
        if panic_rx.await.unwrap_or(false) {
            anyhow::bail!(
                "import scan aborted for library '{}': a fetcher task panicked; \
                 results are incomplete, see earlier error log",
                library.zone_name()
            );
        }
    }

    println!();
    if args.dry_run {
        println!("Import complete (DRY RUN - no changes written to state DB):");
    } else {
        println!("Import complete:");
    }
    println!("  Total assets scanned: {total}");
    println!("  Files matched:        {matched}");
    println!("  Unmatched versions:   {unmatched}");

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
