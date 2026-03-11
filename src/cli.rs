use crate::types::{
    Domain, FileMatchPolicy, LivePhotoMovFilenamePolicy, LivePhotoSize, LogLevel,
    RawTreatmentPolicy, VersionSize,
};
use clap::{Parser, Subcommand};

/// Authentication arguments shared across all commands and subcommands.
/// Username is optional at the clap level; validated at runtime after TOML merge.
#[derive(Parser, Debug, Clone)]
pub struct AuthArgs {
    /// Apple ID email address
    #[arg(short = 'u', long, env = "ICLOUD_USERNAME")]
    pub username: Option<String>,

    /// iCloud password (if not provided, will prompt).
    /// WARNING: passing via --password is visible in process listings.
    /// Prefer the ICLOUD_PASSWORD environment variable instead.
    #[arg(short = 'p', long, env = "ICLOUD_PASSWORD")]
    pub password: Option<String>,

    /// iCloud domain (com or cn)
    #[arg(long, value_enum)]
    pub domain: Option<Domain>,

    /// Directory for cookies/session data
    #[arg(long)]
    pub cookie_directory: Option<String>,
}

/// Arguments for the sync command (also used as default when no subcommand).
#[derive(Parser, Debug, Clone, Default)]
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

    /// Library to download (default: PrimarySync, use "all" for all libraries)
    #[arg(long)]
    pub library: Option<String>,

    /// Image size to download
    #[arg(long, value_enum)]
    pub size: Option<VersionSize>,

    /// Live photo video size
    #[arg(long, value_enum)]
    pub live_photo_size: Option<LivePhotoSize>,

    /// Number of recent photos to download
    #[arg(long)]
    pub recent: Option<u32>,

    /// Number of concurrent download threads (default: 10)
    #[arg(long = "threads-num", value_parser = clap::value_parser!(u16).range(1..))]
    pub threads_num: Option<u16>,

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
    #[arg(long)]
    pub folder_structure: Option<String>,

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
    #[arg(long, value_enum)]
    pub live_photo_mov_filename_policy: Option<LivePhotoMovFilenamePolicy>,

    /// RAW treatment policy
    #[arg(long, value_enum)]
    pub align_raw: Option<RawTreatmentPolicy>,

    /// File matching and dedup policy
    #[arg(long, value_enum)]
    pub file_match_policy: Option<FileMatchPolicy>,

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
    #[arg(long)]
    pub max_retries: Option<u32>,

    /// Initial retry delay in seconds (default: 5)
    #[arg(long)]
    pub retry_delay: Option<u64>,

    /// Temp file suffix for partial downloads (default: .icloudpd-tmp).
    /// Change if the default conflicts with your filesystem (e.g. Nextcloud rejects .part).
    #[arg(long)]
    pub temp_suffix: Option<String>,

    /// Force full library enumeration even if a sync token exists
    #[arg(long)]
    pub no_incremental: bool,

    /// Clear stored sync tokens before syncing (recovery tool)
    #[arg(long)]
    pub reset_sync_token: bool,

    /// Send systemd sd_notify messages (READY, STOPPING, STATUS).
    /// Only effective on Linux with a systemd service unit.
    #[arg(long)]
    pub notify_systemd: bool,

    /// Write PID to file (for service managers).
    #[arg(long)]
    pub pid_file: Option<std::path::PathBuf>,

    /// Script to run on events (2FA required, sync complete, etc.).
    /// Called with ICLOUDPD_EVENT, ICLOUDPD_MESSAGE, ICLOUDPD_USERNAME env vars.
    #[arg(long)]
    pub notification_script: Option<String>,
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

/// Arguments for the submit-code command.
#[derive(Parser, Debug, Clone)]
pub struct SubmitCodeArgs {
    #[command(flatten)]
    pub auth: AuthArgs,

    /// 6-digit 2FA code from your trusted device
    pub code: String,
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

    /// Submit a 2FA code non-interactively (for Docker / headless use)
    SubmitCode(SubmitCodeArgs),
}

