use crate::password::SecretString;
use crate::types::{
    Domain, FileMatchPolicy, LivePhotoMode, LivePhotoMovFilenamePolicy, LivePhotoSize, LogLevel,
    RawTreatmentPolicy, VersionSize,
};
use chrono::{DateTime, Local, NaiveDate, NaiveDateTime};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

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
    pub watch: Option<TomlWatch>,
    pub notifications: Option<TomlNotifications>,
    pub metrics: Option<TomlMetrics>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TomlNotifications {
    pub script: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TomlAuth {
    pub username: Option<String>,
    pub password: Option<String>,
    pub password_file: Option<String>,
    pub password_command: Option<String>,
    pub domain: Option<Domain>,
    pub cookie_directory: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TomlDownload {
    pub directory: Option<String>,
    pub folder_structure: Option<String>,
    pub threads_num: Option<u16>,
    pub bandwidth_limit: Option<String>,
    pub temp_suffix: Option<String>,
    pub set_exif_datetime: Option<bool>,
    pub set_exif_rating: Option<bool>,
    pub set_exif_gps: Option<bool>,
    pub set_exif_description: Option<bool>,
    pub embed_xmp: Option<bool>,
    pub xmp_sidecar: Option<bool>,
    pub no_progress_bar: Option<bool>,
    pub retry: Option<TomlRetry>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TomlRetry {
    pub max_retries: Option<u32>,
    pub delay: Option<u64>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TomlFilters {
    pub library: Option<String>,
    pub albums: Option<Vec<String>>,
    pub exclude_albums: Option<Vec<String>>,
    pub filename_exclude: Option<Vec<String>>,
    pub skip_videos: Option<bool>,
    pub skip_photos: Option<bool>,
    pub skip_live_photos: Option<bool>,
    pub recent: Option<u32>,
    pub skip_created_before: Option<String>,
    pub skip_created_after: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TomlPhotos {
    pub size: Option<VersionSize>,
    pub live_photo_size: Option<LivePhotoSize>,
    pub live_photo_mode: Option<LivePhotoMode>,
    pub live_photo_mov_filename_policy: Option<LivePhotoMovFilenamePolicy>,
    pub align_raw: Option<RawTreatmentPolicy>,
    pub file_match_policy: Option<FileMatchPolicy>,
    pub force_size: Option<bool>,
    pub keep_unicode_in_filenames: Option<bool>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TomlWatch {
    pub interval: Option<u64>,
    pub notify_systemd: Option<bool>,
    pub pid_file: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TomlMetrics {
    pub port: Option<u16>,
}

/// Load a TOML config file. Returns `Ok(None)` if the file doesn't exist
/// and `required` is false. Errors if the file doesn't exist and `required` is true.
pub(crate) fn load_toml_config(path: &Path, required: bool) -> anyhow::Result<Option<TomlConfig>> {
    use anyhow::Context;

    match std::fs::read_to_string(path) {
        Ok(contents) => {
            let config: TomlConfig = toml::from_str(&contents)
                .context(format!("Failed to parse config file {}", path.display()))?;
            // Warn if config contains a password and file permissions are too open
            #[cfg(unix)]
            if config.auth.as_ref().is_some_and(|a| a.password.is_some()) {
                use std::os::unix::fs::MetadataExt;
                if let Ok(meta) = std::fs::metadata(path) {
                    let mode = meta.mode();
                    if mode & 0o077 != 0 {
                        tracing::warn!(
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
        Err(e) => Err(e).context(format!("Failed to read config file {}", path.display()))?,
    }
}

// ── Application Config ──────────────────────────────────────────────

/// Which library (or libraries) to sync.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LibrarySelection {
    /// A single named library (e.g. "`PrimarySync`", "SharedSync-ABCD1234").
    Single(String),
    /// All available libraries (primary + private + shared).
    All,
}

/// Resolve library selection from CLI flag > TOML config > default (`PrimarySync`).
pub(crate) fn resolve_library_selection(
    cli_library: Option<String>,
    toml_filters: Option<&TomlFilters>,
) -> LibrarySelection {
    let library_str = cli_library
        .or_else(|| toml_filters.and_then(|f| f.library.clone()))
        .unwrap_or_else(|| "PrimarySync".to_string());
    if library_str.eq_ignore_ascii_case("all") {
        LibrarySelection::All
    } else {
        LibrarySelection::Single(library_str)
    }
}

impl std::fmt::Display for LibrarySelection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Single(name) => f.write_str(name),
            Self::All => f.write_str("all"),
        }
    }
}

/// Which albums to sync.
///
/// `All` is triggered by explicit `-a all` *or* the smart default: no `-a`
/// flag passed *and* `{album}` appears in `--folder-structure`. In both
/// cases, every discovered album is enumerated; an additional library-wide
/// pass for "unfiled" photos is only added when `{album}` is in the template
/// (decided at `resolve_albums` time, not stored here).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AlbumSelection {
    /// No `-a` filter; enumerate the library as a single stream (today's
    /// default behaviour).
    LibraryOnly,
    /// Explicit list of album names to sync.
    Named(Vec<String>),
    /// `-a all` (explicit) or the smart default when `{album}` is in the
    /// folder template: every discovered album.
    All,
}

impl AlbumSelection {
    /// Serialize to a `Vec<String>` for TOML persistence and JSON reports.
    pub fn to_vec(&self) -> Vec<String> {
        match self {
            Self::LibraryOnly => Vec::new(),
            Self::All => vec!["all".to_string()],
            Self::Named(v) => v.clone(),
        }
    }
}

impl std::fmt::Display for AlbumSelection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::LibraryOnly => f.write_str("<library-only>"),
            Self::All => f.write_str("all"),
            Self::Named(names) => f.write_str(&names.join(", ")),
        }
    }
}

/// Reject `--folder-structure` values that place `{album}` somewhere other
/// than the first path segment, or use it more than once. Both cases would
/// make the "unfiled photos" fallback path shift other segments around
/// unpredictably when `{album}` collapses to an empty string.
fn validate_folder_structure(folder_structure: &str) -> anyhow::Result<()> {
    let stripped = crate::download::paths::strip_python_wrapper(folder_structure);
    let count = stripped.matches("{album}").count();
    if count == 0 {
        return Ok(());
    }
    if count > 1 {
        anyhow::bail!(
            "'{{album}}' may only appear once in --folder-structure; got {count} occurrences in \"{folder_structure}\""
        );
    }
    if stripped.split('/').next() != Some("{album}") {
        anyhow::bail!(
            "'{{album}}' must be the first path segment of --folder-structure; got \"{folder_structure}\""
        );
    }
    Ok(())
}

/// Convert a raw `Vec<String>` (from CLI or TOML) into an [`AlbumSelection`],
/// enforcing that `-a all` is not mixed with specific album names.
fn resolve_album_selection(
    raw: Vec<String>,
    folder_structure: &str,
) -> anyhow::Result<AlbumSelection> {
    let has_all = raw.iter().any(|s| s.eq_ignore_ascii_case("all"));
    if has_all {
        let non_all: Vec<&String> = raw
            .iter()
            .filter(|s| !s.eq_ignore_ascii_case("all"))
            .collect();
        anyhow::ensure!(
            non_all.is_empty(),
            "'-a all' cannot be combined with other album names; got {raw:?}. \
             Pass either '-a all' alone or a list of specific names."
        );
        return Ok(AlbumSelection::All);
    }
    if raw.is_empty() {
        // Smart default: bare `{album}` in the folder template implies
        // "every album, plus an unfiled pass" without the user having to
        // also pass `-a all`.
        if crate::download::paths::strip_python_wrapper(folder_structure).contains("{album}") {
            return Ok(AlbumSelection::All);
        }
        return Ok(AlbumSelection::LibraryOnly);
    }
    Ok(AlbumSelection::Named(raw))
}

/// Application configuration.
///
/// Fields are ordered for optimal memory layout:
/// - Heap types first (String, `PathBuf`, Vec, `Option<String>`)
/// - `DateTime` fields (12-16 bytes each)
/// - 8-byte primitives (u64, `Option<u64>`)
/// - 4-byte primitives (u32, `Option<u32>`)
/// - 2-byte primitives (u16)
/// - 1-byte enums
/// - All booleans grouped at the end
pub struct Config {
    // Heap types first
    pub username: String,
    pub password: Option<SecretString>,
    pub password_file: Option<PathBuf>,
    pub password_command: Option<String>,
    pub directory: PathBuf,
    pub cookie_directory: PathBuf,
    pub folder_structure: String,
    pub albums: AlbumSelection,
    pub exclude_albums: Vec<String>,
    pub filename_exclude: Vec<glob::Pattern>,
    pub library: LibrarySelection,
    pub temp_suffix: String,

    // DateTime fields
    pub skip_created_before: Option<DateTime<Local>>,
    pub skip_created_after: Option<DateTime<Local>>,

    // Optional paths
    pub pid_file: Option<PathBuf>,
    pub notification_script: Option<PathBuf>,
    pub report_json: Option<PathBuf>,

    // 8-byte primitives
    pub watch_with_interval: Option<u64>,
    pub retry_delay_secs: u64,

    // 4-byte primitives
    pub recent: Option<u32>,
    pub max_retries: u32,

    // 8-byte primitives (cont.)
    pub bandwidth_limit: Option<u64>,

    // 2-byte primitives
    pub threads_num: u16,
    pub metrics_port: Option<u16>,

    // 1-byte enums
    pub size: VersionSize,
    pub live_photo_size: LivePhotoSize,
    pub domain: Domain,
    pub live_photo_mode: LivePhotoMode,
    pub live_photo_mov_filename_policy: LivePhotoMovFilenamePolicy,
    pub align_raw: RawTreatmentPolicy,
    pub file_match_policy: FileMatchPolicy,

    // All booleans grouped together
    pub skip_videos: bool,
    pub skip_photos: bool,
    pub force_size: bool,
    pub set_exif_datetime: bool,
    pub set_exif_rating: bool,
    pub set_exif_gps: bool,
    pub set_exif_description: bool,
    pub embed_xmp: bool,
    pub xmp_sidecar: bool,
    pub dry_run: bool,
    pub no_progress_bar: bool,
    pub keep_unicode_in_filenames: bool,
    pub only_print_filenames: bool,
    pub no_incremental: bool,
    pub notify_systemd: bool,
    pub save_password: bool,
}

impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config")
            .field("username", &self.username)
            .field("password", &"<redacted>")
            .field("directory", &self.directory)
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

/// Reject system directories that should never be used as a download target.
fn validate_directory(path: &Path) -> anyhow::Result<()> {
    const DENIED: &[&str] = &[
        "/bin", "/sbin", "/usr", "/etc", "/dev", "/proc", "/sys", "/boot", "/lib", "/lib64",
        "/var", "/root",
    ];
    let s = path.to_string_lossy();
    let trimmed = s.trim_end_matches('/');
    // trimmed.is_empty() catches "/" (trimmed to "")
    if trimmed.is_empty() || DENIED.contains(&trimmed) {
        anyhow::bail!(
            "Refusing to use system directory '{}' as download directory",
            path.display()
        );
    }
    Ok(())
}

/// Pick CLI value, then TOML value, then hardcoded default.
fn resolve<T>(cli: Option<T>, toml: Option<T>, default: T) -> T {
    cli.or(toml).unwrap_or(default)
}

/// Same as `resolve`, but takes references so callers don't clone both
/// sources before choosing the winner. Only the chosen value is cloned.
/// Prefer this for owned types (`String`, `Vec<_>`) where the `resolve`
/// version would double-allocate; for `Copy` types the two are equivalent.
fn resolve_ref<T: Clone>(cli: Option<&T>, toml: Option<&T>, default: T) -> T {
    cli.or(toml).cloned().unwrap_or(default)
}

/// For boolean flags: CLI explicit value wins, then TOML, then false.
/// `Option<bool>` allows the CLI to explicitly pass `--flag false` to
/// override a TOML `true`.
fn resolve_flag(cli_flag: Option<bool>, toml_val: Option<bool>) -> bool {
    cli_flag.or(toml_val).unwrap_or(false)
}

/// Global CLI args needed by `resolve_auth` and `Config::build`.
///
/// Bundles the fields that moved from per-command `AuthArgs` to
/// global options on `Cli`.
pub(crate) struct GlobalArgs {
    pub username: Option<String>,
    pub domain: Option<Domain>,
    pub data_dir: Option<String>,
    pub cookie_directory: Option<String>,
}

impl GlobalArgs {
    pub fn from_cli(cli: &crate::cli::Cli) -> Self {
        Self {
            username: cli.username.clone(),
            domain: cli.domain,
            data_dir: cli.data_dir.clone(),
            cookie_directory: cli.cookie_directory.clone(),
        }
    }
}

/// Resolve auth fields from global CLI args + password args + optional TOML config.
/// Returns (username, password, domain, `cookie_directory`).
pub(crate) fn resolve_auth(
    globals: &GlobalArgs,
    password_args: &crate::cli::PasswordArgs,
    toml: Option<&TomlConfig>,
) -> (String, Option<String>, Domain, PathBuf) {
    let toml_auth = toml.and_then(|t| t.auth.as_ref());

    let username = resolve_ref(
        globals.username.as_ref(),
        toml_auth.and_then(|a| a.username.as_ref()),
        String::new(),
    );

    let password = password_args
        .password
        .clone()
        .or_else(|| toml_auth.and_then(|a| a.password.clone()));

    let domain = resolve(
        globals.domain,
        toml_auth.and_then(|a| a.domain),
        Domain::Com,
    );

    let has_explicit_data_dir = globals.data_dir.is_some()
        || globals.cookie_directory.is_some()
        || toml.and_then(|t| t.data_dir.as_ref()).is_some()
        || toml_auth
            .and_then(|a| a.cookie_directory.as_ref())
            .is_some();
    let cookie_directory = if has_explicit_data_dir {
        let default_config = expand_tilde("~/.config/kei/config.toml");
        resolve_data_dir(
            globals.data_dir.as_deref(),
            globals.cookie_directory.as_deref(),
            toml,
            &default_config,
        )
    } else {
        expand_tilde("~/.config/kei/cookies")
    };

    (username, password, domain, cookie_directory)
}

/// Resolve the data directory (sessions, state DB, credentials, health).
///
/// Resolution order:
/// 1. Explicit `--data-dir` CLI flag
/// 2. Legacy `--cookie-directory` CLI flag (deprecated, warns)
/// 3. TOML top-level `data_dir`
/// 4. TOML `[auth] cookie_directory` (deprecated, warns)
/// 5. Default: parent of the resolved config file path
pub(crate) fn resolve_data_dir(
    data_dir_cli: Option<&str>,
    cookie_directory_cli: Option<&str>,
    toml: Option<&TomlConfig>,
    config_path: &Path,
) -> PathBuf {
    if let Some(d) = data_dir_cli {
        return expand_tilde(d);
    }
    if let Some(d) = cookie_directory_cli {
        #[allow(
            clippy::print_stderr,
            reason = "runs during config load, before tracing subscriber is installed"
        )]
        {
            eprintln!("warning: `--cookie-directory` is deprecated, use `--data-dir` instead");
        }
        return expand_tilde(d);
    }
    if let Some(d) = toml.and_then(|t| t.data_dir.as_deref()) {
        return expand_tilde(d);
    }
    if let Some(d) = toml
        .and_then(|t| t.auth.as_ref())
        .and_then(|a| a.cookie_directory.as_deref())
    {
        #[allow(
            clippy::print_stderr,
            reason = "runs during config load, before tracing subscriber is installed"
        )]
        {
            eprintln!(
                "warning: `[auth] cookie_directory` is deprecated, use top-level `data_dir` instead"
            );
        }
        return expand_tilde(d);
    }
    // Default: parent of config file path
    config_path.parent().map_or_else(
        || expand_tilde("~/.config/kei"),
        std::path::Path::to_path_buf,
    )
}

