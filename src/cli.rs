use crate::types::LogLevel;
use clap::{Args, FromArgMatches, Parser, Subcommand};
use std::ffi::{OsStr, OsString};

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
        .map_err(|_e| format!("invalid bandwidth number `{num_str}`"))?;
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

/// Where a count-form `--recent N` limit is applied when filters create
/// multiple CloudKit streams.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Default,
    clap::ValueEnum,
    serde::Deserialize,
    serde::Serialize,
)]
#[serde(rename_all = "kebab-case")]
pub enum RecentScope {
    /// Apply `recent` once to the selected library, then narrow with filters.
    #[default]
    Global,
    /// Apply `recent` independently to each album, smart-folder, or unfiled pass.
    PerFilter,
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
        .map_err(|_e| format!("expected a positive integer or `Nd` form (got `{s}`)"))?;
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

/// Arguments for the sync command.
#[derive(Parser, Debug, Clone, Default)]
pub struct SyncArgs {
    /// Limit to recent assets (e.g. `--recent 100`) or use a days window
    /// (e.g. `--recent 30d`).
    /// This is not a global exact file count - filters and live-photo parts
    /// can change the final number of downloaded files. Days form maps to
    /// `--skip-created-before` internally.
    #[arg(long, value_parser = parse_recent_limit)]
    pub recent: Option<RecentLimit>,

    /// Where count-form `--recent N` applies when filters create multiple streams.
    ///
    /// `global` means "take the library-wide recent N, then apply filters".
    /// `per-filter` means "take up to N from each album/smart-folder/unfiled pass".
    #[arg(long, value_enum)]
    pub recent_scope: Option<RecentScope>,

    /// Do not modify local system or iCloud
    #[arg(long)]
    pub dry_run: bool,

    /// Disable progress bar
    #[arg(long)]
    pub no_progress_bar: bool,

    /// Save sensitive, unredacted iCloud Photos response bodies under the data directory's
    /// .diagnostics folder (Unix only). Includes errors and retries, not authentication or media bodies.
    /// Explicitly writes diagnostics even with --dry-run or --only-print-filenames.
    #[arg(long)]
    pub capture_icloud_responses: bool,

    /// Skip assets before this cutoff. Date-only values compare the capture-local calendar date;
    /// explicit datetimes and relative intervals compare the instant (e.g., 2025-01-02 or 20d).
    #[arg(long)]
    pub skip_created_before: Option<String>,

    /// Skip assets after this cutoff. Date-only values compare the capture-local calendar date;
    /// explicit datetimes and relative intervals compare the instant (e.g., 2025-01-02 or 20d).
    #[arg(long)]
    pub skip_created_after: Option<String>,

    /// Only print filenames without downloading
    #[arg(long)]
    pub only_print_filenames: bool,

    /// After successful auth, persist the password to the credential store
    /// (OS keyring or encrypted file).
    #[arg(long)]
    pub save_password: bool,

    /// Re-sync only previously failed assets
    #[arg(long, conflicts_with = "dry_run")]
    pub retry_failed: bool,

    /// One-shot full-library repair that refreshes metadata for every downloaded version without re-downloading media.
    /// Requires a complete selected-library sweep and rewrites EXIF/XMP per the configured metadata outputs.
    #[arg(
        long,
        conflicts_with_all = ["dry_run", "retry_failed", "only_print_filenames", "recent"]
    )]
    pub refresh_metadata: bool,

    /// Replace existing embedded capture timestamps with Apple's capture-local timestamp and
    /// matching offset. This can overwrite camera metadata and requires --refresh-metadata.
    #[arg(long, requires = "refresh_metadata")]
    pub repair_capture_timestamps: bool,

    /// Replace a recorded local file only when reconcile marked it truncated.
    /// The replacement is verified before it atomically takes the recorded path.
    #[arg(
        long,
        conflicts_with_all = ["dry_run", "only_print_filenames", "refresh_metadata"]
    )]
    pub repair_truncated: bool,

    /// Test-only durable config overrides used by unit tests that exercise
    /// resolver internals without rebuilding TOML snippets.
    #[cfg(test)]
    #[arg(skip)]
    pub(crate) config_overrides: crate::config::SyncConfigOverrides,
}

/// Runtime-friendly output toggles.
///
/// These are deliberately not global options. They only affect `kei sync`;
/// non-sync subcommands either do no long running terminal rendering or force
/// friendly mode off.
#[derive(Parser, Debug, Clone, Default)]
pub struct FriendlyArgs {
    /// Use friendly terminal progress and summaries (default on interactive terminals)
    #[arg(
        long,
        overrides_with = "no_friendly",
        long_help = "Use friendly terminal progress and summaries. \
                     Default: on for plain TTYs, off in service/container/journal contexts and whenever a \
                     machine-output mode (`--only-print-filenames` or TOML report JSON) or an explicit \
                     `--log-level` / `RUST_LOG` is in play. `--friendly` overrides the TOML `[ui] friendly` \
                     setting and the auto-detected default; environmental hard-off contexts still win."
    )]
    pub friendly: bool,

    /// Disable friendly terminal progress and summaries
    #[arg(
        long,
        overrides_with = "friendly",
        long_help = "Force friendly terminal progress and summaries off and use the plain tracing output. \
                     Overrides `--friendly`, the TOML `[ui] friendly` setting, and the auto-detected default. \
                     Use this when piping kei output to a log aggregator on an interactive TTY where \
                     auto-detection would otherwise enable friendly mode."
    )]
    pub no_friendly: bool,
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

/// Arguments for local diagnostics.
#[derive(Parser, Debug, Clone)]
pub struct DoctorArgs {
    /// Emit a redacted JSON diagnostics report
    #[arg(long)]
    pub json: bool,

    /// Include live iCloud checks. The default doctor run is local-only.
    #[arg(long)]
    pub live: bool,
}

/// Manifest export format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum ManifestFormat {
    /// Pretty-printed JSON array
    Json,
    /// CSV with one row per asset version
    Csv,
}

/// Arguments for the manifest command.
#[derive(Parser, Debug, Clone)]
pub struct ManifestArgs {
    /// Output format
    #[arg(long, value_enum, default_value_t = ManifestFormat::Json)]
    pub format: ManifestFormat,
}

/// Arguments for the import-existing command.
#[derive(Parser, Debug, Clone)]
pub struct ImportArgs {
    #[command(flatten)]
    pub password: PasswordArgs,

    /// Library/libraries to import. Repeatable; default `primary`. Same value
    /// grammar as `[filters].libraries`: a CloudKit zone name (full UUID or
    /// the truncated 8-char `SharedSync-` prefix that `{library}` renders
    /// into paths), the sentinels `primary` / `shared` / `all` / `none`, or
    /// `!name` to exclude.
    #[arg(long = "library", value_parser = non_empty_string)]
    pub libraries: Vec<String>,

    /// Number of recent photos to check (`--recent 100`). The `--recent Nd`
    /// days form is only supported in `sync`; import-existing errors on use.
    #[arg(long, value_parser = parse_recent_limit)]
    pub recent: Option<RecentLimit>,

    /// Scan and report matches without writing to the state DB. Useful for
    /// verifying that TOML path and filename settings match the tree you're
    /// importing before committing.
    #[arg(long)]
    pub dry_run: bool,

    /// Disable progress bar
    #[arg(long)]
    pub no_progress_bar: bool,

    /// Override the empty-library safety guard. Without this flag,
    /// `import-existing` aborts when a selected library returns zero
    /// assets while the state DB has prior asset rows -- often a
    /// transient iCloud permissions glitch or stale auth, where
    /// scanning would silently produce a misleading `matched: 0`
    /// report. Set this if you genuinely emptied the library, or if
    /// you're attaching a new sub-library to an account that already
    /// has data (the prior-row check is global, not per-zone).
    #[arg(long, env = "KEI_FORCE_EMPTY")]
    pub force_empty: bool,

