use crate::types::{
    Domain, FileMatchPolicy, LivePhotoMovFilenamePolicy, LivePhotoSize, LogLevel,
    RawTreatmentPolicy, VersionSize,
};
use clap::{Parser, Subcommand};

/// Common authentication arguments shared across subcommands.
#[derive(Parser, Debug, Clone)]
pub struct AuthArgs {
    /// Apple ID email address
    #[arg(short = 'u', long)]
    pub username: String,

    /// iCloud password (if not provided, will prompt).
    /// WARNING: passing via --password is visible in process listings.
    /// Prefer the ICLOUD_PASSWORD environment variable instead.
    #[arg(short = 'p', long, env = "ICLOUD_PASSWORD")]
    pub password: Option<String>,

    /// iCloud domain (com or cn)
    #[arg(long, value_enum, default_value = "com")]
    pub domain: Domain,

    /// Directory for cookies/session data
    #[arg(long, default_value = "~/.icloudpd-rs")]
    pub cookie_directory: String,
}

/// Top-level auth args (used for backwards compatibility when no subcommand).
/// Username is validated later in Config::from_cli when needed.
#[derive(Parser, Debug, Clone)]
pub struct TopLevelAuthArgs {
    /// Apple ID email address
    #[arg(short = 'u', long)]
    pub username: Option<String>,

    /// iCloud password (if not provided, will prompt).
    /// WARNING: passing via --password is visible in process listings.
    /// Prefer the ICLOUD_PASSWORD environment variable instead.
    #[arg(short = 'p', long, env = "ICLOUD_PASSWORD")]
    pub password: Option<String>,

    /// iCloud domain (com or cn)
    #[arg(long, value_enum, default_value = "com")]
    pub domain: Domain,

    /// Directory for cookies/session data
    #[arg(long, default_value = "~/.icloudpd-rs")]
    pub cookie_directory: String,
}

/// Arguments for the sync command (also used as default when no subcommand).
#[derive(Parser, Debug, Clone)]
pub struct SyncArgs {
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

    /// Number of concurrent download threads (default: 10)
    #[arg(long = "threads-num", default_value_t = 10, value_parser = clap::value_parser!(u16).range(1..))]
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

    /// Run continuously, waiting N seconds between runs
    #[arg(long)]
    pub watch_with_interval: Option<u64>,

    /// Disable progress bar
    #[arg(long)]
    pub no_progress_bar: bool,

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
    /// NOTE: Parsed but not yet wired up - hidden until implemented
    #[arg(long, hide = true)]
    pub only_print_filenames: bool,

    /// Max retries per download (default: 3, 0 = no retries)
    #[arg(long, default_value_t = 3)]
    pub max_retries: u32,

    /// Initial retry delay in seconds (default: 5)
    #[arg(long, default_value_t = 5)]
    pub retry_delay: u64,
}

/// Arguments for the status command.
#[derive(Parser, Debug, Clone)]
pub struct StatusArgs {
    #[command(flatten)]
    pub auth: AuthArgs,

    /// Show failed assets with error messages
    #[arg(long)]
    pub failed: bool,
}

/// Arguments for the retry-failed command.
#[derive(Parser, Debug, Clone)]
pub struct RetryFailedArgs {
    #[command(flatten)]
    pub auth: AuthArgs,

    #[command(flatten)]
    pub sync: SyncArgs,
}

/// Arguments for the reset-state command.
#[derive(Parser, Debug, Clone)]
pub struct ResetStateArgs {
    #[command(flatten)]
    pub auth: AuthArgs,

    /// Skip confirmation prompt
    #[arg(long, short = 'y')]
    pub yes: bool,
}

/// Arguments for the import-existing command.
#[derive(Parser, Debug, Clone)]
pub struct ImportArgs {
    #[command(flatten)]
    pub auth: AuthArgs,

    /// Local directory containing existing downloads
    #[arg(short = 'd', long)]
    pub directory: String,

    /// Folder structure used for existing downloads
    #[arg(long, default_value = "%Y/%m/%d")]
    pub folder_structure: String,

