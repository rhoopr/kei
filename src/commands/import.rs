#![allow(
    clippy::print_stdout,
    reason = "CLI subcommand whose primary purpose is to print import-existing results to stdout"
)]

//! `kei import-existing`: adopt local files already at the paths a sync
//! would plan, without downloading anything.

use crate::auth;
use crate::cli;
use crate::config;
use crate::download::filter::PathDerivationConfig;
use crate::retry;
use crate::state;
use crate::systemd::SystemdNotifier;

use super::service::{init_photos_service, resolve_libraries};

pub(crate) async fn run_import_existing(
    args: cli::ImportArgs,
    globals: &config::GlobalArgs,
    toml: Option<&config::TomlConfig>,
    input_mode: crate::InputMode,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        args.recent.is_none(),
        "`--recent` is no longer supported: import-existing now checks the whole selected library in one listing."
    );
    anyhow::ensure!(
        !resolve_import_strict(&args, toml),
        "`--strict` is not supported by this build yet. Run import-existing without it; files are adopted only at their planned paths with the expected size."
    );
    let shutdown_token = crate::shutdown::install_signal_handler(
        SystemdNotifier::new(false),
        crate::personality::Mode::Off,
    )?;
    let path_config = build_import_path_config(toml)?;
    let directory = path_config.directory.to_path_buf();
    match tokio::fs::metadata(&directory).await {
        Ok(m) if m.is_dir() => {}
        Ok(_) => anyhow::bail!("Download path is not a directory: {}", directory.display()),
        Err(e) => anyhow::bail!(
            "Could not read download directory {}: {e}",
            directory.display()
        ),
    }

    let (username, password, domain, cookie_directory) =
        config::resolve_auth(globals, &args.password, toml)?;
    let location =
        crate::sync_core::access::Location::resolve(&cookie_directory, &username, domain.as_str())
            .await?;
    // Refuse another account's state before signing in.
    crate::sync_core::access::check_existing(
        &location,
        &state::db::account::AccountOwner::configured(&username, domain.as_str()),
    )
    .await?;
    let password_provider = super::super::make_provider_from_auth(
        &args.password,
        password,
        &username,
        &cookie_directory,
        domain.as_str(),
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
    let owner = state::db::account::AccountOwner::authenticated(
        &username,
        domain.as_str(),
        &auth_result.data,
    )?;
    let (shared_session, mut photos_service) = init_photos_service(
        auth_result,
        retry::RetryConfig::default(),
        crate::personality::Mode::Off,
    )
    .await?;
    let toml_filters = toml.and_then(|t| t.filters.as_ref());
    let selector = config::resolve_library_selector(args.libraries.clone(), toml_filters)?;
    let libraries = resolve_libraries(&selector, &mut photos_service).await?;
    let selection = build_import_selection(toml_filters, &selector)?;

    // A preview plans into a throwaway state file and transfers nothing.
    let scratch = args
        .dry_run
        .then(|| location.current.with_extension("preview.db"));
    if let Some(scratch) = &scratch {
        remove_scratch(scratch).await?;
    }
    let state = match &scratch {
        Some(scratch) => crate::sync_core::State::open(scratch).await?,
        None => match crate::sync_core::access::open(&location, &owner, Some(&directory)).await? {
            Some(state) => state,
            None => crate::sync_core::access::create(&location, &owner).await?,
        },
    };
    let core = crate::sync_core::CoreConfig {
        directory: directory.clone(),
        paths: path_config,
        albums: selection.albums.clone(),
        smart_folders: selection.smart_folders.clone(),
        unfiled: selection.unfiled,
        temp_suffix: ".kei-part".to_owned(),
        concurrency: 4,
        retry: retry::RetryConfig::default(),
        transfer: if args.dry_run {
            crate::sync_core::Transfer::None
        } else {
            crate::sync_core::Transfer::AdoptOnly
        },
        bandwidth: None,
        metadata: None,
        refresh_metadata: false,
    };
    let client = shared_session.read().await.download_client().clone();
    let report =
        crate::sync_core::run_cycle(&state, &libraries, &core, &client, &shutdown_token).await?;
    if let Some(zone) = report
        .zones
        .iter()
        .find(|zone| zone.error.is_some() || !zone.caught_up)
    {
        anyhow::bail!(
            "Could not list {} completely{}; nothing was adopted from the incomplete part. Run import-existing again.",
            zone.zone,
            zone.error
                .as_deref()
                .map(|error| format!(": {error}"))
                .unwrap_or_default()
        );
    }
    let listed = state.asset_count().await?;
    anyhow::ensure!(
        listed > 0 || args.force_empty,
        "iCloud returned no photos for the selected libraries. If the library really is empty, rerun with --force-empty."
    );

    let files = state.list_files(&[]).await?;
    let count = |state: &str| {
        files
            .iter()
            .filter(|file| file.wanted && file.state == state)
            .count()
    };
    if let Some(scratch) = &scratch {
        state.close().await?;
        remove_scratch(scratch).await?;
        println!("Dry run: no state was written.");
        println!("  Local files that match a photo: {}", count("adopt"));
        println!("  Copies with no local file yet:  {}", count("pending"));
    } else {
        let adopted: u64 = report.zones.iter().map(|zone| zone.drain.adopted).sum();
        println!("Adopted {adopted} existing local files.");
        println!(
            "  Already recorded:              {}",
            count("done").saturating_sub(usize::try_from(adopted).unwrap_or(usize::MAX))
        );
        println!(
            "  Copies with no local file yet: {} (run `kei sync` to download them)",
            count("pending")
        );
        if report.completeness.failed > 0 {
            println!(
                "  Could not adopt:               {} (a different file is at the planned path)",
                report.completeness.failed
            );
        }
    }
    Ok(())
}