    /// Verify a cloud prefix before adopting a same-name, same-size local file.
    #[arg(long)]
    pub strict: bool,
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
#[derive(Subcommand, Debug, Clone, Copy, PartialEq, Eq)]
pub enum PasswordAction {
    /// Store a password from a prompt, password file, or password command
    #[command(
        after_help = "Headless examples:\n  kei password --password-file <PATH> set\n  kei password --password-command <COMMAND> set"
    )]
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
    /// Clear stored sync tokens so the next sync does a full enumeration.
    ///
    /// Without `--yes`, prompts for confirmation on a TTY (the next sync will
    /// re-enumerate every asset, which can be expensive on large libraries)
    /// and errors out under non-interactive use, matching `reset state`.
    SyncToken {
        /// Skip confirmation prompt
        #[arg(long, short = 'y')]
        yes: bool,
    },
    /// Discard the local iCloud session, including trust tokens.
    ///
    /// Removes the cookie jar, persisted session, and response cache for the
    /// configured account, so the next `kei login` runs the full password +
    /// 2FA flow again. The stored password (`kei password`) and the state
    /// database are kept. Useful when Apple rejects 2FA pushes for a stale
    /// session. Without `--yes`, prompts for confirmation on a TTY and
    /// errors out under non-interactive use, matching `reset sync-token`.
    Session {
        /// Skip confirmation prompt
        #[arg(long, short = 'y')]
        yes: bool,
    },
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

/// Arguments for `kei install`.
///
/// Per-platform defaults: Linux installs a per-user systemd unit unless
/// `--system` is passed; macOS installs a per-user launchd agent; Windows
/// registers the service with the Service Control Manager under the
/// current user's account.
#[derive(Args, Debug, Clone)]
pub struct InstallArgs {
    /// Install per-user (Linux/macOS default; ignored on Windows).
    #[arg(long, conflicts_with = "system")]
    pub user: bool,

    /// Install system-wide (Linux only; requires root). On macOS and
    /// Windows this is rejected with a pointer to the per-user install.
    #[arg(long, conflicts_with = "user")]
    pub system: bool,

    /// Render the service artifact and report what would happen, without
    /// writing files or invoking the platform service manager (no
    /// `systemctl daemon-reload`, no `launchctl bootstrap`, no SCM
    /// `CreateService`).
    #[arg(long)]
    pub dry_run: bool,
}

/// Arguments for `kei uninstall`.
#[derive(Args, Debug, Clone)]
pub struct UninstallArgs {
    /// Also remove the state database, configuration, and stored
    /// credentials. Default off: data is sacred, removal is opt-in.
    #[arg(long)]
    pub purge: bool,
}

/// Arguments for `kei service run`.
///
/// Shares the reduced runtime [`SyncArgs`] with `kei sync`, but omits
/// friendly-mode toggles because service mode forces friendly output off.
/// Carried as its own struct so the surrounding [`ServiceAction`] enum
/// does not balloon by ~600 bytes (the size of `SyncArgs`) on the
/// unrelated `Status` variant.
#[derive(Args, Debug, Clone)]
pub struct ServiceRunArgs {
    #[command(flatten)]
    pub password: PasswordArgs,

    #[command(flatten)]
    pub sync: SyncArgs,
}

/// Subcommands under `kei service`.
#[derive(Subcommand, Debug, Clone)]
pub enum ServiceAction {
    /// Run the service worker (invoked by launchd / systemd / Windows SCM,
    /// or directly for testing). Equivalent to `kei sync` with service-mode
    /// defaults: when no other source provides a watch interval, defaults
    /// to once per day (24 hours / 86,400 seconds).
    Run(Box<ServiceRunArgs>),

    /// Show whether kei is registered as a service on this host and when
    /// it last started. For a combined summary including photo library
    /// stats, use `kei status` instead.
    Status,
}

/// Subcommands for kei.
#[allow(
    clippy::large_enum_variant,
    reason = "parsed once at startup; boxing flattened clap args would add command plumbing without runtime benefit"
)]
#[derive(Subcommand, Debug, Clone)]
pub enum Command {
    /// Download photos from iCloud
    #[command(after_help = "Documentation: https://github.com/rhoopr/kei/wiki/Sync")]
    Sync {
        #[command(flatten)]
        friendly: FriendlyArgs,

        #[command(flatten)]
        password: PasswordArgs,

        #[command(flatten)]
        sync: SyncArgs,
    },

    /// Authenticate interactively (creates/refreshes session tokens)
    #[command(after_help = "Documentation: https://github.com/rhoopr/kei/wiki/Login")]
    Login {
        #[command(flatten)]
        password: PasswordArgs,

        #[command(subcommand)]
        subcommand: Option<LoginCommand>,
    },

    /// List available albums or libraries
    #[command(after_help = "Documentation: https://github.com/rhoopr/kei/wiki/List")]
    List {
        #[command(flatten)]
        password: PasswordArgs,

        /// Library/libraries to list albums from. Repeatable; default
        /// `primary` (the PrimarySync zone). Same value grammar as
        /// `[filters].libraries`: a CloudKit zone name (full UUID or the
        /// truncated 8-char `SharedSync-` prefix that `{library}` renders
        /// into paths), the sentinels `primary` / `shared` / `all` /
        /// `none`, or `!name` to exclude. Only consulted by
        /// `kei list albums`; `kei list libraries` always lists every
        /// library on the account.
        #[arg(long = "library", env = "KEI_LIBRARY", value_parser = non_empty_string)]
        libraries: Vec<String>,

        #[command(subcommand)]
        what: ListCommand,
    },

    /// Manage stored credentials (OS keyring or encrypted file)
    #[command(after_help = "Documentation: https://github.com/rhoopr/kei/wiki/Password")]
    Password {
        #[command(flatten)]
        password: PasswordArgs,

        #[command(subcommand)]
        action: PasswordAction,
    },

    /// Reset state database, sync tokens, or the local session
    #[command(after_help = "Documentation: https://github.com/rhoopr/kei/wiki/Reset")]
    Reset {
        #[command(subcommand)]
        what: ResetCommand,
    },

    /// Config management
    #[command(after_help = "Documentation: https://github.com/rhoopr/kei/wiki/Config")]
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },

    /// Show sync status and database summary
    #[command(after_help = "Documentation: https://github.com/rhoopr/kei/wiki/Status")]
    Status(StatusArgs),

    /// Run redacted local diagnostics for support
    #[command(after_help = "Documentation: https://github.com/rhoopr/kei/wiki/Doctor")]
    Doctor(DoctorArgs),

    /// Export the local state catalog without contacting iCloud
    #[command(after_help = "Documentation: https://github.com/rhoopr/kei/wiki/Manifest")]
    Manifest(ManifestArgs),

    /// Import existing local files into the state database
    #[command(after_help = "Documentation: https://github.com/rhoopr/kei/wiki/Import-Existing")]
    ImportExisting(ImportArgs),

    /// Verify downloaded files exist and optionally check checksums
    #[command(after_help = "Documentation: https://github.com/rhoopr/kei/wiki/Verify")]
    Verify(VerifyArgs),

    /// Reconcile state database with files on disk: mark assets as
    /// failed when their local file is missing or truncated, so the
    /// next sync re-downloads them.
    #[command(after_help = "Documentation: https://github.com/rhoopr/kei/wiki/Reconcile")]
    Reconcile(ReconcileArgs),

    /// Register kei as a system service (launchd on macOS, systemd on
    /// Linux, Service Control Manager on Windows). Inside a Docker
    /// container the command logs that compose-managed deployments are
    /// already supervised and exits without writing anything.
    #[command(after_help = "Documentation: https://github.com/rhoopr/kei/wiki/Install")]
    Install(InstallArgs),

    /// Remove the kei service registered by `kei install`. Pass `--purge`
    /// to also delete the state database, configuration, and stored
    /// credentials.
    #[command(after_help = "Documentation: https://github.com/rhoopr/kei/wiki/Service")]
    Uninstall(UninstallArgs),

    /// Service-mode operations: run the long-lived worker, or query
    /// service registration status.
    #[command(after_help = "Documentation: https://github.com/rhoopr/kei/wiki/Service")]
    Service {
        #[command(subcommand)]
        action: ServiceAction,
    },
}

