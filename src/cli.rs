use crate::types::{
    Domain, FileMatchPolicy, LivePhotoMovFilenamePolicy, LivePhotoSize, LogLevel,
    RawTreatmentPolicy, VersionSize,
};
use clap::Parser;

#[derive(Parser, Debug)]
#[command(name = "icloudpd-rs", about = "Download iCloud photos and videos")]
pub struct Cli {
    /// Apple ID email address
    #[arg(short = 'u', long)]
    pub username: String,

    /// iCloud password (if not provided, will prompt).
    /// WARNING: passing via --password is visible in process listings.
    /// Prefer the ICLOUD_PASSWORD environment variable instead.
    #[arg(short = 'p', long, env = "ICLOUD_PASSWORD")]
    pub password: Option<String>,

    /// Local directory for downloads
    #[arg(short = 'd', long)]
    pub directory: Option<String>,

    /// Only authenticate (create/update session tokens)
    #[arg(long)]
    pub auth_only: bool,

    /// List available albums
    #[arg(short = 'l', long)]
    pub list_albums: bool,

    /// List available libraries
    #[arg(long)]
    pub list_libraries: bool,

    /// Album(s) to download
    #[arg(short = 'a', long = "album")]
    pub albums: Vec<String>,

    /// Library to download (default: PrimarySync)
    #[arg(long, default_value = "PrimarySync")]
    pub library: String,

    /// Image size to download
    #[arg(long, value_enum, default_value = "original")]
    pub size: VersionSize,

    /// Live photo video size
    #[arg(long, value_enum, default_value = "original")]
    pub live_photo_size: LivePhotoSize,

    /// Number of recent photos to download
    #[arg(long)]
    pub recent: Option<u32>,

    /// Number of concurrent download threads (default: 1)
    #[arg(long = "threads-num", default_value_t = 1, value_parser = clap::value_parser!(u16).range(1..))]
    pub threads_num: u16,

    /// Don't download videos
    #[arg(long)]
    pub skip_videos: bool,

    /// Don't download photos
    #[arg(long)]
    pub skip_photos: bool,

    /// Don't download live photos
    #[arg(long)]
    pub skip_live_photos: bool,

    /// Only download requested size (don't fall back to original)
    #[arg(long)]
    pub force_size: bool,

    /// Folder structure for organizing downloads
    #[arg(long, default_value = "%Y/%m/%d")]
    pub folder_structure: String,

    /// Write DateTimeOriginal EXIF tag if missing
    #[arg(long)]
    pub set_exif_datetime: bool,

    /// Do not modify local system or iCloud
    #[arg(long)]
    pub dry_run: bool,

    /// iCloud domain (com or cn)
    #[arg(long, value_enum, default_value = "com")]
    pub domain: Domain,

    /// Run continuously, waiting N seconds between runs
    #[arg(long)]
    pub watch_with_interval: Option<u64>,

    /// Log level
    #[arg(long, value_enum, default_value = "error")]
    pub log_level: LogLevel,

    /// Disable progress bar
    #[arg(long)]
    pub no_progress_bar: bool,

    /// Directory for cookies/session data
    #[arg(long, default_value = "~/.icloudpd-rs")]
    pub cookie_directory: String,

    /// Keep Unicode in filenames
    #[arg(long)]
    pub keep_unicode_in_filenames: bool,

    /// Live photo MOV filename policy
    #[arg(long, value_enum, default_value = "suffix")]
    pub live_photo_mov_filename_policy: LivePhotoMovFilenamePolicy,

    /// RAW treatment policy
    #[arg(long, value_enum, default_value = "as-is")]
    pub align_raw: RawTreatmentPolicy,

    /// File matching and dedup policy
    #[arg(long, value_enum, default_value = "name-size-dedup-with-suffix")]
    pub file_match_policy: FileMatchPolicy,

    /// Skip assets created before this ISO date or interval (e.g., 2025-01-02 or 20d)
    #[arg(long)]
    pub skip_created_before: Option<String>,

