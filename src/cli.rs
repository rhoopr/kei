use crate::types::{
    Domain, FileMatchPolicy, LivePhotoMode, LivePhotoMovFilenamePolicy, LivePhotoSize, LogLevel,
    RawTreatmentPolicy, VersionSize,
};
use clap::{Parser, Subcommand};

/// Reject empty strings at CLI parse time.
fn non_empty_string(s: &str) -> Result<String, String> {
    if s.is_empty() {
        Err("value must not be empty".to_string())
    } else {
        Ok(s.to_string())
    }
}

/// Parse a human-readable byte-rate into bytes per second.
///
/// Accepts a non-negative number (integer or decimal, e.g. `1.5`) followed by
/// an optional unit suffix: decimal `K`/`M`/`G` (x1000) or binary
/// `Ki`/`Mi`/`Gi` (x1024). Suffix is case-insensitive. No suffix means
/// bytes/sec. Values that round to zero bytes/sec are rejected.
pub(crate) fn parse_bandwidth_limit(s: &str) -> Result<u64, String> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return Err("value must not be empty".to_string());
    }
    // Numeric part is digits plus at most one decimal point; everything after
    // that is the unit suffix. Leading sign is not accepted here (negative
    // bandwidth is meaningless; `-5M` errors with "must start with a number").
    let num_end = trimmed
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(trimmed.len());
    if num_end == 0 {
        return Err(format!(
            "invalid bandwidth value `{s}`: must start with a number"
        ));
    }
    let (num_str, suffix) = trimmed.split_at(num_end);
    let n: f64 = num_str
        .parse()
        .map_err(|_| format!("invalid bandwidth number `{num_str}`"))?;
    if !n.is_finite() || n < 0.0 {
        return Err(format!(
            "invalid bandwidth number `{num_str}`: must be a finite non-negative number"
        ));
    }
    let multiplier: u64 = match suffix.trim().to_ascii_lowercase().as_str() {
        "" | "b" => 1,
        "k" | "kb" => 1_000,
        "m" | "mb" => 1_000_000,
        "g" | "gb" => 1_000_000_000,
        "ki" | "kib" => 1_024,
        "mi" | "mib" => 1_024 * 1_024,
        "gi" | "gib" => 1_024 * 1_024 * 1_024,
        other => {
            return Err(format!(
                "invalid bandwidth unit `{other}`: expected one of K, M, G, Ki, Mi, Gi"
            ));
        }
    };
    // f64 multiplication is exact for the typical bandwidth range (KB/s to
    // GB/s) and off by less than a byte for extreme values; round to nearest
    // so inputs like `0.1K` land on 100 bytes/sec rather than 99.
    #[allow(
        clippy::cast_precision_loss,
        reason = "multiplier is a small constant (<= 2^30); u64::MAX is only a comparison bound where exact precision doesn't matter"
    )]
    let (max_f64, multiplier_f64) = (u64::MAX as f64, multiplier as f64);
    let total = n * multiplier_f64;
    if !total.is_finite() || total > max_f64 {
        return Err(format!("bandwidth value `{s}` overflows u64 bytes/sec"));
    }
    let rounded = total.round();
    if rounded < 1.0 {
        return Err(format!(
            "bandwidth value `{s}` rounds to zero bytes/sec; must be at least 1 byte/sec"
        ));
    }
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "bounds checked above: 1.0 <= rounded <= u64::MAX as f64"
    )]
    Ok(rounded as u64)
}

/// Strip non-digit characters and validate that the result is exactly 6 digits.
/// Accepts "123456", "123 456", "123-456", etc.
fn parse_2fa_code(s: &str) -> Result<String, String> {
    let digits: String = s.chars().filter(char::is_ascii_digit).collect();
    if digits.len() == 6 {
        Ok(digits)
    } else {
        Err("must contain exactly 6 digits".to_string())
    }
}

/// Print a deprecation warning to stderr.
#[allow(
    clippy::print_stderr,
    reason = "runs during CLI arg parsing, before tracing subscriber is installed"
)]
pub(crate) fn deprecation_warning(old: &str, new: &str) {
    eprintln!("warning: `{old}` is deprecated and will be removed in v0.20.0; use `{new}` instead");
}

/// Limit on which assets a sync pass processes.
///
/// `--recent 100` is a count limit (top N most-recent assets). `--recent 30d`
/// is a days limit (assets created in the last 30 days) and translates to a
/// `skip_created_before` cutoff at `Config::build` time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecentLimit {
    /// Take the N most-recent assets by creation date.
    Count(u32),
    /// Take assets created in the last N days.
    Days(u32),
}

/// Parse `--recent N` (count) or `--recent Nd` (days). Clap `value_parser`.
///
/// Rejects zero, empty, and unknown suffixes. Only `d` (days) is supported
/// today; this keeps the syntax open for future units without locking us in.
pub(crate) fn parse_recent_limit(s: &str) -> Result<RecentLimit, String> {
    if s.is_empty() {
        return Err("must not be empty".to_string());
    }
    let (num_str, is_days) = if let Some(stripped) = s.strip_suffix('d') {
        (stripped, true)
    } else {
        (s, false)
    };
    let n: u32 = num_str
        .parse()
        .map_err(|_| format!("expected a positive integer or `Nd` form (got `{s}`)"))?;
    if n == 0 {
        return Err(format!("must be greater than zero (got `{s}`)"));
    }
    Ok(if is_days {
        RecentLimit::Days(n)
    } else {
        RecentLimit::Count(n)
    })
}

impl std::fmt::Display for RecentLimit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Count(n) => write!(f, "{n}"),
            Self::Days(n) => write!(f, "{n}d"),
        }
    }
}

impl serde::Serialize for RecentLimit {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            // Count serializes as a bare integer so TOML round-trips cleanly
            // for the common case (`recent = 100`, not `recent = "100"`).
            Self::Count(n) => s.serialize_u32(*n),
            Self::Days(n) => s.serialize_str(&format!("{n}d")),
        }
    }
}

impl<'de> serde::Deserialize<'de> for RecentLimit {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        use serde::de::Error;
        #[derive(serde::Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Int(u32),
            Str(String),
        }
        match Raw::deserialize(d)? {
            Raw::Int(0) => Err(D::Error::custom("`recent` must be greater than zero")),
            Raw::Int(n) => Ok(Self::Count(n)),
            Raw::Str(s) => parse_recent_limit(&s).map_err(D::Error::custom),
        }
    }
}

/// Password arguments shared across commands that authenticate.
/// Flattened only onto commands that actually need a password (sync, login,
/// list, import-existing, password).
#[derive(Parser, Debug, Clone, Default)]
pub struct PasswordArgs {
    /// iCloud password (if not provided, will prompt).
    /// WARNING: passing via --password is visible in process listings.
    /// Prefer the `ICLOUD_PASSWORD` environment variable instead.
    #[arg(short = 'p', long, env = "ICLOUD_PASSWORD", value_parser = non_empty_string)]
    pub password: Option<String>,

    /// Read password from a file on each auth attempt.
    /// Supports Docker secrets (e.g., /run/secrets/icloud_password).
    /// Trailing newline is stripped.
    #[arg(long, env = "KEI_PASSWORD_FILE", conflicts_with = "password")]
    pub password_file: Option<String>,

    /// Execute a shell command to obtain the password on each auth attempt.
    /// Supports external secret managers (1Password, Vault, pass).
    /// Example: --password-command "op read 'op://vault/icloud/password'"
    #[arg(long, env = "KEI_PASSWORD_COMMAND", conflicts_with_all = ["password", "password_file"])]
    pub password_command: Option<String>,
}

/// Arguments for the sync command (also used as default when no subcommand).
#[derive(Parser, Debug, Clone, Default)]
pub struct SyncArgs {
    /// Local directory for downloads
    #[arg(short = 'd', long = "download-dir", env = "KEI_DOWNLOAD_DIR", value_parser = non_empty_string)]
    pub download_dir: Option<String>,

    /// Deprecated: use --download-dir (will be removed in v0.20.0)
    #[arg(long = "directory", env = "KEI_DIRECTORY", value_parser = non_empty_string, hide = true)]
    pub directory: Option<String>,

    /// Album(s) to download. Use `-a all` to sync every user-created album
    /// in one run (Apple's smart folders like "Favorites" are skipped —
    /// name them explicitly if you want them). `-a all` cannot be combined
    /// with specific album names.
    #[arg(short = 'a', long = "album", env = "KEI_ALBUM", value_parser = non_empty_string)]
    pub albums: Vec<String>,

    /// Album(s) to exclude from sync
    #[arg(long = "exclude-album", env = "KEI_EXCLUDE_ALBUM", value_delimiter = ',', value_parser = non_empty_string)]
    pub exclude_albums: Vec<String>,

    /// Exclude files matching glob pattern(s) (e.g. "*.AAE", "Screenshot*")
    #[arg(long = "filename-exclude", env = "KEI_FILENAME_EXCLUDE", value_delimiter = ',', value_parser = non_empty_string)]
    pub filename_exclude: Vec<String>,

    /// Library to download (default: `PrimarySync`, use "all" for all libraries)
    #[arg(long, env = "KEI_LIBRARY")]
    pub library: Option<String>,

    /// Image size to download
    #[arg(long, env = "KEI_SIZE", value_enum)]
    pub size: Option<VersionSize>,

    /// Live photo video size
    #[arg(long, env = "KEI_LIVE_PHOTO_SIZE", value_enum)]
    pub live_photo_size: Option<LivePhotoSize>,

    /// Number of recent photos to download (e.g. `--recent 100`) or a recency
    /// window in days (e.g. `--recent 30d`). Days form maps to
    /// `--skip-created-before` internally.
    #[arg(long, env = "KEI_RECENT", value_parser = parse_recent_limit)]
    pub recent: Option<RecentLimit>,

    /// Number of concurrent download threads (default: 10)
    #[arg(long = "threads", env = "KEI_THREADS", value_parser = clap::value_parser!(u16).range(1..=64))]
    pub threads: Option<u16>,

    /// Deprecated: use --threads (will be removed in v0.20.0)
    #[arg(long = "threads-num", env = "KEI_THREADS_NUM", value_parser = clap::value_parser!(u16).range(1..=64), hide = true)]
    pub threads_num: Option<u16>,