#[derive(Parser, Debug)]
#[command(
    name = "kei",
    about = "kei: photo sync engine",
    version,
    subcommand_required = true,
    arg_required_else_help = true,
    after_help = "Getting started:\n  \
        kei config setup    Generate your config interactively\n  \
        kei sync            Download your photos (runs after setup)\n\n  \
        Common:\n  \
        kei status          See what's been downloaded\n  \
        kei login           Sign in to iCloud\n  \
        kei list albums     Browse albums and libraries\n\n  \
        Advanced:\n  \
        kei install         Register as a background service\n  \
        kei verify          Verify downloaded files\n  \
        kei import-existing  Adopt an existing local photo tree\n\n  \
        Documentation: https://github.com/rhoopr/kei/wiki\n  \
        Support and issues: https://github.com/rhoopr/kei/issues"
)]
pub struct Cli {
    // ── Global options ──────────────────────────────────────────────
    /// Log level
    #[arg(long, value_enum, global = true, env = "KEI_LOG_LEVEL")]
    pub log_level: Option<LogLevel>,

    /// Verbose output (alias for --log-level info, restores full target+timestamp format)
    #[arg(long, short = 'v', global = true)]
    pub verbose: bool,

    /// Path to TOML config file
    #[arg(
        long,
        global = true,
        default_value = "~/.config/kei/config.toml",
        env = "KEI_CONFIG"
    )]
    pub config: String,

    #[command(subcommand)]
    pub command: Command,
}

impl FriendlyArgs {
    /// User-stated friendly-mode preference, distilled from the
    /// `--friendly` / `--no-friendly` pair. `Some(true)` and `Some(false)` are
    /// explicit user requests; `None` means neither flag was set.
    #[must_use]
    pub fn request(&self) -> Option<bool> {
        if self.no_friendly {
            Some(false)
        } else if self.friendly {
            Some(true)
        } else {
            None
        }
    }
}

impl Cli {
    /// User-stated friendly-mode preference, distilled from the
    /// `--friendly` / `--no-friendly` pair. `Some(true)` and `Some(false)` are explicit user requests
    /// that override the TOML and the auto-detected default; `None` means
    /// neither flag was set, so the resolution chain falls through to TOML
    /// then to the default-on-for-TTY policy.
    ///
    /// Clap's `overrides_with` makes the two flags mutually exclusive at the
    /// argument level: when both appear, the last one wins, so the
    /// post-parse state has at most one of `friendly` / `no_friendly` set.
    #[must_use]
    pub fn friendly_request(&self) -> Option<bool> {
        match &self.command {
            Command::Sync { friendly, .. } => friendly.request(),
            _ => None,
        }
    }
}

/// Rendered CLI parse failure with a concrete process exit code.
#[derive(Debug)]
pub struct ParseCliError {
    rendered: String,
    exit_code: i32,
    use_stderr: bool,
}

impl ParseCliError {
    #[must_use]
    pub fn rendered(&self) -> &str {
        &self.rendered
    }

    #[must_use]
    pub const fn exit_code(&self) -> i32 {
        self.exit_code
    }

    #[must_use]
    pub const fn use_stderr(&self) -> bool {
        self.use_stderr
    }
}

impl std::fmt::Display for ParseCliError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.rendered)
    }
}

impl std::error::Error for ParseCliError {}

fn command_context_from_argv(argv: &[OsString]) -> Option<&str> {
    let command = <Cli as clap::CommandFactory>::command();
    argv.iter()
        .skip(1)
        .filter_map(|token| token.to_str())
        .find(|token| command.find_subcommand(token).is_some())
}

fn should_append_removed_sync_surface_hint(argv: &[OsString], err: &clap::Error) -> bool {
    if err.kind() != clap::error::ErrorKind::UnknownArgument {
        return false;
    }

    match command_context_from_argv(argv) {
        None | Some("sync") | Some("import-existing") => true,
        Some("service") => argv
            .iter()
            .skip(1)
            .skip_while(|token| *token != OsStr::new("service"))
            .skip(1)
            .any(|token| token == OsStr::new("run")),
        Some(_) => false,
    }
}

fn with_removed_sync_surface_hint(message: String) -> String {
    format!(
        "{message}\n\nnote: v0.20 removed durable sync CLI flags. Put persistent sync settings in config.toml instead.\n      See {}",
        crate::upgrade_hints::V020_MIGRATION_URL
    )
}

/// Parse CLI arguments and render migration hints for removed sync flags.
pub fn parse_cli_with_sources<I, T>(itr: I) -> Result<Cli, ParseCliError>
where
    I: IntoIterator<Item = T>,
    T: Into<std::ffi::OsString> + Clone,
{
    let argv: Vec<OsString> = itr.into_iter().map(Into::into).collect();
    let cmd = <Cli as clap::CommandFactory>::command();
    let matches = cmd
        .try_get_matches_from(&argv)
        .map_err(|err| ParseCliError {
            rendered: if should_append_removed_sync_surface_hint(&argv, &err) {
                with_removed_sync_surface_hint(err.to_string())
            } else {
                err.to_string()
            },
            exit_code: err.exit_code(),
            use_stderr: err.use_stderr(),
        })?;
    let cli = Cli::from_arg_matches(&matches).map_err(|err| ParseCliError {
        rendered: err.to_string(),
        exit_code: err.exit_code(),
        use_stderr: err.use_stderr(),
    })?;
    Ok(cli)
}

impl Command {
    /// Inject the `ICLOUD_PASSWORD` value captured before `Cli::parse()`.
    ///
    /// The env var is removed from the process environment for security
    /// (prevents leaking via `/proc/*/environ`), but clap's `env` attribute
    /// never sees it. This method restores it into whichever `PasswordArgs`
    /// the active command carries.
    pub fn inject_env_password(&mut self, env_password: Option<String>) {
        let Some(pw) = env_password else { return };
        if let Some(args) = self.password_args_mut()
            && args.password.is_none()
        {
            args.password = Some(pw);
        }
    }

    /// Return a mutable reference to the command's `PasswordArgs`, if any.
    fn password_args_mut(&mut self) -> Option<&mut PasswordArgs> {
        match self {
            Self::Sync { password, .. }
            | Self::Login { password, .. }
            | Self::List { password, .. }
            | Self::Password { password, .. } => Some(password),
            Self::ImportExisting(args) => Some(&mut args.password),
            Self::Service {
                action: ServiceAction::Run(args),
            } => Some(&mut args.password),
            Self::Reset { .. }
            | Self::Config { .. }
            | Self::Status(_)
            | Self::Doctor(_)
            | Self::Manifest(_)
            | Self::Verify(_)
            | Self::Reconcile(_)
            | Self::Install(_)
            | Self::Uninstall(_)
            | Self::Service {
                action: ServiceAction::Status,
            } => None,
        }
    }
}

#[cfg(test)]
#[allow(
    clippy::multiple_unsafe_ops_per_block,
    clippy::undocumented_unsafe_blocks,
    reason = "env var ops in tests are sequenced under a mutex — splitting/documenting adds noise"
)]
mod tests {
    use super::*;
    use clap::Parser;

    fn parse(args: &[&str]) -> Cli {
        Cli::try_parse_from(args).unwrap()
    }

    fn assert_removed_sync_flag(args: &[&str]) {
        let err = Cli::try_parse_from(args).expect_err("removed sync flag must fail to parse");
        assert_eq!(err.kind(), clap::error::ErrorKind::UnknownArgument);
    }

    fn assert_removed_global_option(option: &str, value: &str) {
        let err = Cli::try_parse_from(["kei", option, value])
            .expect_err("removed global option must fail to parse");
        assert_eq!(err.kind(), clap::error::ErrorKind::UnknownArgument);
    }

    fn assert_removed_sync_option(tail: &[&str]) {
        let mut args = vec!["kei"];
        args.extend_from_slice(tail);
        assert_removed_sync_flag(&args);
    }