/// Resolve `password_file` from CLI + TOML.
pub(crate) fn resolve_password_file(
    pw: &crate::cli::PasswordArgs,
    toml_auth: Option<&TomlAuth>,
) -> Option<PathBuf> {
    pw.password_file
        .as_deref()
        .or_else(|| toml_auth.and_then(|a| a.password_file.as_deref()))
        .map(expand_tilde)
}

/// Resolve `password_command` from CLI + TOML.
pub(crate) fn resolve_password_command(
    pw: &crate::cli::PasswordArgs,
    toml_auth: Option<&TomlAuth>,
) -> Option<String> {
    pw.password_command
        .clone()
        .or_else(|| toml_auth.and_then(|a| a.password_command.clone()))
}

impl Config {
    /// Build a Config by merging CLI args with optional TOML config.
    /// Resolution order: CLI > TOML > hardcoded default.
    pub fn build(
        globals: &GlobalArgs,
        pw: crate::cli::PasswordArgs,
        sync: crate::cli::SyncArgs,
        toml: Option<TomlConfig>,
    ) -> anyhow::Result<Self> {
        let toml_auth = toml.as_ref().and_then(|t| t.auth.as_ref());
        let (username, password_str, domain, cookie_directory) =
            resolve_auth(globals, &pw, toml.as_ref());
        let password_file = resolve_password_file(&pw, toml_auth);
        let password_command = resolve_password_command(&pw, toml_auth);
        let save_password = sync.save_password;

        // Reject explicitly provided empty username/password (CLI value_parser
        // catches the CLI case; this catches empty strings from TOML).
        if globals.username.is_some()
            || toml
                .as_ref()
                .and_then(|t| t.auth.as_ref()?.username.as_ref())
                .is_some()
        {
            anyhow::ensure!(!username.is_empty(), "username must not be empty");
        }
        if let Some(pw_str) = &password_str {
            anyhow::ensure!(!pw_str.is_empty(), "password must not be empty");
        }

        // Reject multiple password sources in TOML (CLI enforces this via
        // conflicts_with, but TOML has no such mechanism).
        if let Some(toml_a) = toml_auth {
            let sources = [
                toml_a.password.is_some(),
                toml_a.password_file.is_some(),
                toml_a.password_command.is_some(),
            ];
            anyhow::ensure!(
                sources.iter().filter(|&&s| s).count() <= 1,
                "config file sets multiple password sources (password, password_file, \
                 password_command) — pick one"
            );
        }

        // Convert plain password string to SecretString
        let password = password_str.map(SecretString::from);

        // Validate cookie directory early: check that the path is usable
        // (exists or can be created) so we fail with a clear message rather
        // than erroring deep in auth setup.
        if let Some(existing) = cookie_directory.ancestors().find(|a| a.exists()) {
            anyhow::ensure!(
                existing.is_dir(),
                "cookie directory path contains a non-directory component: {}",
                existing.display()
            );
        }
        std::fs::create_dir_all(&cookie_directory).map_err(|e| {
            anyhow::anyhow!(
                "cannot create cookie directory {}: {e}",
                cookie_directory.display()
            )
        })?;

        let toml_dl = toml.as_ref().and_then(|t| t.download.as_ref());
        let toml_retry = toml_dl.and_then(|d| d.retry.as_ref());
        let toml_filters = toml.as_ref().and_then(|t| t.filters.as_ref());
        let toml_photos = toml.as_ref().and_then(|t| t.photos.as_ref());
        let toml_watch = toml.as_ref().and_then(|t| t.watch.as_ref());
        let toml_metrics = toml.as_ref().and_then(|t| t.metrics.as_ref());

        // Download
        let directory = sync
            .directory
            .or_else(|| toml_dl.and_then(|d| d.directory.clone()))
            .map(|d| expand_tilde(&d))
            .unwrap_or_default();
        if !directory.as_os_str().is_empty() {
            validate_directory(&directory)?;
        }
        let folder_structure = resolve(
            sync.folder_structure,
            toml_dl.and_then(|d| d.folder_structure.clone()),
            "%Y/%m/%d".to_string(),
        );
        validate_folder_structure(&folder_structure)?;
        // Resolve bandwidth limit (CLI bytes/sec > TOML human-readable string > None).
        let bandwidth_limit: Option<u64> = if let Some(n) = sync.bandwidth_limit {
            Some(n)
        } else if let Some(s) = toml_dl.and_then(|d| d.bandwidth_limit.as_ref()) {
            Some(crate::cli::parse_bandwidth_limit(s).map_err(|e| {
                anyhow::anyhow!("invalid [download].bandwidth_limit in config: {e}")
            })?)
        } else {
            None
        };

        // When a bandwidth limit is set without an explicit --threads-num,
        // default concurrency to 1: many connections starving for a capped
        // total budget just fragments downloads and adds connection overhead.
        let threads_explicitly_set =
            sync.threads_num.is_some() || toml_dl.and_then(|d| d.threads_num).is_some();
        let threads_default = if bandwidth_limit.is_some() && !threads_explicitly_set {
            1
        } else {
            10
        };
        let threads_num = resolve(
            sync.threads_num,
            toml_dl.and_then(|d| d.threads_num),
            threads_default,
        );
        anyhow::ensure!(
            (1..=64).contains(&threads_num),
            "threads_num must be in 1..=64, got {threads_num}"
        );
        let temp_suffix = resolve(
            sync.temp_suffix,
            toml_dl.and_then(|d| d.temp_suffix.clone()),
            ".kei-tmp".to_string(),
        );
        let set_exif_datetime = resolve_flag(
            sync.set_exif_datetime,
            toml_dl.and_then(|d| d.set_exif_datetime),
        );
        let set_exif_rating = resolve_flag(
            sync.set_exif_rating,
            toml_dl.and_then(|d| d.set_exif_rating),
        );
        let set_exif_gps = resolve_flag(sync.set_exif_gps, toml_dl.and_then(|d| d.set_exif_gps));
        let set_exif_description = resolve_flag(
            sync.set_exif_description,
            toml_dl.and_then(|d| d.set_exif_description),
        );
        let embed_xmp = resolve_flag(sync.embed_xmp, toml_dl.and_then(|d| d.embed_xmp));
        let xmp_sidecar = resolve_flag(sync.xmp_sidecar, toml_dl.and_then(|d| d.xmp_sidecar));
        let no_progress_bar = resolve_flag(
            sync.no_progress_bar,
            toml_dl.and_then(|d| d.no_progress_bar),
        );

        // Re-validate; clap range attrs run on CLI only.
        let max_retries = resolve(sync.max_retries, toml_retry.and_then(|r| r.max_retries), 3);
        anyhow::ensure!(
            max_retries <= 100,
            "retry max_retries must be <= 100, got {max_retries}"
        );
        let retry_delay_secs = resolve(sync.retry_delay, toml_retry.and_then(|r| r.delay), 5);
        anyhow::ensure!(
            (1..=3600).contains(&retry_delay_secs),
            "retry delay must be in 1..=3600 seconds, got {retry_delay_secs}"
        );

        // Filters
        let library = resolve_library_selection(sync.library, toml_filters);
        let raw_albums = if sync.albums.is_empty() {
            toml_filters
                .and_then(|f| f.albums.clone())
                .unwrap_or_default()
        } else {
            sync.albums
        };
        let albums = resolve_album_selection(raw_albums, &folder_structure)?;
        let skip_videos = resolve_flag(sync.skip_videos, toml_filters.and_then(|f| f.skip_videos));
        let skip_photos = resolve_flag(sync.skip_photos, toml_filters.and_then(|f| f.skip_photos));
        // Resolve live photo mode: --live-photo-mode > --skip-live-photos > TOML photos > TOML filters compat
        let live_photo_mode = if let Some(mode) = sync.live_photo_mode {
            mode
        } else if sync.skip_live_photos == Some(true) {
            crate::cli::deprecation_warning("--skip-live-photos", "--live-photo-mode skip");
            LivePhotoMode::Skip
        } else if let Some(mode) = toml_photos.and_then(|p| p.live_photo_mode) {
            mode
        } else if toml_filters.and_then(|f| f.skip_live_photos) == Some(true) {
            LivePhotoMode::Skip
        } else {
            LivePhotoMode::Both
        };
        let exclude_albums = if sync.exclude_albums.is_empty() {
            toml_filters
                .and_then(|f| f.exclude_albums.clone())
                .unwrap_or_default()
        } else {
            sync.exclude_albums
        };
        let filename_exclude_strs = if sync.filename_exclude.is_empty() {
            toml_filters
                .and_then(|f| f.filename_exclude.clone())
                .unwrap_or_default()
        } else {
            sync.filename_exclude
        };
        // Compile glob patterns once during build
        let filename_exclude: Vec<glob::Pattern> = filename_exclude_strs
            .iter()
            .map(|p| {
                glob::Pattern::new(p)
                    .map_err(|e| anyhow::anyhow!("invalid --filename-exclude pattern '{p}': {e}"))
            })
            .collect::<anyhow::Result<_>>()?;
        let recent = sync.recent.or_else(|| toml_filters.and_then(|f| f.recent));
        if recent == Some(0) {
            anyhow::bail!("recent must be >= 1 (got 0)");
        }
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

        if let (Some(before), Some(after)) = (&skip_created_before, &skip_created_after) {
            if before >= after {
                tracing::warn!(
                    before = %before.format("%Y-%m-%d"),
                    after = %after.format("%Y-%m-%d"),
                    "skip-created-before >= skip-created-after, no assets can match",
                );
            }
        }

        // Photos
        let size = resolve(
            sync.size,
            toml_photos.and_then(|p| p.size),
            VersionSize::Original,
        );
        // When --size adjusted and live-photo-size is not explicitly set,
        // default to adjusted live photo companions too.
        let default_live_photo_size = if size == VersionSize::Adjusted {
            LivePhotoSize::Adjusted
        } else {
            LivePhotoSize::Original
        };
        let live_photo_size = resolve(
            sync.live_photo_size,
            toml_photos.and_then(|p| p.live_photo_size),
            default_live_photo_size,
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
        if let Some(n) = watch_with_interval {
            anyhow::ensure!(
                (60..=86400).contains(&n),
                "watch interval must be in 60..=86400 seconds, got {n}"
            );
        }
        let notify_systemd = resolve_flag(
            sync.notify_systemd,
            toml_watch.and_then(|w| w.notify_systemd),
        );
        let pid_file = sync.pid_file.or_else(|| {
            toml_watch
                .and_then(|w| w.pid_file.as_ref())
                .map(PathBuf::from)
        });

        // Notifications
        let toml_notif = toml.as_ref().and_then(|t| t.notifications.as_ref());
        let notification_script = sync
            .notification_script
            .or_else(|| toml_notif.and_then(|n| n.script.clone()))
            .map(|s| expand_tilde(&s));

        // JSON report
        let report_json = sync.report_json;

        // Prometheus metrics port — CLI takes precedence over TOML.
        let metrics_port = sync
            .metrics_port
            .or_else(|| toml_metrics.and_then(|m| m.port));

        if skip_videos && skip_photos && live_photo_mode == LivePhotoMode::Skip {
            tracing::warn!(
                "All media types are being skipped (--skip-videos, --skip-photos, \
                 --live-photo-mode skip) -- nothing will be downloaded"
            );
        }

        Ok(Self {
            username,
            password,
            password_file,
            password_command,
            directory,
            cookie_directory,
            folder_structure,
            albums,
            exclude_albums,
            filename_exclude,
            library,
            temp_suffix,
            skip_created_before,
            skip_created_after,
            pid_file,
            notification_script,
            report_json,
            metrics_port,
            watch_with_interval,
            retry_delay_secs,
            recent,
            max_retries,
            bandwidth_limit,
            threads_num,
            size,
            live_photo_size,
            domain,
            live_photo_mode,
            live_photo_mov_filename_policy,
            align_raw,
            file_match_policy,
            skip_videos,
            skip_photos,
            force_size,
            set_exif_datetime,
            set_exif_rating,
            set_exif_gps,
            set_exif_description,
            embed_xmp,
            xmp_sidecar,
            dry_run: sync.dry_run,
            no_progress_bar,
            keep_unicode_in_filenames,
            only_print_filenames: sync.only_print_filenames,
            no_incremental: sync.no_incremental,
            notify_systemd,
            save_password,
        })
    }

    /// Convert the resolved config back to a [`TomlConfig`] for serialization.
    ///
    /// Only includes static fields suitable for persistence. Passwords are
    /// never included. Per-run flags (`dry_run`, `recent`, etc.) are omitted.
    pub(crate) fn to_toml(&self) -> TomlConfig {
        let library_str = match &self.library {
            LibrarySelection::Single(name) if name == "PrimarySync" => None,
            LibrarySelection::Single(name) => Some(name.clone()),
            LibrarySelection::All => Some("all".to_string()),
        };

        TomlConfig {
            data_dir: None,  // derived from config path, not serialized unless explicit
            log_level: None, // only written if user explicitly set it
            auth: Some(TomlAuth {
                username: if self.username.is_empty() {
                    None
                } else {
                    Some(self.username.clone())
                },
                password: None, // never persist
                password_file: self.password_file.as_ref().map(|p| p.display().to_string()),
                password_command: self.password_command.clone(),
                domain: if self.domain == Domain::Com {
                    None
                } else {
                    Some(self.domain)
                },
                cookie_directory: None, // deprecated, use data_dir
            }),
            download: Some(TomlDownload {
                directory: if self.directory.as_os_str().is_empty() {
                    None
                } else {
                    Some(self.directory.display().to_string())
                },
                folder_structure: Some(self.folder_structure.clone()),
                threads_num: Some(self.threads_num),
                bandwidth_limit: self.bandwidth_limit.map(|n| n.to_string()),
                temp_suffix: if self.temp_suffix == ".kei-tmp" {
                    None
                } else {
                    Some(self.temp_suffix.clone())
                },
                set_exif_datetime: if self.set_exif_datetime {
                    Some(true)
                } else {
                    None
                },
                set_exif_rating: if self.set_exif_rating {
                    Some(true)
                } else {
                    None
                },
                set_exif_gps: if self.set_exif_gps { Some(true) } else { None },
                set_exif_description: if self.set_exif_description {
                    Some(true)
                } else {
                    None
                },
                embed_xmp: if self.embed_xmp { Some(true) } else { None },
                xmp_sidecar: if self.xmp_sidecar { Some(true) } else { None },
                no_progress_bar: if self.no_progress_bar {
                    Some(true)
                } else {
                    None
                },
                retry: Some(TomlRetry {
                    max_retries: Some(self.max_retries),
                    delay: Some(self.retry_delay_secs),
                }),
            }),
            filters: Some(TomlFilters {
                library: library_str,
                albums: match &self.albums {
                    AlbumSelection::LibraryOnly => None,
                    AlbumSelection::All => Some(vec!["all".to_string()]),
                    AlbumSelection::Named(v) => Some(v.clone()),
                },
                exclude_albums: if self.exclude_albums.is_empty() {
                    None
                } else {
                    Some(self.exclude_albums.clone())
                },
                filename_exclude: if self.filename_exclude.is_empty() {
                    None
                } else {
                    Some(
                        self.filename_exclude
                            .iter()
                            .map(|p| p.as_str().to_string())
                            .collect(),
                    )
                },
                skip_videos: if self.skip_videos { Some(true) } else { None },
                skip_photos: if self.skip_photos { Some(true) } else { None },
                skip_live_photos: None, // deprecated, use live_photo_mode in [photos]
                recent: None,           // per-run
                skip_created_before: None, // per-run
                skip_created_after: None, // per-run
            }),
            photos: Some(TomlPhotos {
                size: if self.size == VersionSize::Original {
                    None
                } else {
                    Some(self.size)
                },
                live_photo_size: if self.live_photo_size == LivePhotoSize::Original {
                    None
                } else {
                    Some(self.live_photo_size)
                },
                live_photo_mode: if self.live_photo_mode == LivePhotoMode::Both {
                    None
                } else {
                    Some(self.live_photo_mode)
                },
                live_photo_mov_filename_policy: if self.live_photo_mov_filename_policy
                    == LivePhotoMovFilenamePolicy::Suffix
                {
                    None
                } else {
                    Some(self.live_photo_mov_filename_policy)
                },
                align_raw: if self.align_raw == RawTreatmentPolicy::Unchanged {
                    None
                } else {
                    Some(self.align_raw)
                },
                file_match_policy: if self.file_match_policy
                    == FileMatchPolicy::NameSizeDedupWithSuffix
                {
                    None
                } else {
                    Some(self.file_match_policy)
                },
                force_size: if self.force_size { Some(true) } else { None },
                keep_unicode_in_filenames: if self.keep_unicode_in_filenames {
                    Some(true)
                } else {
                    None
                },
            }),
            watch: if self.watch_with_interval.is_some()
                || self.notify_systemd
                || self.pid_file.is_some()
            {
                Some(TomlWatch {
                    interval: self.watch_with_interval,
                    notify_systemd: if self.notify_systemd {
                        Some(true)
                    } else {
                        None
                    },
                    pid_file: self.pid_file.as_ref().map(|p| p.display().to_string()),
                })
            } else {
                None
            },
            notifications: self
                .notification_script
                .as_ref()
                .map(|s| TomlNotifications {
                    script: Some(s.display().to_string()),
                }),
            metrics: self
                .metrics_port
                .map(|port| TomlMetrics { port: Some(port) }),
        }
    }
}

/// Persist a minimal config file on first run.
///
/// Converts the resolved [`Config`] to TOML via [`Config::to_toml()`], then
/// strips it down to only the essential no-default fields (username, directory,
/// data-dir, domain, password-file, password-command). Passwords are never
/// included. No-ops if a config file already exists, the parent directory
/// doesn't exist, or `KEI_NO_AUTO_CONFIG=1` is set.
pub(crate) fn persist_first_run_config(
    config_path: &Path,
    config: &Config,
    data_dir_cli: Option<&str>,
) -> anyhow::Result<()> {
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
        data_dir,
        log_level: None,
        auth: full.auth.map(|a| TomlAuth {
            username: a.username,
            password: None, // never persist
            password_file: a.password_file,
            password_command: a.password_command,
            domain: a.domain,
            cookie_directory: None, // deprecated, use data_dir
        }),
        download: full.download.map(|d| TomlDownload {
            directory: d.directory,
            folder_structure: None,
            threads_num: None,
            bandwidth_limit: None,
            temp_suffix: None,
            set_exif_datetime: None,
            set_exif_rating: None,
            set_exif_gps: None,
            set_exif_description: None,
            embed_xmp: None,
            xmp_sidecar: None,
            no_progress_bar: None,
            retry: None,
        }),
        filters: None,
        photos: None,
        watch: None,
        notifications: None,
        metrics: None,
    };