    /// Number of recent photos to check
    #[arg(long)]
    pub recent: Option<u32>,
}

/// Arguments for the verify command.
#[derive(Parser, Debug, Clone)]
pub struct VerifyArgs {
    #[command(flatten)]
    pub auth: AuthArgs,

    /// Verify checksums (slower but more thorough)
    #[arg(long)]
    pub checksums: bool,
}

/// Subcommands for icloudpd-rs.
#[derive(Subcommand, Debug, Clone)]
pub enum Command {
    /// Download photos from iCloud (default command)
    Sync {
        #[command(flatten)]
        auth: AuthArgs,

        #[command(flatten)]
        sync: SyncArgs,
    },

    /// Show sync status and database summary
    Status(StatusArgs),

    /// Reset failed downloads to pending and re-sync
    RetryFailed(RetryFailedArgs),

    /// Delete the state database and start fresh
    ResetState(ResetStateArgs),

    /// Import existing local files into the state database
    ImportExisting(ImportArgs),

    /// Verify downloaded files exist and optionally check checksums
    Verify(VerifyArgs),
}

#[derive(Parser, Debug)]
#[command(name = "icloudpd-rs", about = "Download iCloud photos and videos")]
pub struct Cli {
    /// Log level
    #[arg(long, value_enum, default_value = "info", global = true)]
    pub log_level: LogLevel,

    #[command(subcommand)]
    pub command: Option<Command>,

    // Backwards compatibility: allow all sync args at top level
    // These are only used when no subcommand is provided
    #[command(flatten)]
    pub auth: TopLevelAuthArgs,

    #[command(flatten)]
    pub sync: SyncArgs,
}

impl Cli {
    /// Get the effective command, treating bare invocation as sync.
    pub fn effective_command(&self) -> Command {
        match &self.command {
            Some(cmd) => cmd.clone(),
            None => {
                // Convert top-level args to AuthArgs (username is required at this point)
                let auth = AuthArgs {
                    username: self.auth.username.clone().unwrap_or_default(),
                    password: self.auth.password.clone(),
                    domain: self.auth.domain,
                    cookie_directory: self.auth.cookie_directory.clone(),
                };
                Command::Sync {
                    auth,
                    sync: self.sync.clone(),
                }
            }
        }
    }
}

// Legacy Cli struct for backwards compatibility with existing code.
// TODO: Remove once main.rs is fully migrated to new command structure.
#[derive(Debug)]
pub struct LegacyCli {
    pub username: String,
    pub password: Option<String>,
    pub directory: Option<String>,
    pub auth_only: bool,
    pub list_albums: bool,
    pub list_libraries: bool,
    pub albums: Vec<String>,
    pub library: String,
    pub size: VersionSize,
    pub live_photo_size: LivePhotoSize,
    pub recent: Option<u32>,
    pub threads_num: u16,
    pub skip_videos: bool,
    pub skip_photos: bool,
    pub skip_live_photos: bool,
    pub force_size: bool,
    pub folder_structure: String,
    pub set_exif_datetime: bool,
    pub dry_run: bool,
    pub domain: Domain,
    pub watch_with_interval: Option<u64>,
    pub log_level: LogLevel,
    pub no_progress_bar: bool,
    pub cookie_directory: String,
    pub keep_unicode_in_filenames: bool,
    pub live_photo_mov_filename_policy: LivePhotoMovFilenamePolicy,
    pub align_raw: RawTreatmentPolicy,
    pub file_match_policy: FileMatchPolicy,
    pub skip_created_before: Option<String>,
    pub skip_created_after: Option<String>,
    pub only_print_filenames: bool,
    pub max_retries: u32,
    pub retry_delay: u64,
}

