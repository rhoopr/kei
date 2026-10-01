//! TOML input schema and file loading.

use super::runtime::MediaKind;
use crate::types::{
    Domain, FileMatchPolicy, LivePhotoMode, LivePhotoMovFilenamePolicy, LivePhotoResolution,
    LogLevel, PhotoResolution, RawPolicy,
};
use serde::{Deserialize, Serialize};
use std::path::Path;

// ── TOML config structs ─────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TomlConfig {
    pub data_dir: Option<String>,
    pub log_level: Option<LogLevel>,
    pub auth: Option<TomlAuth>,
    pub download: Option<TomlDownload>,
    pub filters: Option<TomlFilters>,
    pub photos: Option<TomlPhotos>,
    pub import: Option<TomlImport>,
    pub metadata: Option<TomlMetadata>,
    pub watch: Option<TomlWatch>,
    pub notifications: Option<TomlNotifications>,
    pub server: Option<TomlServer>,
    pub report: Option<TomlReport>,
    pub ui: Option<TomlUi>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TomlUi {
    /// Friendly terminal progress and summaries. Defaults to `true` on a plain
    /// TTY; auto-disabled in non-TTY, service, container, systemd,
    /// machine-output, or explicit `--log-level` / `RUST_LOG` contexts. The
    /// CLI flags `--friendly` and `--no-friendly` override this value for one
    /// invocation.
    pub friendly: Option<bool>,
    /// Durable progress-bar default. Defaults to true. The CLI
    /// `--no-progress-bar` flag remains a one-run disable override.
    pub progress_bar: Option<bool>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TomlNotifications {
    pub script: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TomlReport {
    pub json: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TomlAuth {
    pub username: Option<String>,
    pub password: Option<String>,
    pub password_file: Option<String>,
    pub password_command: Option<String>,
    pub domain: Option<Domain>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TomlDownload {
    pub directory: Option<String>,
    pub folder_structure: Option<String>,
    /// v0.13+ per-category template for album passes. Default `{album}`.
    pub folder_structure_albums: Option<String>,
    /// v0.13+ per-category template for smart-folder passes. Default
    /// `{smart-folder}`.
    pub folder_structure_smart_folders: Option<String>,
    pub threads: Option<u16>,
    pub bandwidth_limit: Option<String>,
    pub temp_suffix: Option<String>,
    pub retry: Option<TomlRetry>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TomlRetry {
    /// Retries within a single transfer.
    pub per_transfer: Option<u32>,
    /// Lifetime cap on download attempts per asset across syncs (default
    /// `10`). Distinct from `per_transfer`, which only caps retries within a
    /// single download. `0` disables the cap.
    pub per_asset: Option<u32>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TomlMetadata {
    pub set_exif_datetime: Option<bool>,
    pub set_exif_rating: Option<bool>,
    pub set_exif_gps: Option<bool>,
    pub set_exif_description: Option<bool>,
    #[cfg(feature = "xmp")]
    pub embed_xmp: Option<bool>,
    #[cfg(feature = "xmp")]
    pub xmp_sidecar: Option<bool>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TomlFilters {
    /// Repeatable library selector. Accepts `primary`, `shared`, `all`,
    /// `none`, raw zone names, and `!name` exclusions.
    pub libraries: Option<Vec<String>>,
    pub albums: Option<Vec<String>>,
    /// Smart-folder selector. Same value grammar as `albums`.
    pub smart_folders: Option<Vec<String>>,
    /// Unfiled-pass toggle. Default: `true`.
    pub unfiled: Option<bool>,
    pub media: Option<Vec<MediaKind>>,
    pub filename_exclude: Option<Vec<String>>,
    pub recent: Option<crate::cli::RecentLimit>,
    pub recent_scope: Option<crate::cli::RecentScope>,
    pub skip_created_before: Option<String>,
    pub skip_created_after: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TomlPhotos {
    pub resolution: Option<PhotoResolution>,
    pub live_resolution: Option<LivePhotoResolution>,
    pub live_photo_mode: Option<LivePhotoMode>,
    pub live_photo_mov_filename_policy: Option<LivePhotoMovFilenamePolicy>,
    pub edited: Option<bool>,
    pub alternative: Option<bool>,
    pub raw_policy: Option<RawPolicy>,
    pub file_match_policy: Option<FileMatchPolicy>,
    pub force_resolution: Option<bool>,
    pub keep_unicode_in_filenames: Option<bool>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TomlImport {
    pub strict: Option<bool>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TomlWatch {
    pub interval: Option<u64>,
    pub notify_systemd: Option<bool>,
    pub pid_file: Option<String>,
    /// Run a full local-vs-state reconciliation walk every Nth watch cycle.
    /// `None` or `0` disables the periodic walk (the manual `kei reconcile`
    /// subcommand is unaffected). The walk is read-only: missing files are
    /// reported via `tracing::warn!` and never auto-marked failed in the
    /// state DB. The default is unset to preserve existing behaviour.
    pub reconcile_every_n_cycles: Option<u64>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TomlServer {
    pub port: Option<u16>,
    pub bind: Option<String>,
}

/// Load a TOML config file. Returns `Ok(None)` if the file doesn't exist
/// and `required` is false. Errors if the file doesn't exist and `required` is true.
pub(crate) fn load_toml_config(path: &Path, required: bool) -> anyhow::Result<Option<TomlConfig>> {
    use anyhow::Context;

    match std::fs::read_to_string(path) {
        Ok(contents) => {
            let config: TomlConfig = toml::from_str(&contents).map_err(|err| {
                let parse_error = err.to_string();
                let mut message = format!(
                    "Failed to parse config file {}: {parse_error}",
                    path.display()
                );
                if parse_error.contains("unknown field") {
                    message.push_str("\n\n");
                    message.push_str(&crate::upgrade_hints::toml_unknown_field_hint());
                }
                anyhow::anyhow!(message)
            })?;
            // Warn if config contains a password and file permissions are too open
            #[cfg(unix)]
            if config.auth.as_ref().is_some_and(|a| a.password.is_some()) {
                use std::os::unix::fs::MetadataExt;
                if let Ok(meta) = std::fs::metadata(path) {
                    let mode = meta.mode();
                    if mode & 0o077 != 0 {
                        tracing::warn!(
                            target: "kei::config",
                            path = %path.display(),
                            mode = format_args!("{mode:o}"),
                            "Config file contains password but is group/world-readable. \
                             Consider: chmod 600 {}",
                            path.display()
                        );
                    }
                }
            }
            Ok(Some(config))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && !required => Ok(None),
        Err(e) => Err(e).context(format!("Could not read config file {}", path.display()))?,
    }
}

#[cfg(test)]
mod tests {
    use super::{TomlConfig, load_toml_config};
    use crate::config::runtime::MediaKind;
    use crate::types::{
        Domain, FileMatchPolicy, LivePhotoMovFilenamePolicy, LivePhotoResolution, LogLevel,
        PhotoResolution, RawPolicy,
    };
    use std::collections::BTreeSet;
    use std::path::Path;

    fn documented_config_options() -> BTreeSet<&'static str> {
        [
            "data_dir",
            "log_level",
            "auth.username",
            "auth.password_file",
            "auth.password_command",
            "auth.domain",
            "download.directory",
            "download.folder_structure",
            "download.folder_structure_albums",
            "download.folder_structure_smart_folders",
            "download.threads",
            "download.bandwidth_limit",
            "download.temp_suffix",
            "download.retry.per_transfer",
            "download.retry.per_asset",
            "filters.libraries",
            "filters.albums",
            "filters.smart_folders",
            "filters.unfiled",
            "filters.media",
            "filters.filename_exclude",
            "filters.recent",
            "filters.recent_scope",
            "filters.skip_created_before",
            "filters.skip_created_after",
            "photos.resolution",
            "photos.live_resolution",
            "photos.live_photo_mode",
            "photos.live_photo_mov_filename_policy",
            "photos.edited",
            "photos.alternative",
            "photos.raw_policy",
            "photos.file_match_policy",
            "photos.force_resolution",
            "photos.keep_unicode_in_filenames",
            "metadata.set_exif_datetime",
            "metadata.set_exif_rating",
            "metadata.set_exif_gps",
            "metadata.set_exif_description",
            "metadata.embed_xmp",
            "metadata.xmp_sidecar",
            "watch.interval",
            "watch.notify_systemd",
            "watch.pid_file",
            "watch.reconcile_every_n_cycles",
            "notifications.script",
            "report.json",
            "server.bind",
            "server.port",
            "ui.friendly",
            "ui.progress_bar",
            "import.strict",
        ]
        .into_iter()
        .collect()
    }
    fn example_config_option_markers(raw: &str) -> BTreeSet<&str> {
        raw.lines()
            .filter_map(|line| line.trim().strip_prefix("# Option: "))
            .map(str::trim)
            .collect()
    }
    #[test]
    fn example_config_toml_parses() {
        let raw = include_str!("../../example.config.toml");
        #[cfg(feature = "xmp")]
        let body = raw.to_string();
        #[cfg(not(feature = "xmp"))]
        let body = raw
            .lines()
            .filter(|line| {
                let trimmed = line.trim_start();
                !trimmed.starts_with("embed_xmp") && !trimmed.starts_with("xmp_sidecar")
            })
            .collect::<Vec<_>>()
            .join("\n");
        let config: TomlConfig = toml::from_str(&body).unwrap();

        for (section, present) in [
            ("auth", config.auth.is_some()),
            ("download", config.download.is_some()),
            ("filters", config.filters.is_some()),
            ("photos", config.photos.is_some()),
            ("metadata", config.metadata.is_some()),
            ("watch", config.watch.is_some()),
            ("notifications", config.notifications.is_some()),
            ("report", config.report.is_some()),
            ("server", config.server.is_some()),
            ("ui", config.ui.is_some()),
            ("import", config.import.is_some()),
        ] {
            assert!(present, "example config should include [{section}]");
        }
    }
    #[test]
    fn example_config_documents_supported_options() {
        let raw = include_str!("../../example.config.toml");
        let documented = example_config_option_markers(raw);
        let expected = documented_config_options();
        assert_eq!(
            documented, expected,
            "example.config.toml # Option markers must stay in sync with supported TOML options"
        );
        assert!(
            !documented.contains("auth.password"),
            "plaintext [auth].password must not be listed as a supported option"
        );
        assert!(
            raw.contains("Plaintext [auth].password is not a supported config option"),
            "example config must keep the plaintext password migration note"
        );
    }
    #[test]
    fn test_toml_parse_empty() {
        let config: TomlConfig = toml::from_str("").unwrap();
        assert!(config.auth.is_none());
        assert!(config.download.is_none());
        assert!(config.filters.is_none());
        assert!(config.photos.is_none());
        assert!(config.watch.is_none());
        assert!(config.log_level.is_none());
    }
    #[test]
    fn test_toml_parse_minimal() {
        let toml_str = r#"
            [auth]
            username = "test@example.com"
        "#;
        let config: TomlConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(
            config.auth.as_ref().unwrap().username.as_deref(),
            Some("test@example.com")
        );
    }
    #[test]
    fn test_toml_parse_full() {
        let toml_str = r#"
            log_level = "debug"

            [auth]
            username = "user@example.com"
            domain = "com"

            [download]
            directory = "/photos"
            folder_structure = "%Y/%m/%d"
            threads = 10
            temp_suffix = ".kei-tmp"

            [download.retry]
            per_transfer = 3

            [filters]
            libraries = ["PrimarySync"]
            albums = ["Favorites"]
            media = ["photos", "videos", "live-photos"]
            recent = 500
            skip_created_before = "2024-01-01"
            skip_created_after = "2025-01-01"

            [photos]
            resolution = "original"
            live_resolution = "original"
            live_photo_mov_filename_policy = "suffix"
            raw_policy = "as-is"
            file_match_policy = "name-size-dedup-with-suffix"
            force_resolution = false
            keep_unicode_in_filenames = false

            [watch]
            interval = 3600
            notify_systemd = false
            pid_file = "/run/kei.pid"
        "#;
        let config: TomlConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.log_level, Some(LogLevel::Debug));
        let auth = config.auth.unwrap();
        assert_eq!(auth.username.as_deref(), Some("user@example.com"));
        assert_eq!(auth.domain, Some(Domain::Com));
        let dl = config.download.unwrap();
        assert_eq!(dl.threads, Some(10));

        let retry = dl.retry.unwrap();
        assert_eq!(retry.per_transfer, Some(3));

        let filters = config.filters.unwrap();
        assert_eq!(filters.albums, Some(vec!["Favorites".to_string()]));
        assert_eq!(filters.recent, Some(crate::cli::RecentLimit::Count(500)));
        let photos = config.photos.unwrap();
        assert_eq!(photos.resolution, Some(PhotoResolution::Original));
        assert_eq!(photos.raw_policy, Some(RawPolicy::AsIs));
        assert_eq!(
            photos.file_match_policy,
            Some(FileMatchPolicy::NameSizeDedupWithSuffix)
        );
        let watch = config.watch.unwrap();
        assert_eq!(watch.interval, Some(3600));
    }
    #[test]
    fn test_toml_reject_unknown_fields() {
        let toml_str = r#"
            [auth]
            username = "test@example.com"
            bogus_field = true
        "#;
        assert!(toml::from_str::<TomlConfig>(toml_str).is_err());
    }
    #[test]
    fn test_toml_parse_enum_values() {
        let toml_str = r#"
            [photos]
            resolution = "medium"
            raw_policy = "prefer-jpeg"
            file_match_policy = "name-id7"
        "#;
        let config: TomlConfig = toml::from_str(toml_str).unwrap();
        let photos = config.photos.unwrap();
        assert_eq!(photos.resolution, Some(PhotoResolution::Medium));
        assert_eq!(photos.raw_policy, Some(RawPolicy::PreferJpeg));
        assert_eq!(photos.file_match_policy, Some(FileMatchPolicy::NameId7));
    }
    #[test]
    fn test_toml_nested_retry() {
        let toml_str = r#"
            [download.retry]
            per_transfer = 5
        "#;
        let config: TomlConfig = toml::from_str(toml_str).unwrap();
        let retry = config.download.unwrap().retry.unwrap();
        assert_eq!(retry.per_transfer, Some(5));
    }
    #[test]
    fn test_load_toml_config_missing_file_not_required() {
        let result = load_toml_config(Path::new("/nonexistent/path/config.toml"), false).unwrap();
        assert!(result.is_none());
    }
    #[test]
    fn test_load_toml_config_missing_file_required() {
        let result = load_toml_config(Path::new("/nonexistent/path/config.toml"), true);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("Could not read config file"),
            "Error should mention config file: {err}"
        );
    }
    #[test]
    fn test_toml_parse_all_resolution_variants() {
        for (input, expected) in [
            ("original", PhotoResolution::Original),
            ("medium", PhotoResolution::Medium),
            ("thumb", PhotoResolution::Thumb),
            ("none", PhotoResolution::None),
        ] {
            let toml_str = format!("[photos]\nresolution = \"{input}\"\nedited = true");
            let config: TomlConfig = toml::from_str(&toml_str).unwrap();
            assert_eq!(
                config.photos.unwrap().resolution,
                Some(expected),
                "resolution variant: {input}"
            );
        }
    }
    #[test]
    fn test_toml_parse_all_live_resolution_variants() {
        for (input, expected) in [
            ("original", LivePhotoResolution::Original),
            ("medium", LivePhotoResolution::Medium),
            ("thumb", LivePhotoResolution::Thumb),
        ] {
            let toml_str = format!("[photos]\nlive_resolution = \"{input}\"");
            let config: TomlConfig = toml::from_str(&toml_str).unwrap();
            assert_eq!(
                config.photos.unwrap().live_resolution,
                Some(expected),
                "live_resolution variant: {input}"
            );
        }
    }
    #[test]
    fn test_toml_parse_all_domain_variants() {
        for (input, expected) in [("com", Domain::Com), ("cn", Domain::Cn)] {
            let toml_str = format!("[auth]\ndomain = \"{input}\"");
            let config: TomlConfig = toml::from_str(&toml_str).unwrap();
            assert_eq!(
                config.auth.unwrap().domain,
                Some(expected),
                "domain variant: {input}"
            );
        }
    }
    #[test]
    fn test_toml_parse_all_log_level_variants() {
        for (input, expected) in [
            ("debug", LogLevel::Debug),
            ("info", LogLevel::Info),
            ("warn", LogLevel::Warn),
            ("error", LogLevel::Error),
        ] {
            let toml_str = format!("log_level = \"{input}\"");
            let config: TomlConfig = toml::from_str(&toml_str).unwrap();
            assert_eq!(
                config.log_level,
                Some(expected),
                "log_level variant: {input}"
            );
        }
    }
    #[test]
    fn test_toml_parse_all_mov_filename_policy_variants() {
        for (input, expected) in [
            ("suffix", LivePhotoMovFilenamePolicy::Suffix),
            ("original", LivePhotoMovFilenamePolicy::Original),
        ] {
            let toml_str = format!("[photos]\nlive_photo_mov_filename_policy = \"{input}\"");
            let config: TomlConfig = toml::from_str(&toml_str).unwrap();
            assert_eq!(
                config.photos.unwrap().live_photo_mov_filename_policy,
                Some(expected),
                "mov policy variant: {input}"
            );
        }
    }
    #[test]
    fn test_toml_parse_all_raw_policy_variants() {
        for (input, expected) in [
            ("as-is", RawPolicy::AsIs),
            ("prefer-raw", RawPolicy::PreferRaw),
            ("prefer-jpeg", RawPolicy::PreferJpeg),
        ] {
            let toml_str = format!("[photos]\nraw_policy = \"{input}\"");
            let config: TomlConfig = toml::from_str(&toml_str).unwrap();
            assert_eq!(
                config.photos.unwrap().raw_policy,
                Some(expected),
                "raw_policy variant: {input}"
            );
        }
    }
    #[test]
    fn test_toml_parse_all_file_match_policy_variants() {
        for (input, expected) in [
            (
                "name-size-dedup-with-suffix",
                FileMatchPolicy::NameSizeDedupWithSuffix,
            ),
            ("name-id7", FileMatchPolicy::NameId7),
        ] {
            let toml_str = format!("[photos]\nfile_match_policy = \"{input}\"");
            let config: TomlConfig = toml::from_str(&toml_str).unwrap();
            assert_eq!(
                config.photos.unwrap().file_match_policy,
                Some(expected),
                "file_match_policy variant: {input}"
            );
        }
    }
    #[test]
    fn test_toml_reject_invalid_enum_value() {
        let toml_str = r#"
            [photos]
            resolution = "huge"
        "#;
        assert!(toml::from_str::<TomlConfig>(toml_str).is_err());
    }
    #[test]
    fn test_toml_reject_wrong_type() {
        let toml_str = r#"
            [download]
            threads = "not_a_number"
        "#;
        assert!(toml::from_str::<TomlConfig>(toml_str).is_err());
    }
    #[test]
    fn test_toml_reject_negative_number() {
        let toml_str = r#"
            [download]
            threads = -1
        "#;
        assert!(toml::from_str::<TomlConfig>(toml_str).is_err());
    }
    #[test]
    fn test_toml_reject_unknown_fields_in_each_section() {
        for (section, field) in [
            ("[download]\nbogus = 1", "download"),
            ("[download.retry]\nbogus = 1", "download.retry"),
            ("[filters]\nbogus = true", "filters"),
            ("[photos]\nbogus = true", "photos"),
            ("[watch]\nbogus = 1", "watch"),
            ("[notifications]\nbogus = true", "notifications"),
            ("bogus = true", "top-level"),
        ] {
            assert!(
                toml::from_str::<TomlConfig>(section).is_err(),
                "should reject unknown field in {field}"
            );
        }
    }
    #[test]
    fn test_toml_empty_sections_accepted() {
        let toml_str = r#"
            [auth]
            [download]
            [filters]
            [photos]
            [watch]
            [notifications]
        "#;
        let config: TomlConfig = toml::from_str(toml_str).unwrap();
        assert!(config.auth.unwrap().username.is_none());
        assert!(config.download.unwrap().threads.is_none());
        assert!(config.filters.unwrap().libraries.is_none());
        assert!(config.photos.unwrap().resolution.is_none());
        assert!(config.watch.unwrap().interval.is_none());
        assert!(config.notifications.unwrap().script.is_none());
    }
    #[test]
    fn test_toml_download_all_fields() {
        let toml_str = r#"
            [download]
            directory = "/photos"
            folder_structure = "%Y-%m"
            threads = 4
            temp_suffix = ".part"

            [metadata]
            set_exif_datetime = true

            [ui]
            progress_bar = false
        "#;
        let config: TomlConfig = toml::from_str(toml_str).unwrap();
        let dl = config.download.unwrap();
        assert_eq!(dl.directory.as_deref(), Some("/photos"));
        assert_eq!(dl.folder_structure.as_deref(), Some("%Y-%m"));
        assert_eq!(dl.threads, Some(4));
        assert_eq!(dl.temp_suffix.as_deref(), Some(".part"));
        assert_eq!(config.metadata.unwrap().set_exif_datetime, Some(true));
        assert_eq!(config.ui.unwrap().progress_bar, Some(false));
    }
    #[test]
    fn test_toml_filters_all_fields() {
        let toml_str = r#"
            [filters]
            libraries = ["SharedSync-ABC"]
            albums = ["A", "B"]
            media = ["photos", "videos", "live-photos"]
            recent = 100
            skip_created_before = "2024-01-01"
            skip_created_after = "2025-12-31"
        "#;
        let config: TomlConfig = toml::from_str(toml_str).unwrap();
        let f = config.filters.unwrap();
        assert_eq!(
            f.libraries.as_deref(),
            Some(&["SharedSync-ABC".to_string()][..])
        );
        assert_eq!(f.albums, Some(vec!["A".to_string(), "B".to_string()]));
        assert_eq!(
            f.media.as_deref(),
            Some(&[MediaKind::Photos, MediaKind::Videos, MediaKind::LivePhotos,][..])
        );

        assert_eq!(f.recent, Some(crate::cli::RecentLimit::Count(100)));
        assert_eq!(f.skip_created_before.as_deref(), Some("2024-01-01"));
        assert_eq!(f.skip_created_after.as_deref(), Some("2025-12-31"));
    }
    #[test]
    fn test_toml_photos_all_fields() {
        let toml_str = r#"
            [photos]
            resolution = "thumb"
            live_resolution = "medium"
            live_photo_mov_filename_policy = "original"
            raw_policy = "prefer-raw"
            file_match_policy = "name-id7"
            force_resolution = true
            keep_unicode_in_filenames = true
        "#;
        let config: TomlConfig = toml::from_str(toml_str).unwrap();
        let p = config.photos.unwrap();
        assert_eq!(p.resolution, Some(PhotoResolution::Thumb));
        assert_eq!(p.live_resolution, Some(LivePhotoResolution::Medium));
        assert_eq!(
            p.live_photo_mov_filename_policy,
            Some(LivePhotoMovFilenamePolicy::Original)
        );
        assert_eq!(p.raw_policy, Some(RawPolicy::PreferRaw));
        assert_eq!(p.file_match_policy, Some(FileMatchPolicy::NameId7));
        assert_eq!(p.force_resolution, Some(true));
        assert_eq!(p.keep_unicode_in_filenames, Some(true));
    }
    #[test]
    fn test_toml_watch_all_fields() {
        let toml_str = r#"
            [watch]
            interval = 1800
            notify_systemd = true
            pid_file = "/run/test.pid"
        "#;
        let config: TomlConfig = toml::from_str(toml_str).unwrap();
        let w = config.watch.unwrap();
        assert_eq!(w.interval, Some(1800));
        assert_eq!(w.notify_systemd, Some(true));
        assert_eq!(w.pid_file.as_deref(), Some("/run/test.pid"));
    }
    #[test]
    fn test_toml_server_port_parsed() {
        let toml_str = r#"
            [server]
            port = 9090
        "#;
        let config: TomlConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.server.unwrap().port, Some(9090));
    }
    #[test]
    fn test_toml_server_unknown_field_rejected() {
        let toml_str = r#"
            [server]
            port = 9090
            unknown_field = true
        "#;
        let result: Result<TomlConfig, _> = toml::from_str(toml_str);
        assert!(
            result.is_err(),
            "unknown fields in [server] should be rejected"
        );
    }
    #[test]
    fn test_toml_metrics_section_is_removed() {
        let toml_str = r#"
            [metrics]
            port = 9090
            unknown_field = true
        "#;
        let result: Result<TomlConfig, _> = toml::from_str(toml_str);
        assert!(result.is_err(), "[metrics] should be rejected");
    }
    #[test]
    fn test_load_toml_config_valid_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.toml");
        std::fs::write(
            &path,
            r#"
            [auth]
            username = "disk@example.com"
            "#,
        )
        .unwrap();
        let result = load_toml_config(&path, false).unwrap();
        assert!(result.is_some());
        assert_eq!(
            result.unwrap().auth.unwrap().username.as_deref(),
            Some("disk@example.com")
        );
    }
    #[test]
    fn test_load_toml_config_valid_file_required() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test-required.toml");
        std::fs::write(&path, "log_level = \"warn\"").unwrap();
        let result = load_toml_config(&path, true).unwrap();
        assert!(result.is_some());
        assert_eq!(result.unwrap().log_level, Some(LogLevel::Warn));
    }
    #[test]
    fn test_load_toml_config_invalid_toml_syntax() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad-syntax.toml");
        std::fs::write(&path, "this is not valid toml [[[").unwrap();
        let result = load_toml_config(&path, false);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("Failed to parse config file"), "got: {err}");
    }
    #[test]
    fn test_load_toml_config_empty_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("empty.toml");
        std::fs::write(&path, "").unwrap();
        let result = load_toml_config(&path, false).unwrap();
        let config = result.unwrap();
        assert!(config.auth.is_none());
        assert!(config.download.is_none());
    }
    #[test]
    fn old_photo_toml_names_are_rejected() {
        for (key, value) in [
            ("size", "\"original\""),
            ("live_photo_size", "\"original\""),
            ("align_raw", "\"as-is\""),
            ("force_size", "true"),
        ] {
            let toml = format!("[photos]\n{key} = {value}\n");
            let err = toml::from_str::<TomlConfig>(&toml)
                .expect_err(&format!("old photos.{key} key should hard-error"));
            assert!(
                err.message().contains(&format!("unknown field `{key}`")),
                "unexpected error for {key}: {err}"
            );
        }
    }
    #[test]
    fn old_retry_toml_names_are_rejected() {
        for key in ["max_retries", "max_download_attempts"] {
            let toml = format!("[download.retry]\n{key} = 3\n");
            let err = toml::from_str::<TomlConfig>(&toml)
                .expect_err(&format!("old download.retry.{key} key should hard-error"));
            assert!(
                err.message().contains(&format!("unknown field `{key}`")),
                "unexpected error for {key}: {err}"
            );
        }
    }
    #[test]
    fn old_download_metadata_and_progress_names_are_rejected() {
        for key in [
            "set_exif_datetime",
            "set_exif_rating",
            "set_exif_gps",
            "set_exif_description",
            "embed_xmp",
            "xmp_sidecar",
            "no_progress_bar",
        ] {
            let toml = format!("[download]\n{key} = true\n");
            let err = toml::from_str::<TomlConfig>(&toml)
                .expect_err(&format!("old download.{key} key should hard-error"));
            assert!(
                err.message().contains(&format!("unknown field `{key}`")),
                "unexpected error for {key}: {err}"
            );
        }
    }
    #[test]
    fn test_toml_notifications_section() {
        let toml_str = r#"
            [notifications]
            script = "/path/to/hook.sh"
        "#;
        let config: TomlConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(
            config.notifications.unwrap().script.as_deref(),
            Some("/path/to/hook.sh")
        );
    }
    #[test]
    fn test_toml_ui_parses_friendly_key() {
        let parsed: TomlConfig = toml::from_str("[ui]\nfriendly = false\n").unwrap();
        assert_eq!(parsed.ui.unwrap().friendly, Some(false));

        let parsed: TomlConfig = toml::from_str("[ui]\nfriendly = true\n").unwrap();
        assert_eq!(parsed.ui.unwrap().friendly, Some(true));

        let parsed: TomlConfig = toml::from_str("[ui]\nprogress_bar = false\n").unwrap();
        assert_eq!(parsed.ui.unwrap().progress_bar, Some(false));

        let empty: TomlConfig = toml::from_str("").unwrap();
        assert!(empty.ui.is_none());
    }
    #[test]
    fn test_toml_ui_rejects_unknown_keys() {
        // `deny_unknown_fields` is the standard guard against typos like
        // `friendlly`. Lock the behaviour in so a future refactor can't
        // silently drop it.
        let err = toml::from_str::<TomlConfig>("[ui]\nfriend = true\n")
            .expect_err("unknown key in [ui] must error");
        assert!(
            err.to_string().contains("unknown field"),
            "error must mention unknown field, got: {err}"
        );
    }
    #[test]
    fn removed_filter_aliases_are_rejected() {
        for (field, toml_str) in [
            ("album", "[filters]\nalbum = \"Vacation\"\n"),
            (
                "exclude_albums",
                "[filters]\nexclude_albums = [\"Hidden\", \"Trash\"]\n",
            ),
            ("library", "[filters]\nlibrary = \"SharedSync-ABC\"\n"),
        ] {
            let err = toml::from_str::<TomlConfig>(toml_str).unwrap_err();
            assert!(
                err.to_string()
                    .contains(&format!("unknown field `{field}`")),
                "unexpected error for {field}: {err}"
            );
        }
    }
}