    /// Cap total download throughput across all concurrent downloads.
    /// Accepts human-readable values: `10M` (10 MB/s), `500K` (500 KB/s),
    /// `1Mi` (1 MiB/s), bare integer = bytes/s. When set without an explicit
    /// `--threads`, concurrency defaults to 1 to avoid fragmenting the
    /// capped pipe across many starved connections.
    #[arg(long = "bandwidth-limit", env = "KEI_BANDWIDTH_LIMIT", value_parser = parse_bandwidth_limit)]
    pub bandwidth_limit: Option<u64>,

    /// Don't download videos (pass `false` to override config file)
    #[arg(long, env = "KEI_SKIP_VIDEOS", num_args = 0..=1, default_missing_value = "true", hide_possible_values = true)]
    pub skip_videos: Option<bool>,

    /// Don't download photos (pass `false` to override config file)
    #[arg(long, env = "KEI_SKIP_PHOTOS", num_args = 0..=1, default_missing_value = "true", hide_possible_values = true)]
    pub skip_photos: Option<bool>,

    /// Live photo handling: both, image-only, video-only, skip
    #[arg(long, env = "KEI_LIVE_PHOTO_MODE", value_enum)]
    pub live_photo_mode: Option<LivePhotoMode>,

    /// Deprecated: use `--live-photo-mode skip` (will be removed in v0.20.0)
    #[arg(long, env = "KEI_SKIP_LIVE_PHOTOS", num_args = 0..=1, default_missing_value = "true", hide_possible_values = true, hide = true)]
    pub skip_live_photos: Option<bool>,

    /// Only download requested size (don't fall back to original)
    #[arg(long, env = "KEI_FORCE_SIZE", num_args = 0..=1, default_missing_value = "true", hide_possible_values = true)]
    pub force_size: Option<bool>,

    /// Folder structure for organizing downloads (e.g., "%Y/%m/%d",
    /// "{album}/%Y/%B", "none"). `{album}` must be the first segment and
    /// may only appear once. When `{album}` is used without `-a`, kei
    /// auto-syncs every user-created album plus a library-wide pass for
    /// unfiled photos (where `{album}` collapses to empty).
    #[arg(long, env = "KEI_FOLDER_STRUCTURE")]
    pub folder_structure: Option<String>,

    /// Write `DateTimeOriginal` EXIF tag if missing
    #[cfg(feature = "xmp")]
    #[arg(long, env = "KEI_SET_EXIF_DATETIME", num_args = 0..=1, default_missing_value = "true", hide_possible_values = true)]
    pub set_exif_datetime: Option<bool>,

    /// Write EXIF `Rating` tag (0x4746) for favorited photos (1-5 scale)
    #[cfg(feature = "xmp")]
    #[arg(long, env = "KEI_SET_EXIF_RATING", num_args = 0..=1, default_missing_value = "true", hide_possible_values = true)]
    pub set_exif_rating: Option<bool>,

    /// Write EXIF GPS tags from iCloud location metadata (only when the file lacks GPS)
    #[cfg(feature = "xmp")]
    #[arg(long, env = "KEI_SET_EXIF_GPS", num_args = 0..=1, default_missing_value = "true", hide_possible_values = true)]
    pub set_exif_gps: Option<bool>,

    /// Write EXIF `ImageDescription` tag from iCloud description / title
    #[cfg(feature = "xmp")]
    #[arg(long, env = "KEI_SET_EXIF_DESCRIPTION", num_args = 0..=1, default_missing_value = "true", hide_possible_values = true)]
    pub set_exif_description: Option<bool>,

    /// Embed a full XMP packet (title, keywords, album memberships, people,
    /// hidden/archived, media subtype, burst id) into downloaded media bytes
    /// on supported formats (JPEG/HEIC/PNG/TIFF/MP4/MOV)
    #[cfg(feature = "xmp")]
    #[arg(long, env = "KEI_EMBED_XMP", num_args = 0..=1, default_missing_value = "true", hide_possible_values = true)]
    pub embed_xmp: Option<bool>,

    /// Write a `.xmp` sidecar file next to each downloaded media file with
    /// every available metadata field
    #[cfg(feature = "xmp")]
    #[arg(long, env = "KEI_XMP_SIDECAR", num_args = 0..=1, default_missing_value = "true", hide_possible_values = true)]
    pub xmp_sidecar: Option<bool>,

    /// Do not modify local system or iCloud
    #[arg(long, conflicts_with = "watch_with_interval")]
    pub dry_run: bool,

    /// Run continuously, waiting N seconds between runs (60..=86400)
    #[arg(long, env = "KEI_WATCH_WITH_INTERVAL", value_parser = clap::value_parser!(u64).range(60..=86400))]
    pub watch_with_interval: Option<u64>,

    /// Disable progress bar
    #[arg(long, env = "KEI_NO_PROGRESS_BAR", num_args = 0..=1, default_missing_value = "true", hide_possible_values = true)]
    pub no_progress_bar: Option<bool>,

    /// Keep Unicode in filenames
    #[arg(long, env = "KEI_KEEP_UNICODE_IN_FILENAMES", num_args = 0..=1, default_missing_value = "true", hide_possible_values = true)]
    pub keep_unicode_in_filenames: Option<bool>,

    /// Live photo MOV filename policy
    #[arg(long, env = "KEI_LIVE_PHOTO_MOV_FILENAME_POLICY", value_enum)]
    pub live_photo_mov_filename_policy: Option<LivePhotoMovFilenamePolicy>,

    /// RAW treatment policy
    #[arg(long, env = "KEI_ALIGN_RAW", value_enum)]
    pub align_raw: Option<RawTreatmentPolicy>,

    /// File matching and dedup policy
    #[arg(long, env = "KEI_FILE_MATCH_POLICY", value_enum)]
    pub file_match_policy: Option<FileMatchPolicy>,

    /// Skip assets created before this ISO date or interval (e.g., 2025-01-02 or 20d)
    #[arg(long, env = "KEI_SKIP_CREATED_BEFORE")]
    pub skip_created_before: Option<String>,

    /// Skip assets created after this ISO date or interval (e.g., 2025-01-02 or 20d)
    #[arg(long, env = "KEI_SKIP_CREATED_AFTER")]
    pub skip_created_after: Option<String>,

    /// Only print filenames without downloading
    #[arg(long, conflicts_with = "watch_with_interval")]
    pub only_print_filenames: bool,

    /// Max retries per download (default: 3, 0 = no retries, max: 100)
    #[arg(long, env = "KEI_MAX_RETRIES", value_parser = clap::value_parser!(u32).range(0..=100))]
    pub max_retries: Option<u32>,

    /// Deprecated: initial retry delay is now derived from `--max-retries`
    /// (will be removed in v0.20.0)
    #[arg(long, env = "KEI_RETRY_DELAY", value_parser = clap::value_parser!(u64).range(1..=3600), hide = true)]
    pub retry_delay: Option<u64>,

    /// Temp file suffix for partial downloads (default: .kei-tmp).
    /// Change if the default conflicts with your filesystem (e.g. Nextcloud rejects .part).
    #[arg(long, env = "KEI_TEMP_SUFFIX")]
    pub temp_suffix: Option<String>,

    /// Deprecated: use `kei reset sync-token` before a sync for the same
    /// effect (will be removed in v0.20.0)
    #[arg(long, hide = true)]
    pub no_incremental: bool,

    /// Send systemd `sd_notify` messages (READY, STOPPING, STATUS).
    /// Auto-detected via `NOTIFY_SOCKET` under `Type=notify` units; pass
    /// `--notify-systemd=false` to suppress even then.
    #[arg(long, env = "KEI_NOTIFY_SYSTEMD", num_args = 0..=1, default_missing_value = "true", hide_possible_values = true)]
    pub notify_systemd: Option<bool>,

    /// Write PID to file (for service managers).
    #[arg(long, env = "KEI_PID_FILE")]
    pub pid_file: Option<std::path::PathBuf>,

    /// Script to run on events: 2FA required, sync started/complete/failed, session expired.
    /// Called with `KEI_EVENT`, `KEI_MESSAGE`, `KEI_ICLOUD_USERNAME` env vars.
    /// Fire-and-forget: script failures are logged at warn! but never block
    /// the sync or affect the exit code. Unix only: ignored with a warning on Windows.
    #[arg(long, env = "KEI_NOTIFICATION_SCRIPT")]
    pub notification_script: Option<String>,

    /// Write a JSON run report to this path after each sync cycle.
    /// Also configurable via `[report] json = "..."` in the TOML config.
    /// In watch mode the file is overwritten every cycle - it reflects the
    /// most recent cycle only, not a history log. Atomic write (temp + rename).
    #[arg(long, env = "KEI_REPORT_JSON")]
    pub report_json: Option<std::path::PathBuf>,

    /// Port for the always-on HTTP server that serves `/healthz` and `/metrics` (default: 9090).
    /// Set `KEI_HTTP_PORT` to override via environment.
    #[arg(long, env = "KEI_HTTP_PORT", value_parser = clap::value_parser!(u16).range(1..))]
    pub http_port: Option<u16>,

    /// Bind address for the HTTP server (default: 0.0.0.0 - all interfaces).
    /// Set to `127.0.0.1` to restrict to loopback only (desktop use). Also configurable
    /// via `[server] bind` in TOML. Accepts any IPv4 or IPv6 address.
    #[arg(long, env = "KEI_HTTP_BIND")]
    pub http_bind: Option<std::net::IpAddr>,

    /// After successful auth, persist the password to the credential store
    /// (OS keyring or encrypted file).
    #[arg(long)]
    pub save_password: bool,

    /// Re-sync only previously failed assets
    #[arg(long, conflicts_with_all = ["dry_run", "watch_with_interval"])]
    pub retry_failed: bool,

    /// Maximum download attempts per asset before skipping (default: 10)
    #[arg(long, env = "KEI_MAX_DOWNLOAD_ATTEMPTS")]
    pub max_download_attempts: Option<u32>,

    // ── Hidden compat flags (deprecated, still parse) ──────────────
    /// Deprecated: use `kei login` instead
    #[arg(long, hide = true, conflicts_with_all = ["watch_with_interval"])]
    pub auth_only: bool,

    /// Deprecated: use `kei list albums` instead
    #[arg(short = 'l', long, hide = true, conflicts_with_all = ["watch_with_interval"])]
    pub list_albums: bool,

    /// Deprecated: use `kei list libraries` instead
    #[arg(long, hide = true, conflicts_with = "watch_with_interval")]
    pub list_libraries: bool,

    /// Deprecated: use `kei reset sync-token` instead
    #[arg(long, hide = true)]
    pub reset_sync_token: bool,
}

