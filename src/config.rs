use crate::types::{
    Domain, FileMatchPolicy, LivePhotoMovFilenamePolicy, LivePhotoSize, LogLevel,
    RawTreatmentPolicy, VersionSize,
};
use chrono::{DateTime, Local, NaiveDate, NaiveDateTime};
use serde::Deserialize;
use std::path::{Path, PathBuf};

// ── TOML config structs ─────────────────────────────────────────────

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TomlConfig {
    pub log_level: Option<LogLevel>,
    pub auth: Option<TomlAuth>,
    pub download: Option<TomlDownload>,
    pub filters: Option<TomlFilters>,
    pub photos: Option<TomlPhotos>,
    pub watch: Option<TomlWatch>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TomlAuth {
    pub username: Option<String>,
    pub password: Option<String>,
    pub domain: Option<Domain>,
    pub cookie_directory: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TomlDownload {
    pub directory: Option<String>,
    pub folder_structure: Option<String>,
    pub threads_num: Option<u16>,
    pub temp_suffix: Option<String>,
    pub set_exif_datetime: Option<bool>,
    pub no_progress_bar: Option<bool>,
    pub retry: Option<TomlRetry>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TomlRetry {
    pub max_retries: Option<u32>,
    pub delay: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TomlFilters {
    pub library: Option<String>,
    pub albums: Option<Vec<String>>,
    pub skip_videos: Option<bool>,
    pub skip_photos: Option<bool>,
    pub skip_live_photos: Option<bool>,
    pub recent: Option<u32>,
    pub skip_created_before: Option<String>,
    pub skip_created_after: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TomlPhotos {
    pub size: Option<VersionSize>,
    pub live_photo_size: Option<LivePhotoSize>,
    pub live_photo_mov_filename_policy: Option<LivePhotoMovFilenamePolicy>,
    pub align_raw: Option<RawTreatmentPolicy>,
    pub file_match_policy: Option<FileMatchPolicy>,
    pub force_size: Option<bool>,
    pub keep_unicode_in_filenames: Option<bool>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TomlWatch {
    pub interval: Option<u64>,
    pub notify_systemd: Option<bool>,
    pub pid_file: Option<String>,
}

/// Load a TOML config file. Returns `Ok(None)` if the file doesn't exist.
pub(crate) fn load_toml_config(path: &Path) -> anyhow::Result<Option<TomlConfig>> {
    use anyhow::Context;

    match std::fs::read_to_string(path) {
        Ok(contents) => {
            let config: TomlConfig = toml::from_str(&contents)
                .context(format!("Failed to parse config file {}", path.display()))?;
            Ok(Some(config))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).context(format!("Failed to read config file {}", path.display()))?,
    }
}

// ── Application Config ──────────────────────────────────────────────

/// Application configuration.
///
/// Fields are ordered for optimal memory layout:
/// - Heap types first (String, PathBuf, Vec, `Option<String>`)
/// - DateTime fields (12-16 bytes each)
/// - 8-byte primitives (u64, `Option<u64>`)
/// - 4-byte primitives (u32, `Option<u32>`)
/// - 2-byte primitives (u16)
/// - 1-byte enums
/// - All booleans grouped at the end
pub struct Config {
    // Heap types first
    pub username: String,
    pub password: Option<String>,
    pub directory: PathBuf,
    pub cookie_directory: PathBuf,
    pub folder_structure: String,
    pub albums: Vec<String>,
    pub library: String,
    pub temp_suffix: String,

    // DateTime fields
    pub skip_created_before: Option<DateTime<Local>>,
    pub skip_created_after: Option<DateTime<Local>>,

    // Optional paths
    pub pid_file: Option<PathBuf>,

    // 8-byte primitives
    pub watch_with_interval: Option<u64>,
    pub retry_delay_secs: u64,

    // 4-byte primitives
    pub recent: Option<u32>,
    pub max_retries: u32,

    // 2-byte primitives
    pub threads_num: u16,

    // 1-byte enums
    pub size: VersionSize,
    pub live_photo_size: LivePhotoSize,
    pub domain: Domain,
    #[allow(dead_code)] // Copied from CLI but read from cli.log_level directly in main.rs
    pub log_level: LogLevel,
    pub live_photo_mov_filename_policy: LivePhotoMovFilenamePolicy,
    pub align_raw: RawTreatmentPolicy,
    pub file_match_policy: FileMatchPolicy,

    // All booleans grouped together
    pub auth_only: bool,
    pub list_albums: bool,
    pub list_libraries: bool,
    pub skip_videos: bool,
    pub skip_photos: bool,
    pub skip_live_photos: bool,
    pub force_size: bool,
    pub set_exif_datetime: bool,
    pub dry_run: bool,
    pub no_progress_bar: bool,
    pub keep_unicode_in_filenames: bool,
    #[allow(dead_code)] // CLI flag parsed but not yet wired
    pub only_print_filenames: bool,
    pub notify_systemd: bool,
}

impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config")
            .field("username", &self.username)
            .field("password", &"<redacted>")
            .field("directory", &self.directory)
            .field("auth_only", &self.auth_only)
            .field("list_albums", &self.list_albums)
            .field("domain", &self.domain)
            .field("cookie_directory", &self.cookie_directory)
            .finish_non_exhaustive()
    }
}

pub(crate) fn expand_tilde(path: &str) -> PathBuf {
    if let Some(stripped) = path.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(stripped);
        }
    }
    PathBuf::from(path)
}

/// Pick CLI value, then TOML value, then hardcoded default.
fn resolve<T>(cli: Option<T>, toml: Option<T>, default: T) -> T {
    cli.or(toml).unwrap_or(default)
}

/// For boolean flags: CLI flag present (true) wins, else TOML, else false.
fn resolve_flag(cli_flag: bool, toml_val: Option<bool>) -> bool {
    cli_flag || toml_val.unwrap_or(false)
}

/// Resolve auth fields from CLI auth args + optional TOML config.
/// Returns (username, password, domain, cookie_directory).
pub(crate) fn resolve_auth(
    auth: &crate::cli::AuthArgs,
    toml: &Option<TomlConfig>,
) -> (String, Option<String>, Domain, PathBuf) {
    let toml_auth = toml.as_ref().and_then(|t| t.auth.as_ref());

    let username = if auth.username.is_empty() {
        toml_auth
            .and_then(|a| a.username.clone())
            .unwrap_or_default()
    } else {
        auth.username.clone()
    };

    let password = auth
        .password
        .clone()
        .or_else(|| toml_auth.and_then(|a| a.password.clone()));

    let domain = resolve(auth.domain, toml_auth.and_then(|a| a.domain), Domain::Com);

    let cookie_dir_str = resolve(
        auth.cookie_directory.clone(),
        toml_auth.and_then(|a| a.cookie_directory.clone()),
        "~/.icloudpd-rs".to_string(),
    );
    let cookie_directory = expand_tilde(&cookie_dir_str);

    (username, password, domain, cookie_directory)
}

impl Config {
    /// Build a Config by merging CLI args with optional TOML config.
    /// Resolution order: CLI > TOML > hardcoded default.
    pub fn build(
        auth: crate::cli::AuthArgs,
        sync: crate::cli::SyncArgs,
        log_level: Option<LogLevel>,
        toml: Option<TomlConfig>,
    ) -> anyhow::Result<Self> {
        let (username, password, domain, cookie_directory) = resolve_auth(&auth, &toml);

        let toml_dl = toml.as_ref().and_then(|t| t.download.as_ref());
        let toml_retry = toml_dl.and_then(|d| d.retry.as_ref());
        let toml_filters = toml.as_ref().and_then(|t| t.filters.as_ref());
        let toml_photos = toml.as_ref().and_then(|t| t.photos.as_ref());
        let toml_watch = toml.as_ref().and_then(|t| t.watch.as_ref());

        // Download
        let directory = sync
            .directory
            .or_else(|| toml_dl.and_then(|d| d.directory.clone()))
            .map(|d| expand_tilde(&d))
            .unwrap_or_default();
        let folder_structure = resolve(
            sync.folder_structure,
            toml_dl.and_then(|d| d.folder_structure.clone()),
            "%Y/%m/%d".to_string(),
        );
        let threads_num = resolve(sync.threads_num, toml_dl.and_then(|d| d.threads_num), 10);
        anyhow::ensure!(
            threads_num >= 1,
            "threads_num must be >= 1, got {}",
            threads_num
        );
        let temp_suffix = resolve(
            sync.temp_suffix,
            toml_dl.and_then(|d| d.temp_suffix.clone()),
            ".icloudpd-tmp".to_string(),
        );
        let set_exif_datetime = resolve_flag(
            sync.set_exif_datetime,
            toml_dl.and_then(|d| d.set_exif_datetime),
        );
        let no_progress_bar = resolve_flag(
            sync.no_progress_bar,
            toml_dl.and_then(|d| d.no_progress_bar),
        );

        // Retry
        let max_retries = resolve(sync.max_retries, toml_retry.and_then(|r| r.max_retries), 3);
        let retry_delay_secs = resolve(sync.retry_delay, toml_retry.and_then(|r| r.delay), 5);

        // Filters
        let library = resolve(
            sync.library,
            toml_filters.and_then(|f| f.library.clone()),
            "PrimarySync".to_string(),
        );
        let albums = if sync.albums.is_empty() {
            toml_filters
                .and_then(|f| f.albums.clone())
                .unwrap_or_default()
        } else {
            sync.albums
        };
        let skip_videos = resolve_flag(sync.skip_videos, toml_filters.and_then(|f| f.skip_videos));
        let skip_photos = resolve_flag(sync.skip_photos, toml_filters.and_then(|f| f.skip_photos));
        let skip_live_photos = resolve_flag(
            sync.skip_live_photos,
            toml_filters.and_then(|f| f.skip_live_photos),
        );
        let recent = sync.recent.or_else(|| toml_filters.and_then(|f| f.recent));
        let skip_created_before_str = sync
            .skip_created_before
            .or_else(|| toml_filters.and_then(|f| f.skip_created_before.clone()));
        let skip_created_after_str = sync
            .skip_created_after
            .or_else(|| toml_filters.and_then(|f| f.skip_created_after.clone()));

        let skip_created_before = skip_created_before_str
            .as_deref()
            .map(parse_date_or_interval)
            .transpose()?;
        let skip_created_after = skip_created_after_str
            .as_deref()
            .map(parse_date_or_interval)
            .transpose()?;

        // Photos
        let size = resolve(
            sync.size,
            toml_photos.and_then(|p| p.size),
            VersionSize::Original,
        );
        let live_photo_size = resolve(
            sync.live_photo_size,
            toml_photos.and_then(|p| p.live_photo_size),
            LivePhotoSize::Original,
        );
        let live_photo_mov_filename_policy = resolve(
            sync.live_photo_mov_filename_policy,
            toml_photos.and_then(|p| p.live_photo_mov_filename_policy),
            LivePhotoMovFilenamePolicy::Suffix,
        );
        let align_raw = resolve(
            sync.align_raw,
            toml_photos.and_then(|p| p.align_raw),
            RawTreatmentPolicy::Unchanged,
        );
        let file_match_policy = resolve(
            sync.file_match_policy,
            toml_photos.and_then(|p| p.file_match_policy),
            FileMatchPolicy::NameSizeDedupWithSuffix,
        );
        let force_size = resolve_flag(sync.force_size, toml_photos.and_then(|p| p.force_size));
        let keep_unicode_in_filenames = resolve_flag(
            sync.keep_unicode_in_filenames,
            toml_photos.and_then(|p| p.keep_unicode_in_filenames),
        );

        // Watch
        let watch_with_interval = sync
            .watch_with_interval
            .or_else(|| toml_watch.and_then(|w| w.interval));
        let notify_systemd = resolve_flag(
            sync.notify_systemd,
            toml_watch.and_then(|w| w.notify_systemd),
        );
        let pid_file = sync.pid_file.or_else(|| {
            toml_watch
                .and_then(|w| w.pid_file.as_ref())
                .map(PathBuf::from)
        });

        // Log level
        let resolved_log_level = resolve(
            log_level,
            toml.as_ref().and_then(|t| t.log_level),
            LogLevel::Info,
        );

        Ok(Self {
            username,
            password,
            directory,
            cookie_directory,
            folder_structure,
            albums,
            library,
            temp_suffix,
            skip_created_before,
            skip_created_after,
            pid_file,
            watch_with_interval,
            retry_delay_secs,
            recent,
            max_retries,
            threads_num,
            size,
            live_photo_size,
            domain,
            log_level: resolved_log_level,
            live_photo_mov_filename_policy,
            align_raw,
            file_match_policy,
            auth_only: sync.auth_only,
            list_albums: sync.list_albums,
            list_libraries: sync.list_libraries,
            skip_videos,
            skip_photos,
            skip_live_photos,
            force_size,
            set_exif_datetime,
            dry_run: sync.dry_run,
            no_progress_bar,
            keep_unicode_in_filenames,
            only_print_filenames: sync.only_print_filenames,
            notify_systemd,
        })
    }
}

/// Parse a human-friendly date spec into a concrete timestamp.
///
/// Supports three formats to match the Python CLI's behavior:
/// - Relative interval: `"20d"` (20 days ago from now)
/// - ISO date: `"2025-01-02"` (midnight local time)
/// - ISO datetime: `"2025-01-02T14:30:00"` (local time)
pub(crate) fn parse_date_or_interval(s: &str) -> anyhow::Result<DateTime<Local>> {
    if let Some(days_str) = s.strip_suffix('d') {
        if let Ok(days) = days_str.parse::<i64>() {
            return Ok(Local::now() - chrono::Duration::days(days));
        }
    }
    if let Ok(date) = NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        if let Some(naive_dt) = date.and_hms_opt(0, 0, 0) {
            if let Some(dt) = naive_dt.and_local_timezone(Local).single() {
                return Ok(dt);
            }
        }
    }
    if let Ok(dt) = NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S") {
        if let Some(local) = dt.and_local_timezone(Local).single() {
            return Ok(local);
        }
    }
    anyhow::bail!(
        "Cannot parse '{}' as a date. Expected ISO date (2025-01-02), \
         datetime (2025-01-02T14:30:00), or interval (20d)",
        s
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::{AuthArgs, SyncArgs};

    #[test]
    fn test_expand_tilde_with_home() {
        let result = expand_tilde("~/Documents");
        if let Some(home) = dirs::home_dir() {
            assert_eq!(result, home.join("Documents"));
        }
    }

    #[test]
    fn test_expand_tilde_no_prefix() {
        assert_eq!(
            expand_tilde("/absolute/path"),
            PathBuf::from("/absolute/path")
        );
        assert_eq!(
            expand_tilde("relative/path"),
            PathBuf::from("relative/path")
        );
    }

    #[test]
    fn test_parse_date_iso() {
        let dt = parse_date_or_interval("2025-01-15").unwrap();
        assert_eq!(
            dt.date_naive(),
            NaiveDate::from_ymd_opt(2025, 1, 15).unwrap()
        );
    }

    #[test]
    fn test_parse_datetime_iso() {
        let dt = parse_date_or_interval("2025-06-15T14:30:00").unwrap();
        let naive = dt.naive_local();
        assert_eq!(naive.date(), NaiveDate::from_ymd_opt(2025, 6, 15).unwrap());
        assert_eq!(
            naive.time(),
            chrono::NaiveTime::from_hms_opt(14, 30, 0).unwrap()
        );
    }

    #[test]
    fn test_parse_interval_days() {
        let before = chrono::Local::now();
        let dt = parse_date_or_interval("10d").unwrap();
        let after = chrono::Local::now();
        let expected = before - chrono::Duration::days(10);
        assert!(dt >= expected - chrono::Duration::seconds(1));
        assert!(dt <= after - chrono::Duration::days(10) + chrono::Duration::seconds(1));
    }

    #[test]
    fn test_parse_invalid_date() {
        assert!(parse_date_or_interval("not-a-date").is_err());
        assert!(parse_date_or_interval("").is_err());
    }

    // ── TOML parsing tests ──────────────────────────────────────────

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
            cookie_directory = "~/.icloudpd-rs"

            [download]
            directory = "/photos"
            folder_structure = "%Y/%m/%d"
            threads_num = 10
            temp_suffix = ".icloudpd-tmp"
            set_exif_datetime = true
            no_progress_bar = false

            [download.retry]
            max_retries = 3
            delay = 5

            [filters]
            library = "PrimarySync"
            albums = ["Favorites"]
            skip_videos = false
            skip_photos = false
            skip_live_photos = false
            recent = 500
            skip_created_before = "2024-01-01"
            skip_created_after = "2025-01-01"

            [photos]
            size = "original"
            live_photo_size = "original"
            live_photo_mov_filename_policy = "suffix"
            align_raw = "as-is"
            file_match_policy = "name-size-dedup-with-suffix"
            force_size = false
            keep_unicode_in_filenames = false

            [watch]
            interval = 3600
            notify_systemd = false
            pid_file = "/run/icloudpd-rs.pid"
        "#;
        let config: TomlConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.log_level, Some(LogLevel::Debug));
        let auth = config.auth.unwrap();
        assert_eq!(auth.username.as_deref(), Some("user@example.com"));
        assert_eq!(auth.domain, Some(Domain::Com));
        let dl = config.download.unwrap();
        assert_eq!(dl.threads_num, Some(10));
        let retry = dl.retry.unwrap();
        assert_eq!(retry.max_retries, Some(3));
        assert_eq!(retry.delay, Some(5));
        let filters = config.filters.unwrap();
        assert_eq!(filters.albums, Some(vec!["Favorites".to_string()]));
        assert_eq!(filters.recent, Some(500));
        let photos = config.photos.unwrap();
        assert_eq!(photos.size, Some(VersionSize::Original));
        assert_eq!(photos.align_raw, Some(RawTreatmentPolicy::Unchanged));
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
            size = "medium"
            align_raw = "alternative"
            file_match_policy = "name-id7"
        "#;
        let config: TomlConfig = toml::from_str(toml_str).unwrap();
        let photos = config.photos.unwrap();
        assert_eq!(photos.size, Some(VersionSize::Medium));
        assert_eq!(
            photos.align_raw,
            Some(RawTreatmentPolicy::PreferAlternative)
        );
        assert_eq!(photos.file_match_policy, Some(FileMatchPolicy::NameId7));
    }

    #[test]
    fn test_toml_nested_retry() {
        let toml_str = r#"
            [download.retry]
            max_retries = 5
            delay = 10
        "#;
        let config: TomlConfig = toml::from_str(toml_str).unwrap();
        let retry = config.download.unwrap().retry.unwrap();
        assert_eq!(retry.max_retries, Some(5));
        assert_eq!(retry.delay, Some(10));
    }

    #[test]
    fn test_load_toml_config_missing_file() {
        let result = load_toml_config(Path::new("/nonexistent/path/config.toml")).unwrap();
        assert!(result.is_none());
    }

    // ── Config::build tests ─────────────────────────────────────────

    fn default_auth() -> AuthArgs {
        AuthArgs {
            username: "u@example.com".to_string(),
            password: None,
            domain: None,
            cookie_directory: None,
        }
    }

    fn default_sync() -> SyncArgs {
        SyncArgs::default()
    }

    #[test]
    fn test_build_defaults_no_toml() {
        let cfg = Config::build(default_auth(), default_sync(), None, None).unwrap();
        assert_eq!(cfg.username, "u@example.com");
        assert_eq!(cfg.threads_num, 10);
        assert_eq!(cfg.folder_structure, "%Y/%m/%d");
        assert_eq!(cfg.library, "PrimarySync");
        assert_eq!(cfg.max_retries, 3);
        assert_eq!(cfg.retry_delay_secs, 5);
        assert_eq!(cfg.temp_suffix, ".icloudpd-tmp");
        assert!(matches!(cfg.size, VersionSize::Original));
        assert!(matches!(cfg.domain, Domain::Com));
        assert!(matches!(cfg.log_level, LogLevel::Info));
    }

    #[test]
    fn test_build_toml_provides_defaults() {
        let toml_str = r#"
            [download]
            threads_num = 4
            folder_structure = "%Y-%m"

            [filters]
            library = "SharedSync-ABC"
        "#;
        let toml: TomlConfig = toml::from_str(toml_str).unwrap();
        let cfg = Config::build(default_auth(), default_sync(), None, Some(toml)).unwrap();
        assert_eq!(cfg.threads_num, 4);
        assert_eq!(cfg.folder_structure, "%Y-%m");
        assert_eq!(cfg.library, "SharedSync-ABC");
    }

    #[test]
    fn test_build_cli_overrides_toml() {
        let toml_str = r#"
            [download]
            threads_num = 4

            [filters]
            library = "SharedSync-ABC"
        "#;
        let toml: TomlConfig = toml::from_str(toml_str).unwrap();

        let mut sync = default_sync();
        sync.threads_num = Some(8);
        sync.library = Some("PrimarySync".to_string());

        let cfg = Config::build(default_auth(), sync, None, Some(toml)).unwrap();
        assert_eq!(cfg.threads_num, 8);
        assert_eq!(cfg.library, "PrimarySync");
    }

    #[test]
    fn test_build_hardcoded_default_when_both_absent() {
        let cfg = Config::build(default_auth(), default_sync(), None, None).unwrap();
        assert_eq!(cfg.threads_num, 10);
        assert!(matches!(cfg.align_raw, RawTreatmentPolicy::Unchanged));
    }

    #[test]
    fn test_build_boolean_flag_from_toml() {
        let toml_str = r#"
            [download]
            set_exif_datetime = true

            [filters]
            skip_videos = true
        "#;
        let toml: TomlConfig = toml::from_str(toml_str).unwrap();
        let cfg = Config::build(default_auth(), default_sync(), None, Some(toml)).unwrap();
        assert!(cfg.set_exif_datetime);
        assert!(cfg.skip_videos);
    }

    #[test]
    fn test_build_cli_flag_overrides_toml_false() {
        let toml_str = r#"
            [filters]
            skip_videos = false
        "#;
        let toml: TomlConfig = toml::from_str(toml_str).unwrap();
        let mut sync = default_sync();
        sync.skip_videos = true;
        let cfg = Config::build(default_auth(), sync, None, Some(toml)).unwrap();
        assert!(cfg.skip_videos);
    }

    #[test]
    fn test_build_threads_num_zero_from_toml_rejected() {
        let toml_str = r#"
            [download]
            threads_num = 0
        "#;
        let toml: TomlConfig = toml::from_str(toml_str).unwrap();
        let result = Config::build(default_auth(), default_sync(), None, Some(toml));
        assert!(result.is_err());
        assert!(
            result.unwrap_err().to_string().contains("threads_num"),
            "Error should mention threads_num"
        );
    }

    #[test]
    fn test_build_toml_auth_username() {
        let toml_str = r#"
            [auth]
            username = "toml@example.com"
        "#;
        let toml: TomlConfig = toml::from_str(toml_str).unwrap();
        let mut auth = default_auth();
        auth.username = String::new(); // Simulate no CLI username
        let cfg = Config::build(auth, default_sync(), None, Some(toml)).unwrap();
        assert_eq!(cfg.username, "toml@example.com");
    }

    #[test]
    fn test_build_cli_auth_overrides_toml_username() {
        let toml_str = r#"
            [auth]
            username = "toml@example.com"
        "#;
        let toml: TomlConfig = toml::from_str(toml_str).unwrap();
        let cfg = Config::build(default_auth(), default_sync(), None, Some(toml)).unwrap();
        assert_eq!(cfg.username, "u@example.com");
    }

    #[test]
    fn test_build_toml_albums() {
        let toml_str = r#"
            [filters]
            albums = ["Favorites", "Vacation"]
        "#;
        let toml: TomlConfig = toml::from_str(toml_str).unwrap();
        let cfg = Config::build(default_auth(), default_sync(), None, Some(toml)).unwrap();
        assert_eq!(cfg.albums, vec!["Favorites", "Vacation"]);
    }

    #[test]
    fn test_build_cli_albums_override_toml() {
        let toml_str = r#"
            [filters]
            albums = ["Favorites"]
        "#;
        let toml: TomlConfig = toml::from_str(toml_str).unwrap();
        let mut sync = default_sync();
        sync.albums = vec!["Screenshots".to_string()];
        let cfg = Config::build(default_auth(), sync, None, Some(toml)).unwrap();
        assert_eq!(cfg.albums, vec!["Screenshots"]);
    }

    #[test]
    fn test_build_log_level_from_toml() {
        let toml_str = r#"
            log_level = "debug"
        "#;
        let toml: TomlConfig = toml::from_str(toml_str).unwrap();
        let cfg = Config::build(default_auth(), default_sync(), None, Some(toml)).unwrap();
        assert!(matches!(cfg.log_level, LogLevel::Debug));
    }

    #[test]
    fn test_build_cli_log_level_overrides_toml() {
        let toml_str = r#"
            log_level = "debug"
        "#;
        let toml: TomlConfig = toml::from_str(toml_str).unwrap();
        let cfg = Config::build(
            default_auth(),
            default_sync(),
            Some(LogLevel::Warn),
            Some(toml),
        )
        .unwrap();
        assert!(matches!(cfg.log_level, LogLevel::Warn));
    }

    #[test]
    fn test_build_watch_from_toml() {
        let toml_str = r#"
            [watch]
            interval = 1800
            pid_file = "/run/test.pid"
        "#;
        let toml: TomlConfig = toml::from_str(toml_str).unwrap();
        let cfg = Config::build(default_auth(), default_sync(), None, Some(toml)).unwrap();
        assert_eq!(cfg.watch_with_interval, Some(1800));
        assert_eq!(cfg.pid_file, Some(PathBuf::from("/run/test.pid")));
    }

    #[test]
    fn test_build_skip_dates_from_toml() {
        let toml_str = r#"
            [filters]
            skip_created_before = "2024-01-01"
            skip_created_after = "2025-01-01"
        "#;
        let toml: TomlConfig = toml::from_str(toml_str).unwrap();
        let cfg = Config::build(default_auth(), default_sync(), None, Some(toml)).unwrap();
        assert!(cfg.skip_created_before.is_some());
        assert!(cfg.skip_created_after.is_some());
    }
}