    #[test]
    fn command_context_uses_clap_subcommand_metadata() {
        let command = <Cli as clap::CommandFactory>::command();
        for subcommand in command.get_subcommands() {
            let subcommand_name = subcommand.get_name();
            let argv = [
                OsString::from("kei"),
                OsString::from(subcommand_name),
                OsString::from("--download-dir"),
                OsString::from("/photos"),
            ];
            assert_eq!(
                command_context_from_argv(&argv),
                Some(subcommand_name),
                "subcommand `{subcommand_name}` should be discovered from clap metadata"
            );
        }
    }

    #[test]
    fn removed_sync_surface_hint_stays_on_sync_like_commands() {
        for argv in [
            &["kei", "--download-dir", "/photos"][..],
            &["kei", "sync", "--download-dir", "/photos"][..],
            &["kei", "import-existing", "--download-dir", "/photos"][..],
            &["kei", "service", "run", "--download-dir", "/photos"][..],
        ] {
            let err = parse_cli_with_sources(argv).expect_err("removed flag must fail");
            assert!(
                err.rendered()
                    .contains("v0.20 removed durable sync CLI flags"),
                "sync-like command should include migration hint: {}",
                err.rendered()
            );
        }
    }

    #[test]
    fn removed_sync_surface_hint_skips_other_metadata_commands() {
        for argv in [
            &["kei", "doctor", "--download-dir", "/photos"][..],
            &["kei", "manifest", "--download-dir", "/photos"][..],
            &["kei", "install", "--download-dir", "/photos"][..],
            &["kei", "service", "status", "--download-dir", "/photos"][..],
        ] {
            let err = parse_cli_with_sources(argv).expect_err("removed flag must fail");
            assert!(
                !err.rendered()
                    .contains("v0.20 removed durable sync CLI flags"),
                "non-sync command should not include migration hint: {}",
                err.rendered()
            );
        }
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
    fn recent_scope_parses_global_and_per_filter() {
        let cli = parse(&["kei", "sync", "--recent", "100", "--recent-scope", "global"]);
        let Command::Sync { sync, .. } = cli.command else {
            panic!("expected sync command");
        };
        assert_eq!(sync.recent_scope, Some(RecentScope::Global));

        let cli = parse(&[
            "kei",
            "sync",
            "--recent",
            "100",
            "--recent-scope",
            "per-filter",
        ]);
        let Command::Sync { sync, .. } = cli.command else {
            panic!("expected sync command");
        };
        assert_eq!(sync.recent_scope, Some(RecentScope::PerFilter));
    }

    #[test]
    fn recent_scope_rejects_unknown_value() {
        let err = Cli::try_parse_from(["kei", "sync", "--recent-scope", "per_album"])
            .expect_err("invalid recent scope should fail");
        assert_eq!(err.kind(), clap::error::ErrorKind::InvalidValue);
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
        vec!["kei", "sync"]
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
    fn test_username_global_removed() {
        assert_removed_global_option("--username", "test@example.com");
    }

    #[test]
    fn test_domain_global_removed() {
        assert_removed_global_option("--domain", "cn");
    }

    #[test]
    fn test_data_dir_global_removed() {
        assert_removed_global_option("--data-dir", "/config");
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

    // ── Explicit subcommand required ──────────────────────────────

    #[test]
    fn test_no_subcommand_requires_command() {
        let _guard = scrub_auth_env();
        let err = Cli::try_parse_from(["kei"]).expect_err("missing command must fail");
        assert_eq!(
            err.kind(),
            clap::error::ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
        );
    }
    #[test]
    fn test_no_subcommand_with_sync_flag_rejected() {
        assert_removed_sync_option(&["--download-dir", "/photos"]);
    }

    // ── New subcommands ───────────────────────────────────────────

    #[test]
    fn test_login_subcommand() {
        let cli = Cli::try_parse_from(["kei", "login"]).unwrap();
        assert!(matches!(
            cli.command,
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
            cli.command,
            Command::Login {
                subcommand: Some(LoginCommand::GetCode),
                ..
            }
        ));
    }

    #[test]
    fn test_login_submit_code() {
        let cli = Cli::try_parse_from(["kei", "login", "submit-code", "123456"]).unwrap();
        match cli.command {
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
            cli.command,
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
            cli.command,
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
            cli.command,
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
            cli.command,
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
            cli.command,
            Command::Password {
                action: PasswordAction::Backend,
                ..
            }
        ));
    }

    #[test]
    fn test_reset_state() {
        let cli = Cli::try_parse_from(["kei", "reset", "state", "--yes"]).unwrap();
        match cli.command {
            Command::Reset {
                what: ResetCommand::State { yes },
            } => assert!(yes),
            _ => panic!("Expected Reset State"),
        }
    }

    #[test]
    fn install_dry_run_parses_with_user_flag() {
        let cli = Cli::try_parse_from(["kei", "install", "--user", "--dry-run"]).unwrap();
        let args = match cli.command {
            Command::Install(a) => a.clone(),
            other => panic!("expected Install, got {other:?}"),
        };
        assert!(args.user);
        assert!(args.dry_run);
        assert!(!args.system);
    }

    #[test]
    fn install_user_and_system_conflict_at_parse_time() {
        // clap rejects mutually exclusive flags during parse; this guards
        // the `conflicts_with` annotation on InstallArgs.
        let err = Cli::try_parse_from(["kei", "install", "--user", "--system"]).unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);
    }

    #[test]
    fn uninstall_purge_parses_to_struct_field() {
        let cli = Cli::try_parse_from(["kei", "uninstall", "--purge"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Uninstall(UninstallArgs { purge: true })
        ));
    }

    #[test]
    fn test_reset_sync_token() {
        let cli = Cli::try_parse_from(["kei", "reset", "sync-token"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Reset {
                what: ResetCommand::SyncToken { yes: false },
            }
        ));
    }

    #[test]
    fn test_reset_sync_token_with_yes() {
        let cli = Cli::try_parse_from(["kei", "reset", "sync-token", "--yes"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Reset {
                what: ResetCommand::SyncToken { yes: true },
            }
        ));
    }

    #[test]
    fn test_reset_sync_token_with_short_y() {
        let cli = Cli::try_parse_from(["kei", "reset", "sync-token", "-y"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Reset {
                what: ResetCommand::SyncToken { yes: true },
            }
        ));
    }

    #[test]
    fn test_reset_session() {
        let cli = Cli::try_parse_from(["kei", "reset", "session"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Reset {
                what: ResetCommand::Session { yes: false },
            }
        ));
    }

    #[test]
    fn test_reset_session_with_yes() {
        let cli = Cli::try_parse_from(["kei", "reset", "session", "--yes"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Reset {
                what: ResetCommand::Session { yes: true },
            }
        ));
    }

    #[test]
    fn test_reset_session_with_short_y() {
        let cli = Cli::try_parse_from(["kei", "reset", "session", "-y"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Reset {
                what: ResetCommand::Session { yes: true },
            }
        ));
    }

    #[test]
    fn test_config_show() {
        let cli = Cli::try_parse_from(["kei", "config", "show"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Config {
                action: ConfigAction::Show,
            }
        ));
    }

    #[test]
    fn test_config_setup() {
        let cli = Cli::try_parse_from(["kei", "config", "setup"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Config {
                action: ConfigAction::Setup { output: None },
            }
        ));
    }

    #[test]
    fn test_config_setup_with_output() {
        let cli =
            Cli::try_parse_from(["kei", "config", "setup", "-o", "/tmp/config.toml"]).unwrap();
        match cli.command {
            Command::Config {
                action: ConfigAction::Setup { output },
            } => assert_eq!(output.as_deref(), Some("/tmp/config.toml")),
            _ => panic!("Expected Config Setup"),
        }
    }
    #[test]
    fn test_sync_retry_failed_flag() {
        assert_removed_sync_flag(&["kei", "sync", "--retry-failed", "--download-dir", "/photos"]);
    }

    // ── Legacy subcommand compat ──────────────────────────────────
    #[test]
    fn test_max_download_attempts_cli_parse() {
        assert_removed_sync_flag(&["kei", "sync", "--max-download-attempts", "5"]);
    }
    #[test]
    fn test_max_download_attempts_defaults_to_none() {
        assert_removed_sync_flag(&["kei", "sync", "--max-download-attempts", "5"]);
    }

    #[test]
    fn test_password_flag() {
        let mut args = base_args();
        args.extend(["--password", "secret123"]);
        let cli = parse(&args);
        let Command::Sync { password, .. } = cli.command else {
            panic!("expected Sync command");
        };
        assert_eq!(password.password.as_deref(), Some("secret123"));
    }

    #[test]
    fn test_password_file_flag() {
        let _guard = scrub_auth_env();
        let mut args = base_args();
        args.extend(["--password-file", "/run/secrets/pw"]);
        let cli = parse(&args);
        let Command::Sync { password, .. } = cli.command else {
            panic!("expected Sync command");
        };
        assert_eq!(password.password_file.as_deref(), Some("/run/secrets/pw"));
    }

    #[test]
    fn test_password_command_flag() {
        let _guard = scrub_auth_env();
        let mut args = base_args();
        args.extend(["--password-command", "op read 'op://vault/icloud/pw'"]);
        let cli = parse(&args);
        let Command::Sync { password, .. } = cli.command else {
            panic!("expected Sync command");
        };
        assert_eq!(
            password.password_command.as_deref(),
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
    fn sync_response_capture_is_explicit_and_allows_preview() {
        assert!(!SyncArgs::default().capture_icloud_responses);
        for preview in ["--dry-run", "--only-print-filenames"] {
            let Command::Sync { sync, .. } =
                Cli::try_parse_from(["kei", "sync", "--capture-icloud-responses", preview])
                    .unwrap()
                    .command
            else {
                panic!("expected Sync command");
            };
            assert!(sync.capture_icloud_responses);
        }
    }

    #[test]
    fn sync_refresh_metadata_parses_to_struct_field() {
        let Command::Sync { sync, .. } = Cli::try_parse_from(["kei", "sync", "--refresh-metadata"])
            .unwrap()
            .command
        else {
            panic!("expected Sync command");
        };
        assert!(sync.refresh_metadata);
    }

    #[test]
    fn sync_repair_capture_timestamps_requires_metadata_refresh() {
        assert!(Cli::try_parse_from(["kei", "sync", "--repair-capture-timestamps"]).is_err());

        let Command::Sync { sync, .. } = Cli::try_parse_from([
            "kei",
            "sync",
            "--refresh-metadata",
            "--repair-capture-timestamps",
        ])
        .unwrap()
        .command
        else {
            panic!("expected Sync command");
        };
        assert!(sync.refresh_metadata);
        assert!(sync.repair_capture_timestamps);
    }

    #[test]
    fn sync_repair_truncated_parses_to_struct_field() {
        let Command::Sync { sync, .. } = Cli::try_parse_from(["kei", "sync", "--repair-truncated"])
            .unwrap()
            .command
        else {
            panic!("expected Sync command");
        };
        assert!(sync.repair_truncated);
    }

    #[test]
    fn sync_repair_truncated_rejects_non_download_modes() {
        for conflicting in ["--dry-run", "--only-print-filenames", "--refresh-metadata"] {
            assert!(
                Cli::try_parse_from(["kei", "sync", "--repair-truncated", conflicting]).is_err(),
                "{conflicting} must conflict with --repair-truncated"
            );
        }
    }

    #[test]
    fn test_save_password_flag() {
        let mut args = base_args();
        args.push("--save-password");
        let cli = parse(&args);
        let Command::Sync { sync, .. } = cli.command else {
            panic!("expected Sync command");
        };
        assert!(sync.save_password);
    }

    #[test]
    fn test_save_password_top_level_before_subcommand_is_rejected() {
        let err = Cli::try_parse_from(["kei", "--save-password", "sync"]).unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::UnknownArgument);
    }

    // ── Sync args ─────────────────────────────────────────────────
    #[test]
    fn test_library_accepts_custom_value() {
        assert_removed_sync_flag(&["kei", "--library", "SharedSync-ABCD1234"]);
    }
    #[test]
    fn test_library_repeatable_with_sentinels() {
        assert_removed_sync_flag(&["kei", "--library", "primary", "--library", "shared"]);
    }
    #[test]
    fn test_bandwidth_limit_bare_bytes() {
        assert_removed_sync_option(&["--bandwidth-limit", "1024"]);
    }
    #[test]
    fn test_bandwidth_limit_decimal_suffixes() {
        assert_removed_sync_option(&["--bandwidth-limit", "1.5M"]);
    }

    #[test]
    fn test_bandwidth_limit_binary_suffix() {
        assert_removed_sync_option(&["--bandwidth-limit", "2Mi"]);
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
        assert_removed_sync_option(&["--max-retries", "10"]);
    }
    #[test]
    fn test_max_retries_zero_disables() {
        assert_removed_sync_option(&["--max-retries", "0"]);
    }
    #[test]
    fn test_temp_suffix_custom() {
        assert_removed_sync_option(&["--temp-suffix", ".downloading"]);
    }
    #[test]
    fn test_skip_videos_flag() {
        assert_removed_sync_option(&["--skip-videos"]);
    }

    #[test]
    fn test_unfiled_flag_default_none() {
        let cli = parse(&base_args());
        let Command::Sync { sync, .. } = cli.command else {
            panic!("expected Sync command");
        };
        assert_eq!(sync.config_overrides.unfiled, None);
    }
    #[test]
    fn test_unfiled_flag_bare_true() {
        assert_removed_sync_option(&["--unfiled"]);
    }
    #[test]
    fn test_unfiled_flag_explicit_false() {
        assert_removed_sync_option(&["--unfiled", "false"]);
    }

    #[test]
    fn test_unfiled_flag_explicit_true() {
        assert_removed_sync_option(&["--unfiled", "true"]);
    }

    #[test]
    fn test_skip_videos_explicit_false() {
        assert_removed_sync_option(&["--skip-videos", "false"]);
    }
    #[test]
    fn test_skip_photos_flag() {
        assert_removed_sync_option(&["--skip-photos"]);
    }
    #[test]
    fn test_force_size_flag() {
        assert_removed_sync_option(&["--force-size"]);
    }
    #[cfg(feature = "xmp")]
    #[test]
    fn test_set_exif_datetime_flag() {
        assert_removed_sync_option(&["--set-exif-datetime"]);
    }
    #[cfg(feature = "xmp")]
    #[test]
    fn test_embed_xmp_flag() {
        assert_removed_sync_option(&["--embed-xmp"]);
    }
    #[cfg(feature = "xmp")]
    #[test]
    fn test_embed_xmp_flag_explicit_false() {
        assert_removed_sync_option(&["--embed-xmp=false"]);
    }
    #[cfg(feature = "xmp")]
    #[test]
    fn test_xmp_sidecar_flag() {
        assert_removed_sync_option(&["--xmp-sidecar"]);
    }

    #[test]
    fn test_no_progress_bar_flag() {
        let mut args = base_args();
        args.push("--no-progress-bar");
        let cli = parse(&args);
        let Command::Sync { sync, .. } = cli.command else {
            panic!("expected Sync command");
        };
        assert!(sync.no_progress_bar);
    }
    #[test]
    fn test_no_progress_bar_rejects_value() {
        for value in ["true", "false"] {
            let flag = format!("--no-progress-bar={value}");

            let mut args = base_args();
            args.push("--no-progress-bar");
            args.push(value);
            assert!(Cli::try_parse_from(&args).is_err());

            assert!(Cli::try_parse_from(["kei", "sync", flag.as_str()]).is_err());
            assert!(Cli::try_parse_from(["kei", flag.as_str(), "sync"]).is_err());
        }
    }
    #[test]
    fn test_keep_unicode_in_filenames_flag() {
        assert_removed_sync_option(&["--keep-unicode-in-filenames"]);
    }
    #[test]
    fn test_notify_systemd_flag() {
        assert_removed_sync_option(&["--notify-systemd"]);
    }
    #[test]
    fn test_pid_file_flag() {
        assert_removed_sync_option(&["--pid-file", "/tmp/kei.pid"]);
    }
    #[test]
    fn test_reconcile_every_n_cycles_flag() {
        assert_removed_sync_option(&["--reconcile-every-n-cycles", "24"]);
    }

    #[test]
    fn test_reconcile_every_n_cycles_default_unset() {
        let cli = parse(&base_args());
        let Command::Sync { sync, .. } = cli.command else {
            panic!("expected Sync command");
        };
        assert!(sync.config_overrides.reconcile_every_n_cycles.is_none());
    }

    // 0 is "off" via TOML (or absence); the CLI flag rejects it so users
    // omit the flag instead of passing a magic value. Anything else <0 or
    // non-numeric also fails clap's range parser.
    #[test]
    fn test_reconcile_every_n_cycles_rejects_zero() {
        assert_removed_sync_option(&["--reconcile-every-n-cycles", "0"]);
    }
    #[test]
    fn test_notification_script_flag() {
        assert_removed_sync_option(&["--notification-script", "/tmp/notify.sh"]);
    }
    #[test]
    fn test_report_json_flag() {
        assert_removed_sync_option(&["--report-json", "/tmp/run.json"]);
    }

    // ── Enum variants ──────────────────────────────────────────────
    #[test]
    fn test_size_all_variants() {
        assert_removed_sync_option(&["--size", "medium"]);
    }
    #[test]
    fn test_live_photo_size_all_variants() {
        assert_removed_sync_option(&["--live-photo-size", "medium"]);
    }
    #[test]
    fn test_live_photo_mov_filename_policy_all_variants() {
        assert_removed_sync_option(&["--live-photo-mov-filename-policy", "original"]);
    }
    #[test]
    fn test_align_raw_all_variants() {
        assert_removed_sync_option(&["--align-raw", "prefer-original"]);
    }

    #[test]
    fn test_align_raw_rejects_invalid() {
        let mut args = base_args();
        args.extend(["--align-raw", "bogus"]);
        assert!(Cli::try_parse_from(&args).is_err());
    }
    #[test]
    fn test_file_match_policy_all_variants() {
        assert_removed_sync_option(&["--file-match-policy", "name-id7"]);
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
        assert_removed_sync_option(&["--folder-structure", "%Y-%m"]);
    }
    #[test]
    fn test_download_dir_custom() {
        assert_removed_sync_option(&["--download-dir", "/photos"]);
    }
    #[test]
    fn test_watch_with_interval() {
        assert_removed_sync_option(&["--watch-with-interval", "3600"]);
    }
    #[test]
    fn test_watch_with_interval_rejects_below_minimum() {
        assert_removed_sync_option(&["--watch-with-interval", "59"]);
    }
    #[test]
    fn test_watch_with_interval_rejects_above_maximum() {
        assert_removed_sync_option(&["--watch-with-interval", "86401"]);
    }

    #[test]
    fn test_skip_created_before() {
        let mut args = base_args();
        args.extend(["--skip-created-before", "2024-01-01"]);
        let cli = parse(&args);
        let Command::Sync { sync, .. } = cli.command else {
            panic!("expected Sync command");
        };
        assert_eq!(sync.skip_created_before.as_deref(), Some("2024-01-01"));
    }

    #[test]
    fn test_skip_created_after() {
        let mut args = base_args();
        args.extend(["--skip-created-after", "2025-06-01"]);
        let cli = parse(&args);
        let Command::Sync { sync, .. } = cli.command else {
            panic!("expected Sync command");
        };
        assert_eq!(sync.skip_created_after.as_deref(), Some("2025-06-01"));
    }
    #[test]
    fn test_albums_multiple() {
        assert_removed_sync_option(&["--album", "Favorites", "--album", "Vacation"]);
    }

    #[test]
    fn test_albums_empty_by_default() {
        let cli = parse(&base_args());
        let Command::Sync { sync, .. } = cli.command else {
            panic!("expected Sync command");
        };
        assert!(sync.config_overrides.albums.is_empty());
    }
    #[test]
    fn test_album_all_accepted() {
        assert_removed_sync_flag(&["kei", "-a", "all"]);
    }

    // ── Input validation ───────────────────────────────────────────

    #[test]
    fn test_empty_username_rejected() {
        assert_removed_global_option("--username", "");
    }

    #[test]
    fn test_empty_password_rejected() {
        let mut args = base_args();
        args.extend(["--password", ""]);
        assert!(Cli::try_parse_from(&args).is_err());
    }

    #[test]
    fn test_empty_download_dir_rejected() {
        assert_removed_sync_option(&["--download-dir", ""]);
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
    fn test_max_retries_rejects_above_100() {
        let mut args = base_args();
        args.extend(["--max-retries", "101"]);
        assert!(Cli::try_parse_from(&args).is_err());
    }
    #[test]
    fn test_max_retries_accepts_100() {
        assert_removed_sync_option(&["--max-retries", "100"]);
    }
    #[test]
    fn test_sync_subcommand() {
        let cli = Cli::try_parse_from(["kei", "sync"]).unwrap();
        assert!(matches!(cli.command, Command::Sync { .. }));
    }

    #[test]
    fn test_status_subcommand() {
        let cli = Cli::try_parse_from(["kei", "status"]).unwrap();
        assert!(matches!(cli.command, Command::Status(_)));
    }

    #[test]
    fn test_status_with_failed_flag() {
        let cli = Cli::try_parse_from(["kei", "status", "--failed"]).unwrap();
        if let Command::Status(args) = cli.command {
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
        if let Command::Status(args) = cli.command {
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
        if let Command::Status(args) = cli.command {
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
        if let Command::Status(args) = cli.command {
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
        assert!(matches!(cli.command, Command::Status(_)));
    }

    #[test]
    fn test_username_global_before_subcommand_removed() {
        let err = Cli::try_parse_from(["kei", "--username", "test@example.com", "sync"])
            .expect_err("removed global option must fail to parse");
        assert_eq!(err.kind(), clap::error::ErrorKind::UnknownArgument);
    }

    // ── import-existing ───────────────────────────────────────────

    #[test]
    fn test_import_existing_subcommand() {
        let cli = Cli::try_parse_from(["kei", "import-existing"]).unwrap();
        if let Command::ImportExisting(args) = cli.command {
            assert!(args.recent.is_none());
            assert!(!args.dry_run);
            assert!(!args.strict);
        } else {
            panic!("Expected ImportExisting command");
        }
    }

    #[test]
    fn test_import_existing_library_flag_single() {
        let cli =
            Cli::try_parse_from(["kei", "import-existing", "--library", "SharedSync-ABCD1234"])
                .unwrap();
        if let Command::ImportExisting(args) = cli.command {
            assert_eq!(args.libraries, vec!["SharedSync-ABCD1234".to_string()]);
        } else {
            panic!("Expected ImportExisting command");
        }
    }

    /// Parity check with `kei sync --library`: import-existing's flag is also
    /// repeatable and accepts mixed sentinels, zone names, and `!name`
    /// exclusions in one invocation.
    #[test]
    fn test_import_existing_library_flag_repeatable_with_mixed_grammar() {
        let cli = Cli::try_parse_from([
            "kei",
            "import-existing",
            "--library",
            "primary",
            "--library",
            "SharedSync-ABCD1234",
            "--library",
            "!SharedSync-Photos",
        ])
        .unwrap();
        let Command::ImportExisting(args) = cli.command else {
            panic!("Expected ImportExisting command");
        };
        assert_eq!(
            args.libraries,
            vec![
                "primary".to_string(),
                "SharedSync-ABCD1234".to_string(),
                "!SharedSync-Photos".to_string(),
            ]
        );
    }

    fn parse_import(extra: &[&str]) -> ImportArgs {
        let mut args = vec!["kei", "import-existing"];
        args.extend(extra.iter().copied());
        let cli = Cli::try_parse_from(args).unwrap();
        match cli.command {
            Command::ImportExisting(a) => a,
            _ => panic!("Expected ImportExisting command"),
        }
    }

    #[test]
    fn import_existing_strict_flag_parses_to_true() {
        let args = parse_import(&["--strict"]);
        assert!(args.strict);
    }

    #[test]
    fn import_existing_force_empty_flag_parses_to_true() {
        let _guard = scrub_auth_env();
        unsafe {
            std::env::remove_var("KEI_FORCE_EMPTY");
        }
        let args = parse_import(&["--force-empty"]);
        assert!(args.force_empty);
    }

    #[test]
    fn import_existing_force_empty_default_is_false() {
        let _guard = scrub_auth_env();
        unsafe {
            std::env::remove_var("KEI_FORCE_EMPTY");
        }
        let args = parse_import(&[]);
        assert!(!args.force_empty);
    }

    #[test]
    fn import_existing_force_empty_env_var_parses_to_true() {
        let _guard = scrub_auth_env();
        unsafe {
            std::env::set_var("KEI_FORCE_EMPTY", "true");
        }
        let cli = Cli::try_parse_from(["kei", "import-existing"]).unwrap();
        unsafe {
            std::env::remove_var("KEI_FORCE_EMPTY");
        }
        match cli.command {
            Command::ImportExisting(args) => assert!(args.force_empty),
            _ => panic!("Expected ImportExisting command"),
        }
    }

    #[test]
    fn import_existing_removed_durable_flags_fail() {
        for (flag, value) in [
            ("--download-dir", Some("/tmp")),
            ("-d", Some("/tmp")),
            ("--folder-structure", Some("%Y/%m")),
            ("--folder-structure-albums", Some("{album}")),
            ("--folder-structure-smart-folders", Some("{smart-folder}")),
            ("--file-match-policy", Some("name-id7")),
            ("--size", Some("medium")),
            ("--live-photo-mode", Some("image-only")),
            ("--live-photo-size", Some("medium")),
            ("--live-photo-mov-filename-policy", Some("original")),
            ("--align-raw", Some("original")),
            ("--force-size", None),
            ("--keep-unicode-in-filenames", None),
        ] {
            let mut argv = vec!["kei", "import-existing", flag];
            if let Some(value) = value {
                argv.push(value);
            }
            let err = Cli::try_parse_from(argv).expect_err("removed flag must fail");
            assert_eq!(err.kind(), clap::error::ErrorKind::UnknownArgument);
        }
    }

    #[test]
    fn test_list_albums_library_flag() {
        let cli = Cli::try_parse_from(["kei", "list", "--library", "all", "albums"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::List {
                ref libraries,
                what: ListCommand::Albums,
                ..
            } if libraries == &vec!["all".to_string()]
        ));
    }

    /// Parity check with `kei sync --library`: list's flag is also
    /// repeatable. Pre-fix it was `Option<String>` and a second
    /// `--library` silently won, so multi-value users had no way to
    /// list across both primary and a specific shared library.
    #[test]
    fn test_list_albums_library_flag_repeatable() {
        let cli = Cli::try_parse_from([
            "kei",
            "list",
            "--library",
            "primary",
            "--library",
            "SharedSync-ABCD1234",
            "albums",
        ])
        .unwrap();
        match cli.command {
            Command::List {
                libraries,
                what: ListCommand::Albums,
                ..
            } => {
                assert_eq!(
                    libraries,
                    vec!["primary".to_string(), "SharedSync-ABCD1234".to_string()]
                );
            }
            other => panic!("expected Command::List, got {other:?}"),
        }
    }

    /// `--library` accepts the v0.13 `!name` exclusion sentinel.
    #[test]
    fn test_list_albums_library_flag_exclusion() {
        let cli = Cli::try_parse_from([
            "kei",
            "list",
            "--library",
            "all",
            "--library",
            "!SharedSync-ABCD1234",
            "albums",
        ])
        .unwrap();
        match cli.command {
            Command::List {
                libraries,
                what: ListCommand::Albums,
                ..
            } => {
                assert_eq!(
                    libraries,
                    vec!["all".to_string(), "!SharedSync-ABCD1234".to_string()]
                );
            }
            other => panic!("expected Command::List, got {other:?}"),
        }
    }

    /// `kei list libraries` also accepts the same flag grammar; the
    /// flag is parsed regardless of subcommand and is ignored on the
    /// libraries side (it's documented as albums-only).
    #[test]
    fn test_list_libraries_library_flag_repeatable() {
        let cli = Cli::try_parse_from([
            "kei",
            "list",
            "--library",
            "primary",
            "--library",
            "shared",
            "libraries",
        ])
        .unwrap();
        match cli.command {
            Command::List {
                libraries,
                what: ListCommand::Libraries,
                ..
            } => {
                assert_eq!(libraries, vec!["primary".to_string(), "shared".to_string()]);
            }
            other => panic!("expected Command::List, got {other:?}"),
        }
    }

    #[test]
    fn test_verify_subcommand() {
        let cli = Cli::try_parse_from(["kei", "verify", "--checksums"]).unwrap();
        if let Command::Verify(args) = cli.command {
            assert!(args.checksums);
        } else {
            panic!("Expected Verify command");
        }
    }

    #[test]
    fn test_reconcile_subcommand() {
        let cli = Cli::try_parse_from(["kei", "reconcile"]).unwrap();
        if let Command::Reconcile(args) = cli.command {
            assert!(!args.dry_run);
        } else {
            panic!("Expected Reconcile command");
        }
    }

    #[test]
    fn test_reconcile_dry_run_flag() {
        let cli = Cli::try_parse_from(["kei", "reconcile", "--dry-run"]).unwrap();
        if let Command::Reconcile(args) = cli.command {
            assert!(args.dry_run);
        } else {
            panic!("Expected Reconcile command");
        }
    }

    // ── New filter flags ───────────────────────────────────────────
    #[test]
    fn test_live_photo_mode_all_variants() {
        assert_removed_sync_option(&["--live-photo-mode", "skip"]);
    }

    #[test]
    fn test_filename_exclude_single() {
        assert_removed_sync_option(&["--filename-exclude", "*.AAE"]);
    }
    #[test]
    fn test_filename_exclude_multiple() {
        assert_removed_sync_option(&["--filename-exclude", "*.AAE,Screenshot*"]);
    }

    #[test]
    fn test_exclude_album_rejected() {
        let mut args = base_args();
        args.extend(["--exclude-album", "Hidden"]);
        assert!(Cli::try_parse_from(args).is_err());
    }

    // ── Clap-owned command boundary ────────────────────────────────
    //
    // Sync flags live on `kei sync`, not on the top-level `kei` command. clap
    // should reject them before another subcommand can silently swallow them.
    #[test]
    fn top_level_rejects_skip_videos_before_status() {
        let err = Cli::try_parse_from(["kei", "--skip-videos=true", "status"]).unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::UnknownArgument);
    }
    #[test]
    fn top_level_rejects_skip_photos_before_list_albums() {
        let err = Cli::try_parse_from(["kei", "--skip-photos=true", "list", "albums"]).unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::UnknownArgument);
    }
    #[test]
    fn top_level_rejects_live_photo_mode_before_reset_state() {
        let err = Cli::try_parse_from([
            "kei",
            "--live-photo-mode",
            "skip",
            "reset",
            "state",
            "--yes",
        ])
        .unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::UnknownArgument);
    }

    #[test]
    fn top_level_rejects_value_flag_before_verify() {
        let err = Cli::try_parse_from(["kei", "--download-dir", "/photos", "verify"])
            .expect_err("parse must reject");
        let err = err.to_string();
        assert!(err.contains("--download-dir"), "got: {err}");
    }

    #[test]
    fn top_level_rejects_threads_before_reconcile() {
        let err = Cli::try_parse_from(["kei", "--threads", "4", "reconcile"])
            .expect_err("parse must reject");
        let err = err.to_string();
        assert!(err.contains("--threads"), "got: {err}");
    }

    #[test]
    fn top_level_rejects_dry_run_before_status() {
        let err =
            Cli::try_parse_from(["kei", "--dry-run", "status"]).expect_err("parse must reject");
        let err = err.to_string();
        assert!(err.contains("--dry-run"), "got: {err}");
    }

    #[test]
    fn top_level_rejects_album_before_password_set() {
        let err = Cli::try_parse_from(["kei", "--album", "Vacation", "password", "set"])
            .expect_err("parse must reject");
        let err = err.to_string();
        assert!(err.contains("--album"), "got: {err}");
    }

    #[test]
    fn removed_auth_only_flag_is_rejected() {
        let err = Cli::try_parse_from(["kei", "--auth-only", "status"])
            .expect_err("parse must reject removed flag")
            .to_string();
        assert!(err.contains("--auth-only"), "got: {err}");
    }
    #[test]
    fn top_level_rejects_first_offending_sync_flag() {
        let err = Cli::try_parse_from([
            "kei",
            "--dry-run",
            "--only-print-filenames",
            "--recent",
            "100",
            "status",
        ])
        .expect_err("parse must reject")
        .to_string();
        assert!(err.contains("--dry-run"), "got: {err}");
    }

    #[test]
    fn sync_subcommand_accepts_sync_flags() {
        Cli::try_parse_from([
            "kei",
            "sync",
            "--friendly",
            "--recent",
            "100",
            "--skip-created-before",
            "2025-01-01",
        ])
        .expect("explicit sync subcommand with kept sync flags must parse");
    }
    #[test]
    fn top_level_sync_flag_before_sync_subcommand_is_rejected() {
        let err = Cli::try_parse_from(["kei", "--friendly", "--recent", "100", "sync"])
            .expect_err("top-level sync flags must not parse before subcommand");
        assert_eq!(err.kind(), clap::error::ErrorKind::UnknownArgument);
    }
    #[test]
    fn retry_failed_with_sync_flag_parses_under_sync() {
        Cli::try_parse_from(["kei", "sync", "--retry-failed", "--recent", "100"])
            .expect("sync --retry-failed accepts kept per-run sync flags");
    }

    #[test]
    fn kept_global_flags_parse_before_non_sync_subcommand() {
        Cli::try_parse_from(["kei", "--log-level", "debug", "status"])
            .expect("global flags must parse with any subcommand");
    }

    #[test]
    fn status_with_no_top_level_flags_parses() {
        Cli::try_parse_from(["kei", "status"]).expect("plain `kei status` must parse");
    }
    #[test]
    fn service_run_accepts_own_sync_flags() {
        Cli::try_parse_from(["kei", "service", "run", "--recent", "100", "--dry-run"])
            .expect("service run must accept its own sync flags");
    }

    #[test]
    fn service_run_rejects_friendly_flag() {
        let err = Cli::try_parse_from(["kei", "service", "run", "--friendly"])
            .expect_err("service run does not expose friendly mode")
            .to_string();
        assert!(err.contains("--friendly"), "got: {err}");
    }

    #[test]
    fn service_status_rejects_sync_flags() {
        let err = Cli::try_parse_from(["kei", "--download-dir", "/photos", "service", "status"])
            .expect_err("parse must reject sync flags with service status")
            .to_string();
        assert!(err.contains("--download-dir"), "got: {err}");
    }

    #[test]
    fn top_level_rejects_each_sync_only_flag_before_status() {
        let cases: &[(&str, &[&str])] = &[
            ("skip_videos", &["--skip-videos=true"]),
            ("skip_photos", &["--skip-photos=true"]),
            ("unfiled", &["--unfiled=true"]),
            ("force_size", &["--force-size=true"]),
            ("no_progress_bar", &["--no-progress-bar"]),
            ("keep_unicode", &["--keep-unicode-in-filenames=true"]),
            ("notify_systemd", &["--notify-systemd=true"]),
            ("dry_run", &["--dry-run"]),
            ("friendly", &["--friendly"]),
            ("no_friendly", &["--no-friendly"]),
            ("only_print_filenames", &["--only-print-filenames"]),
            ("save_password", &["--save-password"]),
            ("retry_failed", &["--retry-failed"]),
            ("download_dir", &["--download-dir", "/photos"]),
            ("album", &["--album", "Vacation"]),
            ("smart_folder", &["--smart-folder", "Favorites"]),
            ("filename_exclude", &["--filename-exclude", "*.AAE"]),
            ("library", &["--library", "primary"]),
            ("size", &["--size", "original"]),
            ("live_photo_size", &["--live-photo-size", "original"]),
            ("recent", &["--recent", "100"]),
            ("threads", &["--threads", "4"]),
            ("bandwidth_limit", &["--bandwidth-limit", "10M"]),
            ("live_photo_mode", &["--live-photo-mode", "skip"]),
            ("folder_structure", &["--folder-structure", "%Y/%m"]),
            (
                "folder_structure_albums",
                &["--folder-structure-albums", "{album}"],
            ),
            (
                "folder_structure_smart_folders",
                &["--folder-structure-smart-folders", "{smart-folder}"],
            ),
            ("watch_with_interval", &["--watch-with-interval", "3600"]),
            (
                "live_photo_mov_filename_policy",
                &["--live-photo-mov-filename-policy", "original"],
            ),
            ("align_raw", &["--align-raw", "original"]),
            (
                "file_match_policy",
                &["--file-match-policy", "name-size-dedup-with-suffix"],
            ),
            (
                "skip_created_before",
                &["--skip-created-before", "2025-01-01"],
            ),
            (
                "skip_created_after",
                &["--skip-created-after", "2025-12-31"],
            ),
            ("max_retries", &["--max-retries", "3"]),
            ("temp_suffix", &["--temp-suffix", ".part"]),
            ("pid_file", &["--pid-file", "/tmp/kei.pid"]),
            (
                "reconcile_every_n_cycles",
                &["--reconcile-every-n-cycles", "24"],
            ),
            (
                "notification_script",
                &["--notification-script", "/tmp/notify.sh"],
            ),
            ("report_json", &["--report-json", "/tmp/report.json"]),
            ("http_port", &["--http-port", "9090"]),
            ("http_bind", &["--http-bind", "127.0.0.1"]),
            ("max_download_attempts", &["--max-download-attempts", "5"]),
        ];
        for (name, args) in cases {
            let mut argv: Vec<&str> = vec!["kei"];
            argv.extend_from_slice(args);
            argv.push("status");
            let result = Cli::try_parse_from(&argv);
            assert!(
                result.is_err(),
                "clap must reject sync-only flag `{name}` ({args:?}) before status; got {result:?}"
            );
        }
    }

    // ── friendly_request() resolution ───────────────────────────────────
    //
    // The helper distils the `--friendly` / `--no-friendly` pair into the
    // tristate `lib.rs` actually consumes. Tests here prove clap's
    // `overrides_with` wiring matches the documented behaviour: last flag
    // wins when both appear, neither set yields `None`.

    #[test]
    fn friendly_request_none_when_neither_flag() {
        let cli = parse(&["kei", "status"]);
        assert_eq!(cli.friendly_request(), None);
    }

    #[test]
    fn friendly_request_some_true_with_friendly() {
        let cli = parse(&["kei", "sync", "--friendly"]);
        assert_eq!(cli.friendly_request(), Some(true));
    }

    #[test]
    fn friendly_request_some_false_with_no_friendly() {
        let cli = parse(&["kei", "sync", "--no-friendly"]);
        assert_eq!(cli.friendly_request(), Some(false));
    }

    #[test]
    fn friendly_request_last_wins_no_friendly_after_friendly() {
        let cli = parse(&["kei", "sync", "--friendly", "--no-friendly"]);
        assert_eq!(
            cli.friendly_request(),
            Some(false),
            "--no-friendly after --friendly must win via overrides_with"
        );
    }

    #[test]
    fn friendly_request_last_wins_friendly_after_no_friendly() {
        let cli = parse(&["kei", "sync", "--no-friendly", "--friendly"]);
        assert_eq!(
            cli.friendly_request(),
            Some(true),
            "--friendly after --no-friendly must win via overrides_with"
        );
    }
}