/// Arguments for the status command.
#[derive(Parser, Debug, Clone)]
pub struct StatusArgs {
    /// Show failed assets with error messages
    #[arg(long)]
    pub failed: bool,

    /// Show pending assets (known to the DB, not yet finalized this sync).
    /// Includes assets the current sync scope excludes via filters or album
    /// selection.
    #[arg(long)]
    pub pending: bool,

    /// Show downloaded assets
    #[arg(long)]
    pub downloaded: bool,
}

/// Arguments for the import-existing command.
#[derive(Parser, Debug, Clone)]
pub struct ImportArgs {
    #[command(flatten)]
    pub password: PasswordArgs,

    /// Library to import (default: PrimarySync, use "all" for all libraries)
    #[arg(long, env = "KEI_LIBRARY")]
    pub library: Option<String>,

    /// Local directory containing existing downloads
    #[arg(short = 'd', long = "download-dir", env = "KEI_DOWNLOAD_DIR", value_parser = non_empty_string)]
    pub download_dir: Option<String>,

    /// Deprecated: use --download-dir (will be removed in v0.20.0)
    #[arg(long = "directory", env = "KEI_DIRECTORY", value_parser = non_empty_string, hide = true)]
    pub directory: Option<String>,

    /// Folder structure used for existing downloads
    #[arg(long, env = "KEI_FOLDER_STRUCTURE")]
    pub folder_structure: Option<String>,

    /// Keep Unicode in filenames (must match what was used during sync)
    #[arg(long)]
    pub keep_unicode_in_filenames: Option<bool>,

    /// Number of recent photos to check (`--recent 100`). The `--recent Nd`
    /// days form is only supported in `sync`; import-existing errors on use.
    #[arg(long, value_parser = parse_recent_limit)]
    pub recent: Option<RecentLimit>,

    /// Scan and report matches without writing to the state DB. Useful for
    /// verifying that `--folder-structure` and `--keep-unicode-in-filenames`
    /// match the tree you're importing before committing.
    #[arg(long)]
    pub dry_run: bool,

    /// Disable progress bar
    #[arg(long)]
    pub no_progress_bar: bool,
}

/// Arguments for the verify command.
#[derive(Parser, Debug, Clone)]
pub struct VerifyArgs {
    /// Verify checksums (slower but more thorough)
    #[arg(long)]
    pub checksums: bool,
}

/// Arguments for the reconcile command.
#[derive(Parser, Debug, Clone)]
pub struct ReconcileArgs {
    /// Show what would change without updating the state database.
    #[arg(long)]
    pub dry_run: bool,
}

// ── New subcommand types ─────────────────────────────────────────────

/// Login subcommands.
#[derive(Subcommand, Debug, Clone, PartialEq, Eq)]
pub enum LoginCommand {
    /// Request a 2FA code be sent to your trusted devices
    GetCode,
    /// Submit a 2FA code non-interactively (for Docker / headless use)
    SubmitCode {
        /// 6-digit 2FA code from your trusted device
        #[arg(value_parser = parse_2fa_code)]
        code: String,
    },
}

/// List subcommands.
#[derive(Subcommand, Debug, Clone, PartialEq, Eq)]
pub enum ListCommand {
    /// List available albums
    Albums,
    /// List available libraries
    Libraries,
}

/// Password management actions.
#[derive(Subcommand, Debug, Clone, PartialEq, Eq)]
pub enum PasswordAction {
    /// Store a password in the credential store (prompts interactively)
    Set,
    /// Remove a stored password
    Clear,
    /// Show which credential backend is active (keyring, encrypted-file, none)
    Backend,
}

/// Reset subcommands.
#[derive(Subcommand, Debug, Clone, PartialEq, Eq)]
pub enum ResetCommand {
    /// Delete the state database and start fresh
    State {
        /// Skip confirmation prompt
        #[arg(long, short = 'y')]
        yes: bool,
    },
    /// Clear stored sync tokens so the next sync does a full enumeration
    SyncToken,
}

/// Config management actions.
#[derive(Subcommand, Debug, Clone, PartialEq, Eq)]
pub enum ConfigAction {
    /// Dump resolved config as TOML and exit
    Show,
    /// Interactively generate a config file
    Setup {
        /// Output path (overrides --config)
        #[arg(short = 'o', long)]
        output: Option<String>,
    },
}

/// Credential management actions (legacy, hidden).
#[derive(Subcommand, Debug, Clone, PartialEq, Eq)]
pub(crate) enum CredentialAction {
    /// Store a password in the credential store (prompts interactively)
    Set,
    /// Remove a stored password
    Clear,
    /// Show which credential backend is active (keyring, encrypted-file, none)
    Backend,
}

/// Subcommands for kei.
#[derive(Subcommand, Debug, Clone)]
pub enum Command {
    /// Download photos from iCloud (default command)
    Sync {
        #[command(flatten)]
        password: PasswordArgs,

        #[command(flatten)]
        sync: SyncArgs,
    },

    /// Authenticate interactively (creates/refreshes session tokens)
    Login {
        #[command(flatten)]
        password: PasswordArgs,

        #[command(subcommand)]
        subcommand: Option<LoginCommand>,
    },

    /// List available albums or libraries
    List {
        #[command(flatten)]
        password: PasswordArgs,

        /// Library to list albums from (default: PrimarySync, use "all" for all)
        #[arg(long, env = "KEI_LIBRARY")]
        library: Option<String>,

        #[command(subcommand)]
        what: ListCommand,
    },

    /// Manage stored credentials (OS keyring or encrypted file)
    Password {
        #[command(flatten)]
        password: PasswordArgs,

        #[command(subcommand)]
        action: PasswordAction,
    },

    /// Reset state database or sync tokens
    Reset {
        #[command(subcommand)]
        what: ResetCommand,
    },

    /// Config management
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },

    /// Show sync status and database summary
    Status(StatusArgs),

    /// Import existing local files into the state database
    ImportExisting(ImportArgs),

    /// Verify downloaded files exist and optionally check checksums
    Verify(VerifyArgs),

    /// Reconcile state database with files on disk: mark assets as
    /// failed when their local file is missing, so the next sync
    /// re-downloads them.
    Reconcile(ReconcileArgs),

    // ── Hidden legacy aliases (deprecated, still parse) ──────────
    /// Deprecated: use `kei login get-code`
    #[command(hide = true)]
    GetCode {
        #[command(flatten)]
        password: PasswordArgs,
    },

    /// Deprecated: use `kei login submit-code`
    #[command(hide = true)]
    SubmitCode {
        #[command(flatten)]
        password: PasswordArgs,

        /// 6-digit 2FA code from your trusted device
        #[arg(value_parser = parse_2fa_code)]
        code: String,
    },

    /// Deprecated: use `kei password`
    #[command(hide = true)]
    Credential {
        #[command(flatten)]
        password: PasswordArgs,

        #[command(subcommand)]
        action: CredentialAction,
    },

    /// Deprecated: use `kei sync --retry-failed`
    #[command(hide = true)]
    RetryFailed {
        #[command(flatten)]
        password: PasswordArgs,

        #[command(flatten)]
        sync: SyncArgs,
    },

    /// Deprecated: use `kei reset state`
    #[command(hide = true)]
    ResetState {
        /// Skip confirmation prompt
        #[arg(long, short = 'y')]
        yes: bool,
    },

    /// Deprecated: use `kei reset sync-token`
    #[command(hide = true)]
    ResetSyncToken,

    /// Deprecated: use `kei config setup`
    #[command(hide = true)]
    Setup {
        /// Output path (overrides --config)
        #[arg(short = 'o', long)]
        output: Option<String>,
    },
}

#[derive(Parser, Debug)]
#[command(name = "kei", about = "kei: photo sync engine", version)]
pub struct Cli {
    // ── Global options ──────────────────────────────────────────────
    /// Log level
    #[arg(long, value_enum, global = true, env = "KEI_LOG_LEVEL")]
    pub log_level: Option<LogLevel>,

    /// Path to TOML config file
    #[arg(
        long,
        global = true,
        default_value = "~/.config/kei/config.toml",
        env = "KEI_CONFIG"
    )]
    pub config: String,

    /// Apple ID email address
    #[arg(short = 'u', long, global = true, env = "ICLOUD_USERNAME", value_parser = non_empty_string)]
    pub username: Option<String>,

    /// iCloud domain (com or cn)
    #[arg(long, global = true, value_enum, env = "KEI_DOMAIN")]
    pub domain: Option<Domain>,

    /// Directory for sessions, state DB, and credentials
    #[arg(long, global = true, env = "KEI_DATA_DIR")]
    pub data_dir: Option<String>,

    /// Deprecated: use --data-dir (will be removed in v0.20.0)
    #[arg(long, global = true, hide = true)]
    pub cookie_directory: Option<String>,

    #[command(subcommand)]
    pub command: Option<Command>,

    // ── Backward compat: bare invocation = sync ────────────────────
    #[command(flatten)]
    pub password: PasswordArgs,

    #[command(flatten)]
    pub sync: SyncArgs,
}

