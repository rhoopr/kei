//! Configuration serialization and first-run persistence.

use super::input::{
    TomlAuth, TomlConfig, TomlDownload, TomlFilters, TomlImport, TomlMetadata, TomlNotifications,
    TomlPhotos, TomlReport, TomlRetry, TomlServer, TomlUi, TomlWatch,
};
use super::runtime::Config;
use super::templates::{DEFAULT_FOLDER_STRUCTURE_ALBUMS, DEFAULT_FOLDER_STRUCTURE_SMART_FOLDERS};
use crate::types::{Domain, FileMatchPolicy, LivePhotoMovFilenamePolicy};
use std::path::Path;

impl Config {
    /// Convert the resolved config back to a [`TomlConfig`] for serialization.
    ///
    /// Only includes static fields suitable for persistence. Passwords are
    /// never included. Per-run flags (`dry_run`, CLI date bounds, etc.) are omitted.
    pub(crate) fn to_toml(&self) -> TomlConfig {
        TomlConfig {
            config_version: Some(1),
            data_dir: None,  // Keep automatic discovery unless explicitly configured.
            log_level: None, // only written if user explicitly set it
            auth: Some(TomlAuth {
                username: if self.auth.username.is_empty() {
                    None
                } else {
                    Some(self.auth.username.clone())
                },
                password: None, // never persist
                password_file: self
                    .auth
                    .password_file
                    .as_ref()
                    .map(|p| p.display().to_string()),
                password_command: self.auth.password_command.clone(),
                domain: if self.auth.domain == Domain::Com {
                    None
                } else {
                    Some(self.auth.domain)
                },
            }),
            download: Some(TomlDownload {
                directory: if self.download.directory.as_os_str().is_empty() {
                    None
                } else {
                    Some(self.download.directory.display().to_string())
                },
                folder_structure: Some(self.download.folder_structure.clone()),
                folder_structure_albums: if self.download.folder_structure_albums
                    == DEFAULT_FOLDER_STRUCTURE_ALBUMS
                {
                    None
                } else {
                    Some(self.download.folder_structure_albums.clone())
                },
                folder_structure_smart_folders: if self.download.folder_structure_smart_folders
                    == DEFAULT_FOLDER_STRUCTURE_SMART_FOLDERS
                {
                    None
                } else {
                    Some(self.download.folder_structure_smart_folders.clone())
                },
                threads: Some(self.download.threads_num),
                bandwidth_limit: self.download.bandwidth_limit.map(|n| n.to_string()),
                temp_suffix: if self.download.temp_suffix == ".kei-tmp" {
                    None
                } else {
                    Some(self.download.temp_suffix.clone())
                },
                legacy_preservation_allow_hardlinks: self
                    .download
                    .legacy_preservation_allow_hardlinks
                    .then_some(true),
                retry: Some(TomlRetry {
                    per_transfer: Some(self.retry.max_retries),
                }),
            }),
            filters: Some(TomlFilters {
                libraries: {
                    // Emit only when the user picked something other than
                    // the default (primary). Default `[primary]` round-trips
                    // implicitly so config dumps stay clean.
                    let raw = self.filters.selection.libraries.to_raw();
                    if raw == vec!["primary".to_string()] {
                        None
                    } else {
                        Some(raw)
                    }
                },
                albums: {
                    // Round-trip via the new selector so the same string
                    // rendering used by `--album` echoes back into TOML.
                    // Default `--album` is `all` in v0.13, so we omit the
                    // `["all"]` shape (load resolves to All when missing)
                    // and emit everything else (including the `["none"]`
                    // shape that round-trips to LibraryOnly).
                    let raw = self.filters.selection.albums.to_raw();
                    if raw == vec!["all".to_string()] {
                        None
                    } else {
                        Some(raw)
                    }
                },
                smart_folders: match &self.filters.selection.smart_folders {
                    crate::selection::SmartFolderSelector::None => None,
                    other => Some(other.to_raw()),
                },
                unfiled: if self.filters.selection.unfiled {
                    None
                } else {
                    Some(false)
                },
                exclude: Some(super::runtime::MediaExclusion::from_selection(
                    self.filters.media,
                    self.photos.live_photo_mode,
                )),
                skip_created_before: self.filters.persistent_skip_created_before.clone(),
                skip_created_after: self.filters.persistent_skip_created_after.clone(),
            }),
            photos: Some(TomlPhotos {
                live_photo_mov_filename_policy: if self.photos.live_photo_mov_filename_policy
                    == LivePhotoMovFilenamePolicy::Suffix
                {
                    None
                } else {
                    Some(self.photos.live_photo_mov_filename_policy)
                },
                edited_as_primary: if self.photos.edited_naming
                    == crate::types::EditedNaming::Primary
                {
                    Some(true)
                } else {
                    None
                },
                file_match_policy: if self.photos.file_match_policy
                    == FileMatchPolicy::NameSizeDedupWithSuffix
                {
                    None
                } else {
                    Some(self.photos.file_match_policy)
                },
                keep_unicode_in_filenames: if self.photos.keep_unicode_in_filenames {
                    Some(true)
                } else {
                    None
                },
            }),
            import: if self.import.strict {
                Some(TomlImport { strict: Some(true) })
            } else {
                None
            },
            metadata: Some(TomlMetadata {
                set_exif_datetime: Some(self.metadata.set_exif_datetime),
                set_exif_orientation: Some(self.metadata.set_exif_orientation),
                set_exif_rating: Some(self.metadata.set_exif_rating),
                set_exif_gps: Some(self.metadata.set_exif_gps),
                set_exif_description: Some(self.metadata.set_exif_description),
                #[cfg(feature = "xmp")]
                embed_xmp: Some(self.metadata.embed_xmp),
                #[cfg(feature = "xmp")]
                xmp_sidecar: Some(self.metadata.xmp_sidecar),
                #[cfg(feature = "xmp")]
                include_albums: Some(self.metadata.include_albums),
                #[cfg(feature = "xmp")]
                include_people: Some(self.metadata.include_people),
            }),
            watch: if self.watch.interval.is_some()
                || self.watch.notify_systemd
                || self.watch.pid_file.is_some()
                || self.watch.reconcile_every_n_cycles.is_some()
            {
                Some(TomlWatch {
                    interval: self.watch.interval,
                    notify_systemd: if self.watch.notify_systemd {
                        Some(true)
                    } else {
                        None
                    },
                    pid_file: self
                        .watch
                        .pid_file
                        .as_ref()
                        .map(|p| p.display().to_string()),
                    reconcile_every_n_cycles: self.watch.reconcile_every_n_cycles,
                })
            } else {
                None
            },
            notifications: self
                .notifications
                .script
                .as_ref()
                .map(|s| TomlNotifications {
                    script: Some(s.display().to_string()),
                }),
            server: Some(TomlServer {
                port: Some(self.server.port),
                // Only emit `bind` when it's been changed from the default.
                // Keeps `config show` output clean for the common case where
                // the user hasn't set an explicit bind.
                bind: {
                    let default = std::net::IpAddr::V4(std::net::Ipv4Addr::new(0, 0, 0, 0));
                    if self.server.bind == default {
                        None
                    } else {
                        Some(self.server.bind.to_string())
                    }
                },
            }),
            report: self.report.json.as_ref().map(|p| TomlReport {
                json: Some(p.display().to_string()),
            }),
            // Only emit `[ui]` when the user actually expressed a preference
            // or disabled the durable progress bar default.
            ui: if self.ui.friendly_request.is_some() || self.download.no_progress_bar {
                Some(TomlUi {
                    friendly: self.ui.friendly_request,
                    progress_bar: if self.download.no_progress_bar {
                        Some(false)
                    } else {
                        None
                    },
                })
            } else {
                None
            },
        }
    }
}