impl From<Cli> for LegacyCli {
    fn from(cli: Cli) -> Self {
        match cli.effective_command() {
            Command::Sync { auth, sync } | Command::RetryFailed(RetryFailedArgs { auth, sync }) => {
                Self {
                    username: auth.username,
                    password: auth.password,
                    directory: sync.directory,
                    auth_only: sync.auth_only,
                    list_albums: sync.list_albums,
                    list_libraries: sync.list_libraries,
                    albums: sync.albums,
                    library: sync.library,
                    size: sync.size,
                    live_photo_size: sync.live_photo_size,
                    recent: sync.recent,
                    threads_num: sync.threads_num,
                    skip_videos: sync.skip_videos,
                    skip_photos: sync.skip_photos,
                    skip_live_photos: sync.skip_live_photos,
                    force_size: sync.force_size,
                    folder_structure: sync.folder_structure,
                    set_exif_datetime: sync.set_exif_datetime,
                    dry_run: sync.dry_run,
                    domain: auth.domain,
                    watch_with_interval: sync.watch_with_interval,
                    log_level: cli.log_level,
                    no_progress_bar: sync.no_progress_bar,
                    cookie_directory: auth.cookie_directory,
                    keep_unicode_in_filenames: sync.keep_unicode_in_filenames,
                    live_photo_mov_filename_policy: sync.live_photo_mov_filename_policy,
                    align_raw: sync.align_raw,
                    file_match_policy: sync.file_match_policy,
                    skip_created_before: sync.skip_created_before,
                    skip_created_after: sync.skip_created_after,
                    only_print_filenames: sync.only_print_filenames,
                    max_retries: sync.max_retries,
                    retry_delay: sync.retry_delay,
                }
            }
            // For other commands, provide defaults (they won't use these fields)
            Command::Status(args) => Self {
                username: args.auth.username,
                password: args.auth.password,
                domain: args.auth.domain,
                cookie_directory: args.auth.cookie_directory,
                log_level: cli.log_level,
                // Defaults for unused fields
                directory: None,
                auth_only: false,
                list_albums: false,
                list_libraries: false,
                albums: Vec::new(),
                library: String::new(),
                size: VersionSize::Original,
                live_photo_size: LivePhotoSize::Original,
                recent: None,
                threads_num: 10,
                skip_videos: false,
                skip_photos: false,
                skip_live_photos: false,
                force_size: false,
                folder_structure: String::new(),
                set_exif_datetime: false,
                dry_run: false,
                watch_with_interval: None,
                no_progress_bar: false,
                keep_unicode_in_filenames: false,
                live_photo_mov_filename_policy: LivePhotoMovFilenamePolicy::Suffix,
                align_raw: RawTreatmentPolicy::Unchanged,
                file_match_policy: FileMatchPolicy::NameSizeDedupWithSuffix,
                skip_created_before: None,
                skip_created_after: None,
                only_print_filenames: false,
                max_retries: 3,
                retry_delay: 5,
            },
            Command::ResetState(args) => Self {
                username: args.auth.username,
                password: args.auth.password,
                domain: args.auth.domain,
                cookie_directory: args.auth.cookie_directory,
                log_level: cli.log_level,
                // Defaults for unused fields
                directory: None,
                auth_only: false,
                list_albums: false,
                list_libraries: false,
                albums: Vec::new(),
                library: String::new(),
                size: VersionSize::Original,
                live_photo_size: LivePhotoSize::Original,
                recent: None,
                threads_num: 10,
                skip_videos: false,
                skip_photos: false,
                skip_live_photos: false,
                force_size: false,
                folder_structure: String::new(),
                set_exif_datetime: false,
                dry_run: false,
                watch_with_interval: None,
                no_progress_bar: false,
                keep_unicode_in_filenames: false,
                live_photo_mov_filename_policy: LivePhotoMovFilenamePolicy::Suffix,
                align_raw: RawTreatmentPolicy::Unchanged,
                file_match_policy: FileMatchPolicy::NameSizeDedupWithSuffix,
                skip_created_before: None,
                skip_created_after: None,
                only_print_filenames: false,
                max_retries: 3,
                retry_delay: 5,
            },
            Command::ImportExisting(args) => Self {
                username: args.auth.username,
                password: args.auth.password,
                domain: args.auth.domain,
                cookie_directory: args.auth.cookie_directory,
                directory: Some(args.directory),
                folder_structure: args.folder_structure,
                recent: args.recent,
                log_level: cli.log_level,
                // Defaults for unused fields
                auth_only: false,
                list_albums: false,
                list_libraries: false,
                albums: Vec::new(),
                library: String::new(),
                size: VersionSize::Original,
                live_photo_size: LivePhotoSize::Original,
                threads_num: 10,
                skip_videos: false,
                skip_photos: false,
                skip_live_photos: false,
                force_size: false,
                set_exif_datetime: false,
                dry_run: false,
                watch_with_interval: None,
                no_progress_bar: false,
                keep_unicode_in_filenames: false,
                live_photo_mov_filename_policy: LivePhotoMovFilenamePolicy::Suffix,
                align_raw: RawTreatmentPolicy::Unchanged,
                file_match_policy: FileMatchPolicy::NameSizeDedupWithSuffix,
                skip_created_before: None,
                skip_created_after: None,
                only_print_filenames: false,
                max_retries: 3,
                retry_delay: 5,
            },
            Command::Verify(args) => Self {
                username: args.auth.username,
                password: args.auth.password,
                domain: args.auth.domain,
                cookie_directory: args.auth.cookie_directory,
                log_level: cli.log_level,
                // Defaults for unused fields
                directory: None,
                auth_only: false,
                list_albums: false,
                list_libraries: false,
                albums: Vec::new(),
                library: String::new(),
                size: VersionSize::Original,
                live_photo_size: LivePhotoSize::Original,
                recent: None,
                threads_num: 10,
                skip_videos: false,
                skip_photos: false,
                skip_live_photos: false,
                force_size: false,
                folder_structure: String::new(),
                set_exif_datetime: false,
                dry_run: false,
                watch_with_interval: None,
                no_progress_bar: false,
                keep_unicode_in_filenames: false,
                live_photo_mov_filename_policy: LivePhotoMovFilenamePolicy::Suffix,
                align_raw: RawTreatmentPolicy::Unchanged,
                file_match_policy: FileMatchPolicy::NameSizeDedupWithSuffix,
                skip_created_before: None,
                skip_created_after: None,
                only_print_filenames: false,
                max_retries: 3,
                retry_delay: 5,
            },
        }
    }
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
    fn test_library_default_primary_sync() {
        let cli = parse(&base_args());
        let legacy: LegacyCli = cli.into();
        assert_eq!(legacy.library, "PrimarySync");
    }