impl SyncArgs {
    /// Merge top-level (fallback) sync args into self.
    /// Subcommand values take precedence; top-level fills in gaps.
    fn merge_from(&mut self, fallback: &Self) {
        if self.download_dir.is_none() {
            self.download_dir.clone_from(&fallback.download_dir);
        }
        if self.directory.is_none() {
            self.directory.clone_from(&fallback.directory);
        }
        self.auth_only = self.auth_only || fallback.auth_only;
        self.list_albums = self.list_albums || fallback.list_albums;
        self.list_libraries = self.list_libraries || fallback.list_libraries;
        if self.albums.is_empty() {
            self.albums.clone_from(&fallback.albums);
        }
        if self.library.is_none() {
            self.library.clone_from(&fallback.library);
        }
        if self.size.is_none() {
            self.size = fallback.size;
        }
        if self.live_photo_size.is_none() {
            self.live_photo_size = fallback.live_photo_size;
        }
        if self.recent.is_none() {
            self.recent = fallback.recent;
        }
        if self.threads.is_none() {
            self.threads = fallback.threads;
        }
        if self.threads_num.is_none() {
            self.threads_num = fallback.threads_num;
        }
        if self.bandwidth_limit.is_none() {
            self.bandwidth_limit = fallback.bandwidth_limit;
        }
        if self.skip_videos.is_none() {
            self.skip_videos = fallback.skip_videos;
        }
        if self.skip_photos.is_none() {
            self.skip_photos = fallback.skip_photos;
        }
        if self.skip_live_photos.is_none() {
            self.skip_live_photos = fallback.skip_live_photos;
        }
        if self.force_size.is_none() {
            self.force_size = fallback.force_size;
        }
        if self.folder_structure.is_none() {
            self.folder_structure.clone_from(&fallback.folder_structure);
        }
        #[cfg(feature = "xmp")]
        {
            if self.set_exif_datetime.is_none() {
                self.set_exif_datetime = fallback.set_exif_datetime;
            }
            if self.set_exif_rating.is_none() {
                self.set_exif_rating = fallback.set_exif_rating;
            }
            if self.set_exif_gps.is_none() {
                self.set_exif_gps = fallback.set_exif_gps;
            }
            if self.set_exif_description.is_none() {
                self.set_exif_description = fallback.set_exif_description;
            }
            if self.embed_xmp.is_none() {
                self.embed_xmp = fallback.embed_xmp;
            }
            if self.xmp_sidecar.is_none() {
                self.xmp_sidecar = fallback.xmp_sidecar;
            }
        }
        self.dry_run = self.dry_run || fallback.dry_run;
        if self.watch_with_interval.is_none() {
            self.watch_with_interval = fallback.watch_with_interval;
        }
        if self.no_progress_bar.is_none() {
            self.no_progress_bar = fallback.no_progress_bar;
        }
        if self.keep_unicode_in_filenames.is_none() {
            self.keep_unicode_in_filenames = fallback.keep_unicode_in_filenames;
        }
        if self.live_photo_mov_filename_policy.is_none() {
            self.live_photo_mov_filename_policy = fallback.live_photo_mov_filename_policy;
        }
        if self.align_raw.is_none() {
            self.align_raw = fallback.align_raw;
        }
        if self.file_match_policy.is_none() {
            self.file_match_policy = fallback.file_match_policy;
        }
        if self.skip_created_before.is_none() {
            self.skip_created_before
                .clone_from(&fallback.skip_created_before);
        }
        if self.skip_created_after.is_none() {
            self.skip_created_after
                .clone_from(&fallback.skip_created_after);
        }
        self.only_print_filenames = self.only_print_filenames || fallback.only_print_filenames;
        if self.max_retries.is_none() {
            self.max_retries = fallback.max_retries;
        }
        if self.retry_delay.is_none() {
            self.retry_delay = fallback.retry_delay;
        }
        if self.temp_suffix.is_none() {
            self.temp_suffix.clone_from(&fallback.temp_suffix);
        }
        self.no_incremental = self.no_incremental || fallback.no_incremental;
        self.reset_sync_token = self.reset_sync_token || fallback.reset_sync_token;
        if self.notify_systemd.is_none() {
            self.notify_systemd = fallback.notify_systemd;
        }
        if self.pid_file.is_none() {
            self.pid_file.clone_from(&fallback.pid_file);
        }
        if self.notification_script.is_none() {
            self.notification_script
                .clone_from(&fallback.notification_script);
        }
        if self.report_json.is_none() {
            self.report_json.clone_from(&fallback.report_json);
        }
        self.save_password = self.save_password || fallback.save_password;
        self.retry_failed = self.retry_failed || fallback.retry_failed;
    }
}

impl PasswordArgs {
    /// Merge top-level (fallback) password args into self.
    fn merge_from(&mut self, fallback: &Self) {
        if self.password.is_none() {
            self.password.clone_from(&fallback.password);
        }
        if self.password_file.is_none() {
            self.password_file.clone_from(&fallback.password_file);
        }
        if self.password_command.is_none() {
            self.password_command.clone_from(&fallback.password_command);
        }
    }
}

impl Cli {
    /// Get the effective command, treating bare invocation as sync.
    ///
    /// When a subcommand is present, top-level password/sync args are merged
    /// as fallbacks so `kei --password X sync` works the same as
    /// `kei sync --password X`.
    ///
    /// Legacy subcommands and compat flags are mapped to their new equivalents
    /// with deprecation warnings to stderr.
    pub fn effective_command(&self) -> Command {
        let cmd = if let Some(cmd) = &self.command {
            let mut cmd = cmd.clone();
            cmd.merge_top_level_args(&self.password, &self.sync);
            cmd
        } else {
            // Check hidden compat flags on bare invocation
            if self.sync.auth_only {
                deprecation_warning("--auth-only", "kei login");
                return Command::Login {
                    password: self.password.clone(),
                    subcommand: None,
                };
            }
            if self.sync.list_albums {
                deprecation_warning("--list-albums", "kei list albums");
                return Command::List {
                    password: self.password.clone(),
                    library: self.sync.library.clone(),
                    what: ListCommand::Albums,
                };
            }
            if self.sync.list_libraries {
                deprecation_warning("--list-libraries", "kei list libraries");
                return Command::List {
                    password: self.password.clone(),
                    library: self.sync.library.clone(),
                    what: ListCommand::Libraries,
                };
            }
            Command::Sync {
                password: self.password.clone(),
                sync: self.sync.clone(),
            }
        };

        // Map legacy subcommands to new equivalents
        match cmd {
            Command::GetCode { password } => {
                deprecation_warning("kei get-code", "kei login get-code");
                Command::Login {
                    password,
                    subcommand: Some(LoginCommand::GetCode),
                }
            }
            Command::SubmitCode { password, code } => {
                deprecation_warning("kei submit-code", "kei login submit-code");
                Command::Login {
                    password,
                    subcommand: Some(LoginCommand::SubmitCode { code }),
                }
            }
            Command::Credential { password, action } => {
                deprecation_warning("kei credential", "kei password");
                let new_action = match action {
                    CredentialAction::Set => PasswordAction::Set,
                    CredentialAction::Clear => PasswordAction::Clear,
                    CredentialAction::Backend => PasswordAction::Backend,
                };
                Command::Password {
                    password,
                    action: new_action,
                }
            }
            Command::RetryFailed { password, mut sync } => {
                deprecation_warning("kei retry-failed", "kei sync --retry-failed");
                sync.retry_failed = true;
                Command::Sync { password, sync }
            }
            Command::ResetState { yes } => {
                deprecation_warning("kei reset-state", "kei reset state");
                Command::Reset {
                    what: ResetCommand::State { yes },
                }
            }
            Command::ResetSyncToken => {
                deprecation_warning("kei reset-sync-token", "kei reset sync-token");
                Command::Reset {
                    what: ResetCommand::SyncToken,
                }
            }
            Command::Setup { output } => {
                deprecation_warning("kei setup", "kei config setup");
                Command::Config {
                    action: ConfigAction::Setup { output },
                }
            }
            // Handle compat flags on sync subcommand (e.g. `kei sync --auth-only`)
            Command::Sync {
                ref password,
                ref sync,
            } if sync.auth_only => {
                deprecation_warning("--auth-only", "kei login");
                Command::Login {
                    password: password.clone(),
                    subcommand: None,
                }
            }
            Command::Sync {
                ref password,
                ref sync,
            } if sync.list_albums => {
                deprecation_warning("--list-albums", "kei list albums");
                Command::List {
                    password: password.clone(),
                    library: sync.library.clone(),
                    what: ListCommand::Albums,
                }
            }
            Command::Sync {
                ref password,
                ref sync,
            } if sync.list_libraries => {
                deprecation_warning("--list-libraries", "kei list libraries");
                Command::List {
                    password: password.clone(),
                    library: sync.library.clone(),
                    what: ListCommand::Libraries,
                }
            }
            other => other,
        }
    }
}

impl Command {
    /// Merge top-level CLI password/sync args as fallbacks into the
    /// subcommand's own args.
    fn merge_top_level_args(&mut self, top_password: &PasswordArgs, top_sync: &SyncArgs) {
        // Merge sync args for commands that carry them
        match self {
            Self::Sync { sync, .. } | Self::RetryFailed { sync, .. } => {
                sync.merge_from(top_sync);
            }
            _ => {}
        }
        // Merge password args for commands that carry them
        if let Some(pw) = self.password_args_mut() {
            pw.merge_from(top_password);
        }
    }

    /// Inject the `ICLOUD_PASSWORD` value captured before `Cli::parse()`.
    ///
    /// The env var is removed from the process environment for security
    /// (prevents leaking via `/proc/*/environ`), but clap's `env` attribute
    /// never sees it. This method restores it into whichever `PasswordArgs`
    /// the active command carries.
    pub fn inject_env_password(&mut self, env_password: Option<String>) {
        let Some(pw) = env_password else { return };
        if let Some(args) = self.password_args_mut() {
            if args.password.is_none() {
                args.password = Some(pw);
            }
        }
    }

