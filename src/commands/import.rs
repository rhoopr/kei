use std::sync::Arc;

use crate::auth;
use crate::cli;
use crate::config;
use crate::download;
use crate::retry;
use crate::state;
use crate::state::StateDb;
use crate::types::AssetVersionSize;

use super::service::{init_photos_service, resolve_libraries};

/// This imports existing local files into the state database by:
/// 1. Enumerating all iCloud assets via the photos API
/// 2. Computing the expected local path for each asset
/// 3. If the file exists and size matches, marking it as downloaded in the DB
pub(crate) async fn run_import_existing(
    args: cli::ImportArgs,
    globals: &config::GlobalArgs,
    toml: Option<&config::TomlConfig>,
) -> anyhow::Result<()> {
    use chrono::Local;
    use futures_util::StreamExt;

    let db_path = super::super::get_db_path(globals, toml)?;
    let toml_dl = toml.and_then(|t| t.download.as_ref());
    let toml_photos = toml.and_then(|t| t.photos.as_ref());

    // Resolve directory and path settings from CLI > TOML > default, matching
    // the sync command's resolution so import-existing looks for files at the
    // same paths sync would have created.
    let directory_str = args
        .directory
        .or_else(|| toml_dl.and_then(|d| d.directory.clone()))
        .unwrap_or_default();
    if directory_str.is_empty() {
        anyhow::bail!("--directory is required for import-existing");
    }
    let directory = config::expand_tilde(&directory_str);
    let folder_structure = args
        .folder_structure
        .or_else(|| toml_dl.and_then(|d| d.folder_structure.clone()))
        .unwrap_or_else(|| "%Y/%m/%d".to_string());
    let keep_unicode = args
        .keep_unicode_in_filenames
        .or_else(|| toml_photos.and_then(|p| p.keep_unicode_in_filenames))
        .unwrap_or(false);

    if !directory.exists() {
        anyhow::bail!("Directory does not exist: {}", directory.display());
    }

    // Create or open the state database
    let db = Arc::new(state::SqliteStateDb::open(&db_path).await?);
    tracing::debug!(path = %db_path.display(), "State database opened");

    // Resolve auth from globals + TOML
    let (username, password, domain, cookie_directory) =
        config::resolve_auth(globals, &args.password, toml);

    // Authenticate
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

    // Resolve library selection (CLI > TOML > default PrimarySync)
    let toml_filters = toml.and_then(|t| t.filters.as_ref());
    let selection = config::resolve_library_selection(args.library, toml_filters);
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
        let stream = all_album.photo_stream(args.recent, None, 1);
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

            // Get versions
            if asset.versions().is_empty() {
                tracing::debug!(id = %asset.id(), "Skipping asset with no versions");
                continue;
            }

            // Resolve filename using the same logic as the sync download pipeline:
            // fingerprint fallback → unicode removal → extension mapping.
            let raw_filename = if let Some(f) = asset.filename() {
                f.to_string()
            } else {
                let asset_type = asset
                    .versions()
                    .first()
                    .map_or("", |(_, v)| v.asset_type.as_ref());
                download::paths::generate_fingerprint_filename(asset.id(), asset_type)
            };
            let base_filename = if keep_unicode {
                raw_filename
            } else {
                download::paths::remove_unicode_chars(&raw_filename)
            };

            // Get the created date in local time for path computation
            let created_local = asset.created().with_timezone(&Local);

            // Check each version (we only check "original" for import since that's
            // what the normal sync would download)
            if let Some(version) = asset.get_version(AssetVersionSize::Original) {
                // Map extension from UTI type, matching sync pipeline
                let filename =
                    download::paths::map_filename_extension(&base_filename, &version.asset_type);
                let expected_path = download::paths::local_download_path(
                    &directory,
                    &folder_structure,
                    &created_local,
                    &filename,
                    None, // import-existing doesn't have album context
                );

                if expected_path.exists() {
                    // Check size matches
                    if let Ok(metadata) = std::fs::metadata(&expected_path) {
                        if metadata.len() == version.size {
                            // File exists with matching size - mark as downloaded
                            let version_size = state::VersionSizeKey::Original;
                            let media_type = download::determine_media_type(version_size, &asset);
                            let record = state::AssetRecord::new_pending(
                                asset.id().to_string(),
                                version_size,
                                version.checksum.to_string(),
                                filename.clone(),
                                asset.created(),
                                Some(asset.added_date()),
                                version.size,
                                media_type,
                            );

                            if let Err(e) = db.upsert_seen(&record).await {
                                tracing::warn!(asset_id = %asset.id(), error = %e, "Failed to record asset");
                                continue;
                            }

                            let local_checksum = match download::file::compute_sha256(
                                &expected_path,
                            )
                            .await
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
                                tracing::warn!(asset_id = %asset.id(), error = %e, "Failed to mark as downloaded");
                                continue;
                            }

                            matched += 1;
                            if !args.no_progress_bar && matched.is_multiple_of(100) {
                                println!("  Matched {matched} files so far...");
                            }
                        } else {
                            unmatched += 1;
                        }
                    } else {
                        unmatched += 1;
                    }
                } else {
                    unmatched += 1;
                }
            }
        }
    }

    println!();
    println!("Import complete:");
    println!("  Total assets scanned: {total}");
    println!("  Files matched:        {matched}");
    println!("  Unmatched versions:   {unmatched}");

    Ok(())
}