    #[test]
    fn test_library_accepts_custom_value() {
        let mut args = base_args();
        args.extend(["--library", "SharedSync-ABCD1234"]);
        let cli = parse(&args);
        let legacy: LegacyCli = cli.into();
        assert_eq!(legacy.library, "SharedSync-ABCD1234");
    }

    #[test]
    fn test_threads_num_defaults_to_10() {
        let cli = parse(&base_args());
        let legacy: LegacyCli = cli.into();
        assert_eq!(legacy.threads_num, 10);
    }

    #[test]
    fn test_threads_num_accepts_valid_value() {
        let mut args = base_args();
        args.extend(["--threads-num", "8"]);
        let cli = parse(&args);
        let legacy: LegacyCli = cli.into();
        assert_eq!(legacy.threads_num, 8);
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
        let legacy: LegacyCli = cli.into();
        assert!(!legacy.dry_run);
    }

    #[test]
    fn test_size_default_original() {
        let cli = parse(&base_args());
        let legacy: LegacyCli = cli.into();
        assert!(matches!(legacy.size, VersionSize::Original));
    }

    #[test]
    fn test_recent_none_by_default() {
        let cli = parse(&base_args());
        let legacy: LegacyCli = cli.into();
        assert!(legacy.recent.is_none());
    }

    #[test]
    fn test_recent_accepts_value() {
        let mut args = base_args();
        args.extend(["--recent", "50"]);
        let cli = parse(&args);
        let legacy: LegacyCli = cli.into();
        assert_eq!(legacy.recent, Some(50));
    }

    #[test]
    fn test_max_retries_default() {
        let cli = parse(&base_args());
        let legacy: LegacyCli = cli.into();
        assert_eq!(legacy.max_retries, 3);
    }

