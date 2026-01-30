use std::path::PathBuf;
use chrono::{DateTime, Local, NaiveDate, NaiveDateTime};
use crate::types::*;

#[allow(dead_code)]
pub struct Config {
    pub username: String,
    pub password: Option<String>,
    pub directory: PathBuf,
    pub auth_only: bool,
    pub list_albums: bool,
    pub list_libraries: bool,
    pub albums: Vec<String>,
    pub library: String,
    pub size: VersionSize,
    pub live_photo_size: LivePhotoSize,
    pub recent: Option<u32>,
    pub until_found: Option<u32>,
    pub skip_videos: bool,
    pub skip_photos: bool,
    pub skip_live_photos: bool,
    pub force_size: bool,
    pub auto_delete: bool,
    pub folder_structure: String,
    pub set_exif_datetime: bool,
    pub dry_run: bool,
    pub domain: Domain,
    pub watch_with_interval: Option<u64>,
    pub log_level: LogLevel,
    pub no_progress_bar: bool,
    pub cookie_directory: PathBuf,
    pub keep_unicode_in_filenames: bool,
    pub live_photo_mov_filename_policy: LivePhotoMovFilenamePolicy,
    pub align_raw: RawTreatmentPolicy,
    pub file_match_policy: FileMatchPolicy,
    pub skip_created_before: Option<DateTime<Local>>,
    pub skip_created_after: Option<DateTime<Local>>,
    pub delete_after_download: bool,
    pub keep_icloud_recent_days: Option<u32>,
    pub only_print_filenames: bool,
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
        let directory = cli.directory
            .map(|d| expand_tilde(&d))
            .unwrap_or_default();

        let cookie_directory = expand_tilde(&cli.cookie_directory);

        let skip_created_before = cli.skip_created_before
            .as_deref()
            .map(parse_date_or_interval)
            .transpose()?;
        let skip_created_after = cli.skip_created_after
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
            until_found: cli.until_found,
            skip_videos: cli.skip_videos,
            skip_photos: cli.skip_photos,
            skip_live_photos: cli.skip_live_photos,
            force_size: cli.force_size,
            auto_delete: cli.auto_delete,
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
            delete_after_download: cli.delete_after_download,
            keep_icloud_recent_days: cli.keep_icloud_recent_days,
            only_print_filenames: cli.only_print_filenames,
        })
    }
}

/// Parse ISO date (2025-01-02) or interval (20d) to DateTime<Local>.
///
/// Returns an error if the input cannot be parsed as any supported format.
pub(crate) fn parse_date_or_interval(s: &str) -> anyhow::Result<DateTime<Local>> {
    // Try interval first (e.g., "20d")
    if let Some(days_str) = s.strip_suffix('d') {
        if let Ok(days) = days_str.parse::<i64>() {
            return Ok(Local::now() - chrono::Duration::days(days));
        }
    }
    // Try ISO date
    if let Ok(date) = NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        if let Some(naive_dt) = date.and_hms_opt(0, 0, 0) {
            if let Some(dt) = naive_dt.and_local_timezone(Local).single() {
                return Ok(dt);
            }
        }
    }
    // Try full ISO datetime
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
        assert_eq!(expand_tilde("/absolute/path"), PathBuf::from("/absolute/path"));
        assert_eq!(expand_tilde("relative/path"), PathBuf::from("relative/path"));
    }

    #[test]
    fn test_parse_date_iso() {
        let dt = parse_date_or_interval("2025-01-15").unwrap();
        assert_eq!(dt.date_naive(), NaiveDate::from_ymd_opt(2025, 1, 15).unwrap());
    }

    #[test]
    fn test_parse_datetime_iso() {
        let dt = parse_date_or_interval("2025-06-15T14:30:00").unwrap();
        let naive = dt.naive_local();
        assert_eq!(naive.date(), NaiveDate::from_ymd_opt(2025, 6, 15).unwrap());
        assert_eq!(naive.time(), chrono::NaiveTime::from_hms_opt(14, 30, 0).unwrap());
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
}