    // Don't write if there's nothing meaningful to persist
    let has_content =
        minimal.auth.is_some() || minimal.download.is_some() || minimal.data_dir.is_some();
    if !has_content {
        return Ok(());
    }

    let content = toml::to_string_pretty(&minimal)
        .map_err(|e| anyhow::anyhow!("failed to serialize config: {e}"))?;

    let output = format!("# Generated by kei on first run. Edit freely.\n\n{content}");
    std::fs::write(config_path, &output)?;

    // Restrict permissions on Unix
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(config_path, std::fs::Permissions::from_mode(0o600))?;
    }

    tracing::info!(path = %config_path.display(), "Saved configuration for future runs");
    Ok(())
}

/// Parse a human-friendly date spec into a concrete timestamp.
///
/// Supports three formats to match the Python CLI's behavior:
/// - Relative interval: `"20d"` (20 days ago from now)
/// - ISO date: `"2025-01-02"` (midnight local time)
/// - ISO datetime: `"2025-01-02T14:30:00"` (local time)
pub(crate) fn parse_date_or_interval(s: &str) -> anyhow::Result<DateTime<Local>> {
    if let Some(days_str) = s.strip_suffix('d') {
        if let Ok(days) = days_str.parse::<u64>() {
            let days =
                i64::try_from(days).map_err(|_| anyhow::anyhow!("interval '{s}' is too large"))?;
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
        "Cannot parse '{s}' as a date. Expected ISO date (2025-01-02), \
         datetime (2025-01-02T14:30:00), or interval (20d)"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::SyncArgs;
    use secrecy::ExposeSecret;

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

    #[test]
    fn test_parse_negative_interval_rejected() {
        assert!(parse_date_or_interval("-5d").is_err());
        assert!(parse_date_or_interval("-1d").is_err());
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
            cookie_directory = "~/.config/kei/cookies"

            [download]
            directory = "/photos"
            folder_structure = "%Y/%m/%d"
            threads_num = 10
            temp_suffix = ".kei-tmp"
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
            pid_file = "/run/kei.pid"
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
            err.contains("Failed to read config file"),
            "Error should mention config file: {err}"
        );
    }

    // ── Config::build tests ─────────────────────────────────────────

    fn default_globals() -> GlobalArgs {
        GlobalArgs {
            username: Some("u@example.com".to_string()),
            domain: None,
            data_dir: None,
            cookie_directory: None,
        }
    }

    fn default_password() -> crate::cli::PasswordArgs {
        crate::cli::PasswordArgs::default()
    }

    fn default_sync() -> SyncArgs {
        SyncArgs::default()
    }

    #[test]
    fn test_build_defaults_no_toml() {
        let cfg =
            Config::build(&default_globals(), default_password(), default_sync(), None).unwrap();
        assert_eq!(cfg.username, "u@example.com");
        assert_eq!(cfg.threads_num, 10);
        assert_eq!(cfg.folder_structure, "%Y/%m/%d");
        assert_eq!(
            cfg.library,
            LibrarySelection::Single("PrimarySync".to_string())
        );
        assert_eq!(cfg.max_retries, 3);
        assert_eq!(cfg.retry_delay_secs, 5);
        assert_eq!(cfg.temp_suffix, ".kei-tmp");
        assert!(matches!(cfg.size, VersionSize::Original));
        assert!(matches!(cfg.domain, Domain::Com));
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
        let cfg = Config::build(
            &default_globals(),
            default_password(),
            default_sync(),
            Some(toml),
        )
        .unwrap();
        assert_eq!(cfg.threads_num, 4);
        assert_eq!(cfg.folder_structure, "%Y-%m");
        assert_eq!(
            cfg.library,
            LibrarySelection::Single("SharedSync-ABC".to_string())
        );
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

        let cfg = Config::build(&default_globals(), default_password(), sync, Some(toml)).unwrap();
        assert_eq!(cfg.threads_num, 8);
        assert_eq!(
            cfg.library,
            LibrarySelection::Single("PrimarySync".to_string())
        );
    }

    #[test]
    fn test_library_all_value() {
        let mut sync = default_sync();
        sync.library = Some("all".to_string());
        let cfg = Config::build(&default_globals(), default_password(), sync, None).unwrap();
        assert_eq!(cfg.library, LibrarySelection::All);
    }

    #[test]
    fn test_library_all_case_insensitive() {
        let mut sync = default_sync();
        sync.library = Some("ALL".to_string());
        let cfg = Config::build(&default_globals(), default_password(), sync, None).unwrap();
        assert_eq!(cfg.library, LibrarySelection::All);
    }

    #[test]
    fn test_library_all_from_toml() {
        let toml_str = r#"
            [filters]
            library = "all"
        "#;
        let toml: TomlConfig = toml::from_str(toml_str).unwrap();
        let cfg = Config::build(
            &default_globals(),
            default_password(),
            default_sync(),
            Some(toml),
        )
        .unwrap();
        assert_eq!(cfg.library, LibrarySelection::All);
    }

    #[test]
    fn test_build_hardcoded_default_when_both_absent() {
        let cfg =
            Config::build(&default_globals(), default_password(), default_sync(), None).unwrap();
        assert_eq!(cfg.threads_num, 10);
        assert!(matches!(cfg.align_raw, RawTreatmentPolicy::Unchanged));
    }

    #[test]
    fn test_build_bandwidth_limit_resolution() {
        struct Case {
            name: &'static str,
            cli: Option<u64>,
            toml_cli_threads: Option<u16>,
            toml: Option<&'static str>,
            toml_threads: Option<u16>,
            want_limit: Option<u64>,
            want_threads: u16,
        }
        let cases = [
            Case {
                name: "cli sets limit, threads defaults to 1",
                cli: Some(5_000_000),
                toml_cli_threads: None,
                toml: None,
                toml_threads: None,
                want_limit: Some(5_000_000),
                want_threads: 1,
            },
            Case {
                name: "toml string parses into u64",
                cli: None,
                toml_cli_threads: None,
                toml: Some("2M"),
                toml_threads: None,
                want_limit: Some(2_000_000),
                want_threads: 1,
            },
            Case {
                name: "cli overrides toml",
                cli: Some(10_000_000),
                toml_cli_threads: None,
                toml: Some("1M"),
                toml_threads: None,
                want_limit: Some(10_000_000),
                want_threads: 1,
            },
            Case {
                name: "explicit cli threads overrides auto-1",
                cli: Some(500_000),
                toml_cli_threads: Some(4),
                toml: None,
                toml_threads: None,
                want_limit: Some(500_000),
                want_threads: 4,
            },
            Case {
                name: "toml threads overrides auto-1",
                cli: None,
                toml_cli_threads: None,
                toml: Some("1M"),
                toml_threads: Some(3),
                want_limit: Some(1_000_000),
                want_threads: 3,
            },
            Case {
                name: "no limit keeps default 10 threads",
                cli: None,
                toml_cli_threads: None,
                toml: None,
                toml_threads: None,
                want_limit: None,
                want_threads: 10,
            },
        ];

        for case in cases {
            let toml = match (case.toml, case.toml_threads) {
                (None, None) => None,
                (limit, threads) => {
                    let mut body = "[download]\n".to_string();
                    if let Some(l) = limit {
                        body.push_str(&format!("bandwidth_limit = \"{l}\"\n"));
                    }
                    if let Some(t) = threads {
                        body.push_str(&format!("threads_num = {t}\n"));
                    }
                    Some(toml::from_str::<TomlConfig>(&body).unwrap())
                }
            };
            let mut sync = default_sync();
            sync.bandwidth_limit = case.cli;
            sync.threads_num = case.toml_cli_threads;
            let cfg = Config::build(&default_globals(), default_password(), sync, toml)
                .unwrap_or_else(|e| panic!("{}: build failed: {e}", case.name));
            assert_eq!(cfg.bandwidth_limit, case.want_limit, "{}", case.name);
            assert_eq!(cfg.threads_num, case.want_threads, "{}", case.name);
        }
    }

    #[test]
    fn test_build_bandwidth_limit_invalid_toml_rejected() {
        let toml_str = r#"
            [download]
            bandwidth_limit = "not_a_value"
        "#;
        let toml: TomlConfig = toml::from_str(toml_str).unwrap();
        let err = Config::build(
            &default_globals(),
            default_password(),
            default_sync(),
            Some(toml),
        )
        .expect_err("invalid bandwidth_limit should fail build");
        assert!(
            err.to_string().contains("bandwidth_limit"),
            "error should mention bandwidth_limit: {err}"
        );
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
        let cfg = Config::build(
            &default_globals(),
            default_password(),
            default_sync(),
            Some(toml),
        )
        .unwrap();
        assert!(cfg.set_exif_datetime);
        assert!(cfg.skip_videos);
    }

    #[test]
    fn test_build_embed_xmp_and_sidecar_from_toml() {
        let toml_str = r#"
            [download]
            embed_xmp = true
            xmp_sidecar = true
        "#;
        let toml: TomlConfig = toml::from_str(toml_str).unwrap();
        let cfg = Config::build(
            &default_globals(),
            default_password(),
            default_sync(),
            Some(toml),
        )
        .unwrap();
        assert!(cfg.embed_xmp);
        assert!(cfg.xmp_sidecar);
    }

    #[test]
    fn test_cli_embed_xmp_overrides_toml() {
        let toml_str = r#"
            [download]
            embed_xmp = true
        "#;
        let toml: TomlConfig = toml::from_str(toml_str).unwrap();
        let mut sync = default_sync();
        sync.embed_xmp = Some(false);
        let cfg = Config::build(&default_globals(), default_password(), sync, Some(toml)).unwrap();
        assert!(
            !cfg.embed_xmp,
            "--embed-xmp=false must override TOML embed_xmp = true"
        );
    }

    #[test]
    fn test_embed_xmp_default_false_when_unset() {
        let cfg =
            Config::build(&default_globals(), default_password(), default_sync(), None).unwrap();
        assert!(!cfg.embed_xmp);
        assert!(!cfg.xmp_sidecar);
    }

    #[test]
    fn test_build_cli_flag_overrides_toml_false() {
        let toml_str = r#"
            [filters]
            skip_videos = false
        "#;
        let toml: TomlConfig = toml::from_str(toml_str).unwrap();
        let mut sync = default_sync();
        sync.skip_videos = Some(true);
        let cfg = Config::build(&default_globals(), default_password(), sync, Some(toml)).unwrap();
        assert!(cfg.skip_videos);
    }

    #[test]
    fn test_build_cli_false_overrides_toml_true() {
        let toml_str = r#"
            [filters]
            skip_videos = true
        "#;
        let toml: TomlConfig = toml::from_str(toml_str).unwrap();
        let mut sync = default_sync();
        sync.skip_videos = Some(false);
        let cfg = Config::build(&default_globals(), default_password(), sync, Some(toml)).unwrap();
        assert!(
            !cfg.skip_videos,
            "CLI --skip-videos false should override TOML true"
        );
    }

    #[test]
    fn test_build_threads_num_zero_from_toml_rejected() {
        let toml_str = r#"
            [download]
            threads_num = 0
        "#;
        let toml: TomlConfig = toml::from_str(toml_str).unwrap();
        let result = Config::build(
            &default_globals(),
            default_password(),
            default_sync(),
            Some(toml),
        );
        assert!(result.is_err());
        assert!(
            result.unwrap_err().to_string().contains("threads_num"),
            "Error should mention threads_num"
        );
    }

    #[test]
    fn test_build_threads_num_above_upper_bound_from_toml_rejected() {
        let toml_str = r#"
            [download]
            threads_num = 128
        "#;
        let toml: TomlConfig = toml::from_str(toml_str).unwrap();
        let result = Config::build(
            &default_globals(),
            default_password(),
            default_sync(),
            Some(toml),
        );
        assert!(result.is_err());
        assert!(
            result.unwrap_err().to_string().contains("threads_num"),
            "Error should mention threads_num"
        );
    }

    #[test]
    fn test_build_watch_interval_above_upper_bound_from_toml_rejected() {
        let toml_str = r#"
            [watch]
            interval = 100000
        "#;
        let toml: TomlConfig = toml::from_str(toml_str).unwrap();
        let result = Config::build(
            &default_globals(),
            default_password(),
            default_sync(),
            Some(toml),
        );
        assert!(result.is_err());
        assert!(
            result.unwrap_err().to_string().contains("watch interval"),
            "Error should mention watch interval"
        );
    }

    #[test]
    fn test_build_toml_auth_username() {
        let toml_str = r#"
            [auth]
            username = "toml@example.com"
        "#;
        let toml: TomlConfig = toml::from_str(toml_str).unwrap();
        let mut globals = default_globals();
        let pw = default_password();
        globals.username = None; // Simulate no CLI username
        let cfg = Config::build(&globals, pw, default_sync(), Some(toml)).unwrap();
        assert_eq!(cfg.username, "toml@example.com");
    }

    #[test]
    fn test_build_cli_auth_overrides_toml_username() {
        let toml_str = r#"
            [auth]
            username = "toml@example.com"
        "#;
        let toml: TomlConfig = toml::from_str(toml_str).unwrap();
        let cfg = Config::build(
            &default_globals(),
            default_password(),
            default_sync(),
            Some(toml),
        )
        .unwrap();
        assert_eq!(cfg.username, "u@example.com");
    }

    #[test]
    fn test_build_toml_albums() {
        let toml_str = r#"
            [filters]
            albums = ["Favorites", "Vacation"]
        "#;
        let toml: TomlConfig = toml::from_str(toml_str).unwrap();
        let cfg = Config::build(
            &default_globals(),
            default_password(),
            default_sync(),
            Some(toml),
        )
        .unwrap();
        assert_eq!(
            cfg.albums,
            AlbumSelection::Named(vec!["Favorites".to_string(), "Vacation".to_string()])
        );
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
        let cfg = Config::build(&default_globals(), default_password(), sync, Some(toml)).unwrap();
        assert_eq!(
            cfg.albums,
            AlbumSelection::Named(vec!["Screenshots".to_string()])
        );
    }

    #[test]
    fn test_build_watch_from_toml() {
        let toml_str = r#"
            [watch]
            interval = 1800
            pid_file = "/run/test.pid"
        "#;
        let toml: TomlConfig = toml::from_str(toml_str).unwrap();
        let cfg = Config::build(
            &default_globals(),
            default_password(),
            default_sync(),
            Some(toml),
        )
        .unwrap();
        assert_eq!(cfg.watch_with_interval, Some(1800));
        assert_eq!(cfg.pid_file, Some(PathBuf::from("/run/test.pid")));
    }

    #[test]
    fn test_build_watch_interval_below_minimum_from_toml_rejected() {
        for interval in [0, 1, 59] {
            let toml_str = format!(
                r#"
                [watch]
                interval = {interval}
            "#
            );
            let toml: TomlConfig = toml::from_str(&toml_str).unwrap();
            let result = Config::build(
                &default_globals(),
                default_password(),
                default_sync(),
                Some(toml),
            );
            assert!(result.is_err(), "interval {interval} should be rejected");
            assert!(
                result.unwrap_err().to_string().contains("watch interval"),
                "Error should mention watch interval"
            );
        }
    }

    #[test]
    fn test_build_retry_delay_zero_from_toml_rejected() {
        let toml_str = r#"
            [download.retry]
            delay = 0
        "#;
        let toml: TomlConfig = toml::from_str(toml_str).unwrap();
        let result = Config::build(
            &default_globals(),
            default_password(),
            default_sync(),
            Some(toml),
        );
        assert!(result.is_err());
        assert!(
            result.unwrap_err().to_string().contains("retry delay"),
            "Error should mention retry delay"
        );
    }

    #[test]
    fn test_build_retry_delay_above_upper_bound_from_toml_rejected() {
        let toml_str = r#"
            [download.retry]
            delay = 86400
        "#;
        let toml: TomlConfig = toml::from_str(toml_str).unwrap();
        let result = Config::build(
            &default_globals(),
            default_password(),
            default_sync(),
            Some(toml),
        );
        assert!(result.is_err(), "TOML delay > 3600 must be rejected");
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("retry delay") && msg.contains("3600"),
            "Error should mention retry delay and the bound: {msg}"
        );
    }

    #[test]
    fn test_build_max_retries_above_upper_bound_from_toml_rejected() {
        let toml_str = r#"
            [download.retry]
            max_retries = 9999
        "#;
        let toml: TomlConfig = toml::from_str(toml_str).unwrap();
        let result = Config::build(
            &default_globals(),
            default_password(),
            default_sync(),
            Some(toml),
        );
        assert!(result.is_err(), "TOML max_retries > 100 must be rejected");
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("max_retries") && msg.contains("100"),
            "Error should mention max_retries and the bound: {msg}"
        );
    }

    #[test]
    fn test_build_retry_clamp_accepts_upper_bound_from_toml() {
        let toml_str = r#"
            [download.retry]
            max_retries = 100
            delay = 3600
        "#;
        let toml: TomlConfig = toml::from_str(toml_str).unwrap();
        let cfg = Config::build(
            &default_globals(),
            default_password(),
            default_sync(),
            Some(toml),
        )
        .expect("max_retries=100, delay=3600 must be accepted");
        assert_eq!(cfg.max_retries, 100);
        assert_eq!(cfg.retry_delay_secs, 3600);
    }

    #[test]
    fn test_build_empty_username_from_toml_rejected() {
        let toml_str = r#"
            [auth]
            username = ""
        "#;
        let toml: TomlConfig = toml::from_str(toml_str).unwrap();
        let mut globals = default_globals();
        let pw = default_password();
        globals.username = None;
        let result = Config::build(&globals, pw, default_sync(), Some(toml));
        assert!(result.is_err());
        assert!(
            result.unwrap_err().to_string().contains("username"),
            "Error should mention username"
        );
    }

    #[test]
    fn test_build_empty_password_from_toml_rejected() {
        let toml_str = r#"
            [auth]
            password = ""
        "#;
        let toml: TomlConfig = toml::from_str(toml_str).unwrap();
        let result = Config::build(
            &default_globals(),
            default_password(),
            default_sync(),
            Some(toml),
        );
        assert!(result.is_err());
        assert!(
            result.unwrap_err().to_string().contains("password"),
            "Error should mention password"
        );
    }

    #[test]
    fn test_build_cookie_directory_under_file_rejected() {
        // Create a regular file, then try to use a path under it as cookie dir
        let dir = tempfile::tempdir().unwrap();
        let tmp = dir.path().join("kei_config_test_cookie_file");
        std::fs::write(&tmp, b"not a dir").unwrap();
        let path = tmp.join("nested").join("cookies");
        let mut globals = default_globals();
        let pw = default_password();
        globals.cookie_directory = Some(path.to_string_lossy().to_string());
        let result = Config::build(&globals, pw, default_sync(), None);
        assert!(result.is_err());
        assert!(
            result.unwrap_err().to_string().contains("cookie directory"),
            "Error should mention cookie directory"
        );
    }

    #[test]
    fn test_build_cookie_directory_nonexistent_rejected() {
        let mut globals = default_globals();
        let pw = default_password();
        // Use a path with a null byte which is invalid on all platforms
        globals.cookie_directory = Some("\0invalid/cookies".to_string());
        let result = Config::build(&globals, pw, default_sync(), None);
        assert!(result.is_err());
    }

    #[test]
    fn test_build_skip_dates_from_toml() {
        let toml_str = r#"
            [filters]
            skip_created_before = "2024-01-01"
            skip_created_after = "2025-01-01"
        "#;
        let toml: TomlConfig = toml::from_str(toml_str).unwrap();
        let cfg = Config::build(
            &default_globals(),
            default_password(),
            default_sync(),
            Some(toml),
        )
        .unwrap();
        assert!(cfg.skip_created_before.is_some());
        assert!(cfg.skip_created_after.is_some());
    }

    // ── TOML enum variant exhaustive tests ─────────────────────────

    #[test]
    fn test_toml_parse_all_size_variants() {
        for (input, expected) in [
            ("original", VersionSize::Original),
            ("medium", VersionSize::Medium),
            ("thumb", VersionSize::Thumb),
            ("adjusted", VersionSize::Adjusted),
            ("alternative", VersionSize::Alternative),
        ] {
            let toml_str = format!("[photos]\nsize = \"{input}\"");
            let config: TomlConfig = toml::from_str(&toml_str).unwrap();
            assert_eq!(
                config.photos.unwrap().size,
                Some(expected),
                "size variant: {input}"
            );
        }
    }

    #[test]
    fn test_toml_parse_all_live_photo_size_variants() {
        for (input, expected) in [
            ("original", LivePhotoSize::Original),
            ("medium", LivePhotoSize::Medium),
            ("thumb", LivePhotoSize::Thumb),
            ("adjusted", LivePhotoSize::Adjusted),
        ] {
            let toml_str = format!("[photos]\nlive_photo_size = \"{input}\"");
            let config: TomlConfig = toml::from_str(&toml_str).unwrap();
            assert_eq!(
                config.photos.unwrap().live_photo_size,
                Some(expected),
                "live_photo_size variant: {input}"
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
    fn test_toml_parse_all_align_raw_variants() {
        for (input, expected) in [
            ("as-is", RawTreatmentPolicy::Unchanged),
            ("original", RawTreatmentPolicy::PreferOriginal),
            ("alternative", RawTreatmentPolicy::PreferAlternative),
        ] {
            let toml_str = format!("[photos]\nalign_raw = \"{input}\"");
            let config: TomlConfig = toml::from_str(&toml_str).unwrap();
            assert_eq!(
                config.photos.unwrap().align_raw,
                Some(expected),
                "align_raw variant: {input}"
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

    // ── TOML invalid values ────────────────────────────────────────

    #[test]
    fn test_toml_reject_invalid_enum_value() {
        let toml_str = r#"
            [photos]
            size = "huge"
        "#;
        assert!(toml::from_str::<TomlConfig>(toml_str).is_err());
    }

    #[test]
    fn test_toml_reject_wrong_type() {
        let toml_str = r#"
            [download]
            threads_num = "not_a_number"
        "#;
        assert!(toml::from_str::<TomlConfig>(toml_str).is_err());
    }

    #[test]
    fn test_toml_reject_negative_number() {
        let toml_str = r#"
            [download]
            threads_num = -1
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

    // ── TOML empty sections ────────────────────────────────────────

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
        assert!(config.download.unwrap().threads_num.is_none());
        assert!(config.filters.unwrap().library.is_none());
        assert!(config.photos.unwrap().size.is_none());
        assert!(config.watch.unwrap().interval.is_none());
        assert!(config.notifications.unwrap().script.is_none());
    }

    // ── TOML individual field parsing ──────────────────────────────

    #[test]
    fn test_toml_auth_password() {
        let toml_str = r#"
            [auth]
            password = "secret"
        "#;
        let config: TomlConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.auth.unwrap().password.as_deref(), Some("secret"));
    }

    #[test]
    fn test_toml_download_all_fields() {
        let toml_str = r#"
            [download]
            directory = "/photos"
            folder_structure = "%Y-%m"
            threads_num = 4
            temp_suffix = ".part"
            set_exif_datetime = true
            no_progress_bar = true
        "#;
        let config: TomlConfig = toml::from_str(toml_str).unwrap();
        let dl = config.download.unwrap();
        assert_eq!(dl.directory.as_deref(), Some("/photos"));
        assert_eq!(dl.folder_structure.as_deref(), Some("%Y-%m"));
        assert_eq!(dl.threads_num, Some(4));
        assert_eq!(dl.temp_suffix.as_deref(), Some(".part"));
        assert_eq!(dl.set_exif_datetime, Some(true));
        assert_eq!(dl.no_progress_bar, Some(true));
    }

    #[test]
    fn test_toml_filters_all_fields() {
        let toml_str = r#"
            [filters]
            library = "SharedSync-ABC"
            albums = ["A", "B"]
            skip_videos = true
            skip_photos = true
            skip_live_photos = true
            recent = 100
            skip_created_before = "2024-01-01"
            skip_created_after = "2025-12-31"
        "#;
        let config: TomlConfig = toml::from_str(toml_str).unwrap();
        let f = config.filters.unwrap();
        assert_eq!(f.library.as_deref(), Some("SharedSync-ABC"));
        assert_eq!(f.albums, Some(vec!["A".to_string(), "B".to_string()]));
        assert_eq!(f.skip_videos, Some(true));
        assert_eq!(f.skip_photos, Some(true));
        assert_eq!(f.skip_live_photos, Some(true));
        assert_eq!(f.recent, Some(100));
        assert_eq!(f.skip_created_before.as_deref(), Some("2024-01-01"));
        assert_eq!(f.skip_created_after.as_deref(), Some("2025-12-31"));
    }

    #[test]
    fn test_toml_photos_all_fields() {
        let toml_str = r#"
            [photos]
            size = "thumb"
            live_photo_size = "medium"
            live_photo_mov_filename_policy = "original"
            align_raw = "original"
            file_match_policy = "name-id7"
            force_size = true
            keep_unicode_in_filenames = true
        "#;
        let config: TomlConfig = toml::from_str(toml_str).unwrap();
        let p = config.photos.unwrap();
        assert_eq!(p.size, Some(VersionSize::Thumb));
        assert_eq!(p.live_photo_size, Some(LivePhotoSize::Medium));
        assert_eq!(
            p.live_photo_mov_filename_policy,
            Some(LivePhotoMovFilenamePolicy::Original)
        );
        assert_eq!(p.align_raw, Some(RawTreatmentPolicy::PreferOriginal));
        assert_eq!(p.file_match_policy, Some(FileMatchPolicy::NameId7));
        assert_eq!(p.force_size, Some(true));
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
    fn test_toml_metrics_port_parsed() {
        let toml_str = r#"
            [metrics]
            port = 9090
        "#;
        let config: TomlConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.metrics.unwrap().port, Some(9090));
    }

    #[test]
    fn test_toml_metrics_port_resolves_in_config() {
        let toml_str = r#"
            [auth]
            username = "user@example.com"
            [download]
            directory = "/photos"
            [metrics]
            port = 9090
        "#;
        let toml: TomlConfig = toml::from_str(toml_str).unwrap();
        let config = Config::build(
            &default_globals(),
            default_password(),
            default_sync(),
            Some(toml),
        )
        .unwrap();
        assert_eq!(config.metrics_port, Some(9090));
    }

    #[test]
    fn test_cli_metrics_port_overrides_toml() {
        let toml_str = r#"
            [auth]
            username = "user@example.com"
            [download]
            directory = "/photos"
            [metrics]
            port = 9090
        "#;
        let toml: TomlConfig = toml::from_str(toml_str).unwrap();
        let mut sync = default_sync();
        sync.metrics_port = Some(8080);
        let config =
            Config::build(&default_globals(), default_password(), sync, Some(toml)).unwrap();
        assert_eq!(config.metrics_port, Some(8080));
    }

    #[test]
    fn test_toml_metrics_unknown_field_rejected() {
        let toml_str = r#"
            [metrics]
            port = 9090
            unknown_field = true
        "#;
        let result: Result<TomlConfig, _> = toml::from_str(toml_str);
        assert!(
            result.is_err(),
            "unknown fields in [metrics] should be rejected"
        );
    }

    // ── TOML file loading from disk ────────────────────────────────

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

    // ── Config::build: exhaustive field merge tests ────────────────

    #[test]
    fn test_build_all_defaults_no_toml_exhaustive() {
        let cfg =
            Config::build(&default_globals(), default_password(), default_sync(), None).unwrap();
        // Auth
        assert_eq!(cfg.username, "u@example.com");
        assert!(cfg.password.is_none());
        assert!(matches!(cfg.domain, Domain::Com));
        assert!(cfg.cookie_directory.ends_with("kei/cookies"));
        // Download
        assert_eq!(cfg.folder_structure, "%Y/%m/%d");
        assert_eq!(cfg.threads_num, 10);
        assert_eq!(cfg.temp_suffix, ".kei-tmp");
        assert!(!cfg.set_exif_datetime);
        assert!(!cfg.no_progress_bar);
        // Retry
        assert_eq!(cfg.max_retries, 3);
        assert_eq!(cfg.retry_delay_secs, 5);
        // Filters
        assert_eq!(
            cfg.library,
            LibrarySelection::Single("PrimarySync".to_string())
        );
        assert_eq!(cfg.albums, AlbumSelection::LibraryOnly);
        assert!(!cfg.skip_videos);
        assert!(!cfg.skip_photos);
        assert_eq!(cfg.live_photo_mode, LivePhotoMode::Both);
        assert!(cfg.recent.is_none());
        assert!(cfg.skip_created_before.is_none());
        assert!(cfg.skip_created_after.is_none());
        // Photos
        assert!(matches!(cfg.size, VersionSize::Original));
        assert!(matches!(cfg.live_photo_size, LivePhotoSize::Original));
        assert!(matches!(
            cfg.live_photo_mov_filename_policy,
            LivePhotoMovFilenamePolicy::Suffix
        ));
        assert!(matches!(cfg.align_raw, RawTreatmentPolicy::Unchanged));
        assert!(matches!(
            cfg.file_match_policy,
            FileMatchPolicy::NameSizeDedupWithSuffix
        ));
        assert!(!cfg.force_size);
        assert!(!cfg.keep_unicode_in_filenames);
        // Watch
        assert!(cfg.watch_with_interval.is_none());
        assert!(!cfg.notify_systemd);
        assert!(cfg.pid_file.is_none());
        // Misc
        assert!(!cfg.dry_run);
        assert!(!cfg.only_print_filenames);
        // Notifications
        assert!(cfg.notification_script.is_none());
    }

    #[test]
    fn test_build_password_cli_overrides_toml() {
        let toml_str = r#"
            [auth]
            password = "toml-pw"
        "#;
        let toml: TomlConfig = toml::from_str(toml_str).unwrap();
        let globals = default_globals();
        let mut pw = default_password();
        pw.password = Some("cli-pw".to_string());
        let cfg = Config::build(&globals, pw, default_sync(), Some(toml)).unwrap();
        assert_eq!(
            cfg.password.as_ref().map(|s| s.expose_secret()),
            Some("cli-pw")
        );
    }

    #[test]
    fn test_build_password_from_toml() {
        let toml_str = r#"
            [auth]
            password = "toml-pw"
        "#;
        let toml: TomlConfig = toml::from_str(toml_str).unwrap();
        let cfg = Config::build(
            &default_globals(),
            default_password(),
            default_sync(),
            Some(toml),
        )
        .unwrap();
        assert_eq!(
            cfg.password.as_ref().map(|s| s.expose_secret()),
            Some("toml-pw")
        );
    }

    #[test]
    fn test_build_domain_cli_overrides_toml() {
        let toml_str = r#"
            [auth]
            domain = "cn"
        "#;
        let toml: TomlConfig = toml::from_str(toml_str).unwrap();
        let mut globals = default_globals();
        let pw = default_password();
        globals.domain = Some(Domain::Com);
        let cfg = Config::build(&globals, pw, default_sync(), Some(toml)).unwrap();
        assert!(matches!(cfg.domain, Domain::Com));
    }

    #[test]
    fn test_build_domain_from_toml() {
        let toml_str = r#"
            [auth]
            domain = "cn"
        "#;
        let toml: TomlConfig = toml::from_str(toml_str).unwrap();
        let cfg = Config::build(
            &default_globals(),
            default_password(),
            default_sync(),
            Some(toml),
        )
        .unwrap();
        assert!(matches!(cfg.domain, Domain::Cn));
    }

    /// Escape backslashes for embedding a path in a TOML string literal.
    fn toml_escape(path: &std::path::Path) -> String {
        path.to_string_lossy().replace('\\', "\\\\")
    }

    #[test]
    fn test_build_cookie_directory_cli_overrides_toml() {
        let dir = tempfile::tempdir().unwrap();
        let cli_path = dir.path().join("cli_cookies");
        let toml_path = dir.path().join("toml_cookies");
        let toml_str = format!("[auth]\ncookie_directory = \"{}\"", toml_escape(&toml_path));
        let toml: TomlConfig = toml::from_str(&toml_str).unwrap();
        let mut globals = default_globals();
        let pw = default_password();
        globals.cookie_directory = Some(cli_path.to_string_lossy().to_string());
        let cfg = Config::build(&globals, pw, default_sync(), Some(toml)).unwrap();
        assert_eq!(cfg.cookie_directory, cli_path);
    }

    #[test]
    fn test_build_cookie_directory_from_toml() {
        let dir = tempfile::tempdir().unwrap();
        let toml_path = dir.path().join("toml_cookies");
        let toml_str = format!("[auth]\ncookie_directory = \"{}\"", toml_escape(&toml_path));
        let toml: TomlConfig = toml::from_str(&toml_str).unwrap();
        let cfg = Config::build(
            &default_globals(),
            default_password(),
            default_sync(),
            Some(toml),
        )
        .unwrap();
        assert_eq!(cfg.cookie_directory, toml_path);
    }

    #[test]
    fn test_build_cookie_directory_tilde_expansion() {
        // Use a path under the home directory that we can actually create
        let home = dirs::home_dir().expect("home dir required for test");
        let unique = format!(".kei-test-{}", std::process::id());
        let toml_str = format!("[auth]\ncookie_directory = \"~/{unique}\"");
        let toml: TomlConfig = toml::from_str(&toml_str).unwrap();
        let cfg = Config::build(
            &default_globals(),
            default_password(),
            default_sync(),
            Some(toml),
        )
        .unwrap();
        assert_eq!(cfg.cookie_directory, home.join(&unique));
        let _ = std::fs::remove_dir(&cfg.cookie_directory);
    }

    #[test]
    fn test_build_directory_tilde_expansion() {
        let toml_str = r#"
            [download]
            directory = "~/photos"
        "#;
        let toml: TomlConfig = toml::from_str(toml_str).unwrap();
        let cfg = Config::build(
            &default_globals(),
            default_password(),
            default_sync(),
            Some(toml),
        )
        .unwrap();
        if let Some(home) = dirs::home_dir() {
            assert_eq!(cfg.directory, home.join("photos"));
        }
    }

    #[test]
    fn test_build_folder_structure_cli_overrides_toml() {
        let toml_str = r#"
            [download]
            folder_structure = "%Y-%m"
        "#;
        let toml: TomlConfig = toml::from_str(toml_str).unwrap();
        let mut sync = default_sync();
        sync.folder_structure = Some("%Y/%m/%d".to_string());
        let cfg = Config::build(&default_globals(), default_password(), sync, Some(toml)).unwrap();
        assert_eq!(cfg.folder_structure, "%Y/%m/%d");
    }

    #[test]
    fn test_build_temp_suffix_cli_overrides_toml() {
        let toml_str = r#"
            [download]
            temp_suffix = ".toml-tmp"
        "#;
        let toml: TomlConfig = toml::from_str(toml_str).unwrap();
        let mut sync = default_sync();
        sync.temp_suffix = Some(".cli-tmp".to_string());
        let cfg = Config::build(&default_globals(), default_password(), sync, Some(toml)).unwrap();
        assert_eq!(cfg.temp_suffix, ".cli-tmp");
    }

    #[test]
    fn test_build_temp_suffix_from_toml() {
        let toml_str = r#"
            [download]
            temp_suffix = ".downloading"
        "#;
        let toml: TomlConfig = toml::from_str(toml_str).unwrap();
        let cfg = Config::build(
            &default_globals(),
            default_password(),
            default_sync(),
            Some(toml),
        )
        .unwrap();
        assert_eq!(cfg.temp_suffix, ".downloading");
    }

    #[test]
    fn test_build_max_retries_cli_overrides_toml() {
        let toml_str = r#"
            [download.retry]
            max_retries = 5
        "#;
        let toml: TomlConfig = toml::from_str(toml_str).unwrap();
        let mut sync = default_sync();
        sync.max_retries = Some(10);
        let cfg = Config::build(&default_globals(), default_password(), sync, Some(toml)).unwrap();
        assert_eq!(cfg.max_retries, 10);
    }

    #[test]
    fn test_build_retry_delay_cli_overrides_toml() {
        let toml_str = r#"
            [download.retry]
            delay = 10
        "#;
        let toml: TomlConfig = toml::from_str(toml_str).unwrap();
        let mut sync = default_sync();
        sync.retry_delay = Some(30);
        let cfg = Config::build(&default_globals(), default_password(), sync, Some(toml)).unwrap();
        assert_eq!(cfg.retry_delay_secs, 30);
    }

    #[test]
    fn test_build_retry_delay_from_toml() {
        let toml_str = r#"
            [download.retry]
            delay = 15
        "#;
        let toml: TomlConfig = toml::from_str(toml_str).unwrap();
        let cfg = Config::build(
            &default_globals(),
            default_password(),
            default_sync(),
            Some(toml),
        )
        .unwrap();
        assert_eq!(cfg.retry_delay_secs, 15);
    }

    #[test]
    fn test_build_size_cli_overrides_toml() {
        let toml_str = r#"
            [photos]
            size = "thumb"
        "#;
        let toml: TomlConfig = toml::from_str(toml_str).unwrap();
        let mut sync = default_sync();
        sync.size = Some(VersionSize::Medium);
        let cfg = Config::build(&default_globals(), default_password(), sync, Some(toml)).unwrap();
        assert!(matches!(cfg.size, VersionSize::Medium));
    }

    #[test]
    fn test_build_size_from_toml() {
        let toml_str = r#"
            [photos]
            size = "thumb"
        "#;
        let toml: TomlConfig = toml::from_str(toml_str).unwrap();
        let cfg = Config::build(
            &default_globals(),
            default_password(),
            default_sync(),
            Some(toml),
        )
        .unwrap();
        assert!(matches!(cfg.size, VersionSize::Thumb));
    }

    #[test]
    fn test_build_live_photo_size_cli_overrides_toml() {
        let toml_str = r#"
            [photos]
            live_photo_size = "thumb"
        "#;
        let toml: TomlConfig = toml::from_str(toml_str).unwrap();
        let mut sync = default_sync();
        sync.live_photo_size = Some(LivePhotoSize::Medium);
        let cfg = Config::build(&default_globals(), default_password(), sync, Some(toml)).unwrap();
        assert!(matches!(cfg.live_photo_size, LivePhotoSize::Medium));
    }

    #[test]
    fn test_build_live_photo_size_from_toml() {
        let toml_str = r#"
            [photos]
            live_photo_size = "thumb"
        "#;
        let toml: TomlConfig = toml::from_str(toml_str).unwrap();
        let cfg = Config::build(
            &default_globals(),
            default_password(),
            default_sync(),
            Some(toml),
        )
        .unwrap();
        assert!(matches!(cfg.live_photo_size, LivePhotoSize::Thumb));
    }

    #[test]
    fn test_build_live_photo_size_defaults_to_adjusted_when_size_adjusted() {
        let mut sync = default_sync();
        sync.size = Some(VersionSize::Adjusted);
        let cfg = Config::build(&default_globals(), default_password(), sync, None).unwrap();
        assert!(matches!(cfg.live_photo_size, LivePhotoSize::Adjusted));
    }

    #[test]
    fn test_build_live_photo_size_explicit_overrides_adjusted_default() {
        let mut sync = default_sync();
        sync.size = Some(VersionSize::Adjusted);
        sync.live_photo_size = Some(LivePhotoSize::Original);
        let cfg = Config::build(&default_globals(), default_password(), sync, None).unwrap();
        assert!(matches!(cfg.live_photo_size, LivePhotoSize::Original));
    }

    #[test]
    fn test_build_mov_filename_policy_cli_overrides_toml() {
        let toml_str = r#"
            [photos]
            live_photo_mov_filename_policy = "original"
        "#;
        let toml: TomlConfig = toml::from_str(toml_str).unwrap();
        let mut sync = default_sync();
        sync.live_photo_mov_filename_policy = Some(LivePhotoMovFilenamePolicy::Suffix);
        let cfg = Config::build(&default_globals(), default_password(), sync, Some(toml)).unwrap();
        assert!(matches!(
            cfg.live_photo_mov_filename_policy,
            LivePhotoMovFilenamePolicy::Suffix
        ));
    }

    #[test]
    fn test_build_mov_filename_policy_from_toml() {
        let toml_str = r#"
            [photos]
            live_photo_mov_filename_policy = "original"
        "#;
        let toml: TomlConfig = toml::from_str(toml_str).unwrap();
        let cfg = Config::build(
            &default_globals(),
            default_password(),
            default_sync(),
            Some(toml),
        )
        .unwrap();
        assert!(matches!(
            cfg.live_photo_mov_filename_policy,
            LivePhotoMovFilenamePolicy::Original
        ));
    }

    #[test]
    fn test_build_align_raw_cli_overrides_toml() {
        let toml_str = r#"
            [photos]
            align_raw = "original"
        "#;
        let toml: TomlConfig = toml::from_str(toml_str).unwrap();
        let mut sync = default_sync();
        sync.align_raw = Some(RawTreatmentPolicy::PreferAlternative);
        let cfg = Config::build(&default_globals(), default_password(), sync, Some(toml)).unwrap();
        assert!(matches!(
            cfg.align_raw,
            RawTreatmentPolicy::PreferAlternative
        ));
    }

    #[test]
    fn test_build_file_match_policy_cli_overrides_toml() {
        let toml_str = r#"
            [photos]
            file_match_policy = "name-id7"
        "#;
        let toml: TomlConfig = toml::from_str(toml_str).unwrap();
        let mut sync = default_sync();
        sync.file_match_policy = Some(FileMatchPolicy::NameSizeDedupWithSuffix);
        let cfg = Config::build(&default_globals(), default_password(), sync, Some(toml)).unwrap();
        assert!(matches!(
            cfg.file_match_policy,
            FileMatchPolicy::NameSizeDedupWithSuffix
        ));
    }

    #[test]
    fn test_build_file_match_policy_from_toml() {
        let toml_str = r#"
            [photos]
            file_match_policy = "name-id7"
        "#;
        let toml: TomlConfig = toml::from_str(toml_str).unwrap();
        let cfg = Config::build(
            &default_globals(),
            default_password(),
            default_sync(),
            Some(toml),
        )
        .unwrap();
        assert!(matches!(cfg.file_match_policy, FileMatchPolicy::NameId7));
    }

    // ── Config::build: boolean flag merge exhaustive ───────────────

    #[test]
    fn test_build_all_boolean_flags_from_toml() {
        let toml_str = r#"
            [download]
            set_exif_datetime = true
            no_progress_bar = true

            [filters]
            skip_videos = true
            skip_photos = true
            skip_live_photos = true

            [photos]
            force_size = true
            keep_unicode_in_filenames = true

            [watch]
            notify_systemd = true
        "#;
        let toml: TomlConfig = toml::from_str(toml_str).unwrap();
        let cfg = Config::build(
            &default_globals(),
            default_password(),
            default_sync(),
            Some(toml),
        )
        .unwrap();
        assert!(cfg.set_exif_datetime);
        assert!(cfg.no_progress_bar);
        assert!(cfg.skip_videos);
        assert!(cfg.skip_photos);
        assert_eq!(cfg.live_photo_mode, LivePhotoMode::Skip);
        assert!(cfg.force_size);
        assert!(cfg.keep_unicode_in_filenames);
        assert!(cfg.notify_systemd);
    }

    #[test]
    fn test_build_all_boolean_flags_cli_overrides() {
        let mut sync = default_sync();
        sync.set_exif_datetime = Some(true);
        sync.no_progress_bar = Some(true);
        sync.skip_videos = Some(true);
        sync.skip_photos = Some(true);
        sync.skip_live_photos = Some(true);
        sync.force_size = Some(true);
        sync.keep_unicode_in_filenames = Some(true);
        sync.notify_systemd = Some(true);
        let cfg = Config::build(&default_globals(), default_password(), sync, None).unwrap();
        assert!(cfg.set_exif_datetime);
        assert!(cfg.no_progress_bar);
        assert!(cfg.skip_videos);
        assert!(cfg.skip_photos);
        assert_eq!(cfg.live_photo_mode, LivePhotoMode::Skip);
        assert!(cfg.force_size);
        assert!(cfg.keep_unicode_in_filenames);
        assert!(cfg.notify_systemd);
    }

    #[test]
    fn test_build_boolean_flags_false_in_toml_stays_false() {
        let toml_str = r#"
            [filters]
            skip_videos = false
            skip_photos = false
        "#;
        let toml: TomlConfig = toml::from_str(toml_str).unwrap();
        let cfg = Config::build(
            &default_globals(),
            default_password(),
            default_sync(),
            Some(toml),
        )
        .unwrap();
        assert!(!cfg.skip_videos);
        assert!(!cfg.skip_photos);
    }

    // ── Config::build: watch/interval ──────────────────────────────

    #[test]
    fn test_build_watch_interval_cli_overrides_toml() {
        let toml_str = r#"
            [watch]
            interval = 1800
        "#;
        let toml: TomlConfig = toml::from_str(toml_str).unwrap();
        let mut sync = default_sync();
        sync.watch_with_interval = Some(600);
        let cfg = Config::build(&default_globals(), default_password(), sync, Some(toml)).unwrap();
        assert_eq!(cfg.watch_with_interval, Some(600));
    }

    #[test]
    fn test_build_pid_file_cli_overrides_toml() {
        let toml_str = r#"
            [watch]
            pid_file = "/toml/pid"
        "#;
        let toml: TomlConfig = toml::from_str(toml_str).unwrap();
        let mut sync = default_sync();
        sync.pid_file = Some(PathBuf::from("/cli/pid"));
        let cfg = Config::build(&default_globals(), default_password(), sync, Some(toml)).unwrap();
        assert_eq!(cfg.pid_file, Some(PathBuf::from("/cli/pid")));
    }

    // ── Config::build: notification_script merge ────────────────────

    #[test]
    fn test_build_notification_script_from_toml() {
        let toml_str = r#"
            [notifications]
            script = "/config/notify.sh"
        "#;
        let toml: TomlConfig = toml::from_str(toml_str).unwrap();
        let cfg = Config::build(
            &default_globals(),
            default_password(),
            default_sync(),
            Some(toml),
        )
        .unwrap();
        assert_eq!(
            cfg.notification_script,
            Some(PathBuf::from("/config/notify.sh"))
        );
    }

    #[test]
    fn test_build_notification_script_cli_overrides_toml() {
        let toml_str = r#"
            [notifications]
            script = "/toml/notify.sh"
        "#;
        let toml: TomlConfig = toml::from_str(toml_str).unwrap();
        let mut sync = default_sync();
        sync.notification_script = Some("/cli/notify.sh".to_string());
        let cfg = Config::build(&default_globals(), default_password(), sync, Some(toml)).unwrap();
        assert_eq!(
            cfg.notification_script,
            Some(PathBuf::from("/cli/notify.sh"))
        );
    }

    #[test]
    fn test_build_notification_script_none_by_default() {
        let cfg =
            Config::build(&default_globals(), default_password(), default_sync(), None).unwrap();
        assert!(cfg.notification_script.is_none());
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

    // ── Config::build: recent/dates merge ──────────────────────────

    #[test]
    fn test_build_recent_cli_overrides_toml() {
        let toml_str = r#"
            [filters]
            recent = 500
        "#;
        let toml: TomlConfig = toml::from_str(toml_str).unwrap();
        let mut sync = default_sync();
        sync.recent = Some(100);
        let cfg = Config::build(&default_globals(), default_password(), sync, Some(toml)).unwrap();
        assert_eq!(cfg.recent, Some(100));
    }

    #[test]
    fn test_build_recent_from_toml() {
        let toml_str = r#"
            [filters]
            recent = 500
        "#;
        let toml: TomlConfig = toml::from_str(toml_str).unwrap();
        let cfg = Config::build(
            &default_globals(),
            default_password(),
            default_sync(),
            Some(toml),
        )
        .unwrap();
        assert_eq!(cfg.recent, Some(500));
    }

    #[test]
    fn test_build_skip_dates_cli_overrides_toml() {
        let toml_str = r#"
            [filters]
            skip_created_before = "2024-01-01"
            skip_created_after = "2025-01-01"
        "#;
        let toml: TomlConfig = toml::from_str(toml_str).unwrap();
        let mut sync = default_sync();
        sync.skip_created_before = Some("2023-06-01".to_string());
        sync.skip_created_after = Some("2024-06-01".to_string());
        let cfg = Config::build(&default_globals(), default_password(), sync, Some(toml)).unwrap();
        let before = cfg.skip_created_before.unwrap();
        assert_eq!(
            before.date_naive(),
            NaiveDate::from_ymd_opt(2023, 6, 1).unwrap()
        );
        let after = cfg.skip_created_after.unwrap();
        assert_eq!(
            after.date_naive(),
            NaiveDate::from_ymd_opt(2024, 6, 1).unwrap()
        );
    }

    #[test]
    fn test_build_skip_dates_interval_syntax_from_toml() {
        let toml_str = r#"
            [filters]
            skip_created_before = "30d"
        "#;
        let toml: TomlConfig = toml::from_str(toml_str).unwrap();
        let cfg = Config::build(
            &default_globals(),
            default_password(),
            default_sync(),
            Some(toml),
        )
        .unwrap();
        let before = cfg.skip_created_before.unwrap();
        let expected = chrono::Local::now() - chrono::Duration::days(30);
        assert!((before - expected).num_seconds().abs() < 2);
    }

    #[test]
    fn test_build_invalid_date_from_toml_errors() {
        let toml_str = r#"
            [filters]
            skip_created_before = "not-a-date"
        "#;
        let toml: TomlConfig = toml::from_str(toml_str).unwrap();
        let result = Config::build(
            &default_globals(),
            default_password(),
            default_sync(),
            Some(toml),
        );
        assert!(result.is_err());
    }

    // ── Config::build: full TOML config ────────────────────────────

    #[test]
    fn test_build_full_toml_all_sections() {
        let dir = tempfile::tempdir().unwrap();
        let cookie_path = dir.path().join("full_cookies");
        let toml_str = format!(
            r#"
            log_level = "warn"

            [auth]
            username = "full@example.com"
            password = "fullpw"
            domain = "cn"
            cookie_directory = "{cookie}"

            [download]
            directory = "/full/photos"
            folder_structure = "%Y"
            threads_num = 2
            temp_suffix = ".full-tmp"
            set_exif_datetime = true
            no_progress_bar = true

            [download.retry]
            max_retries = 1
            delay = 2

            [filters]
            library = "SharedSync-FULL"
            albums = ["Album1"]
            skip_videos = true
            recent = 50

            [photos]
            size = "medium"
            live_photo_size = "thumb"
            live_photo_mov_filename_policy = "original"
            align_raw = "alternative"
            file_match_policy = "name-id7"
            force_size = true

            [watch]
            interval = 900
            pid_file = "/full/pid"
        "#,
            cookie = toml_escape(&cookie_path)
        );
        let toml: TomlConfig = toml::from_str(&toml_str).unwrap();
        let cfg = Config::build(
            &default_globals(),
            default_password(),
            default_sync(),
            Some(toml),
        )
        .unwrap();
        // default_auth username overrides toml
        assert_eq!(cfg.username, "u@example.com");
        assert_eq!(
            cfg.password.as_ref().map(|s| s.expose_secret()),
            Some("fullpw")
        );
        assert!(matches!(cfg.domain, Domain::Cn));
        assert_eq!(cfg.cookie_directory, cookie_path);
        assert_eq!(cfg.directory, PathBuf::from("/full/photos"));
        assert_eq!(cfg.folder_structure, "%Y");
        assert_eq!(cfg.threads_num, 2);
        assert_eq!(cfg.temp_suffix, ".full-tmp");
        assert!(cfg.set_exif_datetime);
        assert!(cfg.no_progress_bar);
        assert_eq!(cfg.max_retries, 1);
        assert_eq!(cfg.retry_delay_secs, 2);
        assert_eq!(
            cfg.library,
            LibrarySelection::Single("SharedSync-FULL".to_string())
        );
        assert_eq!(
            cfg.albums,
            AlbumSelection::Named(vec!["Album1".to_string()])
        );
        assert!(cfg.skip_videos);
        assert_eq!(cfg.recent, Some(50));
        assert!(matches!(cfg.size, VersionSize::Medium));
        assert!(matches!(cfg.live_photo_size, LivePhotoSize::Thumb));
        assert!(matches!(
            cfg.live_photo_mov_filename_policy,
            LivePhotoMovFilenamePolicy::Original
        ));
        assert!(matches!(
            cfg.align_raw,
            RawTreatmentPolicy::PreferAlternative
        ));
        assert!(matches!(cfg.file_match_policy, FileMatchPolicy::NameId7));
        assert!(cfg.force_size);
        assert_eq!(cfg.watch_with_interval, Some(900));
        assert_eq!(cfg.pid_file, Some(PathBuf::from("/full/pid")));
    }

    // ── resolve_auth tests ─────────────────────────────────────────

    #[test]
    fn test_resolve_auth_all_from_toml() {
        let toml_str = r#"
            [auth]
            username = "toml@example.com"
            password = "toml-pw"
            domain = "cn"
            cookie_directory = "/toml/cookies"
        "#;
        let toml: TomlConfig = toml::from_str(toml_str).unwrap();
        let globals = GlobalArgs {
            username: None,
            domain: None,
            data_dir: None,
            cookie_directory: None,
        };
        let pw = crate::cli::PasswordArgs::default();
        let (username, password, domain, cookie_dir) = resolve_auth(&globals, &pw, Some(&toml));
        assert_eq!(username, "toml@example.com");
        assert_eq!(password.as_deref(), Some("toml-pw"));
        assert!(matches!(domain, Domain::Cn));
        assert_eq!(cookie_dir, PathBuf::from("/toml/cookies"));
    }

    #[test]
    fn test_resolve_auth_cli_overrides_all() {
        let toml_str = r#"
            [auth]
            username = "toml@example.com"
            password = "toml-pw"
            domain = "cn"
            cookie_directory = "/toml/cookies"
        "#;
        let toml: TomlConfig = toml::from_str(toml_str).unwrap();
        let globals = GlobalArgs {
            username: Some("cli@example.com".to_string()),
            domain: Some(Domain::Com),
            data_dir: None,
            cookie_directory: Some("/cli/cookies".to_string()),
        };
        let pw = crate::cli::PasswordArgs {
            password: Some("cli-pw".to_string()),
            ..crate::cli::PasswordArgs::default()
        };
        let (username, password, domain, cookie_dir) = resolve_auth(&globals, &pw, Some(&toml));
        assert_eq!(username, "cli@example.com");
        assert_eq!(password.as_deref(), Some("cli-pw"));
        assert!(matches!(domain, Domain::Com));
        assert_eq!(cookie_dir, PathBuf::from("/cli/cookies"));
    }

    #[test]
    fn test_resolve_auth_defaults_when_both_absent() {
        let globals = GlobalArgs {
            username: None,
            domain: None,
            data_dir: None,
            cookie_directory: None,
        };
        let pw = crate::cli::PasswordArgs::default();
        let (username, password, domain, cookie_dir) = resolve_auth(&globals, &pw, None);
        assert!(username.is_empty());
        assert!(password.is_none());
        assert!(matches!(domain, Domain::Com));
        assert!(cookie_dir.ends_with("kei/cookies"));
    }

    // ── Config::build: albums edge cases ───────────────────────────

    #[test]
    fn test_build_albums_empty_toml_empty_cli() {
        let toml_str = r#"
            [filters]
            albums = []
        "#;
        let toml: TomlConfig = toml::from_str(toml_str).unwrap();
        let cfg = Config::build(
            &default_globals(),
            default_password(),
            default_sync(),
            Some(toml),
        )
        .unwrap();
        assert_eq!(cfg.albums, AlbumSelection::LibraryOnly);
    }

    #[test]
    fn test_build_albums_no_toml_no_cli() {
        let cfg =
            Config::build(&default_globals(), default_password(), default_sync(), None).unwrap();
        assert_eq!(cfg.albums, AlbumSelection::LibraryOnly);
    }

    #[test]
    fn test_album_selection_to_vec_roundtrip() {
        assert!(AlbumSelection::LibraryOnly.to_vec().is_empty());
        assert_eq!(AlbumSelection::All.to_vec(), vec!["all".to_string()]);
        assert_eq!(
            AlbumSelection::Named(vec!["A".into(), "B".into()]).to_vec(),
            vec!["A".to_string(), "B".to_string()]
        );
    }

    // ── AlbumSelection resolution tests ────────────────────────────

    #[test]
    fn test_build_album_all_maps_to_all_variant() {
        let mut sync = default_sync();
        sync.albums = vec!["all".to_string()];
        let cfg = Config::build(&default_globals(), default_password(), sync, None).unwrap();
        assert_eq!(cfg.albums, AlbumSelection::All);
    }

    #[test]
    fn test_build_album_all_is_case_insensitive() {
        for raw in ["all", "ALL", "All", "aLL"] {
            let mut sync = default_sync();
            sync.albums = vec![raw.to_string()];
            let cfg = Config::build(&default_globals(), default_password(), sync, None).unwrap();
            assert_eq!(
                cfg.albums,
                AlbumSelection::All,
                "'{raw}' should resolve to AlbumSelection::All"
            );
        }
    }

    #[test]
    fn test_build_album_all_mixed_with_names_errors() {
        let mut sync = default_sync();
        sync.albums = vec!["all".to_string(), "Vacation".to_string()];
        let err = Config::build(&default_globals(), default_password(), sync, None).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("'-a all' cannot be combined"),
            "unexpected error: {msg}"
        );
    }

    #[test]
    fn test_build_album_all_from_toml() {
        let toml_str = r#"
            [filters]
            albums = ["all"]
        "#;
        let toml: TomlConfig = toml::from_str(toml_str).unwrap();
        let cfg = Config::build(
            &default_globals(),
            default_password(),
            default_sync(),
            Some(toml),
        )
        .unwrap();
        assert_eq!(cfg.albums, AlbumSelection::All);
    }

    #[test]
    fn test_build_album_smart_default_kicks_in_with_album_token() {
        // No -a passed, but {album} in folder_structure -> implicit All.
        let mut sync = default_sync();
        sync.folder_structure = Some("{album}/%Y/%m".to_string());
        let cfg = Config::build(&default_globals(), default_password(), sync, None).unwrap();
        assert_eq!(cfg.albums, AlbumSelection::All);
    }

    #[test]
    fn test_build_album_smart_default_inactive_without_album_token() {
        // No -a, no {album} -> LibraryOnly (today's default).
        let mut sync = default_sync();
        sync.folder_structure = Some("%Y/%m/%d".to_string());
        let cfg = Config::build(&default_globals(), default_password(), sync, None).unwrap();
        assert_eq!(cfg.albums, AlbumSelection::LibraryOnly);
    }

    #[test]
    fn test_build_album_named_preserved() {
        let mut sync = default_sync();
        sync.albums = vec!["Vacation".to_string(), "Trip".to_string()];
        let cfg = Config::build(&default_globals(), default_password(), sync, None).unwrap();
        assert_eq!(
            cfg.albums,
            AlbumSelection::Named(vec!["Vacation".to_string(), "Trip".to_string()])
        );
    }

    // ── folder_structure {album} placement validation ──────────────

    #[test]
    fn test_build_album_token_rejected_mid_path() {
        let mut sync = default_sync();
        sync.folder_structure = Some("Photos/{album}/%Y".to_string());
        let err = Config::build(&default_globals(), default_password(), sync, None).unwrap_err();
        assert!(
            err.to_string()
                .contains("'{album}' must be the first path segment"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn test_build_album_token_rejected_after_date() {
        let mut sync = default_sync();
        sync.folder_structure = Some("%Y/{album}/%m".to_string());
        let err = Config::build(&default_globals(), default_password(), sync, None).unwrap_err();
        assert!(
            err.to_string()
                .contains("'{album}' must be the first path segment"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn test_build_album_token_rejected_as_trailing() {
        let mut sync = default_sync();
        sync.folder_structure = Some("%Y/%m/{album}".to_string());
        let err = Config::build(&default_globals(), default_password(), sync, None).unwrap_err();
        assert!(err.to_string().contains("must be the first path segment"));
    }

    #[test]
    fn test_build_album_token_rejected_duplicate() {
        let mut sync = default_sync();
        sync.folder_structure = Some("{album}/%Y/{album}".to_string());
        let err = Config::build(&default_globals(), default_password(), sync, None).unwrap_err();
        assert!(
            err.to_string().contains("may only appear once"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn test_build_album_token_accepted_at_root() {
        let mut sync = default_sync();
        sync.albums = vec!["Vacation".to_string()];
        sync.folder_structure = Some("{album}/%Y/%m".to_string());
        let cfg = Config::build(&default_globals(), default_password(), sync, None).unwrap();
        assert_eq!(cfg.folder_structure, "{album}/%Y/%m");
    }

    #[test]
    fn test_build_album_token_accepted_alone() {
        let mut sync = default_sync();
        sync.albums = vec!["Vacation".to_string()];
        sync.folder_structure = Some("{album}".to_string());
        let cfg = Config::build(&default_globals(), default_password(), sync, None).unwrap();
        assert_eq!(cfg.folder_structure, "{album}");
    }

    #[test]
    fn test_build_album_token_accepted_within_python_wrapper() {
        let mut sync = default_sync();
        sync.albums = vec!["Vacation".to_string()];
        sync.folder_structure = Some("{:{album}/%Y/%m}".to_string());
        let cfg = Config::build(&default_globals(), default_password(), sync, None).unwrap();
        assert_eq!(cfg.folder_structure, "{:{album}/%Y/%m}");
    }

    #[test]
    fn test_build_directory_cli_overrides_toml() {
        let toml_str = r#"
            [download]
            directory = "/toml/photos"
        "#;
        let toml: TomlConfig = toml::from_str(toml_str).unwrap();
        let mut sync = default_sync();
        sync.directory = Some("/cli/photos".to_string());
        let cfg = Config::build(&default_globals(), default_password(), sync, Some(toml)).unwrap();
        assert_eq!(cfg.directory, PathBuf::from("/cli/photos"));
    }

    // ── Config::build: passthrough flags ───────────────────────────

    #[test]
    fn test_build_passthrough_flags() {
        let mut sync = default_sync();
        sync.dry_run = true;
        let cfg = Config::build(&default_globals(), default_password(), sync, None).unwrap();
        assert!(cfg.dry_run);
    }

    #[test]
    fn test_folder_structure_valid_tokens_accepted() {
        let mut sync = default_sync();
        sync.folder_structure = Some("%Y/%m/%d".to_string());
        assert!(Config::build(&default_globals(), default_password(), sync, None).is_ok());
    }

    #[test]
    fn test_folder_structure_all_tokens_accepted() {
        let mut sync = default_sync();
        sync.folder_structure = Some("%Y/%m/%d/%H/%M/%S".to_string());
        assert!(Config::build(&default_globals(), default_password(), sync, None).is_ok());
    }

    #[test]
    fn test_folder_structure_none_bypasses_validation() {
        let mut sync = default_sync();
        sync.folder_structure = Some("none".to_string());
        assert!(Config::build(&default_globals(), default_password(), sync, None).is_ok());
    }

    #[test]
    fn test_folder_structure_strftime_tokens_accepted() {
        // Full strftime support: %B (month name), %X (locale time), etc. are valid
        let mut sync = default_sync();
        sync.folder_structure = Some("%Y/%B/%d".to_string());
        let cfg = Config::build(&default_globals(), default_password(), sync, None).unwrap();
        assert_eq!(cfg.folder_structure, "%Y/%B/%d");
    }

    #[test]
    fn test_folder_structure_wrapped_format_accepted() {
        let mut sync = default_sync();
        sync.folder_structure = Some("{:%Y/%m/%d}".to_string());
        assert!(Config::build(&default_globals(), default_password(), sync, None).is_ok());
    }

    // ── to_toml() tests ─────────────────────────────────────────────

    #[test]
    fn test_to_toml_roundtrip_preserves_username() {
        let cfg =
            Config::build(&default_globals(), default_password(), default_sync(), None).unwrap();
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
        let cfg = Config::build(&globals, pw, default_sync(), None).unwrap();
        let toml = cfg.to_toml();
        assert!(toml.auth.as_ref().unwrap().password.is_none());
    }

    #[test]
    fn test_to_toml_omits_default_values() {
        let cfg =
            Config::build(&default_globals(), default_password(), default_sync(), None).unwrap();
        let toml = cfg.to_toml();
        // Default domain (com) should be omitted
        assert!(toml.auth.as_ref().unwrap().domain.is_none());
        // Default size (original) should be omitted
        assert!(toml.photos.as_ref().unwrap().size.is_none());
        // Default temp_suffix should be omitted
        assert!(toml.download.as_ref().unwrap().temp_suffix.is_none());
    }

    #[test]
    fn test_to_toml_includes_non_default_values() {
        let mut globals = default_globals();
        let pw = default_password();
        globals.domain = Some(crate::types::Domain::Cn);
        let mut sync = default_sync();
        sync.size = Some(crate::types::VersionSize::Medium);
        let cfg = Config::build(&globals, pw, sync, None).unwrap();
        let toml = cfg.to_toml();
        assert_eq!(
            toml.auth.as_ref().unwrap().domain,
            Some(crate::types::Domain::Cn)
        );
        assert_eq!(
            toml.photos.as_ref().unwrap().size,
            Some(crate::types::VersionSize::Medium)
        );
    }

    #[test]
    fn test_to_toml_serializes_to_valid_toml() {
        let cfg =
            Config::build(&default_globals(), default_password(), default_sync(), None).unwrap();
        let toml_cfg = cfg.to_toml();
        let serialized = toml::to_string_pretty(&toml_cfg).unwrap();
        // Should be parseable back
        let _parsed: TomlConfig = toml::from_str(&serialized).unwrap();
    }

    #[test]
    fn test_to_toml_per_run_fields_omitted() {
        let mut sync = default_sync();
        sync.recent = Some(50);
        sync.skip_created_before = Some("2025-01-01".to_string());
        sync.skip_created_after = Some("2025-12-31".to_string());
        let cfg = Config::build(&default_globals(), default_password(), sync, None).unwrap();
        let toml = cfg.to_toml();
        let filters = toml.filters.as_ref().unwrap();
        assert!(filters.recent.is_none());
        assert!(filters.skip_created_before.is_none());
        assert!(filters.skip_created_after.is_none());
    }

    #[test]
    fn test_to_toml_roundtrip_exclude_albums() {
        let mut sync = default_sync();
        sync.exclude_albums = vec!["Hidden".to_string(), "Trash".to_string()];
        let cfg = Config::build(&default_globals(), default_password(), sync, None).unwrap();
        let toml = cfg.to_toml();
        let filters = toml.filters.as_ref().unwrap();
        assert_eq!(
            filters.exclude_albums.as_deref(),
            Some(&["Hidden".to_string(), "Trash".to_string()][..])
        );
    }

    #[test]
    fn test_to_toml_roundtrip_filename_exclude() {
        let mut sync = default_sync();
        sync.filename_exclude = vec!["*.AAE".to_string(), "Screenshot*".to_string()];
        let cfg = Config::build(&default_globals(), default_password(), sync, None).unwrap();
        let toml = cfg.to_toml();
        let filters = toml.filters.as_ref().unwrap();
        assert_eq!(
            filters.filename_exclude.as_deref(),
            Some(&["*.AAE".to_string(), "Screenshot*".to_string()][..])
        );
        // Round-trip: serialize then deserialize
        let serialized = ::toml::to_string_pretty(&toml).unwrap();
        let parsed: TomlConfig = ::toml::from_str(&serialized).unwrap();
        assert_eq!(
            parsed.filters.as_ref().unwrap().filename_exclude.as_deref(),
            Some(&["*.AAE".to_string(), "Screenshot*".to_string()][..])
        );
    }

    #[test]
    fn test_to_toml_roundtrip_live_photo_mode() {
        let mut sync = default_sync();
        sync.live_photo_mode = Some(crate::types::LivePhotoMode::ImageOnly);
        let cfg = Config::build(&default_globals(), default_password(), sync, None).unwrap();
        let toml = cfg.to_toml();
        assert_eq!(
            toml.photos.as_ref().unwrap().live_photo_mode,
            Some(crate::types::LivePhotoMode::ImageOnly)
        );
        // Round-trip
        let serialized = ::toml::to_string_pretty(&toml).unwrap();
        let parsed: TomlConfig = ::toml::from_str(&serialized).unwrap();
        assert_eq!(
            parsed.photos.as_ref().unwrap().live_photo_mode,
            Some(crate::types::LivePhotoMode::ImageOnly)
        );
    }

    #[test]
    fn test_to_toml_empty_exclude_albums_omitted() {
        let cfg =
            Config::build(&default_globals(), default_password(), default_sync(), None).unwrap();
        let toml = cfg.to_toml();
        assert!(toml.filters.as_ref().unwrap().exclude_albums.is_none());
    }

    #[test]
    fn test_to_toml_default_live_photo_mode_omitted() {
        let cfg =
            Config::build(&default_globals(), default_password(), default_sync(), None).unwrap();
        let toml = cfg.to_toml();
        assert!(toml.photos.as_ref().unwrap().live_photo_mode.is_none());
    }

    #[test]
    fn test_to_toml_roundtrip_bandwidth_limit() {
        let mut sync = default_sync();
        sync.bandwidth_limit = Some(5_000_000);
        let cfg = Config::build(&default_globals(), default_password(), sync, None).unwrap();
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
            default_password(),
            default_sync(),
            Some(serialized),
        )
        .unwrap();
        assert_eq!(reparsed.bandwidth_limit, Some(5_000_000));
    }

    #[test]
    fn test_to_toml_bandwidth_limit_none_omitted() {
        let cfg =
            Config::build(&default_globals(), default_password(), default_sync(), None).unwrap();
        let toml = cfg.to_toml();
        assert!(toml.download.as_ref().unwrap().bandwidth_limit.is_none());
    }

    // ── TOML-only skip_live_photos legacy path ──────────────────────

    #[test]
    fn test_toml_skip_live_photos_legacy_maps_to_skip_mode() {
        let toml_str = r#"
            [auth]
            username = "u@example.com"

            [filters]
            skip_live_photos = true
        "#;
        let toml: TomlConfig = ::toml::from_str(toml_str).unwrap();
        let cfg = Config::build(
            &default_globals(),
            default_password(),
            default_sync(),
            Some(toml),
        )
        .unwrap();
        assert_eq!(cfg.live_photo_mode, crate::types::LivePhotoMode::Skip);
    }

    #[test]
    fn test_toml_skip_live_photos_false_stays_both() {
        let toml_str = r#"
            [auth]
            username = "u@example.com"

            [filters]
            skip_live_photos = false
        "#;
        let toml: TomlConfig = ::toml::from_str(toml_str).unwrap();
        let cfg = Config::build(
            &default_globals(),
            default_password(),
            default_sync(),
            Some(toml),
        )
        .unwrap();
        assert_eq!(cfg.live_photo_mode, crate::types::LivePhotoMode::Both);
    }

    #[test]
    fn test_toml_photos_live_photo_mode_overrides_filters_skip_live_photos() {
        let toml_str = r#"
            [auth]
            username = "u@example.com"

            [filters]
            skip_live_photos = true

            [photos]
            live_photo_mode = "image-only"
        "#;
        let toml: TomlConfig = ::toml::from_str(toml_str).unwrap();
        let cfg = Config::build(
            &default_globals(),
            default_password(),
            default_sync(),
            Some(toml),
        )
        .unwrap();
        assert_eq!(cfg.live_photo_mode, crate::types::LivePhotoMode::ImageOnly);
    }

    // ── resolve_data_dir() tests ────────────────────────────────────

    #[test]
    fn test_resolve_data_dir_explicit_cli() {
        let result = resolve_data_dir(
            Some("/explicit"),
            None,
            None,
            Path::new("/config/config.toml"),
        );
        assert_eq!(result, PathBuf::from("/explicit"));
    }

    #[test]
    fn test_resolve_data_dir_legacy_cookie_dir() {
        let result = resolve_data_dir(
            None,
            Some("/legacy/cookies"),
            None,
            Path::new("/config/config.toml"),
        );
        assert_eq!(result, PathBuf::from("/legacy/cookies"));
    }

    #[test]
    fn test_resolve_data_dir_toml_data_dir() {
        let toml = TomlConfig {
            data_dir: Some("/toml/data".to_string()),
            log_level: None,
            auth: None,
            download: None,
            filters: None,
            photos: None,
            watch: None,
            notifications: None,
            metrics: None,
        };
        let result = resolve_data_dir(None, None, Some(&toml), Path::new("/config/config.toml"));
        assert_eq!(result, PathBuf::from("/toml/data"));
    }

    #[test]
    fn test_resolve_data_dir_toml_legacy_cookie_directory() {
        let toml = TomlConfig {
            data_dir: None,
            log_level: None,
            auth: Some(TomlAuth {
                username: None,
                password: None,
                password_file: None,
                password_command: None,
                domain: None,
                cookie_directory: Some("/toml/cookies".to_string()),
            }),
            download: None,
            filters: None,
            photos: None,
            watch: None,
            notifications: None,
            metrics: None,
        };
        let result = resolve_data_dir(None, None, Some(&toml), Path::new("/config/config.toml"));
        assert_eq!(result, PathBuf::from("/toml/cookies"));
    }

    #[test]
    fn test_resolve_data_dir_defaults_to_config_parent() {
        let result = resolve_data_dir(None, None, None, Path::new("/config/config.toml"));
        assert_eq!(result, PathBuf::from("/config"));
    }

    #[test]
    fn test_resolve_data_dir_cli_takes_precedence_over_toml() {
        let toml = TomlConfig {
            data_dir: Some("/toml/data".to_string()),
            log_level: None,
            auth: None,
            download: None,
            filters: None,
            photos: None,
            watch: None,
            notifications: None,
            metrics: None,
        };
        let result = resolve_data_dir(
            Some("/cli/data"),
            None,
            Some(&toml),
            Path::new("/config/config.toml"),
        );
        assert_eq!(result, PathBuf::from("/cli/data"));
    }

    // ── persist_first_run_config() tests ────────────────────────────

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
            sync.directory = Some(d.to_string());
        }
        Config::build(&globals, pw_args, sync, None).unwrap()
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
        sync.directory = Some("/photos".to_string());
        let config = Config::build(&globals, pw, sync, None).unwrap();

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

    // ── Filter + LivePhotoMode config resolution ──────────────────

    #[test]
    fn test_live_photo_mode_cli_overrides_toml() {
        let mut sync = default_sync();
        sync.live_photo_mode = Some(LivePhotoMode::ImageOnly);
        let toml_str = "[photos]\nlive_photo_mode = \"skip\"\n";
        let toml: TomlConfig = toml::from_str(toml_str).unwrap();
        let cfg = Config::build(&default_globals(), default_password(), sync, Some(toml)).unwrap();
        assert_eq!(cfg.live_photo_mode, LivePhotoMode::ImageOnly);
    }

    #[test]
    fn test_live_photo_mode_from_toml() {
        let toml_str = "[photos]\nlive_photo_mode = \"video-only\"\n";
        let toml: TomlConfig = toml::from_str(toml_str).unwrap();
        let cfg = Config::build(
            &default_globals(),
            default_password(),
            default_sync(),
            Some(toml),
        )
        .unwrap();
        assert_eq!(cfg.live_photo_mode, LivePhotoMode::VideoOnly);
    }

    #[test]
    fn test_filename_exclude_from_toml() {
        let toml_str = "[filters]\nfilename_exclude = [\"*.AAE\", \"*.TMP\"]\n";
        let toml: TomlConfig = toml::from_str(toml_str).unwrap();
        let cfg = Config::build(
            &default_globals(),
            default_password(),
            default_sync(),
            Some(toml),
        )
        .unwrap();
        let patterns: Vec<&str> = cfg.filename_exclude.iter().map(|p| p.as_str()).collect();
        assert_eq!(patterns, vec!["*.AAE", "*.TMP"]);
    }

    #[test]
    fn test_filename_exclude_invalid_glob_rejected() {
        let mut sync = default_sync();
        sync.filename_exclude = vec!["[invalid".to_string()];
        let err = Config::build(&default_globals(), default_password(), sync, None).unwrap_err();
        assert!(err
            .to_string()
            .contains("invalid --filename-exclude pattern"));
    }

    #[test]
    fn test_exclude_albums_from_toml() {
        let toml_str = "[filters]\nexclude_albums = [\"Hidden\", \"Trash\"]\n";
        let toml: TomlConfig = toml::from_str(toml_str).unwrap();
        let cfg = Config::build(
            &default_globals(),
            default_password(),
            default_sync(),
            Some(toml),
        )
        .unwrap();
        assert_eq!(cfg.exclude_albums, vec!["Hidden", "Trash"]);
    }

    #[test]
    fn test_contradictory_date_filter_succeeds() {
        // before >= after is a warning, not an error -- Config::build should succeed
        let mut sync = default_sync();
        sync.skip_created_before = Some("2025-06-01".to_string());
        sync.skip_created_after = Some("2025-01-01".to_string());
        let cfg = Config::build(&default_globals(), default_password(), sync, None);
        assert!(
            cfg.is_ok(),
            "Contradictory date filters should warn, not error"
        );
        let cfg = cfg.unwrap();
        assert!(cfg.skip_created_before >= cfg.skip_created_after);
    }

    #[test]
    fn test_exclude_album_cli_overrides_toml() {
        let mut sync = default_sync();
        sync.exclude_albums = vec!["CLI_Album".to_string()];
        let toml_str = "[filters]\nexclude_albums = [\"TOML_Album\"]\n";
        let toml: TomlConfig = toml::from_str(toml_str).unwrap();
        let cfg = Config::build(&default_globals(), default_password(), sync, Some(toml)).unwrap();
        assert_eq!(cfg.exclude_albums, vec!["CLI_Album"]);
    }

    #[test]
    fn test_exclude_album_falls_back_to_toml() {
        let toml_str = "[filters]\nexclude_albums = [\"TOML_Album\"]\n";
        let toml: TomlConfig = toml::from_str(toml_str).unwrap();
        let cfg = Config::build(
            &default_globals(),
            default_password(),
            default_sync(),
            Some(toml),
        )
        .unwrap();
        assert_eq!(cfg.exclude_albums, vec!["TOML_Album"]);
    }

    #[test]
    fn test_filename_exclude_cli_overrides_toml() {
        let mut sync = default_sync();
        sync.filename_exclude = vec!["*.AAE".to_string()];
        let toml_str = "[filters]\nfilename_exclude = [\"*.TMP\"]\n";
        let toml: TomlConfig = toml::from_str(toml_str).unwrap();
        let cfg = Config::build(&default_globals(), default_password(), sync, Some(toml)).unwrap();
        let patterns: Vec<&str> = cfg.filename_exclude.iter().map(|p| p.as_str()).collect();
        assert_eq!(patterns, vec!["*.AAE"]);
    }

    #[test]
    fn test_filename_exclude_falls_back_to_toml() {
        let toml_str = "[filters]\nfilename_exclude = [\"*.TMP\"]\n";
        let toml: TomlConfig = toml::from_str(toml_str).unwrap();
        let cfg = Config::build(
            &default_globals(),
            default_password(),
            default_sync(),
            Some(toml),
        )
        .unwrap();
        let patterns: Vec<&str> = cfg.filename_exclude.iter().map(|p| p.as_str()).collect();
        assert_eq!(patterns, vec!["*.TMP"]);
    }

    #[test]
    fn test_validate_directory_rejects_root() {
        assert!(validate_directory(Path::new("/")).is_err());
    }

    #[test]
    fn test_validate_directory_rejects_system_paths() {
        for path in ["/usr", "/etc", "/boot", "/sys", "/proc", "/dev", "/var"] {
            assert!(
                validate_directory(Path::new(path)).is_err(),
                "should reject {path}"
            );
        }
    }

    #[test]
    fn test_validate_directory_rejects_trailing_slash() {
        assert!(validate_directory(Path::new("/etc/")).is_err());
    }

    #[test]
    fn test_validate_directory_accepts_normal_paths() {
        assert!(validate_directory(Path::new("/home/user/photos")).is_ok());
        assert!(validate_directory(Path::new("/mnt/photos")).is_ok());
        assert!(validate_directory(Path::new("/data/sync")).is_ok());
    }

    // ── resolve_library_selection ──────────────────────────────────

    #[test]
    fn resolve_library_defaults_to_primary_sync() {
        let result = resolve_library_selection(None, None);
        assert_eq!(result, LibrarySelection::Single("PrimarySync".to_string()));
    }

    #[test]
    fn resolve_library_cli_overrides_toml() {
        let toml_filters = TomlFilters {
            library: Some("SharedSync-FROM-TOML".to_string()),
            ..Default::default()
        };
        let result =
            resolve_library_selection(Some("SharedSync-FROM-CLI".to_string()), Some(&toml_filters));
        assert_eq!(
            result,
            LibrarySelection::Single("SharedSync-FROM-CLI".to_string())
        );
    }

    #[test]
    fn resolve_library_falls_back_to_toml() {
        let toml_filters = TomlFilters {
            library: Some("SharedSync-ABCD".to_string()),
            ..Default::default()
        };
        let result = resolve_library_selection(None, Some(&toml_filters));
        assert_eq!(
            result,
            LibrarySelection::Single("SharedSync-ABCD".to_string())
        );
    }

    #[test]
    fn resolve_library_all_case_insensitive() {
        assert_eq!(
            resolve_library_selection(Some("ALL".to_string()), None),
            LibrarySelection::All
        );
        assert_eq!(
            resolve_library_selection(Some("All".to_string()), None),
            LibrarySelection::All
        );
        assert_eq!(
            resolve_library_selection(Some("all".to_string()), None),
            LibrarySelection::All
        );
    }
}