    #[test]
    fn test_max_retries_custom() {
        let mut args = base_args();
        args.extend(["--max-retries", "10"]);
        let cli = parse(&args);
        let legacy: LegacyCli = cli.into();
        assert_eq!(legacy.max_retries, 10);
    }

    #[test]
    fn test_max_retries_zero_disables() {
        let mut args = base_args();
        args.extend(["--max-retries", "0"]);
        let cli = parse(&args);
        let legacy: LegacyCli = cli.into();
        assert_eq!(legacy.max_retries, 0);
    }

    #[test]
    fn test_retry_delay_default() {
        let cli = parse(&base_args());
        let legacy: LegacyCli = cli.into();
        assert_eq!(legacy.retry_delay, 5);
    }

    #[test]
    fn test_retry_delay_custom() {
        let mut args = base_args();
        args.extend(["--retry-delay", "15"]);
        let cli = parse(&args);
        let legacy: LegacyCli = cli.into();
        assert_eq!(legacy.retry_delay, 15);
    }

    #[test]
    fn test_align_raw_default_as_is() {
        let cli = parse(&base_args());
        let legacy: LegacyCli = cli.into();
        assert!(matches!(legacy.align_raw, RawTreatmentPolicy::Unchanged));
    }

    #[test]
    fn test_align_raw_accepts_original() {
        let mut args = base_args();
        args.extend(["--align-raw", "original"]);
        let cli = parse(&args);
        let legacy: LegacyCli = cli.into();
        assert!(matches!(
            legacy.align_raw,
            RawTreatmentPolicy::PreferOriginal
        ));
    }

    #[test]
    fn test_align_raw_accepts_alternative() {
        let mut args = base_args();
        args.extend(["--align-raw", "alternative"]);
        let cli = parse(&args);
        let legacy: LegacyCli = cli.into();
        assert!(matches!(
            legacy.align_raw,
            RawTreatmentPolicy::PreferAlternative
        ));
    }

    #[test]
    fn test_align_raw_rejects_invalid() {
        let mut args = base_args();
        args.extend(["--align-raw", "bogus"]);
        assert!(Cli::try_parse_from(&args).is_err());
    }

    #[test]
    fn test_sync_subcommand() {
        let cli = Cli::try_parse_from([
            "icloudpd-rs",
            "sync",
            "--username",
            "test@example.com",
            "--directory",
            "/photos",
        ])
        .unwrap();
        assert!(matches!(cli.command, Some(Command::Sync { .. })));
    }

    #[test]
    fn test_status_subcommand() {
        let cli = Cli::try_parse_from(["icloudpd-rs", "status", "--username", "test@example.com"])
            .unwrap();
        assert!(matches!(cli.command, Some(Command::Status(_))));
    }

    #[test]
    fn test_status_with_failed_flag() {
        let cli = Cli::try_parse_from([
            "icloudpd-rs",
            "status",
            "--username",
            "test@example.com",
            "--failed",
        ])
        .unwrap();
        if let Some(Command::Status(args)) = cli.command {
            assert!(args.failed);
        } else {
            panic!("Expected Status command");
        }
    }

    #[test]
    fn test_reset_state_subcommand() {
        let cli = Cli::try_parse_from([
            "icloudpd-rs",
            "reset-state",
            "--username",
            "test@example.com",
            "--yes",
        ])
        .unwrap();
        if let Some(Command::ResetState(args)) = cli.command {
            assert!(args.yes);
        } else {
            panic!("Expected ResetState command");
        }
    }

    #[test]
    fn test_backwards_compatibility_no_subcommand() {
        // Old-style invocation without subcommand should still work
        let cli = Cli::try_parse_from([
            "icloudpd-rs",
            "--username",
            "test@example.com",
            "--directory",
            "/photos",
        ])
        .unwrap();
        assert!(cli.command.is_none());
        let legacy: LegacyCli = cli.into();
        assert_eq!(legacy.username, "test@example.com");
        assert_eq!(legacy.directory, Some("/photos".to_string()));
    }
}