/// Persist a minimal config file on first run.
///
/// Converts the resolved [`Config`] to TOML via [`Config::to_toml()`], then
/// retains the resolved metadata choices and essential connection/path fields.
/// Explicit false metadata choices remain false on subsequent runs. Passwords are never
/// included. No-ops if a config file already exists, the parent directory
/// doesn't exist, or `KEI_NO_AUTO_CONFIG=1` is set.
pub(crate) fn persist_first_run_config(
    config_path: &Path,
    config: &Config,
    data_dir_cli: Option<&str>,
) -> anyhow::Result<()> {
    use anyhow::Context;
    // Opt-out via env var
    if std::env::var("KEI_NO_AUTO_CONFIG").is_ok_and(|v| v == "1") {
        return Ok(());
    }

    // Never overwrite an existing config
    if config_path.exists() {
        return Ok(());
    }

    // Only write if the config's parent directory already exists.
    // This prevents surprise writes during test runs or when the user
    // hasn't established a kei config directory yet. Users who run
    // `kei setup` or manually create the directory opt into auto-config.
    let parent_dir_exists = config_path
        .parent()
        .is_some_and(|p| p.exists() && p.is_dir());
    if !parent_dir_exists {
        return Ok(());
    }

    // Build a minimal TOML from the resolved config, keeping only
    // essential fields that have no defaults.
    let full = config.to_toml();

    // Resolve which data_dir value to persist (only if explicitly provided)
    let data_dir = data_dir_cli.map(String::from);

    let minimal = TomlConfig {
        config_version: Some(1),
        data_dir,
        log_level: None,
        auth: full.auth.map(|a| TomlAuth {
            username: a.username,
            password: None, // never persist
            password_file: a.password_file,
            password_command: a.password_command,
            domain: a.domain,
        }),
        download: full.download.map(|d| TomlDownload {
            directory: d.directory,
            folder_structure: None,
            folder_structure_albums: None,
            folder_structure_smart_folders: None,
            threads: None,
            bandwidth_limit: None,
            temp_suffix: None,
            legacy_preservation_allow_hardlinks: d.legacy_preservation_allow_hardlinks,
            retry: None,
        }),
        filters: None,
        photos: None,
        import: None,
        metadata: full.metadata,
        watch: None,
        notifications: None,
        server: None,
        report: None,
        ui: None,
    };

    // Don't write if there's nothing meaningful to persist
    let has_content =
        minimal.auth.is_some() || minimal.download.is_some() || minimal.data_dir.is_some();
    if !has_content {
        return Ok(());
    }

    let content = toml::to_string_pretty(&minimal)
        .map_err(|e| anyhow::anyhow!("Could not serialize config: {e}"))?;

    let output = format!("# Generated by kei on first run. Edit freely.\n\n{content}");
    std::fs::write(config_path, &output)
        .with_context(|| format!("Could not write config to {}", config_path.display()))?;

    // Restrict permissions on Unix
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(config_path, std::fs::Permissions::from_mode(0o600))
            .with_context(|| {
                format!(
                    "Could not set secure permissions on {}",
                    config_path.display()
                )
            })?;
    }

    tracing::info!(
        target: "kei::config",
        path = %config_path.display(),
        "Saved configuration for future runs"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn metadata_omissions_and_explicit_opt_outs_survive_roundtrip() {
        for input in [
            "config_version = 1",
            "config_version = 1\n[metadata]\nset_exif_datetime = true\nset_exif_rating = false",
        ] {
            let parsed: super::TomlConfig = toml::from_str(input).unwrap();
            let resolved = super::Config::build(
                &default_globals(),
                &default_password(),
                default_sync(),
                Some(&parsed),
            )
            .unwrap();
            let emitted = resolved.to_toml();
            let metadata = emitted.metadata.as_ref().unwrap();
            assert_eq!(metadata.set_exif_rating, Some(false));
            assert_eq!(metadata.set_exif_orientation, Some(false));
            assert_eq!(metadata.set_exif_gps, Some(false));
            assert_eq!(metadata.set_exif_description, Some(false));
            let serialized = toml::to_string(&emitted).unwrap();
            let parsed = toml::from_str(&serialized).unwrap();
            let reopened = super::Config::build(
                &default_globals(),
                &default_password(),
                default_sync(),
                Some(&parsed),
            )
            .unwrap();
            assert_eq!(
                reopened.metadata.set_exif_datetime,
                resolved.metadata.set_exif_datetime
            );
            assert!(!reopened.metadata.set_exif_rating);
            #[cfg(feature = "xmp")]
            {
                assert!(!reopened.metadata.embed_xmp);
                assert!(!reopened.metadata.xmp_sidecar);
                assert!(!reopened.metadata.include_albums);
                assert!(!reopened.metadata.include_people);
            }
        }
    }

    use super::persist_first_run_config;
    use crate::config::input::TomlConfig;
    use crate::config::runtime::Config;
    use crate::config::test_support::{default_globals, default_password, default_sync};
    use std::path::PathBuf;

    #[test]
    fn legacy_preservation_allow_hardlinks_roundtrip_and_bootstrap() {
        for enabled in [false, true] {
            let toml: TomlConfig = toml::from_str(&format!(
                "[download]\nlegacy_preservation_allow_hardlinks = {enabled}"
            ))
            .unwrap();
            let cfg = Config::build(
                &default_globals(),
                &default_password(),
                default_sync(),
                Some(&toml),
            )
            .unwrap();
            let persisted = cfg.to_toml();
            assert_eq!(
                persisted
                    .download
                    .as_ref()
                    .unwrap()
                    .legacy_preservation_allow_hardlinks,
                enabled.then_some(true)
            );
            let serialized = toml::to_string(&persisted).unwrap();
            let reparsed: TomlConfig = toml::from_str(&serialized).unwrap();
            let rebuilt = Config::build(
                &default_globals(),
                &default_password(),
                default_sync(),
                Some(&reparsed),
            )
            .unwrap();
            assert_eq!(
                rebuilt.download.legacy_preservation_allow_hardlinks,
                enabled
            );

            let (_directory, path) = persist_test_dir("legacy_hardlinks");
            persist_first_run_config(&path, &cfg, None).unwrap();
            let bootstrap: TomlConfig =
                toml::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
            assert_eq!(
                bootstrap
                    .download
                    .unwrap()
                    .legacy_preservation_allow_hardlinks,
                enabled.then_some(true)
            );
        }
    }

    #[test]
    fn test_to_toml_omits_retired_lifetime_retry_limit() {
        let cfg = Config::build(
            &default_globals(),
            &default_password(),
            default_sync(),
            None,
        )
        .unwrap();
        let text = toml::to_string(&cfg.to_toml()).unwrap();
        assert!(!text.contains("per_asset"));
        let parsed: TomlConfig = toml::from_str(&text).unwrap();
        assert_eq!(
            parsed.download.unwrap().retry.unwrap().per_transfer,
            Some(cfg.retry.max_retries)
        );
    }
    #[test]
    fn test_to_toml_roundtrip_preserves_username() {
        let cfg = Config::build(
            &default_globals(),
            &default_password(),
            default_sync(),
            None,
        )
        .unwrap();
        let toml = cfg.to_toml();
        assert_eq!(
            toml.auth.as_ref().unwrap().username.as_deref(),
            Some("u@example.com")
        );
    }
    #[test]
    fn test_to_toml_never_includes_password() {
        let globals = default_globals();
        let mut pw = default_password();
        pw.password = Some("secret123".to_string());
        let cfg = Config::build(&globals, &pw, default_sync(), None).unwrap();
        let toml = cfg.to_toml();
        assert!(toml.auth.as_ref().unwrap().password.is_none());
    }
    #[test]
    fn test_to_toml_omits_default_values() {
        let cfg = Config::build(
            &default_globals(),
            &default_password(),
            default_sync(),
            None,
        )
        .unwrap();
        let toml = cfg.to_toml();
        // Default domain (com) should be omitted
        assert!(toml.auth.as_ref().unwrap().domain.is_none());
        assert_eq!(toml.config_version, Some(1));
        // Default temp_suffix should be omitted
        assert!(toml.download.as_ref().unwrap().temp_suffix.is_none());
    }
    //
    // The friendly toggle has three observable states from the TOML's
    // perspective: absent (None), `friendly = true`, `friendly = false`.
    // Together with the CLI tristate this is the contract `lib.rs` and
    // `kei config show` rely on, so each state gets its own assertion.

    #[test]
    fn test_to_toml_omits_ui_section_when_no_preference() {
        let cfg = Config::build(
            &default_globals(),
            &default_password(),
            default_sync(),
            None,
        )
        .unwrap();
        let toml = cfg.to_toml();
        assert!(
            toml.ui.is_none(),
            "config show must not invent a [ui] section for users who never set one"
        );
    }
    #[test]
    fn test_to_toml_includes_non_default_values() {
        let mut globals = default_globals();
        let pw = default_password();
        globals.domain = Some(crate::types::Domain::Cn);
        let mut sync = default_sync();
        sync.config_overrides.file_match_policy = Some(crate::types::FileMatchPolicy::NameId7);
        let cfg = Config::build(&globals, &pw, sync, None).unwrap();
        let toml = cfg.to_toml();
        assert_eq!(
            toml.auth.as_ref().unwrap().domain,
            Some(crate::types::Domain::Cn)
        );
        assert_eq!(
            toml.photos.as_ref().unwrap().file_match_policy,
            Some(crate::types::FileMatchPolicy::NameId7)
        );
    }
    #[test]
    fn edited_as_primary_roundtrips_through_config_export() {
        use crate::types::EditedNaming;

        let input: TomlConfig =
            toml::from_str("config_version = 1\n[photos]\nedited_as_primary = true").unwrap();
        let resolved = Config::build(
            &default_globals(),
            &default_password(),
            default_sync(),
            Some(&input),
        )
        .unwrap();
        assert_eq!(resolved.photos.edited_naming, EditedNaming::Primary);

        let emitted = resolved.to_toml();
        assert_eq!(
            emitted.photos.as_ref().unwrap().edited_as_primary,
            Some(true)
        );
        let serialized = toml::to_string(&emitted).unwrap();
        let reparsed: TomlConfig = toml::from_str(&serialized).unwrap();
        let reopened = Config::build(
            &default_globals(),
            &default_password(),
            default_sync(),
            Some(&reparsed),
        )
        .unwrap();
        assert_eq!(reopened.photos.edited_naming, EditedNaming::Primary);

        let default = Config::build(
            &default_globals(),
            &default_password(),
            default_sync(),
            None,
        )
        .unwrap();
        let default_toml = default.to_toml();
        assert_eq!(
            default_toml.photos.as_ref().unwrap().edited_as_primary,
            None
        );
        assert!(
            !toml::to_string(&default_toml)
                .unwrap()
                .contains("edited_as_primary")
        );
    }

    #[test]
    fn test_to_toml_serializes_to_valid_toml() {
        let cfg = Config::build(
            &default_globals(),
            &default_password(),
            default_sync(),
            None,
        )
        .unwrap();
        let toml_cfg = cfg.to_toml();
        let serialized = toml::to_string_pretty(&toml_cfg).unwrap();
        // Should be parseable back
        let _parsed: TomlConfig = toml::from_str(&serialized).unwrap();
    }
    #[test]
    fn test_to_toml_per_run_fields_omitted() {
        let mut sync = default_sync();
        sync.skip_created_before = Some("2025-01-01".to_string());
        sync.skip_created_after = Some("2025-12-31".to_string());
        let cfg = Config::build(&default_globals(), &default_password(), sync, None).unwrap();
        let toml = cfg.to_toml();
        let filters = toml.filters.as_ref().unwrap();
        assert!(filters.skip_created_before.is_none());
        assert!(filters.skip_created_after.is_none());
    }
    #[test]
    fn test_to_toml_persists_fixed_dates_from_toml() {
        let toml_str = r#"
            [filters]
            skip_created_before = "2024-01-01"
            skip_created_after = "2025-06-01T14:30:00"
        "#;
        let toml: TomlConfig = toml::from_str(toml_str).unwrap();
        let cfg = Config::build(
            &default_globals(),
            &default_password(),
            default_sync(),
            Some(&toml),
        )
        .unwrap();
        let serialized = cfg.to_toml();
        let filters = serialized.filters.as_ref().unwrap();
        assert_eq!(filters.skip_created_before.as_deref(), Some("2024-01-01"));
        assert_eq!(
            filters.skip_created_after.as_deref(),
            Some("2025-06-01T14:30:00")
        );
        let encoded = ::toml::to_string(&serialized).unwrap();
        let reopened: TomlConfig = ::toml::from_str(&encoded).unwrap();
        let reopened_filters = reopened.filters.as_ref().unwrap();
        assert_eq!(
            reopened_filters.skip_created_before,
            filters.skip_created_before
        );
        assert_eq!(
            reopened_filters.skip_created_after,
            filters.skip_created_after
        );
    }
    #[test]
    fn test_to_toml_roundtrip_media() {
        let toml_str = r#"
            [filters]
            exclude = ["videos"]
        "#;
        let toml: TomlConfig = toml::from_str(toml_str).unwrap();
        let cfg = Config::build(
            &default_globals(),
            &default_password(),
            default_sync(),
            Some(&toml),
        )
        .unwrap();
        let serialized = cfg.to_toml();
        let filters = serialized.filters.as_ref().unwrap();
        assert_eq!(
            filters.exclude.as_deref(),
            Some(&[crate::config::MediaExclusion::Videos][..])
        );
    }
    #[test]
    fn test_to_toml_keeps_inline_album_excludes_canonical() {
        let mut sync = default_sync();
        sync.config_overrides.albums = vec!["!Family".to_string()];
        let cfg = Config::build(&default_globals(), &default_password(), sync, None).unwrap();
        let toml = cfg.to_toml();
        let filters = toml.filters.as_ref().unwrap();
        assert_eq!(
            filters.albums.as_deref(),
            Some(&["all".to_string(), "!Family".to_string()][..])
        );

        let serialized = ::toml::to_string_pretty(&toml).unwrap();
        assert!(
            !serialized.contains("exclude_albums"),
            "config show must not re-emit removed exclude_albums:\n{serialized}"
        );
    }
    #[test]
    fn test_to_toml_omits_retired_filename_exclude() {
        let cfg = Config::build(
            &default_globals(),
            &default_password(),
            default_sync(),
            None,
        )
        .unwrap();
        let toml = cfg.to_toml();
        let serialized = ::toml::to_string_pretty(&toml).unwrap();
        assert!(!serialized.contains("filename_exclude"));
        let _: TomlConfig = ::toml::from_str(&serialized).unwrap();
    }
    #[test]
    fn test_to_toml_roundtrip_component_exclusions() {
        use crate::config::MediaExclusion;
        let components = [
            MediaExclusion::Photos,
            MediaExclusion::Videos,
            MediaExclusion::LivePhotoStills,
            MediaExclusion::LivePhotoVideos,
        ];
        for mask in 0..16 {
            let exclusions: Vec<_> = components
                .iter()
                .enumerate()
                .filter_map(|(index, component)| (mask & (1 << index) != 0).then_some(*component))
                .collect();
            let mut input: TomlConfig =
                ::toml::from_str("config_version=1\n[filters]\nexclude=[]").unwrap();
            input.filters.as_mut().unwrap().exclude = Some(exclusions.clone());
            let result = Config::build(
                &default_globals(),
                &default_password(),
                default_sync(),
                Some(&input),
            );
            if mask == 15 {
                assert!(
                    result
                        .unwrap_err()
                        .to_string()
                        .contains("at least one media component")
                );
                continue;
            }
            let config = result.unwrap();
            let serialized = ::toml::to_string_pretty(&config.to_toml()).unwrap();
            let parsed: TomlConfig = ::toml::from_str(&serialized).unwrap();
            assert_eq!(parsed.config_version, Some(1));
            assert_eq!(
                parsed.filters.as_ref().unwrap().exclude.as_ref(),
                Some(&exclusions)
            );
            let reopened = Config::build(
                &default_globals(),
                &default_password(),
                default_sync(),
                Some(&parsed),
            )
            .unwrap();
            assert_eq!(reopened.filters.media, config.filters.media);
            assert_eq!(
                reopened.photos.live_photo_mode,
                config.photos.live_photo_mode
            );
            assert!(reopened.photos.edited && reopened.photos.alternative);
        }
    }
    #[test]
    fn test_to_toml_default_live_photo_mode_omitted() {
        let cfg = Config::build(
            &default_globals(),
            &default_password(),
            default_sync(),
            None,
        )
        .unwrap();
        let toml = cfg.to_toml();
        assert_eq!(
            toml.filters.as_ref().unwrap().exclude.as_deref(),
            Some([].as_slice())
        );
    }
    #[test]
    fn test_to_toml_roundtrip_bandwidth_limit() {
        let mut sync = default_sync();
        sync.config_overrides.bandwidth_limit = Some(5_000_000);
        let cfg = Config::build(&default_globals(), &default_password(), sync, None).unwrap();
        let serialized = cfg.to_toml();
        assert_eq!(
            serialized
                .download
                .as_ref()
                .unwrap()
                .bandwidth_limit
                .as_deref(),
            Some("5000000")
        );

        let reparsed = Config::build(
            &default_globals(),
            &default_password(),
            default_sync(),
            Some(&serialized),
        )
        .unwrap();
        assert_eq!(reparsed.download.bandwidth_limit, Some(5_000_000));
    }
    #[test]
    fn test_to_toml_bandwidth_limit_none_omitted() {
        let cfg = Config::build(
            &default_globals(),
            &default_password(),
            default_sync(),
            None,
        )
        .unwrap();
        let toml = cfg.to_toml();
        assert!(toml.download.as_ref().unwrap().bandwidth_limit.is_none());
    }
    /// Create a unique temp dir for a persist test, returning
    /// (TempDir handle, config_path).
    fn persist_test_dir(_id: &str) -> (tempfile::TempDir, PathBuf) {
        let td = tempfile::tempdir().unwrap();
        let config_path = td.path().join("config.toml");
        (td, config_path)
    }
    /// Build a Config with the given overrides for persist tests.
    fn build_config_for_persist(
        username: &str,
        directory: Option<&str>,
        password: Option<&str>,
    ) -> Config {
        let mut globals = default_globals();
        let mut pw_args = default_password();
        globals.username = Some(username.to_string());
        if let Some(p) = password {
            pw_args.password = Some(p.to_string());
        }
        let mut sync = default_sync();
        if let Some(d) = directory {
            sync.config_overrides.download_dir = Some(d.to_string());
        }
        Config::build(&globals, &pw_args, sync, None).unwrap()
    }
    #[test]
    fn test_persist_first_run_creates_config() {
        let (_td, config_path) = persist_test_dir("creates");
        let config = build_config_for_persist("test@example.com", Some("/photos"), None);

        persist_first_run_config(&config_path, &config, None).unwrap();

        assert!(config_path.exists());
        let content = std::fs::read_to_string(&config_path).unwrap();
        assert!(content.contains("test@example.com"));
        assert!(content.contains("/photos"));
        assert!(content.contains("Generated by kei"));
        let parsed: TomlConfig = toml::from_str(&content).unwrap();
        let metadata = parsed.metadata.unwrap();
        assert_eq!(metadata.set_exif_datetime, Some(false));
        assert_eq!(metadata.set_exif_orientation, Some(false));
        assert_eq!(metadata.set_exif_rating, Some(false));
        assert_eq!(metadata.set_exif_gps, Some(false));
        assert_eq!(metadata.set_exif_description, Some(false));
        #[cfg(feature = "xmp")]
        {
            assert_eq!(metadata.embed_xmp, Some(false));
            assert_eq!(metadata.include_albums, Some(false));
            assert_eq!(metadata.include_people, Some(false));
        }
    }
    #[cfg(feature = "xmp")]
    #[test]
    fn first_run_and_config_show_projection_preserve_independent_metadata_choices() {
        let (_td, path) = persist_test_dir("metadata_choices");
        let mut config = build_config_for_persist("test@example.com", Some("/photos"), None);
        config.metadata.embed_xmp = true;
        config.metadata.set_exif_description = true;
        config.metadata.include_people = true;
        persist_first_run_config(&path, &config, None).unwrap();
        for input in [
            std::fs::read_to_string(path).unwrap(),
            toml::to_string(&config.to_toml()).unwrap(),
        ] {
            let parsed = toml::from_str(&input).unwrap();
            let reopened = Config::build(
                &default_globals(),
                &default_password(),
                default_sync(),
                Some(&parsed),
            )
            .unwrap();
            assert!(reopened.metadata.embed_xmp);
            assert!(reopened.metadata.set_exif_description);
            assert!(reopened.metadata.include_people);
            assert!(!reopened.metadata.include_albums);
            assert!(!reopened.metadata.set_exif_datetime);
        }
    }
    #[test]
    fn test_persist_first_run_never_writes_password() {
        let (_td, config_path) = persist_test_dir("no_pw");
        let config = build_config_for_persist("test@example.com", None, Some("secret123"));

        persist_first_run_config(&config_path, &config, None).unwrap();

        let content = std::fs::read_to_string(&config_path).unwrap();
        assert!(!content.contains("secret123"));
    }
    #[test]
    fn test_persist_first_run_does_not_overwrite_existing() {
        let (_td, config_path) = persist_test_dir("no_overwrite");
        std::fs::write(&config_path, "# existing config\n").unwrap();

        let config = build_config_for_persist("new@example.com", None, None);
        persist_first_run_config(&config_path, &config, None).unwrap();

        let content = std::fs::read_to_string(&config_path).unwrap();
        assert_eq!(content, "# existing config\n");
    }
    #[test]
    fn test_persist_first_run_noop_without_parent_dir() {
        let td = tempfile::tempdir().unwrap();
        // Point config_path at a subdirectory that doesn't exist
        let config_path = td.path().join("nonexistent_sub").join("config.toml");

        let config = build_config_for_persist("test@example.com", None, None);
        persist_first_run_config(&config_path, &config, None).unwrap();

        assert!(!config_path.exists());
    }
    #[test]
    fn test_persist_first_run_with_data_dir() {
        let (_td, config_path) = persist_test_dir("data_dir");

        let mut globals = default_globals();
        let mut pw = default_password();
        pw.password_file = Some("/run/secrets/pw".to_string());
        globals.domain = Some(crate::types::Domain::Cn);
        let mut sync = default_sync();
        sync.config_overrides.download_dir = Some("/photos".to_string());
        let config = Config::build(&globals, &pw, sync, None).unwrap();

        persist_first_run_config(&config_path, &config, Some("/data")).unwrap();

        let content = std::fs::read_to_string(&config_path).unwrap();
        let toml_content: &str = content
            .strip_prefix("# Generated by kei on first run. Edit freely.\n\n")
            .unwrap_or(&content);
        let parsed: TomlConfig = toml::from_str(toml_content).unwrap();
        assert_eq!(
            parsed.auth.as_ref().unwrap().username.as_deref(),
            Some("u@example.com")
        );
        assert_eq!(parsed.data_dir.as_deref(), Some("/data"));
        assert_eq!(
            parsed.download.as_ref().unwrap().directory.as_deref(),
            Some("/photos")
        );
        assert_eq!(
            parsed.auth.as_ref().unwrap().domain,
            Some(crate::types::Domain::Cn)
        );
        assert_eq!(
            parsed.auth.as_ref().unwrap().password_file.as_deref(),
            Some("/run/secrets/pw")
        );
    }
    #[test]
    fn test_folder_structure_per_category_round_trips_default() {
        let cfg = Config::build(
            &default_globals(),
            &default_password(),
            default_sync(),
            None,
        )
        .unwrap();
        let toml = cfg.to_toml();
        // Default value is suppressed on round-trip so config dumps stay clean.
        assert!(
            toml.download
                .as_ref()
                .unwrap()
                .folder_structure_albums
                .is_none()
        );
        assert!(
            toml.download
                .as_ref()
                .unwrap()
                .folder_structure_smart_folders
                .is_none()
        );
    }
    #[test]
    fn test_folder_structure_per_category_round_trips_custom() {
        let mut sync = default_sync();
        sync.config_overrides.folder_structure_albums = Some("{album}/%Y".to_string());
        sync.config_overrides.folder_structure_smart_folders =
            Some("{smart-folder}/%Y".to_string());
        let cfg = Config::build(&default_globals(), &default_password(), sync, None).unwrap();
        let toml = cfg.to_toml();
        let dl = toml.download.unwrap();
        assert_eq!(dl.folder_structure_albums.as_deref(), Some("{album}/%Y"));
        assert_eq!(
            dl.folder_structure_smart_folders.as_deref(),
            Some("{smart-folder}/%Y")
        );
    }
    #[test]
    fn test_to_toml_omits_delay_when_matches_smart_default() {
        // Smart default for per_transfer=3 is 5. to_toml should NOT write
        // `delay = 5` back out because it's redundant.
        let mut sync = default_sync();
        sync.config_overrides.max_retries = Some(3);
        let cfg = Config::build(&default_globals(), &default_password(), sync, None).unwrap();
        let toml = cfg.to_toml();
        let retry = toml.download.unwrap().retry.unwrap();
        assert_eq!(retry.per_transfer, Some(3));
    }
}