/// Remove a preview's scratch state file and its SQLite companions.
async fn remove_scratch(path: &std::path::Path) -> anyhow::Result<()> {
    for suffix in ["", "-wal", "-shm", "-journal"] {
        let mut name = path.as_os_str().to_owned();
        name.push(suffix);
        match tokio::fs::remove_file(std::path::PathBuf::from(name)).await {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => return Err(error.into()),
            _ => {}
        }
    }
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
    let directory: std::sync::Arc<std::path::Path> = std::sync::Arc::from(directory_path.as_path());

    let path_fields =
        config::resolve_path_derivation_fields(config::PathDerivationCliArgs::default(), toml)?;
    let media = config::resolve_media_selection(toml.and_then(|t| t.filters.as_ref()), None, None)?;

    let config = PathDerivationConfig::from_path_fields(directory, path_fields, media);
    Ok(config)
}

#[cfg(test)]
mod build_selection_tests {
    //! Sync vs. import-existing parity for the current selector resolution.
    //! Both commands must produce the same `Selection` from the same TOML.
    use super::build_import_selection;
    use crate::config::TomlFilters;
    use crate::selection::{AlbumSelector, LibrarySelector, Selection, SmartFolderSelector};
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
    use crate::types::{FileMatchPolicy, LivePhotoMode, LivePhotoMovFilenamePolicy, RawPolicy};
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
            file_match_policy = "name-id7"
            live_photo_mov_filename_policy = "original"
            keep_unicode_in_filenames = true

            [filters]
            exclude = ["videos", "live-photo-stills"]
            "#,
        );

        let cfg = build_import_path_config(Some(&toml)).unwrap();

        assert_eq!(cfg.folder_structure, "%Y/%m");
        assert_eq!(cfg.file_match_policy, FileMatchPolicy::NameId7);
        assert_eq!(cfg.live_photo_mode, LivePhotoMode::VideoOnly);
        assert_eq!(
            cfg.live_photo_mov_filename_policy,
            LivePhotoMovFilenamePolicy::Original
        );
        assert_eq!(cfg.raw_policy, RawPolicy::AsIs);
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
        assert_eq!(
            cfg.file_match_policy,
            FileMatchPolicy::NameSizeDedupWithSuffix
        );
        assert_eq!(cfg.live_photo_mode, LivePhotoMode::Both);
        assert_eq!(
            cfg.live_photo_mov_filename_policy,
            LivePhotoMovFilenamePolicy::Suffix
        );
        assert_eq!(cfg.raw_policy, RawPolicy::AsIs);
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
            file_match_policy = "name-id7"
            live_photo_mov_filename_policy = "original"
            keep_unicode_in_filenames = true
        "#;
        let toml: TomlConfig = toml::from_str(toml_str).unwrap();

        let import_cfg = build_import_path_config(Some(&toml)).unwrap();

        let sync = SyncArgs::default();
        let globals = GlobalArgs {
            config_source: Default::default(),
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
        assert_eq!(import_cfg.live_photo_mode, sync_cfg.photos.live_photo_mode);
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
            config_source: Default::default(),
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
}