    /// Skip assets created after this ISO date or interval
    #[arg(long)]
    pub skip_created_after: Option<String>,

    /// Only print filenames without downloading
    #[arg(long)]
    pub only_print_filenames: bool,

    /// Max retries per download (default: 2, 0 = no retries)
    #[arg(long, default_value_t = 2)]
    pub max_retries: u32,

    /// Initial retry delay in seconds (default: 5)
    #[arg(long, default_value_t = 5)]
    pub retry_delay: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    fn parse(args: &[&str]) -> Cli {
        Cli::try_parse_from(args).unwrap()
    }

    fn base_args() -> Vec<&'static str> {
        vec!["icloudpd-rs", "--username", "test@example.com"]
    }

    #[test]
    fn test_threads_num_defaults_to_1() {
        let cli = parse(&base_args());
        assert_eq!(cli.threads_num, 1);
    }

    #[test]
    fn test_threads_num_accepts_valid_value() {
        let mut args = base_args();
        args.extend(["--threads-num", "8"]);
        let cli = parse(&args);
        assert_eq!(cli.threads_num, 8);
    }

    #[test]
    fn test_threads_num_rejects_zero() {
        let mut args = base_args();
        args.extend(["--threads-num", "0"]);
        assert!(Cli::try_parse_from(&args).is_err());
    }

    #[test]
    fn test_dry_run_default_false() {
        let cli = parse(&base_args());
        assert!(!cli.dry_run);
    }

    #[test]
    fn test_size_default_original() {
        let cli = parse(&base_args());
        assert!(matches!(cli.size, VersionSize::Original));
    }

    #[test]
    fn test_recent_none_by_default() {
        let cli = parse(&base_args());
        assert!(cli.recent.is_none());
    }

    #[test]
    fn test_recent_accepts_value() {
        let mut args = base_args();
        args.extend(["--recent", "50"]);
        let cli = parse(&args);
        assert_eq!(cli.recent, Some(50));
    }

    #[test]
    fn test_max_retries_default() {
        let cli = parse(&base_args());
        assert_eq!(cli.max_retries, 2);
    }

    #[test]
    fn test_max_retries_custom() {
        let mut args = base_args();
        args.extend(["--max-retries", "10"]);
        let cli = parse(&args);
        assert_eq!(cli.max_retries, 10);
    }

    #[test]
    fn test_max_retries_zero_disables() {
        let mut args = base_args();
        args.extend(["--max-retries", "0"]);
        let cli = parse(&args);
        assert_eq!(cli.max_retries, 0);
    }

    #[test]
    fn test_retry_delay_default() {
        let cli = parse(&base_args());
        assert_eq!(cli.retry_delay, 5);
    }

    #[test]
    fn test_retry_delay_custom() {
        let mut args = base_args();
        args.extend(["--retry-delay", "15"]);
        let cli = parse(&args);
        assert_eq!(cli.retry_delay, 15);
    }

    #[test]
    fn test_align_raw_default_as_is() {
        let cli = parse(&base_args());
        assert!(matches!(cli.align_raw, RawTreatmentPolicy::AsIs));
    }

    #[test]
    fn test_align_raw_accepts_original() {
        let mut args = base_args();
        args.extend(["--align-raw", "original"]);
        let cli = parse(&args);
        assert!(matches!(cli.align_raw, RawTreatmentPolicy::AsOriginal));
    }

    #[test]
    fn test_align_raw_accepts_alternative() {
        let mut args = base_args();
        args.extend(["--align-raw", "alternative"]);
        let cli = parse(&args);
        assert!(matches!(cli.align_raw, RawTreatmentPolicy::AsAlternative));
    }

    #[test]
    fn test_align_raw_rejects_invalid() {
        let mut args = base_args();
        args.extend(["--align-raw", "bogus"]);
        assert!(Cli::try_parse_from(&args).is_err());
    }
}