    /// Return a mutable reference to the command's `PasswordArgs`, if any.
    fn password_args_mut(&mut self) -> Option<&mut PasswordArgs> {
        match self {
            Self::Sync { password, .. }
            | Self::Login { password, .. }
            | Self::List { password, .. }
            | Self::Password { password, .. }
            | Self::GetCode { password, .. }
            | Self::SubmitCode { password, .. }
            | Self::Credential { password, .. }
            | Self::RetryFailed { password, .. } => Some(password),
            Self::ImportExisting(args) => Some(&mut args.password),
            Self::Reset { .. }
            | Self::Config { .. }
            | Self::Status(_)
            | Self::Verify(_)
            | Self::Reconcile(_)
            | Self::ResetState { .. }
            | Self::ResetSyncToken
            | Self::Setup { .. } => None,
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

    // ── RecentLimit parser ──────────────────────────────────────────

    #[test]
    fn parse_recent_limit_bare_count() {
        assert_eq!(parse_recent_limit("100").unwrap(), RecentLimit::Count(100));
        assert_eq!(parse_recent_limit("1").unwrap(), RecentLimit::Count(1));
    }

    #[test]
    fn parse_recent_limit_days_suffix() {
        assert_eq!(parse_recent_limit("30d").unwrap(), RecentLimit::Days(30));
        assert_eq!(parse_recent_limit("1d").unwrap(), RecentLimit::Days(1));
    }

    #[test]
    fn parse_recent_limit_rejects_zero() {
        assert!(parse_recent_limit("0").unwrap_err().contains("zero"));
        assert!(parse_recent_limit("0d").unwrap_err().contains("zero"));
    }

    #[test]
    fn parse_recent_limit_rejects_empty() {
        assert!(parse_recent_limit("").is_err());
    }

    #[test]
    fn parse_recent_limit_rejects_unknown_suffix() {
        // Only `d` is accepted today. Other units (w, m, y) would need
        // explicit design decisions around month/year boundaries.
        assert!(parse_recent_limit("3w").is_err());
        assert!(parse_recent_limit("1y").is_err());
        assert!(parse_recent_limit("2m").is_err());
        assert!(parse_recent_limit("30days").is_err());
    }

    #[test]
    fn parse_recent_limit_rejects_garbage() {
        assert!(parse_recent_limit("abc").is_err());
        assert!(parse_recent_limit("-5").is_err());
        assert!(parse_recent_limit("10.5").is_err());
        assert!(parse_recent_limit("10 d").is_err());
    }

    #[test]
    fn recent_limit_toml_parses_integer() {
        #[derive(serde::Deserialize)]
        struct Wrap {
            recent: RecentLimit,
        }
        let got: Wrap = toml::from_str("recent = 100").unwrap();
        assert_eq!(got.recent, RecentLimit::Count(100));
    }

    #[test]
    fn recent_limit_toml_parses_days_string() {
        #[derive(serde::Deserialize)]
        struct Wrap {
            recent: RecentLimit,
        }
        let got: Wrap = toml::from_str(r#"recent = "30d""#).unwrap();
        assert_eq!(got.recent, RecentLimit::Days(30));
    }

    #[test]
    fn recent_limit_toml_rejects_zero_integer() {
        #[derive(serde::Deserialize, Debug)]
        struct Wrap {
            #[allow(dead_code)]
            recent: RecentLimit,
        }
        let err = toml::from_str::<Wrap>("recent = 0")
            .unwrap_err()
            .to_string();
        assert!(err.contains("zero"), "got: {err}");
    }

    #[test]
    fn recent_limit_toml_rejects_garbage_string() {
        #[derive(serde::Deserialize, Debug)]
        struct Wrap {
            #[allow(dead_code)]
            recent: RecentLimit,
        }
        assert!(toml::from_str::<Wrap>(r#"recent = "abc""#).is_err());
    }

    #[test]
    fn recent_limit_display_roundtrip() {
        assert_eq!(RecentLimit::Count(100).to_string(), "100");
        assert_eq!(RecentLimit::Days(30).to_string(), "30d");
    }

    #[test]
    fn recent_limit_serializes_count_as_integer() {
        let json = serde_json::to_string(&RecentLimit::Count(5)).unwrap();
        assert_eq!(json, "5");
    }

    #[test]
    fn recent_limit_serializes_days_as_string() {
        let json = serde_json::to_string(&RecentLimit::Days(7)).unwrap();
        assert_eq!(json, "\"7d\"");
    }

    fn base_args() -> Vec<&'static str> {
        vec!["kei", "--username", "test@example.com"]
    }

    /// Scrub auth-related env vars for the duration of the returned guard so
    /// tests that exercise clap's flag parsing don't get contaminated when the
    /// developer has `ICLOUD_USERNAME` / `ICLOUD_PASSWORD` exported (via
    /// `.env` sourcing for live tests). A process-wide mutex serializes
    /// concurrent calls to this helper.
    ///
    /// Note that this only protects against other callers of `scrub_auth_env`.
    /// `setenv`/`getenv` on POSIX aren't thread-safe against each other, so an
    /// unrelated test reading an env var while the guard is mutating could
    /// theoretically race. The CLI unit tests only touch these two vars via
    /// clap's `env = "..."` attributes during `try_parse_from`, which happens
    /// synchronously on one thread per test — so in practice the guard is
    /// sufficient for this suite. If a future test reads env from another
    /// thread, it needs to coordinate through the same mutex.
    fn scrub_auth_env() -> AuthEnvGuard {
        use std::sync::Mutex;
        static LOCK: Mutex<()> = Mutex::new(());
        let guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prev_user = std::env::var("ICLOUD_USERNAME").ok();
        let prev_pw = std::env::var("ICLOUD_PASSWORD").ok();
        // SAFETY: the enclosing MutexGuard serializes every other caller of
        // scrub_auth_env, and the test suite does not read these env vars
        // from separate threads.
        unsafe {
            std::env::remove_var("ICLOUD_USERNAME");
            std::env::remove_var("ICLOUD_PASSWORD");
        }
        AuthEnvGuard {
            _lock: guard,
            prev_user,
            prev_pw,
        }
    }

    struct AuthEnvGuard {
        _lock: std::sync::MutexGuard<'static, ()>,
        prev_user: Option<String>,
        prev_pw: Option<String>,
    }

    impl Drop for AuthEnvGuard {
        fn drop(&mut self) {
            // SAFETY: still holding the static mutex, so restoration is
            // exclusive under the same "no cross-thread readers" condition
            // described on scrub_auth_env.
            unsafe {
                if let Some(v) = self.prev_user.take() {
                    std::env::set_var("ICLOUD_USERNAME", v);
                }
                if let Some(v) = self.prev_pw.take() {
                    std::env::set_var("ICLOUD_PASSWORD", v);
                }
            }
        }
    }

    // ── Global args ───────────────────────────────────────────────

    #[test]
    fn test_username_global() {
        let cli = parse(&["kei", "--username", "test@example.com"]);
        assert_eq!(cli.username.as_deref(), Some("test@example.com"));
    }

    #[test]
    fn test_domain_global() {
        let cli = parse(&["kei", "--domain", "cn"]);
        assert_eq!(cli.domain, Some(Domain::Cn));
    }

    #[test]
    fn test_data_dir_global() {
        let cli = parse(&["kei", "--data-dir", "/config"]);
        assert_eq!(cli.data_dir.as_deref(), Some("/config"));
    }

    #[test]
    fn test_cookie_directory_compat() {
        let cli = parse(&["kei", "--cookie-directory", "/cookies"]);
        assert_eq!(cli.cookie_directory.as_deref(), Some("/cookies"));
    }

    #[test]
    fn test_config_flag_default() {
        let cli = parse(&base_args());
        assert_eq!(cli.config, "~/.config/kei/config.toml");
    }

    #[test]
    fn test_config_flag_custom() {
        let mut args = base_args();
        args.extend(["--config", "/etc/kei.toml"]);
        let cli = parse(&args);
        assert_eq!(cli.config, "/etc/kei.toml");
    }

    // ── Bare invocation (no subcommand = sync) ────────────────────

    #[test]
    fn test_bare_invocation_without_username() {
        let _guard = scrub_auth_env();
        let cli = Cli::try_parse_from(["kei"]).unwrap();
        assert!(cli.username.is_none());
        assert!(cli.command.is_none());
    }

    #[test]
    fn test_backwards_compatibility_no_subcommand() {
        let cli = Cli::try_parse_from([
            "kei",
            "--username",
            "test@example.com",
            "--download-dir",
            "/photos",
        ])
        .unwrap();
        assert!(cli.command.is_none());
        match cli.effective_command() {
            Command::Sync { sync, .. } => {
                assert_eq!(sync.download_dir, Some("/photos".to_string()));
            }
            _ => panic!("Expected Sync command"),
        }
    }

    // ── New subcommands ───────────────────────────────────────────

    #[test]
    fn test_login_subcommand() {
        let cli = Cli::try_parse_from(["kei", "login"]).unwrap();
        assert!(matches!(
            cli.effective_command(),
            Command::Login {
                subcommand: None,
                ..
            }
        ));
    }

    #[test]
    fn test_login_get_code() {
        let cli = Cli::try_parse_from(["kei", "login", "get-code"]).unwrap();
        assert!(matches!(
            cli.effective_command(),
            Command::Login {
                subcommand: Some(LoginCommand::GetCode),
                ..
            }
        ));
    }

    #[test]
    fn test_login_submit_code() {
        let cli = Cli::try_parse_from(["kei", "login", "submit-code", "123456"]).unwrap();
        match cli.effective_command() {
            Command::Login {
                subcommand: Some(LoginCommand::SubmitCode { code }),
                ..
            } => assert_eq!(code, "123456"),
            _ => panic!("Expected Login SubmitCode"),
        }
    }

    #[test]
    fn test_list_albums() {
        let cli = Cli::try_parse_from(["kei", "list", "albums"]).unwrap();
        assert!(matches!(
            cli.effective_command(),
            Command::List {
                what: ListCommand::Albums,
                ..
            }
        ));
    }

    #[test]
    fn test_list_libraries() {
        let cli = Cli::try_parse_from(["kei", "list", "libraries"]).unwrap();
        assert!(matches!(
            cli.effective_command(),
            Command::List {
                what: ListCommand::Libraries,
                ..
            }
        ));
    }

    #[test]
    fn test_password_set() {
        let cli = Cli::try_parse_from(["kei", "password", "set"]).unwrap();
        assert!(matches!(
            cli.effective_command(),
            Command::Password {
                action: PasswordAction::Set,
                ..
            }
        ));
    }

    #[test]
    fn test_password_clear() {
        let cli = Cli::try_parse_from(["kei", "password", "clear"]).unwrap();
        assert!(matches!(
            cli.effective_command(),
            Command::Password {
                action: PasswordAction::Clear,
                ..
            }
        ));
    }

    #[test]
    fn test_password_backend() {
        let cli = Cli::try_parse_from(["kei", "password", "backend"]).unwrap();
        assert!(matches!(
            cli.effective_command(),
            Command::Password {
                action: PasswordAction::Backend,
                ..
            }
        ));
    }

    #[test]
    fn test_reset_state() {
        let cli = Cli::try_parse_from(["kei", "reset", "state", "--yes"]).unwrap();
        match cli.effective_command() {
            Command::Reset {
                what: ResetCommand::State { yes },
            } => assert!(yes),
            _ => panic!("Expected Reset State"),
        }
    }

    #[test]
    fn test_reset_sync_token() {
        let cli = Cli::try_parse_from(["kei", "reset", "sync-token"]).unwrap();
        assert!(matches!(
            cli.effective_command(),
            Command::Reset {
                what: ResetCommand::SyncToken,
            }
        ));
    }

    #[test]
    fn test_config_show() {
        let cli = Cli::try_parse_from(["kei", "config", "show"]).unwrap();
        assert!(matches!(
            cli.effective_command(),
            Command::Config {
                action: ConfigAction::Show,
            }
        ));
    }

    #[test]
    fn test_config_setup() {
        let cli = Cli::try_parse_from(["kei", "config", "setup"]).unwrap();
        assert!(matches!(
            cli.effective_command(),
            Command::Config {
                action: ConfigAction::Setup { output: None },
            }
        ));
    }

    #[test]
    fn test_config_setup_with_output() {
        let cli =
            Cli::try_parse_from(["kei", "config", "setup", "-o", "/tmp/config.toml"]).unwrap();
        match cli.effective_command() {
            Command::Config {
                action: ConfigAction::Setup { output },
            } => assert_eq!(output.as_deref(), Some("/tmp/config.toml")),
            _ => panic!("Expected Config Setup"),
        }
    }

    #[test]
    fn test_sync_retry_failed_flag() {
        let cli =
            Cli::try_parse_from(["kei", "sync", "--retry-failed", "--download-dir", "/photos"])
                .unwrap();
        match cli.effective_command() {
            Command::Sync { sync, .. } => assert!(sync.retry_failed),
            _ => panic!("Expected Sync"),
        }
    }

    // ── Legacy subcommand compat ──────────────────────────────────

    #[test]
    fn test_legacy_get_code_maps_to_login() {
        let cli = Cli::try_parse_from(["kei", "get-code"]).unwrap();
        assert!(matches!(
            cli.effective_command(),
            Command::Login {
                subcommand: Some(LoginCommand::GetCode),
                ..
            }
        ));
    }

    #[test]
    fn test_legacy_submit_code_maps_to_login() {
        let cli = Cli::try_parse_from(["kei", "submit-code", "123456"]).unwrap();
        match cli.effective_command() {
            Command::Login {
                subcommand: Some(LoginCommand::SubmitCode { code }),
                ..
            } => assert_eq!(code, "123456"),
            _ => panic!("Expected Login SubmitCode"),
        }
    }

    #[test]
    fn test_legacy_credential_maps_to_password() {
        let cli = Cli::try_parse_from(["kei", "credential", "set"]).unwrap();
        assert!(matches!(
            cli.effective_command(),
            Command::Password {
                action: PasswordAction::Set,
                ..
            }
        ));
    }

    #[test]
    fn test_legacy_retry_failed_maps_to_sync() {
        let cli =
            Cli::try_parse_from(["kei", "retry-failed", "--download-dir", "/photos"]).unwrap();
        match cli.effective_command() {
            Command::Sync { sync, .. } => {
                assert!(sync.retry_failed);
                assert_eq!(sync.download_dir, Some("/photos".to_string()));
            }
            _ => panic!("Expected Sync with retry_failed"),
        }
    }

    #[test]
    fn test_max_download_attempts_cli_parse() {
        let cli = Cli::try_parse_from([
            "kei",
            "sync",
            "--max-download-attempts",
            "5",
            "--download-dir",
            "/photos",
        ])
        .unwrap();
        match cli.effective_command() {
            Command::Sync { sync, .. } => {
                assert_eq!(sync.max_download_attempts, Some(5));
            }
            _ => panic!("Expected Sync"),
        }
    }

    #[test]
    fn test_max_download_attempts_defaults_to_none() {
        let cli = Cli::try_parse_from(["kei", "sync", "--download-dir", "/photos"]).unwrap();
        match cli.effective_command() {
            Command::Sync { sync, .. } => {
                assert_eq!(sync.max_download_attempts, None);
            }
            _ => panic!("Expected Sync"),
        }
    }

    #[test]
    fn test_legacy_reset_state_maps_to_reset() {
        let cli = Cli::try_parse_from(["kei", "reset-state", "--yes"]).unwrap();
        match cli.effective_command() {
            Command::Reset {
                what: ResetCommand::State { yes },
            } => assert!(yes),
            _ => panic!("Expected Reset State"),
        }
    }

    #[test]
    fn test_legacy_reset_sync_token_maps_to_reset() {
        let cli = Cli::try_parse_from(["kei", "reset-sync-token"]).unwrap();
        assert!(matches!(
            cli.effective_command(),
            Command::Reset {
                what: ResetCommand::SyncToken,
            }
        ));
    }

    #[test]
    fn test_legacy_setup_maps_to_config() {
        let cli = Cli::try_parse_from(["kei", "setup"]).unwrap();
        assert!(matches!(
            cli.effective_command(),
            Command::Config {
                action: ConfigAction::Setup { output: None },
            }
        ));
    }

    #[test]
    fn test_legacy_auth_only_maps_to_login() {
        let cli = Cli::try_parse_from(["kei", "--auth-only"]).unwrap();
        assert!(matches!(
            cli.effective_command(),
            Command::Login {
                subcommand: None,
                ..
            }
        ));
    }

    #[test]
    fn test_legacy_list_albums_maps_to_list() {
        let cli = Cli::try_parse_from(["kei", "--list-albums"]).unwrap();
        assert!(matches!(
            cli.effective_command(),
            Command::List {
                what: ListCommand::Albums,
                ..
            }
        ));
    }

    #[test]
    fn test_sync_auth_only_maps_to_login() {
        let cli = Cli::try_parse_from(["kei", "sync", "--auth-only"]).unwrap();
        assert!(matches!(
            cli.effective_command(),
            Command::Login {
                subcommand: None,
                ..
            }
        ));
    }

    #[test]
    fn test_sync_list_albums_maps_to_list() {
        let cli = Cli::try_parse_from(["kei", "sync", "--list-albums"]).unwrap();
        assert!(matches!(
            cli.effective_command(),
            Command::List {
                what: ListCommand::Albums,
                ..
            }
        ));
    }

    #[test]
    fn test_sync_list_libraries_maps_to_list() {
        let cli = Cli::try_parse_from(["kei", "sync", "--list-libraries"]).unwrap();
        assert!(matches!(
            cli.effective_command(),
            Command::List {
                what: ListCommand::Libraries,
                ..
            }
        ));
    }

    #[test]
    fn test_legacy_list_libraries_maps_to_list() {
        let cli = Cli::try_parse_from(["kei", "--list-libraries"]).unwrap();
        assert!(matches!(
            cli.effective_command(),
            Command::List {
                what: ListCommand::Libraries,
                ..
            }
        ));
    }

    // ── Password args ─────────────────────────────────────────────

    #[test]
    fn test_password_flag() {
        let mut args = base_args();
        args.extend(["--password", "secret123"]);
        let cli = parse(&args);
        assert_eq!(cli.password.password.as_deref(), Some("secret123"));
    }

    #[test]
    fn test_password_file_flag() {
        let _guard = scrub_auth_env();
        let mut args = base_args();
        args.extend(["--password-file", "/run/secrets/pw"]);
        let cli = parse(&args);
        assert_eq!(
            cli.password.password_file.as_deref(),
            Some("/run/secrets/pw")
        );
    }

    #[test]
    fn test_password_command_flag() {
        let _guard = scrub_auth_env();
        let mut args = base_args();
        args.extend(["--password-command", "op read 'op://vault/icloud/pw'"]);
        let cli = parse(&args);
        assert_eq!(
            cli.password.password_command.as_deref(),
            Some("op read 'op://vault/icloud/pw'")
        );
    }

    #[test]
    fn test_password_conflicts_with_password_file() {
        let mut args = base_args();
        args.extend(["--password", "pw", "--password-file", "/tmp/pw.txt"]);
        assert!(Cli::try_parse_from(&args).is_err());
    }

    #[test]
    fn test_password_conflicts_with_password_command() {
        let mut args = base_args();
        args.extend(["--password", "pw", "--password-command", "echo pw"]);
        assert!(Cli::try_parse_from(&args).is_err());
    }

    #[test]
    fn test_password_file_conflicts_with_password_command() {
        let mut args = base_args();
        args.extend([
            "--password-file",
            "/tmp/pw",
            "--password-command",
            "echo pw",
        ]);
        assert!(Cli::try_parse_from(&args).is_err());
    }

    #[test]
    fn test_save_password_flag() {
        let mut args = base_args();
        args.push("--save-password");
        let cli = parse(&args);
        assert!(cli.sync.save_password);
    }

    #[test]
    fn test_save_password_merges_into_subcommand() {
        let cli = parse(&["kei", "--username", "u@e.com", "--save-password", "sync"]);
        if let Command::Sync { sync, .. } = cli.effective_command() {
            assert!(sync.save_password);
        } else {
            panic!("expected Sync command");
        }
    }

    // ── Sync args ─────────────────────────────────────────────────

    #[test]
    fn test_library_accepts_custom_value() {
        let mut args = base_args();
        args.extend(["--library", "SharedSync-ABCD1234"]);
        let cli = parse(&args);
        assert_eq!(cli.sync.library.as_deref(), Some("SharedSync-ABCD1234"));
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
    fn test_threads_num_accepts_upper_bound() {
        let mut args = base_args();
        args.extend(["--threads-num", "64"]);
        let cli = parse(&args);
        assert_eq!(cli.sync.threads_num, Some(64));
    }

    #[test]
    fn test_threads_num_rejects_above_upper_bound() {
        let mut args = base_args();
        args.extend(["--threads-num", "65"]);
        assert!(Cli::try_parse_from(&args).is_err());

        let mut args = base_args();
        args.extend(["--threads-num", "5000"]);
        assert!(Cli::try_parse_from(&args).is_err());
    }

    #[test]
    fn test_bandwidth_limit_bare_bytes() {
        let mut args = base_args();
        args.extend(["--bandwidth-limit", "1500000"]);
        let cli = parse(&args);
        assert_eq!(cli.sync.bandwidth_limit, Some(1_500_000));
    }

    #[test]
    fn test_bandwidth_limit_decimal_suffixes() {
        let mut args = base_args();
        args.extend(["--bandwidth-limit", "10M"]);
        let cli = parse(&args);
        assert_eq!(cli.sync.bandwidth_limit, Some(10_000_000));
    }

    #[test]
    fn test_bandwidth_limit_binary_suffix() {
        let mut args = base_args();
        args.extend(["--bandwidth-limit", "2Mi"]);
        let cli = parse(&args);
        assert_eq!(cli.sync.bandwidth_limit, Some(2 * 1024 * 1024));
    }

    #[test]
    fn test_bandwidth_limit_case_insensitive() {
        assert_eq!(parse_bandwidth_limit("500k"), Ok(500_000));
        assert_eq!(parse_bandwidth_limit("500K"), Ok(500_000));
        assert_eq!(parse_bandwidth_limit("1gib"), Ok(1024 * 1024 * 1024));
    }

    #[test]
    fn test_bandwidth_limit_rejects_zero() {
        let mut args = base_args();
        args.extend(["--bandwidth-limit", "0"]);
        assert!(Cli::try_parse_from(&args).is_err());
        assert!(parse_bandwidth_limit("0K").is_err());
    }

    #[test]
    fn test_bandwidth_limit_rejects_invalid() {
        assert!(parse_bandwidth_limit("").is_err());
        assert!(parse_bandwidth_limit("abc").is_err());
        assert!(parse_bandwidth_limit("10X").is_err());
        assert!(parse_bandwidth_limit("-5M").is_err());
        // 1.5M is NOW accepted (see test_bandwidth_limit_decimal_*).
        // Malformed decimals like `1..5M` or `1.5.5M` stay rejected.
        assert!(parse_bandwidth_limit("1..5M").is_err());
        assert!(parse_bandwidth_limit("1.5.5M").is_err());
    }

    // ── Decimal bandwidth values ───────────────────────────────────

    #[test]
    fn test_bandwidth_limit_decimal_mb() {
        assert_eq!(parse_bandwidth_limit("1.5M"), Ok(1_500_000));
        assert_eq!(parse_bandwidth_limit("2.5G"), Ok(2_500_000_000));
        assert_eq!(parse_bandwidth_limit("0.5K"), Ok(500));
    }

    #[test]
    fn test_bandwidth_limit_decimal_binary() {
        // 2.5 * 1_048_576 = 2_621_440
        assert_eq!(parse_bandwidth_limit("2.5Mi"), Ok(2_621_440));
        // 1.5 * 1024 = 1536
        assert_eq!(parse_bandwidth_limit("1.5Ki"), Ok(1_536));
    }

    #[test]
    fn test_bandwidth_limit_decimal_leading_dot() {
        assert_eq!(parse_bandwidth_limit(".5M"), Ok(500_000));
        assert_eq!(parse_bandwidth_limit(".25K"), Ok(250));
    }

    #[test]
    fn test_bandwidth_limit_decimal_trailing_dot() {
        // Trailing dot means integer-valued decimal - `1.M` is 1.0 MB/s.
        assert_eq!(parse_bandwidth_limit("1.M"), Ok(1_000_000));
    }

    #[test]
    fn test_bandwidth_limit_decimal_rounds_to_zero_rejected() {
        // 0.0001 * 1000 = 0.1 bytes/sec, rounds to 0 - reject so users
        // don't think kei is throttling when it effectively isn't.
        let err = parse_bandwidth_limit("0.0001K").unwrap_err();
        assert!(err.contains("rounds to zero"), "{err}");
        // 0.4 bare bytes/sec rounds to 0 too.
        assert!(parse_bandwidth_limit("0.4").is_err());
    }

    #[test]
    fn test_bandwidth_limit_decimal_rounds_to_nearest_byte() {
        // 0.1 * 1000 = 100 in theory, 99.99999... in f64. Round to 100.
        assert_eq!(parse_bandwidth_limit("0.1K"), Ok(100));
    }

    #[test]
    fn test_bandwidth_limit_rejects_special_floats() {
        // f64::parse accepts "nan", "inf", "infinity" - but these make no
        // sense as a bandwidth value.
        assert!(parse_bandwidth_limit("nanK").is_err());
        assert!(parse_bandwidth_limit("infM").is_err());
        assert!(parse_bandwidth_limit("inf").is_err());
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
    fn test_retry_delay_custom() {
        let mut args = base_args();
        args.extend(["--retry-delay", "15"]);
        let cli = parse(&args);
        assert_eq!(cli.sync.retry_delay, Some(15));
    }

    #[test]
    fn test_temp_suffix_custom() {
        let mut args = base_args();
        args.extend(["--temp-suffix", ".downloading"]);
        let cli = parse(&args);
        assert_eq!(cli.sync.temp_suffix.as_deref(), Some(".downloading"));
    }

    #[test]
    fn test_skip_videos_flag() {
        let mut args = base_args();
        args.push("--skip-videos");
        let cli = parse(&args);
        assert_eq!(cli.sync.skip_videos, Some(true));
    }

    #[test]
    fn test_skip_videos_explicit_false() {
        let mut args = base_args();
        args.extend(["--skip-videos", "false"]);
        let cli = parse(&args);
        assert_eq!(cli.sync.skip_videos, Some(false));
    }

    #[test]
    fn test_skip_photos_flag() {
        let mut args = base_args();
        args.push("--skip-photos");
        let cli = parse(&args);
        assert_eq!(cli.sync.skip_photos, Some(true));
    }

    #[test]
    fn test_skip_live_photos_flag() {
        let mut args = base_args();
        args.push("--skip-live-photos");
        let cli = parse(&args);
        assert_eq!(cli.sync.skip_live_photos, Some(true));
    }

    #[test]
    fn test_force_size_flag() {
        let mut args = base_args();
        args.push("--force-size");
        let cli = parse(&args);
        assert_eq!(cli.sync.force_size, Some(true));
    }

    #[cfg(feature = "xmp")]
    #[test]
    fn test_set_exif_datetime_flag() {
        let mut args = base_args();
        args.push("--set-exif-datetime");
        let cli = parse(&args);
        assert_eq!(cli.sync.set_exif_datetime, Some(true));
    }

    #[cfg(feature = "xmp")]
    #[test]
    fn test_embed_xmp_flag() {
        let mut args = base_args();
        args.push("--embed-xmp");
        let cli = parse(&args);
        assert_eq!(cli.sync.embed_xmp, Some(true));
    }

    #[cfg(feature = "xmp")]
    #[test]
    fn test_embed_xmp_flag_explicit_false() {
        let mut args = base_args();
        args.push("--embed-xmp=false");
        let cli = parse(&args);
        assert_eq!(cli.sync.embed_xmp, Some(false));
    }

    #[cfg(feature = "xmp")]
    #[test]
    fn test_xmp_sidecar_flag() {
        let mut args = base_args();
        args.push("--xmp-sidecar");
        let cli = parse(&args);
        assert_eq!(cli.sync.xmp_sidecar, Some(true));
    }

    #[test]
    fn test_no_progress_bar_flag() {
        let mut args = base_args();
        args.push("--no-progress-bar");
        let cli = parse(&args);
        assert_eq!(cli.sync.no_progress_bar, Some(true));
    }

    #[test]
    fn test_keep_unicode_in_filenames_flag() {
        let mut args = base_args();
        args.push("--keep-unicode-in-filenames");
        let cli = parse(&args);
        assert_eq!(cli.sync.keep_unicode_in_filenames, Some(true));
    }

    #[test]
    fn test_notify_systemd_flag() {
        let mut args = base_args();
        args.push("--notify-systemd");
        let cli = parse(&args);
        assert_eq!(cli.sync.notify_systemd, Some(true));
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
    fn test_notification_script_flag() {
        let mut args = base_args();
        args.extend(["--notification-script", "/path/to/notify.sh"]);
        let cli = parse(&args);
        assert_eq!(
            cli.sync.notification_script.as_deref(),
            Some("/path/to/notify.sh")
        );
    }

    #[test]
    fn test_report_json_flag() {
        let mut args = base_args();
        args.extend(["--report-json", "/tmp/report.json"]);
        let cli = parse(&args);
        assert_eq!(
            cli.sync.report_json.as_deref(),
            Some(std::path::Path::new("/tmp/report.json"))
        );
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
    fn test_align_raw_all_variants() {
        for (input, expected) in [
            ("as-is", RawTreatmentPolicy::Unchanged),
            ("original", RawTreatmentPolicy::PreferOriginal),
            ("alternative", RawTreatmentPolicy::PreferAlternative),
        ] {
            let mut args = base_args();
            args.extend(["--align-raw", input]);
            let cli = parse(&args);
            assert_eq!(
                cli.sync.align_raw,
                Some(expected),
                "align_raw variant: {input}"
            );
        }
    }

    #[test]
    fn test_align_raw_rejects_invalid() {
        let mut args = base_args();
        args.extend(["--align-raw", "bogus"]);
        assert!(Cli::try_parse_from(&args).is_err());
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
    fn test_download_dir_custom() {
        let mut args = base_args();
        args.extend(["--download-dir", "/photos"]);
        let cli = parse(&args);
        assert_eq!(cli.sync.download_dir.as_deref(), Some("/photos"));
    }

    #[test]
    fn test_legacy_directory_still_parses() {
        // `--directory` is deprecated but must still parse into the legacy
        // field so Config::build can emit the deprecation warning. Removal
        // happens in v0.20.0, not now.
        let mut args = base_args();
        args.extend(["--directory", "/photos"]);
        let cli = parse(&args);
        assert_eq!(cli.sync.directory.as_deref(), Some("/photos"));
        assert!(cli.sync.download_dir.is_none());
    }

    #[test]
    fn test_watch_with_interval() {
        let mut args = base_args();
        args.extend(["--watch-with-interval", "3600"]);
        let cli = parse(&args);
        assert_eq!(cli.sync.watch_with_interval, Some(3600));
    }

    #[test]
    fn test_watch_with_interval_rejects_below_minimum() {
        let mut args = base_args();
        args.extend(["--watch-with-interval", "0"]);
        assert!(Cli::try_parse_from(&args).is_err());

        let mut args = base_args();
        args.extend(["--watch-with-interval", "59"]);
        assert!(Cli::try_parse_from(&args).is_err());

        let mut args = base_args();
        args.extend(["--watch-with-interval", "60"]);
        assert!(Cli::try_parse_from(&args).is_ok());
    }

    #[test]
    fn test_watch_with_interval_rejects_above_maximum() {
        // 86400s = 24h ceiling
        let mut args = base_args();
        args.extend(["--watch-with-interval", "86400"]);
        assert!(Cli::try_parse_from(&args).is_ok());

        let mut args = base_args();
        args.extend(["--watch-with-interval", "86401"]);
        assert!(Cli::try_parse_from(&args).is_err());
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

    #[test]
    fn test_album_all_accepted() {
        let mut args = base_args();
        args.extend(["-a", "all"]);
        let cli = parse(&args);
        // CLI layer stays dumb: Config::build interprets "all".
        assert_eq!(cli.sync.albums, vec!["all"]);
    }

    // ── Input validation ───────────────────────────────────────────

    #[test]
    fn test_empty_username_rejected() {
        let args = ["kei", "--username", ""];
        assert!(Cli::try_parse_from(args).is_err());
    }

    #[test]
    fn test_empty_password_rejected() {
        let mut args = base_args();
        args.extend(["--password", ""]);
        assert!(Cli::try_parse_from(&args).is_err());
    }

    #[test]
    fn test_empty_download_dir_rejected() {
        let args = [
            "kei",
            "--username",
            "user@example.com",
            "--download-dir",
            "",
        ];
        assert!(Cli::try_parse_from(args).is_err());
    }

    #[test]
    fn test_empty_legacy_directory_rejected() {
        // `non_empty_string` validator applies to the legacy field too, so an
        // empty `--directory` fails at parse time rather than slipping through
        // to emit a deprecation warning on an invalid value.
        let args = ["kei", "--username", "user@example.com", "--directory", ""];
        assert!(Cli::try_parse_from(args).is_err());
    }

    #[test]
    fn test_empty_album_rejected() {
        let mut args = base_args();
        args.extend(["--album", ""]);
        assert!(Cli::try_parse_from(&args).is_err());
    }

    #[test]
    fn test_submit_code_rejects_non_digits() {
        assert!(Cli::try_parse_from(["kei", "submit-code", "abcdef"]).is_err());
    }

    #[test]
    fn test_submit_code_rejects_too_short() {
        assert!(Cli::try_parse_from(["kei", "submit-code", "12345"]).is_err());
    }

    #[test]
    fn test_submit_code_rejects_too_long() {
        assert!(Cli::try_parse_from(["kei", "submit-code", "1234567"]).is_err());
    }

    #[test]
    fn test_submit_code_rejects_empty() {
        assert!(Cli::try_parse_from(["kei", "submit-code", ""]).is_err());
    }

    #[test]
    fn test_retry_delay_rejects_zero() {
        let mut args = base_args();
        args.extend(["--retry-delay", "0"]);
        assert!(Cli::try_parse_from(&args).is_err());
    }

    #[test]
    fn test_retry_delay_accepts_one() {
        let mut args = base_args();
        args.extend(["--retry-delay", "1"]);
        let cli = parse(&args);
        assert_eq!(cli.sync.retry_delay, Some(1));
    }

    #[test]
    fn test_retry_delay_rejects_above_3600() {
        let mut args = base_args();
        args.extend(["--retry-delay", "3601"]);
        assert!(Cli::try_parse_from(&args).is_err());
    }

    #[test]
    fn test_retry_delay_accepts_3600() {
        let mut args = base_args();
        args.extend(["--retry-delay", "3600"]);
        let cli = parse(&args);
        assert_eq!(cli.sync.retry_delay, Some(3600));
    }

    #[test]
    fn test_max_retries_rejects_above_100() {
        let mut args = base_args();
        args.extend(["--max-retries", "101"]);
        assert!(Cli::try_parse_from(&args).is_err());
    }

    #[test]
    fn test_max_retries_accepts_100() {
        let mut args = base_args();
        args.extend(["--max-retries", "100"]);
        let cli = parse(&args);
        assert_eq!(cli.sync.max_retries, Some(100));
    }

    // ── no-incremental / reset-sync-token compat flags ───────────

    #[test]
    fn test_no_incremental_flag() {
        let mut args = base_args();
        args.push("--no-incremental");
        let cli = parse(&args);
        assert!(cli.sync.no_incremental);
    }

    #[test]
    fn test_reset_sync_token_compat_flag() {
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

    // ── Sync subcommand ───────────────────────────────────────────

    #[test]
    fn test_sync_subcommand() {
        let cli = Cli::try_parse_from([
            "kei",
            "sync",
            "--username",
            "test@example.com",
            "--download-dir",
            "/photos",
        ])
        .unwrap();
        assert!(matches!(cli.command, Some(Command::Sync { .. })));
    }

    #[test]
    fn test_status_subcommand() {
        let cli = Cli::try_parse_from(["kei", "status"]).unwrap();
        assert!(matches!(cli.command, Some(Command::Status(_))));
    }

    #[test]
    fn test_status_with_failed_flag() {
        let cli = Cli::try_parse_from(["kei", "status", "--failed"]).unwrap();
        if let Some(Command::Status(args)) = cli.command {
            assert!(args.failed);
            assert!(!args.pending);
            assert!(!args.downloaded);
        } else {
            panic!("Expected Status command");
        }
    }

    #[test]
    fn test_status_with_pending_flag() {
        let cli = Cli::try_parse_from(["kei", "status", "--pending"]).unwrap();
        if let Some(Command::Status(args)) = cli.command {
            assert!(!args.failed);
            assert!(args.pending);
            assert!(!args.downloaded);
        } else {
            panic!("Expected Status command");
        }
    }

    #[test]
    fn test_status_with_downloaded_flag() {
        let cli = Cli::try_parse_from(["kei", "status", "--downloaded"]).unwrap();
        if let Some(Command::Status(args)) = cli.command {
            assert!(!args.failed);
            assert!(!args.pending);
            assert!(args.downloaded);
        } else {
            panic!("Expected Status command");
        }
    }

    #[test]
    fn test_status_flags_combine() {
        let cli = Cli::try_parse_from(["kei", "status", "--failed", "--pending", "--downloaded"])
            .unwrap();
        if let Some(Command::Status(args)) = cli.command {
            assert!(args.failed);
            assert!(args.pending);
            assert!(args.downloaded);
        } else {
            panic!("Expected Status command");
        }
    }

    // ── Global flags work with subcommands ────────────────────────

    #[test]
    fn test_config_global_before_subcommand() {
        let cli =
            Cli::try_parse_from(["kei", "--config", "/custom/config.toml", "status"]).unwrap();
        assert_eq!(cli.config, "/custom/config.toml");
        assert!(matches!(cli.command, Some(Command::Status(_))));
    }

    #[test]
    fn test_username_global_before_subcommand() {
        let cli = Cli::try_parse_from(["kei", "--username", "test@example.com", "sync"]).unwrap();
        assert_eq!(cli.username.as_deref(), Some("test@example.com"));
    }

    // ── import-existing ───────────────────────────────────────────

    #[test]
    fn test_import_existing_subcommand() {
        let cli =
            Cli::try_parse_from(["kei", "import-existing", "--download-dir", "/photos"]).unwrap();
        if let Some(Command::ImportExisting(args)) = cli.command {
            assert_eq!(args.download_dir.as_deref(), Some("/photos"));
            assert!(args.directory.is_none());
            assert!(args.folder_structure.is_none());
            assert!(args.recent.is_none());
        } else {
            panic!("Expected ImportExisting command");
        }
    }

    #[test]
    fn test_import_existing_legacy_directory_still_parses() {
        let cli =
            Cli::try_parse_from(["kei", "import-existing", "--directory", "/photos"]).unwrap();
        if let Some(Command::ImportExisting(args)) = cli.command {
            assert_eq!(args.directory.as_deref(), Some("/photos"));
            assert!(args.download_dir.is_none());
        } else {
            panic!("Expected ImportExisting command");
        }
    }

    #[test]
    fn test_import_existing_library_flag() {
        let cli = Cli::try_parse_from([
            "kei",
            "import-existing",
            "--library",
            "SharedSync-ABCD1234",
            "--download-dir",
            "/photos",
        ])
        .unwrap();
        if let Some(Command::ImportExisting(args)) = cli.command {
            assert_eq!(args.library.as_deref(), Some("SharedSync-ABCD1234"));
        } else {
            panic!("Expected ImportExisting command");
        }
    }

    #[test]
    fn test_list_albums_library_flag() {
        let cli = Cli::try_parse_from(["kei", "list", "--library", "all", "albums"]).unwrap();
        assert!(matches!(
            cli.effective_command(),
            Command::List {
                library: Some(ref l),
                what: ListCommand::Albums,
                ..
            } if l == "all"
        ));
    }

    #[test]
    fn test_verify_subcommand() {
        let cli = Cli::try_parse_from(["kei", "verify", "--checksums"]).unwrap();
        if let Some(Command::Verify(args)) = cli.command {
            assert!(args.checksums);
        } else {
            panic!("Expected Verify command");
        }
    }

    #[test]
    fn test_reconcile_subcommand() {
        let cli = Cli::try_parse_from(["kei", "reconcile"]).unwrap();
        if let Some(Command::Reconcile(args)) = cli.command {
            assert!(!args.dry_run);
        } else {
            panic!("Expected Reconcile command");
        }
    }

    #[test]
    fn test_reconcile_dry_run_flag() {
        let cli = Cli::try_parse_from(["kei", "reconcile", "--dry-run"]).unwrap();
        if let Some(Command::Reconcile(args)) = cli.command {
            assert!(args.dry_run);
        } else {
            panic!("Expected Reconcile command");
        }
    }

    // ── New filter flags ───────────────────────────────────────────

    #[test]
    fn test_live_photo_mode_all_variants() {
        for (flag, expected) in [
            ("both", LivePhotoMode::Both),
            ("image-only", LivePhotoMode::ImageOnly),
            ("video-only", LivePhotoMode::VideoOnly),
            ("skip", LivePhotoMode::Skip),
        ] {
            let mut args = base_args();
            args.extend(["--live-photo-mode", flag]);
            let cli = parse(&args);
            assert_eq!(cli.sync.live_photo_mode, Some(expected));
        }
    }

    #[test]
    fn test_skip_live_photos_compat_still_parses() {
        let mut args = base_args();
        args.push("--skip-live-photos");
        let cli = parse(&args);
        assert_eq!(cli.sync.skip_live_photos, Some(true));
    }

    #[test]
    fn test_filename_exclude_single() {
        let mut args = base_args();
        args.extend(["--filename-exclude", "*.AAE"]);
        let cli = parse(&args);
        assert_eq!(cli.sync.filename_exclude, vec!["*.AAE"]);
    }

    #[test]
    fn test_filename_exclude_multiple() {
        let mut args = base_args();
        args.extend([
            "--filename-exclude",
            "*.AAE",
            "--filename-exclude",
            "Screenshot*",
        ]);
        let cli = parse(&args);
        assert_eq!(cli.sync.filename_exclude, vec!["*.AAE", "Screenshot*"]);
    }

    #[test]
    fn test_exclude_album_single() {
        let mut args = base_args();
        args.extend(["--exclude-album", "Hidden"]);
        let cli = parse(&args);
        assert_eq!(cli.sync.exclude_albums, vec!["Hidden"]);
    }

    #[test]
    fn test_exclude_album_multiple() {
        let mut args = base_args();
        args.extend(["--exclude-album", "Hidden", "--exclude-album", "Trash"]);
        let cli = parse(&args);
        assert_eq!(cli.sync.exclude_albums, vec!["Hidden", "Trash"]);
    }
}