#[derive(Parser, Debug)]
#[command(name = "icloudpd-rs", about = "Download iCloud photos and videos")]
pub struct Cli {
    /// Log level
    #[arg(long, value_enum, global = true)]
    pub log_level: Option<LogLevel>,

    /// Path to TOML config file
    #[arg(
        long,
        global = true,
        default_value = "~/.config/icloudpd-rs/config.toml"
    )]
    pub config: String,

    #[command(subcommand)]
    pub command: Option<Command>,

    // Backwards compatibility: allow all sync args at top level
    // These are only used when no subcommand is provided
    #[command(flatten)]
    pub auth: AuthArgs,

    #[command(flatten)]
    pub sync: SyncArgs,
}

impl Cli {
    /// Get the effective command, treating bare invocation as sync.
    pub fn effective_command(&self) -> Command {
        match &self.command {
            Some(cmd) => cmd.clone(),
            None => Command::Sync {
                auth: self.auth.clone(),
                sync: self.sync.clone(),
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
    fn test_library_none_by_default() {
        let cli = parse(&base_args());
        assert!(cli.sync.library.is_none());
    }

    #[test]
    fn test_library_accepts_custom_value() {
        let mut args = base_args();
        args.extend(["--library", "SharedSync-ABCD1234"]);
        let cli = parse(&args);
        assert_eq!(cli.sync.library.as_deref(), Some("SharedSync-ABCD1234"));
    }

    #[test]
    fn test_threads_num_none_by_default() {
        let cli = parse(&base_args());
        assert!(cli.sync.threads_num.is_none());
    }

    #[test]
    fn test_threads_num_accepts_valid_value() {
        let mut args = base_args();
        args.extend(["--threads-num", "8"]);
        let cli = parse(&args);
        assert_eq!(cli.sync.threads_num, Some(8));
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
        assert!(!cli.sync.dry_run);
    }

    #[test]
    fn test_size_none_by_default() {
        let cli = parse(&base_args());
        assert!(cli.sync.size.is_none());
    }

    #[test]
    fn test_size_accepts_value() {
        let mut args = base_args();
        args.extend(["--size", "medium"]);
        let cli = parse(&args);
        assert_eq!(cli.sync.size, Some(VersionSize::Medium));
    }

    #[test]
    fn test_recent_none_by_default() {
        let cli = parse(&base_args());
        assert!(cli.sync.recent.is_none());
    }

    #[test]
    fn test_recent_accepts_value() {
        let mut args = base_args();
        args.extend(["--recent", "50"]);
        let cli = parse(&args);
        assert_eq!(cli.sync.recent, Some(50));
    }

    #[test]
    fn test_max_retries_none_by_default() {
        let cli = parse(&base_args());
        assert!(cli.sync.max_retries.is_none());
    }

    #[test]
    fn test_max_retries_custom() {
        let mut args = base_args();
        args.extend(["--max-retries", "10"]);
        let cli = parse(&args);
        assert_eq!(cli.sync.max_retries, Some(10));
    }

    #[test]
    fn test_max_retries_zero_disables() {
        let mut args = base_args();
        args.extend(["--max-retries", "0"]);
        let cli = parse(&args);
        assert_eq!(cli.sync.max_retries, Some(0));
    }

    #[test]
    fn test_retry_delay_none_by_default() {
        let cli = parse(&base_args());
        assert!(cli.sync.retry_delay.is_none());
    }

    #[test]
    fn test_retry_delay_custom() {
        let mut args = base_args();
        args.extend(["--retry-delay", "15"]);
        let cli = parse(&args);
        assert_eq!(cli.sync.retry_delay, Some(15));
    }

    #[test]
    fn test_temp_suffix_none_by_default() {
        let cli = parse(&base_args());
        assert!(cli.sync.temp_suffix.is_none());
    }

    #[test]
    fn test_temp_suffix_custom() {
        let mut args = base_args();
        args.extend(["--temp-suffix", ".downloading"]);
        let cli = parse(&args);
        assert_eq!(cli.sync.temp_suffix.as_deref(), Some(".downloading"));
    }

    #[test]
    fn test_align_raw_none_by_default() {
        let cli = parse(&base_args());
        assert!(cli.sync.align_raw.is_none());
    }

    #[test]
    fn test_align_raw_accepts_original() {
        let mut args = base_args();
        args.extend(["--align-raw", "original"]);
        let cli = parse(&args);
        assert_eq!(cli.sync.align_raw, Some(RawTreatmentPolicy::PreferOriginal));
    }

    #[test]
    fn test_align_raw_accepts_alternative() {
        let mut args = base_args();
        args.extend(["--align-raw", "alternative"]);
        let cli = parse(&args);
        assert_eq!(
            cli.sync.align_raw,
            Some(RawTreatmentPolicy::PreferAlternative)
        );
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
    fn test_status_subcommand_without_username() {
        let cli = Cli::try_parse_from(["icloudpd-rs", "status"]).unwrap();
        if let Some(Command::Status(args)) = cli.command {
            assert!(args.auth.username.is_none());
        } else {
            panic!("Expected Status command");
        }
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
    fn test_notify_systemd_default_false() {
        let cli = parse(&base_args());
        assert!(!cli.sync.notify_systemd);
    }

    #[test]
    fn test_notify_systemd_flag() {
        let mut args = base_args();
        args.push("--notify-systemd");
        let cli = parse(&args);
        assert!(cli.sync.notify_systemd);
    }

    #[test]
    fn test_pid_file_default_none() {
        let cli = parse(&base_args());
        assert!(cli.sync.pid_file.is_none());
    }

    #[test]
    fn test_pid_file_flag() {
        let mut args = base_args();
        args.extend(["--pid-file", "/tmp/claude/test.pid"]);
        let cli = parse(&args);
        assert_eq!(
            cli.sync.pid_file,
            Some(std::path::PathBuf::from("/tmp/claude/test.pid"))
        );
    }

    #[test]
    fn test_backwards_compatibility_no_subcommand() {
        let cli = Cli::try_parse_from([
            "icloudpd-rs",
            "--username",
            "test@example.com",
            "--directory",
            "/photos",
        ])
        .unwrap();
        assert!(cli.command.is_none());
        match cli.effective_command() {
            Command::Sync { auth, sync } => {
                assert_eq!(auth.username.as_deref(), Some("test@example.com"));
                assert_eq!(sync.directory, Some("/photos".to_string()));
            }
            _ => panic!("Expected Sync command"),
        }
    }

    #[test]
    fn test_config_flag_default() {
        let cli = parse(&base_args());
        assert_eq!(cli.config, "~/.config/icloudpd-rs/config.toml");
    }

    #[test]
    fn test_config_flag_custom() {
        let mut args = base_args();
        args.extend(["--config", "/etc/icloudpd-rs.toml"]);
        let cli = parse(&args);
        assert_eq!(cli.config, "/etc/icloudpd-rs.toml");
    }

    #[test]
    fn test_domain_none_by_default() {
        let cli = parse(&base_args());
        assert!(cli.auth.domain.is_none());
    }

    #[test]
    fn test_domain_accepts_cn() {
        let mut args = base_args();
        args.extend(["--domain", "cn"]);
        let cli = parse(&args);
        assert_eq!(cli.auth.domain, Some(Domain::Cn));
    }

    #[test]
    fn test_cookie_directory_none_by_default() {
        let cli = parse(&base_args());
        assert!(cli.auth.cookie_directory.is_none());
    }

    #[test]
    fn test_log_level_none_by_default() {
        let cli = parse(&base_args());
        assert!(cli.log_level.is_none());
    }

    #[test]
    fn test_log_level_accepts_value() {
        let mut args = base_args();
        args.extend(["--log-level", "debug"]);
        let cli = parse(&args);
        assert_eq!(cli.log_level, Some(LogLevel::Debug));
    }

    // ── Username is optional at clap level ─────────────────────────

    #[test]
    fn test_bare_invocation_without_username() {
        let cli = Cli::try_parse_from(["icloudpd-rs"]).unwrap();
        assert!(cli.auth.username.is_none());
        assert!(cli.command.is_none());
    }

    // ── Auth flags ─────────────────────────────────────────────────

    #[test]
    fn test_password_flag() {
        let mut args = base_args();
        args.extend(["--password", "secret123"]);
        let cli = parse(&args);
        assert_eq!(cli.auth.password.as_deref(), Some("secret123"));
    }

    #[test]
    fn test_password_none_by_default() {
        let cli = parse(&base_args());
        assert!(cli.auth.password.is_none());
    }

    #[test]
    fn test_cookie_directory_custom() {
        let mut args = base_args();
        args.extend(["--cookie-directory", "/tmp/claude/cookies"]);
        let cli = parse(&args);
        assert_eq!(
            cli.auth.cookie_directory.as_deref(),
            Some("/tmp/claude/cookies")
        );
    }

    #[test]
    fn test_domain_accepts_com() {
        let mut args = base_args();
        args.extend(["--domain", "com"]);
        let cli = parse(&args);
        assert_eq!(cli.auth.domain, Some(Domain::Com));
    }

    #[test]
    fn test_domain_rejects_invalid() {
        let mut args = base_args();
        args.extend(["--domain", "uk"]);
        assert!(Cli::try_parse_from(&args).is_err());
    }

    // ── Boolean flags ──────────────────────────────────────────────

    #[test]
    fn test_auth_only_flag() {
        let mut args = base_args();
        args.push("--auth-only");
        let cli = parse(&args);
        assert!(cli.sync.auth_only);
    }

    #[test]
    fn test_auth_only_default_false() {
        let cli = parse(&base_args());
        assert!(!cli.sync.auth_only);
    }

    #[test]
    fn test_list_albums_flag() {
        let mut args = base_args();
        args.push("--list-albums");
        let cli = parse(&args);
        assert!(cli.sync.list_albums);
    }

    #[test]
    fn test_list_libraries_flag() {
        let mut args = base_args();
        args.push("--list-libraries");
        let cli = parse(&args);
        assert!(cli.sync.list_libraries);
    }

    #[test]
    fn test_skip_videos_flag() {
        let mut args = base_args();
        args.push("--skip-videos");
        let cli = parse(&args);
        assert!(cli.sync.skip_videos);
    }

    #[test]
    fn test_skip_photos_flag() {
        let mut args = base_args();
        args.push("--skip-photos");
        let cli = parse(&args);
        assert!(cli.sync.skip_photos);
    }

    #[test]
    fn test_skip_live_photos_flag() {
        let mut args = base_args();
        args.push("--skip-live-photos");
        let cli = parse(&args);
        assert!(cli.sync.skip_live_photos);
    }

    #[test]
    fn test_force_size_flag() {
        let mut args = base_args();
        args.push("--force-size");
        let cli = parse(&args);
        assert!(cli.sync.force_size);
    }

    #[test]
    fn test_set_exif_datetime_flag() {
        let mut args = base_args();
        args.push("--set-exif-datetime");
        let cli = parse(&args);
        assert!(cli.sync.set_exif_datetime);
    }

    #[test]
    fn test_no_progress_bar_flag() {
        let mut args = base_args();
        args.push("--no-progress-bar");
        let cli = parse(&args);
        assert!(cli.sync.no_progress_bar);
    }

    #[test]
    fn test_keep_unicode_in_filenames_flag() {
        let mut args = base_args();
        args.push("--keep-unicode-in-filenames");
        let cli = parse(&args);
        assert!(cli.sync.keep_unicode_in_filenames);
    }

    // ── Enum variants ──────────────────────────────────────────────

    #[test]
    fn test_size_all_variants() {
        for (input, expected) in [
            ("original", VersionSize::Original),
            ("medium", VersionSize::Medium),
            ("thumb", VersionSize::Thumb),
            ("adjusted", VersionSize::Adjusted),
            ("alternative", VersionSize::Alternative),
        ] {
            let mut args = base_args();
            args.extend(["--size", input]);
            let cli = parse(&args);
            assert_eq!(cli.sync.size, Some(expected), "size variant: {input}");
        }
    }

    #[test]
    fn test_live_photo_size_all_variants() {
        for (input, expected) in [
            ("original", LivePhotoSize::Original),
            ("medium", LivePhotoSize::Medium),
            ("thumb", LivePhotoSize::Thumb),
        ] {
            let mut args = base_args();
            args.extend(["--live-photo-size", input]);
            let cli = parse(&args);
            assert_eq!(
                cli.sync.live_photo_size,
                Some(expected),
                "live_photo_size variant: {input}"
            );
        }
    }

    #[test]
    fn test_live_photo_mov_filename_policy_all_variants() {
        for (input, expected) in [
            ("suffix", LivePhotoMovFilenamePolicy::Suffix),
            ("original", LivePhotoMovFilenamePolicy::Original),
        ] {
            let mut args = base_args();
            args.extend(["--live-photo-mov-filename-policy", input]);
            let cli = parse(&args);
            assert_eq!(
                cli.sync.live_photo_mov_filename_policy,
                Some(expected),
                "mov policy variant: {input}"
            );
        }
    }

    #[test]
    fn test_align_raw_accepts_as_is() {
        let mut args = base_args();
        args.extend(["--align-raw", "as-is"]);
        let cli = parse(&args);
        assert_eq!(cli.sync.align_raw, Some(RawTreatmentPolicy::Unchanged));
    }

    #[test]
    fn test_file_match_policy_all_variants() {
        for (input, expected) in [
            (
                "name-size-dedup-with-suffix",
                FileMatchPolicy::NameSizeDedupWithSuffix,
            ),
            ("name-id7", FileMatchPolicy::NameId7),
        ] {
            let mut args = base_args();
            args.extend(["--file-match-policy", input]);
            let cli = parse(&args);
            assert_eq!(
                cli.sync.file_match_policy,
                Some(expected),
                "file_match_policy variant: {input}"
            );
        }
    }

    #[test]
    fn test_log_level_all_variants() {
        for (input, expected) in [
            ("debug", LogLevel::Debug),
            ("info", LogLevel::Info),
            ("warn", LogLevel::Warn),
            ("error", LogLevel::Error),
        ] {
            let mut args = base_args();
            args.extend(["--log-level", input]);
            let cli = parse(&args);
            assert_eq!(cli.log_level, Some(expected), "log_level variant: {input}");
        }
    }

    // ── Optional value flags ───────────────────────────────────────

    #[test]
    fn test_folder_structure_custom() {
        let mut args = base_args();
        args.extend(["--folder-structure", "%Y-%m"]);
        let cli = parse(&args);
        assert_eq!(cli.sync.folder_structure.as_deref(), Some("%Y-%m"));
    }

    #[test]
    fn test_folder_structure_none_by_default() {
        let cli = parse(&base_args());
        assert!(cli.sync.folder_structure.is_none());
    }

    #[test]
    fn test_directory_custom() {
        let mut args = base_args();
        args.extend(["--directory", "/photos"]);
        let cli = parse(&args);
        assert_eq!(cli.sync.directory.as_deref(), Some("/photos"));
    }

    #[test]
    fn test_directory_none_by_default() {
        let cli = parse(&base_args());
        assert!(cli.sync.directory.is_none());
    }

    #[test]
    fn test_watch_with_interval() {
        let mut args = base_args();
        args.extend(["--watch-with-interval", "3600"]);
        let cli = parse(&args);
        assert_eq!(cli.sync.watch_with_interval, Some(3600));
    }

    #[test]
    fn test_skip_created_before() {
        let mut args = base_args();
        args.extend(["--skip-created-before", "2024-01-01"]);
        let cli = parse(&args);
        assert_eq!(cli.sync.skip_created_before.as_deref(), Some("2024-01-01"));
    }

    #[test]
    fn test_skip_created_after() {
        let mut args = base_args();
        args.extend(["--skip-created-after", "2025-06-01"]);
        let cli = parse(&args);
        assert_eq!(cli.sync.skip_created_after.as_deref(), Some("2025-06-01"));
    }

    #[test]
    fn test_albums_multiple() {
        let mut args = base_args();
        args.extend(["--album", "Favorites", "--album", "Vacation"]);
        let cli = parse(&args);
        assert_eq!(cli.sync.albums, vec!["Favorites", "Vacation"]);
    }

    #[test]
    fn test_albums_empty_by_default() {
        let cli = parse(&base_args());
        assert!(cli.sync.albums.is_empty());
    }

    // ── Subcommands without username ───────────────────────────────

    #[test]
    fn test_verify_subcommand_without_username() {
        let cli = Cli::try_parse_from(["icloudpd-rs", "verify"]).unwrap();
        if let Some(Command::Verify(args)) = cli.command {
            assert!(args.auth.username.is_none());
            assert!(!args.checksums);
        } else {
            panic!("Expected Verify command");
        }
    }

    #[test]
    fn test_verify_subcommand_with_checksums() {
        let cli = Cli::try_parse_from([
            "icloudpd-rs",
            "verify",
            "--username",
            "test@example.com",
            "--checksums",
        ])
        .unwrap();
        if let Some(Command::Verify(args)) = cli.command {
            assert!(args.checksums);
        } else {
            panic!("Expected Verify command");
        }
    }

    #[test]
    fn test_reset_state_subcommand_without_username() {
        let cli = Cli::try_parse_from(["icloudpd-rs", "reset-state"]).unwrap();
        if let Some(Command::ResetState(args)) = cli.command {
            assert!(args.auth.username.is_none());
            assert!(!args.yes);
        } else {
            panic!("Expected ResetState command");
        }
    }

    #[test]
    fn test_import_existing_subcommand() {
        let cli = Cli::try_parse_from([
            "icloudpd-rs",
            "import-existing",
            "--username",
            "test@example.com",
            "--directory",
            "/photos",
        ])
        .unwrap();
        if let Some(Command::ImportExisting(args)) = cli.command {
            assert_eq!(args.auth.username.as_deref(), Some("test@example.com"));
            assert_eq!(args.directory, "/photos");
            assert_eq!(args.folder_structure, "%Y/%m/%d");
            assert!(args.recent.is_none());
        } else {
            panic!("Expected ImportExisting command");
        }
    }

    #[test]
    fn test_import_existing_without_username() {
        let cli = Cli::try_parse_from(["icloudpd-rs", "import-existing", "--directory", "/photos"])
            .unwrap();
        if let Some(Command::ImportExisting(args)) = cli.command {
            assert!(args.auth.username.is_none());
        } else {
            panic!("Expected ImportExisting command");
        }
    }

    #[test]
    fn test_retry_failed_subcommand() {
        let cli = Cli::try_parse_from([
            "icloudpd-rs",
            "retry-failed",
            "--username",
            "test@example.com",
            "--directory",
            "/photos",
        ])
        .unwrap();
        if let Some(Command::RetryFailed(args)) = cli.command {
            assert_eq!(args.auth.username.as_deref(), Some("test@example.com"));
            assert_eq!(args.sync.directory.as_deref(), Some("/photos"));
        } else {
            panic!("Expected RetryFailed command");
        }
    }

    #[test]
    fn test_retry_failed_without_username() {
        let cli = Cli::try_parse_from(["icloudpd-rs", "retry-failed"]).unwrap();
        if let Some(Command::RetryFailed(args)) = cli.command {
            assert!(args.auth.username.is_none());
        } else {
            panic!("Expected RetryFailed command");
        }
    }

    // ── --config global flag works with all subcommands ────────────

    #[test]
    fn test_config_global_with_sync_subcommand() {
        let cli = Cli::try_parse_from([
            "icloudpd-rs",
            "sync",
            "--config",
            "/custom/config.toml",
            "--username",
            "test@example.com",
        ])
        .unwrap();
        assert_eq!(cli.config, "/custom/config.toml");
    }

    #[test]
    fn test_config_global_with_status_subcommand() {
        let cli = Cli::try_parse_from(["icloudpd-rs", "status", "--config", "/custom/config.toml"])
            .unwrap();
        assert_eq!(cli.config, "/custom/config.toml");
    }

    #[test]
    fn test_config_global_with_verify_subcommand() {
        let cli = Cli::try_parse_from(["icloudpd-rs", "verify", "--config", "/custom/config.toml"])
            .unwrap();
        assert_eq!(cli.config, "/custom/config.toml");
    }

    #[test]
    fn test_config_global_with_reset_state_subcommand() {
        let cli = Cli::try_parse_from([
            "icloudpd-rs",
            "reset-state",
            "--config",
            "/custom/config.toml",
        ])
        .unwrap();
        assert_eq!(cli.config, "/custom/config.toml");
    }

    #[test]
    fn test_config_global_before_subcommand() {
        let cli = Cli::try_parse_from(["icloudpd-rs", "--config", "/custom/config.toml", "status"])
            .unwrap();
        assert_eq!(cli.config, "/custom/config.toml");
        assert!(matches!(cli.command, Some(Command::Status(_))));
    }

    // ── submit-code subcommand ────────────────────────────────────

    #[test]
    fn test_submit_code_subcommand() {
        let cli = Cli::try_parse_from([
            "icloudpd-rs",
            "submit-code",
            "--username",
            "test@example.com",
            "123456",
        ])
        .unwrap();
        if let Some(Command::SubmitCode(args)) = cli.command {
            assert_eq!(args.auth.username.as_deref(), Some("test@example.com"));
            assert_eq!(args.code, "123456");
        } else {
            panic!("Expected SubmitCode command");
        }
    }

    #[test]
    fn test_submit_code_without_username() {
        let cli = Cli::try_parse_from(["icloudpd-rs", "submit-code", "123456"]).unwrap();
        if let Some(Command::SubmitCode(args)) = cli.command {
            assert!(args.auth.username.is_none());
            assert_eq!(args.code, "123456");
        } else {
            panic!("Expected SubmitCode command");
        }
    }

    #[test]
    fn test_submit_code_requires_code_arg() {
        assert!(Cli::try_parse_from(["icloudpd-rs", "submit-code"]).is_err());
    }

    #[test]
    fn test_submit_code_with_config() {
        let cli = Cli::try_parse_from([
            "icloudpd-rs",
            "submit-code",
            "--config",
            "/custom/config.toml",
            "654321",
        ])
        .unwrap();
        assert_eq!(cli.config, "/custom/config.toml");
        if let Some(Command::SubmitCode(args)) = cli.command {
            assert_eq!(args.code, "654321");
        } else {
            panic!("Expected SubmitCode command");
        }
    }

    // ── no-incremental / reset-sync-token flags ───────────────────

    #[test]
    fn test_no_incremental_default_false() {
        let cli = parse(&base_args());
        assert!(!cli.sync.no_incremental);
    }

    #[test]
    fn test_no_incremental_flag() {
        let mut args = base_args();
        args.push("--no-incremental");
        let cli = parse(&args);
        assert!(cli.sync.no_incremental);
    }

    #[test]
    fn test_reset_sync_token_default_false() {
        let cli = parse(&base_args());
        assert!(!cli.sync.reset_sync_token);
    }

    #[test]
    fn test_reset_sync_token_flag() {
        let mut args = base_args();
        args.push("--reset-sync-token");
        let cli = parse(&args);
        assert!(cli.sync.reset_sync_token);
    }

    #[test]
    fn test_no_incremental_and_reset_sync_token_together() {
        let mut args = base_args();
        args.push("--no-incremental");
        args.push("--reset-sync-token");
        let cli = parse(&args);
        assert!(cli.sync.no_incremental);
        assert!(cli.sync.reset_sync_token);
    }

    // ── notification-script flag ──────────────────────────────────

    #[test]
    fn test_notification_script_none_by_default() {
        let cli = parse(&base_args());
        assert!(cli.sync.notification_script.is_none());
    }

    #[test]
    fn test_notification_script_flag() {
        let mut args = base_args();
        args.extend(["--notification-script", "/path/to/notify.sh"]);
        let cli = parse(&args);
        assert_eq!(
            cli.sync.notification_script.as_deref(),
            Some("/path/to/notify.sh")
        );
    }
}
