use crate::types::{
    Domain, FileMatchPolicy, LivePhotoMovFilenamePolicy, LivePhotoSize, LogLevel,
    RawTreatmentPolicy, VersionSize,
};
use chrono::{DateTime, Local, NaiveDate, NaiveDateTime};
use std::path::PathBuf;

pub struct Config {
    pub username: String,
    pub password: Option<String>,
    pub directory: PathBuf,
    pub auth_only: bool,
    pub list_albums: bool,
    pub list_libraries: bool,
    pub albums: Vec<String>,
    #[allow(dead_code)] // CLI flag parsed but not yet wired
    pub library: String,
    pub size: VersionSize,
    pub live_photo_size: LivePhotoSize,
    pub recent: Option<u32>,
    pub threads_num: u16,
    pub skip_videos: bool,
    pub skip_photos: bool,
    pub skip_live_photos: bool,
    #[allow(dead_code)] // CLI flag parsed but not yet wired
    pub force_size: bool,
    pub folder_structure: String,
    pub set_exif_datetime: bool,
    pub dry_run: bool,
    pub domain: Domain,
    pub watch_with_interval: Option<u64>,
    #[allow(dead_code)] // CLI flag parsed but not yet wired
    pub log_level: LogLevel,
    pub no_progress_bar: bool,
    pub cookie_directory: PathBuf,
    #[allow(dead_code)] // CLI flag parsed but not yet wired
    pub keep_unicode_in_filenames: bool,
    pub live_photo_mov_filename_policy: LivePhotoMovFilenamePolicy,
    pub align_raw: RawTreatmentPolicy,
    #[allow(dead_code)] // CLI flag parsed but not yet wired
    pub file_match_policy: FileMatchPolicy,
    pub skip_created_before: Option<DateTime<Local>>,
    pub skip_created_after: Option<DateTime<Local>>,
    #[allow(dead_code)] // CLI flag parsed but not yet wired
    pub only_print_filenames: bool,
    pub max_retries: u32,
    pub retry_delay_secs: u64,
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

fn expand_tilde(path: &str) -> PathBuf {
    if let Some(stripped) = path.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(stripped);
        }
    }
    PathBuf::from(path)
}

impl Config {
    pub fn from_cli(cli: crate::cli::Cli) -> anyhow::Result<Self> {
        let directory = cli.directory.map(|d| expand_tilde(&d)).unwrap_or_default();

        let cookie_directory = expand_tilde(&cli.cookie_directory);

        let skip_created_before = cli
            .skip_created_before
            .as_deref()
            .map(parse_date_or_interval)
            .transpose()?;
        let skip_created_after = cli
            .skip_created_after
            .as_deref()
            .map(parse_date_or_interval)
            .transpose()?;

        Ok(Self {
            username: cli.username,
            password: cli.password,
            directory,
            auth_only: cli.auth_only,
            list_albums: cli.list_albums,
            list_libraries: cli.list_libraries,
            albums: cli.albums,
            library: cli.library,
            size: cli.size,
            live_photo_size: cli.live_photo_size,
            recent: cli.recent,
            threads_num: cli.threads_num,
            skip_videos: cli.skip_videos,
            skip_photos: cli.skip_photos,
            skip_live_photos: cli.skip_live_photos,
            force_size: cli.force_size,
            folder_structure: cli.folder_structure,
            set_exif_datetime: cli.set_exif_datetime,
            dry_run: cli.dry_run,
            domain: cli.domain,
            watch_with_interval: cli.watch_with_interval,
            log_level: cli.log_level,
            no_progress_bar: cli.no_progress_bar,
            cookie_directory,
            keep_unicode_in_filenames: cli.keep_unicode_in_filenames,
            live_photo_mov_filename_policy: cli.live_photo_mov_filename_policy,
            align_raw: cli.align_raw,
            file_match_policy: cli.file_match_policy,
            skip_created_before,
            skip_created_after,
            only_print_filenames: cli.only_print_filenames,
            max_retries: cli.max_retries,
            retry_delay_secs: cli.retry_delay,
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
        // Allow 1 second tolerance
        assert!(dt >= expected - chrono::Duration::seconds(1));
        assert!(dt <= after - chrono::Duration::days(10) + chrono::Duration::seconds(1));
    }

    #[test]
    fn test_parse_invalid_date() {
        assert!(parse_date_or_interval("not-a-date").is_err());
        assert!(parse_date_or_interval("").is_err());
    }

    fn make_cli(overrides: impl FnOnce(&mut crate::cli::Cli)) -> crate::cli::Cli {
        use clap::Parser;
        let mut cli =
            crate::cli::Cli::try_parse_from(["icloudpd-rs", "--username", "u@example.com"])
                .unwrap();
        overrides(&mut cli);
        cli
    }

    #[test]
    fn test_from_cli_threads_num_passthrough() {
        let cli = make_cli(|c| c.threads_num = 4);
        let cfg = Config::from_cli(cli).unwrap();
        assert_eq!(cfg.threads_num, 4);
    }

    #[test]
    fn test_from_cli_skip_flags() {
        let cli = make_cli(|c| {
            c.skip_videos = true;
            c.skip_photos = true;
            c.skip_live_photos = true;
        });
        let cfg = Config::from_cli(cli).unwrap();
        assert!(cfg.skip_videos);
        assert!(cfg.skip_photos);
        assert!(cfg.skip_live_photos);
    }

    #[test]
    fn test_from_cli_dry_run() {
        let cli = make_cli(|c| c.dry_run = true);
        let cfg = Config::from_cli(cli).unwrap();
        assert!(cfg.dry_run);
    }
}
