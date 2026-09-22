//! Configuration precedence and runtime policy assembly.

use super::input::{TomlConfig, TomlFilters};
use super::paths::{
    GlobalArgs, default_config_path, default_cookie_dir, expand_tilde, resolve_data_dir,
    resolve_password_command, resolve_password_file, validate_download_dir,
};
use super::runtime::{
    AuthConfig, Config, DownloadSettings, FilterConfig, ImportConfig, MediaKind, MediaSelection,
    MetadataConfig, NotificationConfig, PhotoConfig, ReportConfig, ResolvedRetryConfig,
    RuntimeConfig, ServerConfig, UiConfig, WatchConfig, parse_created_date_filter,
};
use super::templates::{
    DEFAULT_FOLDER_STRUCTURE_ALBUMS, DEFAULT_FOLDER_STRUCTURE_SMART_FOLDERS, TemplateKind,
    validate_template_tokens,
};
use crate::password::SecretString;
use crate::types::{
    Domain, FileMatchPolicy, LivePhotoMode, LivePhotoMovFilenamePolicy, LivePhotoResolution,
    PhotoResolution, RawPolicy,
};
use std::collections::BTreeSet;
use std::path::PathBuf;

/// Programmatic durable sync overrides.
///
/// Public CLI/env no longer expose these fields in v0.20. Tests and internal
/// call sites that need to exercise the resolver can still pass explicit
/// overrides without re-growing [`crate::cli::SyncArgs`].
#[derive(Debug, Clone, Default)]
pub(crate) struct SyncConfigOverrides {
    pub download_dir: Option<String>,
    pub albums: Vec<String>,
    pub smart_folders: Vec<String>,
    pub unfiled: Option<bool>,
    pub filename_exclude: Vec<String>,
    pub libraries: Vec<String>,
    pub resolution: Option<PhotoResolution>,
    pub live_resolution: Option<LivePhotoResolution>,
    pub edited: Option<bool>,
    pub alternative: Option<bool>,
    pub threads: Option<u16>,
    pub bandwidth_limit: Option<u64>,
    pub skip_videos: Option<bool>,
    pub skip_photos: Option<bool>,
    pub live_photo_mode: Option<LivePhotoMode>,
    pub force_resolution: Option<bool>,
    pub folder_structure: Option<String>,
    pub folder_structure_albums: Option<String>,
    pub folder_structure_smart_folders: Option<String>,
    pub set_exif_datetime: Option<bool>,
    pub set_exif_rating: Option<bool>,
    pub set_exif_gps: Option<bool>,
    pub set_exif_description: Option<bool>,
    #[cfg(feature = "xmp")]
    pub embed_xmp: Option<bool>,
    #[cfg(feature = "xmp")]
    pub xmp_sidecar: Option<bool>,
    pub watch_with_interval: Option<u64>,
    pub keep_unicode_in_filenames: Option<bool>,
    pub live_photo_mov_filename_policy: Option<LivePhotoMovFilenamePolicy>,
    pub raw_policy: Option<RawPolicy>,
    pub file_match_policy: Option<FileMatchPolicy>,
    pub max_retries: Option<u32>,
    pub temp_suffix: Option<String>,
    pub notify_systemd: Option<bool>,
    pub pid_file: Option<PathBuf>,
    pub reconcile_every_n_cycles: Option<u64>,
    pub notification_script: Option<String>,
    pub report_json: Option<PathBuf>,
    pub http_port: Option<u16>,
    pub http_bind: Option<std::net::IpAddr>,
    pub max_download_attempts: Option<u32>,
}

// ── Application Config ──────────────────────────────────────────────

/// Resolve library selection from the programmatic override, TOML, or the
/// default (`primary`). The override and TOML `[filters].libraries` array share
/// the selector grammar
/// (`primary` / `shared` / `all` / `none` / `!name` / raw zone names);
///
/// Returns the parsed [`crate::selection::LibrarySelector`]; the matching
/// against live CloudKit zones happens in
/// `commands::service::resolve_libraries`.
pub(crate) fn resolve_library_selector(
    cli_libraries: Vec<String>,
    toml_filters: Option<&TomlFilters>,
) -> anyhow::Result<crate::selection::LibrarySelector> {
    let toml_libraries = toml_filters.and_then(|f| f.libraries.clone());
    let raw = resolve_vec(cli_libraries, toml_libraries);
    crate::selection::parse_library_selector(&raw)
}

pub(crate) fn resolve_media_selection(
    toml_filters: Option<&TomlFilters>,
    skip_videos_override: Option<bool>,
    skip_photos_override: Option<bool>,
) -> anyhow::Result<MediaSelection> {
    let mut selection = if let Some(kinds) = toml_filters.and_then(|f| f.media.as_ref()) {
        anyhow::ensure!(!kinds.is_empty(), "[filters].media cannot be empty.");
        let mut seen = BTreeSet::new();
        let mut selection = MediaSelection {
            photos: false,
            videos: false,
            live_photos: false,
        };
        for kind in kinds {
            anyhow::ensure!(
                seen.insert(*kind),
                "[filters].media contains duplicate `{}`.",
                media_kind_name(*kind)
            );
            match kind {
                MediaKind::Photos => selection.photos = true,
                MediaKind::Videos => selection.videos = true,
                MediaKind::LivePhotos => selection.live_photos = true,
            }
        }
        selection
    } else {
        MediaSelection::all()
    };

    if let Some(skip_videos) = skip_videos_override {
        selection.videos = !skip_videos;
    }
    if let Some(skip_photos) = skip_photos_override {
        selection.photos = !skip_photos;
    }

    anyhow::ensure!(
        selection.photos || selection.videos || selection.live_photos,
        "[filters].media must select at least one media category."
    );

    Ok(selection)
}

const fn media_kind_name(kind: MediaKind) -> &'static str {
    match kind {
        MediaKind::Photos => "photos",
        MediaKind::Videos => "videos",
        MediaKind::LivePhotos => "live-photos",
    }
}

/// Default for `Selection.unfiled` when the user did not pass `--unfiled`
/// explicitly: always `true` in v0.13. The unfiled pass is independent of
/// `--album`: `kei sync --album Vacation` runs the Vacation pass *and* the
/// unfiled pass unless the user explicitly disables it with
/// `--unfiled false`. `--unfiled` always wins when supplied.
pub(crate) const fn unfiled_default() -> bool {
    true
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

/// For repeatable Vec flags where empty CLI input means "no override":
/// CLI value wins iff non-empty, else TOML, else empty. Mirrors how
/// `clap` represents an absent repeatable flag (`Vec::new()`).
fn resolve_vec(cli: Vec<String>, toml: Option<Vec<String>>) -> Vec<String> {
    if cli.is_empty() {
        toml.unwrap_or_default()
    } else {
        cli
    }
}

/// CLI inputs for [`resolve_path_derivation_fields`].
///
/// Each field is `Option<T>`; `Some` means the CLI (or env var) supplied
/// the value, `None` means fall through to TOML and then to the default.
#[derive(Debug, Default)]
pub(crate) struct PathDerivationCliArgs {
    pub folder_structure: Option<String>,
    pub folder_structure_albums: Option<String>,
    pub folder_structure_smart_folders: Option<String>,
    pub resolution: Option<PhotoResolution>,
    pub live_photo_mode: Option<LivePhotoMode>,
    pub live_resolution: Option<LivePhotoResolution>,
    pub live_photo_mov_filename_policy: Option<LivePhotoMovFilenamePolicy>,
    pub edited: Option<bool>,
    pub alternative: Option<bool>,
    pub raw_policy: Option<RawPolicy>,
    pub file_match_policy: Option<FileMatchPolicy>,
    pub force_resolution: Option<bool>,
    pub keep_unicode_in_filenames: Option<bool>,
}

impl PathDerivationCliArgs {
    fn from_overrides(overrides: &SyncConfigOverrides) -> Self {
        Self {
            folder_structure: overrides.folder_structure.clone(),
            folder_structure_albums: overrides.folder_structure_albums.clone(),
            folder_structure_smart_folders: overrides.folder_structure_smart_folders.clone(),
            resolution: overrides.resolution,
            live_photo_mode: overrides.live_photo_mode,
            live_resolution: overrides.live_resolution,
            live_photo_mov_filename_policy: overrides.live_photo_mov_filename_policy,
            edited: overrides.edited,
            alternative: overrides.alternative,
            raw_policy: overrides.raw_policy,
            file_match_policy: overrides.file_match_policy,
            force_resolution: overrides.force_resolution,
            keep_unicode_in_filenames: overrides.keep_unicode_in_filenames,
        }
    }
}

/// Resolved path-derivation fields used by both `Config::build` (sync) and
/// `build_import_path_config` (import-existing) so the two code paths
/// derive identical expected file paths for the same inputs.
#[derive(Debug)]
pub(crate) struct PathDerivationFields {
    pub folder_structure: String,
    pub folder_structure_albums: String,
    pub folder_structure_smart_folders: String,
    pub resolution: PhotoResolution,
    pub live_photo_mode: LivePhotoMode,
    pub live_resolution: LivePhotoResolution,
    pub live_photo_mov_filename_policy: LivePhotoMovFilenamePolicy,
    pub edited: bool,
    pub alternative: bool,
    pub raw_policy: RawPolicy,
    pub file_match_policy: FileMatchPolicy,
    pub force_resolution: bool,
    pub keep_unicode_in_filenames: bool,
}

/// Resolve the CLI > TOML > default chain for every field that affects
/// path derivation, shared by sync and import.
pub(crate) fn resolve_path_derivation_fields(
    cli: PathDerivationCliArgs,
    toml: Option<&TomlConfig>,
) -> anyhow::Result<PathDerivationFields> {
    let toml_dl = toml.and_then(|t| t.download.as_ref());
    let toml_photos = toml.and_then(|t| t.photos.as_ref());

    let folder_structure = resolve_ref(
        cli.folder_structure.as_ref(),
        toml_dl.and_then(|d| d.folder_structure.as_ref()),
        "%Y/%m/%d".to_string(),
    );
    let folder_structure_albums = resolve_ref(
        cli.folder_structure_albums.as_ref(),
        toml_dl.and_then(|d| d.folder_structure_albums.as_ref()),
        DEFAULT_FOLDER_STRUCTURE_ALBUMS.to_string(),
    );
    let folder_structure_smart_folders = resolve_ref(
        cli.folder_structure_smart_folders.as_ref(),
        toml_dl.and_then(|d| d.folder_structure_smart_folders.as_ref()),
        DEFAULT_FOLDER_STRUCTURE_SMART_FOLDERS.to_string(),
    );
    let resolution = resolve(
        cli.resolution,
        toml_photos.and_then(|p| p.resolution),
        PhotoResolution::Original,
    );
    let live_resolution = resolve(
        cli.live_resolution,
        toml_photos.and_then(|p| p.live_resolution),
        LivePhotoResolution::Original,
    );
    let live_photo_mode = resolve(
        cli.live_photo_mode,
        toml_photos.and_then(|p| p.live_photo_mode),
        LivePhotoMode::Both,
    );
    let live_photo_mov_filename_policy = resolve(
        cli.live_photo_mov_filename_policy,
        toml_photos.and_then(|p| p.live_photo_mov_filename_policy),
        LivePhotoMovFilenamePolicy::Suffix,
    );
    let edited = resolve_flag(cli.edited, toml_photos.and_then(|p| p.edited));
    let alternative = resolve_flag(cli.alternative, toml_photos.and_then(|p| p.alternative));
    anyhow::ensure!(
        resolution != PhotoResolution::None || edited || alternative,
        "[photos].resolution = \"none\" requires [photos].edited = true or [photos].alternative = true."
    );
    let raw_policy = resolve(
        cli.raw_policy,
        toml_photos.and_then(|p| p.raw_policy),
        RawPolicy::AsIs,
    );
    let file_match_policy = resolve(
        cli.file_match_policy,
        toml_photos.and_then(|p| p.file_match_policy),
        FileMatchPolicy::NameSizeDedupWithSuffix,
    );
    let force_resolution = resolve_flag(
        cli.force_resolution,
        toml_photos.and_then(|p| p.force_resolution),
    );
    let keep_unicode_in_filenames = resolve_flag(
        cli.keep_unicode_in_filenames,
        toml_photos.and_then(|p| p.keep_unicode_in_filenames),
    );

    Ok(PathDerivationFields {
        folder_structure,
        folder_structure_albums,
        folder_structure_smart_folders,
        resolution,
        live_photo_mode,
        live_resolution,
        live_photo_mov_filename_policy,
        edited,
        alternative,
        raw_policy,
        file_match_policy,
        force_resolution,
        keep_unicode_in_filenames,
    })
}

/// Resolve `--notify-systemd` given the explicit CLI / TOML values and
/// whether `NOTIFY_SOCKET` is present in the environment.
///
/// Explicit settings (CLI or TOML) take precedence in that order. When
/// nothing is set, auto-detect: `NOTIFY_SOCKET` is the env var systemd's
/// `Type=notify` units publish, so its presence is a reliable signal that
/// the sd_notify messages will have a listener. No other launcher sets it,
/// so false positives are effectively zero.
///
/// Pure policy function so the truth table is testable without touching
/// process environment state.
pub(crate) fn resolve_notify_systemd(
    cli: Option<bool>,
    toml: Option<bool>,
    notify_socket_present: bool,
) -> bool {
    cli.or(toml).unwrap_or(notify_socket_present)
}

/// Smart initial retry delay (seconds) derived from retry `per_transfer`.
///
/// Higher max implies the user is patient and wants retries to give failing
/// services time to recover (rate limits, 5xx, slow endpoints). Lower max
/// implies "fail fast" and retries should be quick.
///
/// `per_transfer == 0` means no retries happen so the delay is irrelevant;
/// returns 5 (within the validation range) as a non-load-bearing placeholder.
pub(crate) fn smart_retry_delay(max_retries: u32) -> u64 {
    match max_retries {
        0 => 5,
        1 | 2 => 2,
        3 => 5,
        4..=6 => 10,
        _ => 30,
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

    // `[auth].password` in TOML is rejected in `Config::build()`; resolve_auth
    // only pulls the password from CLI / env.
    let password = password_args.password.clone();

    let domain = resolve(
        globals.domain,
        toml_auth.and_then(|a| a.domain),
        Domain::Com,
    );

    let has_explicit_data_dir =
        globals.data_dir.is_some() || toml.and_then(|t| t.data_dir.as_ref()).is_some();
    let cookie_directory = if has_explicit_data_dir {
        let default_config = default_config_path();
        resolve_data_dir(globals.data_dir.as_deref(), toml, &default_config)
    } else {
        default_cookie_dir()
    };

    (username, password, domain, cookie_directory)
}

#[cfg(test)]
fn take_sync_config_overrides(sync: &mut crate::cli::SyncArgs) -> SyncConfigOverrides {
    std::mem::take(&mut sync.config_overrides)
}

#[cfg(not(test))]
fn take_sync_config_overrides(_sync: &mut crate::cli::SyncArgs) -> SyncConfigOverrides {
    SyncConfigOverrides::default()
}

impl Config {
    /// Build a Config by merging CLI args with optional TOML config.
    /// Runtime CLI flags override TOML where they remain public. Durable sync
    /// settings come from TOML and then hardcoded defaults.
    pub fn build(
        globals: &GlobalArgs,
        pw: &crate::cli::PasswordArgs,
        sync: crate::cli::SyncArgs,
        toml: Option<&TomlConfig>,
    ) -> anyhow::Result<Self> {
        let mut sync = sync;
        let overrides = take_sync_config_overrides(&mut sync);
        let friendly_request = toml.and_then(|t| t.ui.as_ref()).and_then(|u| u.friendly);
        Self::build_inner_impl(
            globals,
            pw,
            sync,
            overrides,
            toml,
            crate::personality::Mode::Off,
            friendly_request,
        )
    }

    pub(crate) fn build_inner(
        globals: &GlobalArgs,
        pw: &crate::cli::PasswordArgs,
        sync: crate::cli::SyncArgs,
        toml: Option<&TomlConfig>,
        personality_mode: crate::personality::Mode,
        friendly_request: Option<bool>,
    ) -> anyhow::Result<Self> {
        Self::build_inner_impl(
            globals,
            pw,
            sync,
            SyncConfigOverrides::default(),
            toml,
            personality_mode,
            friendly_request,
        )
    }

    fn build_inner_impl(
        globals: &GlobalArgs,
        pw: &crate::cli::PasswordArgs,
        sync: crate::cli::SyncArgs,
        overrides: SyncConfigOverrides,
        toml: Option<&TomlConfig>,
        personality_mode: crate::personality::Mode,
        friendly_request: Option<bool>,
    ) -> anyhow::Result<Self> {
        let toml_auth = toml.and_then(|t| t.auth.as_ref());

        // `[auth].password` is no longer accepted. Plaintext passwords in config
        // files are a standing security risk; kei ships a credential store
        // (`kei password set`), password files, and shell-command sources.
        if toml_auth.and_then(|a| a.password.as_ref()).is_some() {
            anyhow::bail!(
                "The config file sets `[auth].password`, which kei no longer accepts. \
                 Plaintext passwords in config files are a security risk. \
                 Use one of: `kei password set` (OS keyring or encrypted file), \
                 `[auth] password_file`, or `[auth] password_command` instead."
            );
        }

        let (username, password_str, domain, cookie_directory) = resolve_auth(globals, pw, toml);
        let password_file = resolve_password_file(pw, toml_auth);
        let password_command = resolve_password_command(pw, toml_auth);
        let save_password = sync.save_password;

        // `--password-command` / `[auth] password_command` is Unix-only: the
        // command runs via `sh -c`, which isn't on a stock Windows PATH. Fail
        // at startup with a clear message instead of a cryptic "No such file
        // or directory" from the first auth attempt.
        #[cfg(windows)]
        if password_command.is_some() {
            anyhow::bail!(
                "`--password-command` and `[auth].password_command` are not supported on Windows because kei runs commands through `sh -c`, which is not on the default Windows PATH. \
                 Use `--password-file` / `[auth] password_file`, or run kei under WSL."
            );
        }

        // Reject explicitly provided empty username/password (CLI value_parser
        // catches the CLI case; this catches empty strings from TOML).
        if globals.username.is_some()
            || toml
                .and_then(|t| t.auth.as_ref()?.username.as_ref())
                .is_some()
        {
            anyhow::ensure!(!username.is_empty(), "The iCloud username cannot be empty.");
        }
        if let Some(pw_str) = &password_str {
            anyhow::ensure!(!pw_str.is_empty(), "The iCloud password cannot be empty.");
        }

        // Reject both `password_file` and `password_command` in the same TOML
        // (CLI enforces this via `conflicts_with`, TOML has no such mechanism).
        if let Some(toml_a) = toml_auth {
            anyhow::ensure!(
                !(toml_a.password_file.is_some() && toml_a.password_command.is_some()),
                "The config file sets both `[auth].password_file` and `[auth].password_command`. Pick one password source."
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
                "The cookie directory path contains a file where a directory is needed: {}",
                existing.display()
            );
        }
        std::fs::create_dir_all(&cookie_directory).map_err(|e| {
            anyhow::anyhow!(
                "Could not create cookie directory {}: {e}",
                cookie_directory.display()
            )
        })?;

        let toml_dl = toml.and_then(|t| t.download.as_ref());
        let toml_retry = toml_dl.and_then(|d| d.retry.as_ref());
        let toml_filters = toml.and_then(|t| t.filters.as_ref());
        let toml_import = toml.and_then(|t| t.import.as_ref());
        let toml_metadata = toml.and_then(|t| t.metadata.as_ref());
        let toml_ui = toml.and_then(|t| t.ui.as_ref());
        let toml_watch = toml.and_then(|t| t.watch.as_ref());
        let toml_server = toml.and_then(|t| t.server.as_ref());
        let path_derivation_args = PathDerivationCliArgs::from_overrides(&overrides);

        // Download
        let directory = overrides
            .download_dir
            .or_else(|| toml_dl.and_then(|d| d.directory.clone()))
            .map(|d| expand_tilde(&d))
            .unwrap_or_default();
        if !directory.as_os_str().is_empty() {
            validate_download_dir(&directory)?;
        }
        // Resolve bandwidth limit (CLI bytes/sec > TOML human-readable string > None).
        let bandwidth_limit: Option<u64> = if let Some(n) = overrides.bandwidth_limit {
            Some(n)
        } else if let Some(s) = toml_dl.and_then(|d| d.bandwidth_limit.as_ref()) {
            Some(crate::cli::parse_bandwidth_limit(s).map_err(|e| {
                anyhow::anyhow!("Invalid [download].bandwidth_limit in config: {e}")
            })?)
        } else {
            None
        };

        let toml_threads = toml_dl.and_then(|d| d.threads);

        // When a bandwidth limit is set without an explicit thread-count flag,
        // default concurrency to 1: many connections starving for a capped
        // total budget just fragments downloads and adds connection overhead.
        let threads_explicitly_set = overrides.threads.is_some() || toml_threads.is_some();
        let threads_default = if bandwidth_limit.is_some() && !threads_explicitly_set {
            1
        } else {
            10
        };

        let threads_num = overrides
            .threads
            .or(toml_threads)
            .unwrap_or(threads_default);
        anyhow::ensure!(
            (1..=64).contains(&threads_num),
            "[download].threads must be between 1 and 64, got {threads_num}."
        );
        let temp_suffix = resolve(
            overrides.temp_suffix,
            toml_dl.and_then(|d| d.temp_suffix.clone()),
            ".kei-tmp".to_string(),
        );
        let PathDerivationFields {
            folder_structure,
            folder_structure_albums,
            folder_structure_smart_folders,
            resolution,
            live_photo_mode,
            live_resolution,
            live_photo_mov_filename_policy,
            edited,
            alternative,
            raw_policy,
            file_match_policy,
            force_resolution,
            keep_unicode_in_filenames,
        } = resolve_path_derivation_fields(path_derivation_args, toml)?;
        let set_exif_datetime = resolve_flag(
            overrides.set_exif_datetime,
            toml_metadata.and_then(|m| m.set_exif_datetime),
        );
        let set_exif_rating = resolve_flag(
            overrides.set_exif_rating,
            toml_metadata.and_then(|m| m.set_exif_rating),
        );
        let set_exif_gps = resolve_flag(
            overrides.set_exif_gps,
            toml_metadata.and_then(|m| m.set_exif_gps),
        );
        let set_exif_description = resolve_flag(
            overrides.set_exif_description,
            toml_metadata.and_then(|m| m.set_exif_description),
        );
        #[cfg(feature = "xmp")]
        let embed_xmp = resolve_flag(overrides.embed_xmp, toml_metadata.and_then(|m| m.embed_xmp));
        #[cfg(feature = "xmp")]
        let xmp_sidecar = resolve_flag(
            overrides.xmp_sidecar,
            toml_metadata.and_then(|m| m.xmp_sidecar),
        );
        let no_progress_bar =
            sync.no_progress_bar || !toml_ui.and_then(|u| u.progress_bar).unwrap_or(true);

        // Re-validate; clap range attrs run on CLI only.
        let max_retries = resolve(
            overrides.max_retries,
            toml_retry.and_then(|r| r.per_transfer),
            3,
        );
        anyhow::ensure!(
            max_retries <= 100,
            "[download.retry].per_transfer must be 100 or less, got {max_retries}."
        );
        // Lifetime cap on download attempts per asset (0 disables). CLI >
        // TOML > 10, matching every other resolved value. The runtime
        // skip check in download::pipeline::process_asset short-circuits
        // when this is 0, so 0 is a valid (cap-off) sentinel.
        let max_download_attempts = resolve(
            overrides.max_download_attempts,
            toml_retry.and_then(|r| r.per_asset),
            10,
        );
        let retry_delay_secs = smart_retry_delay(max_retries);

        // Filters
        let library_selector = resolve_library_selector(overrides.libraries, toml_filters)?;
        let toml_albums = toml_filters.and_then(|f| f.albums.clone());
        let raw_albums = resolve_vec(overrides.albums, toml_albums);

        // The base template is for unfiled/library-only paths. Album-specific
        // paths must use `folder_structure_albums`.
        validate_template_tokens(&folder_structure, TemplateKind::Unfiled)?;
        validate_template_tokens(&folder_structure_albums, TemplateKind::Albums)?;
        validate_template_tokens(&folder_structure_smart_folders, TemplateKind::SmartFolders)?;

        let media =
            resolve_media_selection(toml_filters, overrides.skip_videos, overrides.skip_photos)?;
        let skip_videos = media.skip_videos();
        let skip_photos = media.skip_photos();
        let raw_smart_folders = resolve_vec(
            overrides.smart_folders,
            toml_filters.and_then(|f| f.smart_folders.clone()),
        );

        let unfiled_override = overrides
            .unfiled
            .or_else(|| toml_filters.and_then(|f| f.unfiled));

        let selection = crate::selection::Selection {
            albums: crate::selection::parse_album_selector(&raw_albums, true)?,
            albums_explicit: !raw_albums.is_empty(),
            smart_folders: crate::selection::parse_smart_folder_selector(&raw_smart_folders)?,
            smart_folders_explicit: !raw_smart_folders.is_empty(),
            libraries: library_selector.clone(),
            unfiled: unfiled_override.unwrap_or_else(unfiled_default),
        };
        let filename_exclude_strs = resolve_vec(
            overrides.filename_exclude,
            toml_filters.and_then(|f| f.filename_exclude.clone()),
        );
        // Compile glob patterns once during build
        let filename_exclude: Vec<glob::Pattern> = filename_exclude_strs
            .iter()
            .map(|p| {
                glob::Pattern::new(p)
                    .map_err(|e| anyhow::anyhow!("Invalid --filename-exclude pattern '{p}': {e}"))
            })
            .collect::<anyhow::Result<_>>()?;
        let persistent_recent = toml_filters.and_then(|f| f.recent);
        let recent_raw = sync.recent.or(persistent_recent);
        let persistent_recent_scope = toml_filters.and_then(|f| f.recent_scope);
        let recent_scope = sync
            .recent_scope
            .or(persistent_recent_scope)
            .unwrap_or_default();
        let recent_scope_was_set = sync.recent_scope.is_some() || persistent_recent_scope.is_some();
        let persistent_skip_created_before =
            toml_filters.and_then(|f| f.skip_created_before.clone());
        let persistent_skip_created_after = toml_filters.and_then(|f| f.skip_created_after.clone());
        let explicit_skip_created_before_str = sync
            .skip_created_before
            .or_else(|| persistent_skip_created_before.clone());

        // Split the RecentLimit: Count(n) is a post-enumeration cap held on
        // config.recent. Days(n) translates into a skip_created_before cutoff
        // since "last N days" = "skip everything created before N days ago".
        // Reject combining the two forms on the same invocation so user
        // intent stays unambiguous.
        let (recent, recent_days) = match recent_raw {
            None => (None, None),
            Some(crate::cli::RecentLimit::Count(n)) => (Some(n), None),
            Some(crate::cli::RecentLimit::Days(n)) => {
                anyhow::ensure!(
                    explicit_skip_created_before_str.is_none(),
                    "`--recent {n}d` and `--skip-created-before` do the same thing. Pick one."
                );
                (None, Some(n))
            }
        };
        anyhow::ensure!(
            recent_raw.is_some() || !recent_scope_was_set,
            "`recent_scope` only applies when `recent` is set."
        );
        anyhow::ensure!(
            !matches!(recent_raw, Some(crate::cli::RecentLimit::Days(_))) || !recent_scope_was_set,
            "`recent_scope` only applies to count-form `recent` values, not `Nd` day windows."
        );
        let skip_created_before_str = if let Some(n) = recent_days {
            Some(format!("{n}d"))
        } else {
            explicit_skip_created_before_str
        };
        let skip_created_after_str = sync
            .skip_created_after
            .or_else(|| persistent_skip_created_after.clone());

        let skip_created_before = skip_created_before_str
            .as_deref()
            .map(parse_created_date_filter)
            .transpose()?;
        let skip_created_after = skip_created_after_str
            .as_deref()
            .map(parse_created_date_filter)
            .transpose()?;

        if let (Some(before), Some(after)) = (&skip_created_before, &skip_created_after)
            && before.is_strictly_after(*after)
        {
            tracing::warn!(
                target: "kei::config",
                %before,
                %after,
                "skip-created-before is after skip-created-after, no assets can match",
            );
        }

        let watch_with_interval = overrides
            .watch_with_interval
            .or_else(|| toml_watch.and_then(|w| w.interval));
        if let Some(n) = watch_with_interval {
            anyhow::ensure!(
                (60..=86400).contains(&n),
                "watch interval must be in 60..=86400 seconds, got {n}"
            );
        }
        // Auto-detect systemd via `NOTIFY_SOCKET` when neither CLI nor TOML
        // sets the flag explicitly. See `resolve_notify_systemd`.
        let notify_systemd = resolve_notify_systemd(
            overrides.notify_systemd,
            toml_watch.and_then(|w| w.notify_systemd),
            std::env::var_os("NOTIFY_SOCKET").is_some(),
        );
        let pid_file = overrides.pid_file.or_else(|| {
            toml_watch
                .and_then(|w| w.pid_file.as_ref())
                .map(PathBuf::from)
        });
        // `.filter` collapses TOML's `reconcile_every_n_cycles = 0` to None,
        // matching the documented "0 = off" semantic. The CLI parser already
        // rejects 0, so the filter only fires for the TOML path.
        let reconcile_every_n_cycles = overrides
            .reconcile_every_n_cycles
            .or_else(|| toml_watch.and_then(|w| w.reconcile_every_n_cycles))
            .filter(|n| *n > 0);

        // Notifications
        let toml_notif = toml.and_then(|t| t.notifications.as_ref());
        let notification_script = overrides
            .notification_script
            .or_else(|| toml_notif.and_then(|n| n.script.clone()))
            .map(|s| expand_tilde(&s));

        // JSON report: CLI > [report] json TOML > none.
        let toml_report = toml.and_then(|t| t.report.as_ref());
        let report_json = overrides.report_json.or_else(|| {
            toml_report
                .and_then(|r| r.json.as_deref())
                .map(expand_tilde)
        });

        // HTTP server port - CLI > [server] TOML > default 9090.
        const DEFAULT_HTTP_PORT: u16 = 9090;
        let http_port = overrides
            .http_port
            .or_else(|| toml_server.and_then(|s| s.port))
            .unwrap_or(DEFAULT_HTTP_PORT);

        // HTTP server bind address — CLI > [server] bind TOML > default 0.0.0.0.
        // 0.0.0.0 preserves the historical behavior and keeps Docker's `-p 9090:9090`
        // working out of the box; desktop users can set 127.0.0.1 to restrict
        // /healthz and /metrics to loopback.
        const DEFAULT_HTTP_BIND: std::net::IpAddr =
            std::net::IpAddr::V4(std::net::Ipv4Addr::new(0, 0, 0, 0));
        let http_bind = match overrides.http_bind {
            Some(addr) => addr,
            None => match toml_server.and_then(|s| s.bind.as_deref()) {
                Some(raw) => raw.parse::<std::net::IpAddr>().map_err(|e| {
                    anyhow::anyhow!("[server].bind must be an IP address, but got {raw:?}: {e}")
                })?,
                None => DEFAULT_HTTP_BIND,
            },
        };

        if !media.photos
            && !media.videos
            && media.live_photos
            && live_photo_mode == LivePhotoMode::Skip
        {
            anyhow::bail!(
                "[filters].media selects only live photos, but [photos].live_photo_mode = \"skip\" would download nothing. Enable photos or videos, or change live_photo_mode."
            );
        }

        Ok(Self {
            auth: AuthConfig {
                username,
                password,
                password_file,
                password_command,
                cookie_directory,
                domain,
                save_password,
            },
            download: DownloadSettings {
                directory,
                folder_structure,
                folder_structure_albums,
                folder_structure_smart_folders,
                filename_exclude,
                temp_suffix,
                threads_num,
                bandwidth_limit,
                no_progress_bar,
            },
            filters: FilterConfig {
                selection,
                media,
                skip_created_before,
                skip_created_after,
                recent,
                recent_scope,
                persistent_recent,
                persistent_recent_scope,
                persistent_skip_created_before,
                persistent_skip_created_after,
                skip_videos,
                skip_photos,
            },
            photos: PhotoConfig {
                resolution,
                live_resolution,
                live_photo_mode,
                live_photo_mov_filename_policy,
                edited,
                alternative,
                raw_policy,
                file_match_policy,
                force_resolution,
                keep_unicode_in_filenames,
            },
            retry: ResolvedRetryConfig {
                max_retries,
                retry_delay_secs,
                max_download_attempts,
            },
            watch: WatchConfig {
                interval: watch_with_interval,
                notify_systemd,
                pid_file,
                reconcile_every_n_cycles,
            },
            notifications: NotificationConfig {
                script: notification_script,
            },
            report: ReportConfig { json: report_json },
            server: ServerConfig {
                port: http_port,
                bind: http_bind,
            },
            ui: UiConfig {
                // Supplied by the caller. Config::build() (used by tests and
                // non-sync commands) defaults to Off/None; sync_loop::run_sync
                // calls build_inner() directly with the resolved Mode from
                // lib.rs's gate (CLI > TOML > default-on-for-TTY, then
                // environmental hard-off check).
                personality_mode,
                friendly_request,
            },
            metadata: MetadataConfig {
                set_exif_datetime,
                set_exif_rating,
                set_exif_gps,
                set_exif_description,
                #[cfg(feature = "xmp")]
                embed_xmp,
                #[cfg(feature = "xmp")]
                xmp_sidecar,
            },
            import: ImportConfig {
                strict: toml_import.and_then(|i| i.strict).unwrap_or(false),
            },
            runtime: RuntimeConfig {
                dry_run: sync.dry_run,
                only_print_filenames: sync.only_print_filenames,
                refresh_metadata: sync.refresh_metadata,
                repair_capture_timestamps: sync.repair_capture_timestamps,
                repair_truncated: sync.repair_truncated,
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{
        PathDerivationCliArgs, resolve_auth, resolve_library_selector, resolve_notify_systemd,
        resolve_path_derivation_fields, smart_retry_delay,
    };
    use crate::config::input::{TomlConfig, TomlFilters, TomlUi};
    use crate::config::paths::GlobalArgs;
    use crate::config::runtime::{Config, CreatedDateFilter, MediaKind};
    use crate::config::templates::{
        DEFAULT_FOLDER_STRUCTURE_ALBUMS, DEFAULT_FOLDER_STRUCTURE_SMART_FOLDERS,
    };
    use crate::config::test_support::{
        assert_album_raw, default_globals, default_password, default_sync, strings,
    };
    use crate::types::{
        Domain, FileMatchPolicy, LivePhotoMode, LivePhotoMovFilenamePolicy, LivePhotoResolution,
        PhotoResolution, RawPolicy,
    };
    use chrono::NaiveDate;
    use std::path::PathBuf;

    #[test]
    fn test_build_defaults_no_toml() {
        let cfg = Config::build(
            &default_globals(),
            &default_password(),
            default_sync(),
            None,
        )
        .unwrap();
        assert_eq!(cfg.auth.username, "u@example.com");
        assert_eq!(cfg.download.threads_num, 10);
        assert_eq!(cfg.download.folder_structure, "%Y/%m/%d");
        assert_eq!(
            cfg.filters.selection.libraries.to_raw(),
            vec!["primary".to_string()]
        );
        assert_eq!(cfg.retry.max_retries, 3);
        assert_eq!(cfg.retry.retry_delay_secs, 5);
        assert_eq!(cfg.download.temp_suffix, ".kei-tmp");
        assert!(matches!(cfg.photos.resolution, PhotoResolution::Original));
        assert!(matches!(cfg.auth.domain, Domain::Com));
    }
    #[test]
    fn test_build_toml_provides_defaults() {
        let toml_str = r#"
            [download]
            threads = 4
            folder_structure = "%Y-%m"

            [filters]
            libraries = ["SharedSync-ABC"]
        "#;
        let toml: TomlConfig = toml::from_str(toml_str).unwrap();
        let cfg = Config::build(
            &default_globals(),
            &default_password(),
            default_sync(),
            Some(&toml),
        )
        .unwrap();
        assert_eq!(cfg.download.threads_num, 4);
        assert_eq!(cfg.download.folder_structure, "%Y-%m");
        assert_eq!(
            cfg.filters.selection.libraries.to_raw(),
            vec!["SharedSync-ABC".to_string()]
        );
    }
    #[test]
    fn test_build_cli_overrides_toml() {
        let toml_str = r#"
            [download]
            threads = 4

            [filters]
            libraries = ["SharedSync-ABC"]
        "#;
        let toml: TomlConfig = toml::from_str(toml_str).unwrap();

        let mut sync = default_sync();
        sync.config_overrides.threads = Some(8);
        sync.config_overrides.libraries = vec!["PrimarySync".to_string()];

        let cfg =
            Config::build(&default_globals(), &default_password(), sync, Some(&toml)).unwrap();
        assert_eq!(cfg.download.threads_num, 8);
        assert_eq!(
            cfg.filters.selection.libraries.to_raw(),
            vec!["PrimarySync".to_string()]
        );
    }
    #[test]
    fn test_library_all_value() {
        let mut sync = default_sync();
        sync.config_overrides.libraries = vec!["all".to_string()];
        let cfg = Config::build(&default_globals(), &default_password(), sync, None).unwrap();
        assert_eq!(
            cfg.filters.selection.libraries.to_raw(),
            vec!["all".to_string()]
        );
    }
    #[test]
    fn test_library_all_case_insensitive() {
        let mut sync = default_sync();
        sync.config_overrides.libraries = vec!["ALL".to_string()];
        let cfg = Config::build(&default_globals(), &default_password(), sync, None).unwrap();
        assert_eq!(
            cfg.filters.selection.libraries.to_raw(),
            vec!["all".to_string()]
        );
    }
    #[test]
    fn test_library_all_from_toml() {
        let toml_str = r#"
            [filters]
            libraries = ["all"]
        "#;
        let toml: TomlConfig = toml::from_str(toml_str).unwrap();
        let cfg = Config::build(
            &default_globals(),
            &default_password(),
            default_sync(),
            Some(&toml),
        )
        .unwrap();
        assert_eq!(
            cfg.filters.selection.libraries.to_raw(),
            vec!["all".to_string()]
        );
    }
    #[test]
    fn config_build_unfiled_bare_flag_resolves_to_true() {
        let mut sync = default_sync();
        sync.config_overrides.unfiled = Some(true);
        let mut globals = default_globals();
        globals.username = Some("u@example.com".to_string());
        let cfg = Config::build(&globals, &default_password(), sync, None).unwrap();
        assert!(
            cfg.filters.selection.unfiled,
            "bare --unfiled must resolve Selection.unfiled = true"
        );
    }
    #[test]
    fn config_build_unfiled_explicit_false_resolves_to_false() {
        let mut sync = default_sync();
        sync.config_overrides.unfiled = Some(false);
        let mut globals = default_globals();
        globals.username = Some("u@example.com".to_string());
        let cfg = Config::build(&globals, &default_password(), sync, None).unwrap();
        assert!(
            !cfg.filters.selection.unfiled,
            "explicit `--unfiled false` must resolve Selection.unfiled = false"
        );
    }
    //
    // Each new CLI flag introduced on this branch (--smart-folder, repeatable
    // --library, --folder-structure-albums, --folder-structure-smart-folders,
    // --unfiled) had a parse test in tests/cli.rs but no test asserting the
    // *runtime effect* through `Config::build`. The tests below drive every
    // flag through `Cli::try_parse_from` -> `Cli::command` ->
    // `Config::build` so a regression in the resolution chain surfaces here
    // before anything reaches CloudKit.

    #[test]
    fn config_build_smart_folder_resolves_to_named_selector() {
        let mut sync = default_sync();
        sync.config_overrides.smart_folders = vec!["Favorites".to_string()];
        let cfg = Config::build(&default_globals(), &default_password(), sync, None).unwrap();
        assert_eq!(
            cfg.filters.selection.smart_folders.to_raw(),
            vec!["Favorites".to_string()]
        );
    }
    #[test]
    fn config_build_smart_folder_all_with_sensitive_resolves() {
        let mut sync = default_sync();
        sync.config_overrides.smart_folders =
            vec!["all-with-sensitive".to_string(), "!Hidden".to_string()];
        let cfg = Config::build(&default_globals(), &default_password(), sync, None).unwrap();
        match cfg.filters.selection.smart_folders {
            crate::selection::SmartFolderSelector::All {
                include_sensitive,
                ref excluded,
            } => {
                assert!(
                    include_sensitive,
                    "all-with-sensitive must set include_sensitive = true"
                );
                assert!(
                    excluded.contains("Hidden"),
                    "!Hidden must land in excluded set"
                );
            }
            other => panic!("expected All variant, got {other:?}"),
        }
    }
    #[test]
    fn config_build_library_repeatable_primary_plus_shared() {
        let mut sync = default_sync();
        sync.config_overrides.libraries = vec!["primary".to_string(), "shared".to_string()];
        let cfg = Config::build(&default_globals(), &default_password(), sync, None).unwrap();
        assert!(
            cfg.filters.selection.libraries.primary,
            "--library primary must set primary = true"
        );
        assert!(
            cfg.filters.selection.libraries.shared_all,
            "--library shared must set shared_all = true"
        );
        // Both sentinels collapse to "all" in `to_raw()`.
        assert_eq!(
            cfg.filters.selection.libraries.to_raw(),
            vec!["all".to_string()],
            "primary + shared must round-trip through `all`"
        );
    }
    #[test]
    fn config_build_library_repeatable_named_zone_with_primary() {
        let mut sync = default_sync();
        sync.config_overrides.libraries =
            vec!["primary".to_string(), "SharedSync-A1B2C3D4".to_string()];
        let cfg = Config::build(&default_globals(), &default_password(), sync, None).unwrap();
        let lib = &cfg.filters.selection.libraries;
        assert!(lib.primary, "primary must remain set");
        assert!(!lib.shared_all, "no `shared` sentinel was passed");
        assert!(
            lib.named.contains("SharedSync-A1B2C3D4"),
            "named zone must land in selector.named, got {:?}",
            lib.named
        );
    }
    #[test]
    fn config_build_folder_structure_albums_resolves_through_cli() {
        let mut sync = default_sync();
        sync.config_overrides.folder_structure_albums = Some("{album}/%Y/%m/%d".to_string());
        let cfg = Config::build(&default_globals(), &default_password(), sync, None).unwrap();
        assert_eq!(cfg.download.folder_structure_albums, "{album}/%Y/%m/%d");
        // Default unfiled / smart-folder templates are untouched.
        assert_eq!(
            cfg.download.folder_structure_smart_folders,
            "{smart-folder}"
        );
    }
    #[test]
    fn config_build_folder_structure_smart_folders_resolves_through_cli() {
        let mut sync = default_sync();
        sync.config_overrides.folder_structure_smart_folders =
            Some("{smart-folder}/%Y".to_string());
        let cfg = Config::build(&default_globals(), &default_password(), sync, None).unwrap();
        assert_eq!(
            cfg.download.folder_structure_smart_folders,
            "{smart-folder}/%Y"
        );
        assert_eq!(cfg.download.folder_structure_albums, "{album}");
    }
    #[test]
    fn config_build_folder_structure_albums_default_is_documented_value() {
        // Per `feedback_default_value_change_test`: pin the *documented*
        // default, not the value Config::build happens to return. The
        // documentation in cli.rs / wiki claims the default is `{album}`;
        // a regression that ships any other default must fail this test.
        let cfg = Config::build(
            &default_globals(),
            &default_password(),
            default_sync(),
            None,
        )
        .unwrap();
        assert_eq!(
            cfg.download.folder_structure_albums, DEFAULT_FOLDER_STRUCTURE_ALBUMS,
            "documented default for --folder-structure-albums is `{{album}}`"
        );
        assert_eq!(
            cfg.download.folder_structure_smart_folders, DEFAULT_FOLDER_STRUCTURE_SMART_FOLDERS,
            "documented default for --folder-structure-smart-folders is `{{smart-folder}}`"
        );
    }
    #[test]
    fn config_build_filters_section_end_to_end_through_toml() {
        // Every new key in `[filters]` -- libraries, smart_folders, unfiled,
        // and the v0.13 `albums` array -- must flow through TomlConfig +
        // Config::build into Config.selection. Pinning all four together
        // catches drop-throughs where one key is read but another is
        // silently ignored.
        let toml_str = r#"
            [filters]
            albums = ["Vacation", "!Drafts"]
            smart_folders = ["Favorites"]
            unfiled = false
            libraries = ["primary", "shared"]
        "#;
        let toml: TomlConfig = toml::from_str(toml_str).unwrap();
        let cfg = Config::build(
            &default_globals(),
            &default_password(),
            default_sync(),
            Some(&toml),
        )
        .unwrap();

        // Albums round-trip via the new selector grammar.
        let raw_albums = cfg.filters.selection.albums.to_raw();
        assert!(
            raw_albums.contains(&"Vacation".to_string()),
            "albums must include Vacation, got {raw_albums:?}"
        );
        assert!(
            raw_albums.contains(&"!Drafts".to_string()),
            "albums must include !Drafts, got {raw_albums:?}"
        );

        // Smart folders.
        assert_eq!(
            cfg.filters.selection.smart_folders.to_raw(),
            vec!["Favorites".to_string()]
        );

        // Unfiled disabled.
        assert!(
            !cfg.filters.selection.unfiled,
            "[filters].unfiled = false must reach Selection.unfiled"
        );

        // Libraries: primary + shared collapses to all.
        assert!(cfg.filters.selection.libraries.primary);
        assert!(cfg.filters.selection.libraries.shared_all);
    }
    #[test]
    fn config_build_toml_folder_structure_keys_resolved() {
        // [download].folder_structure_albums and
        // [download].folder_structure_smart_folders are new TOML keys with
        // no prior runtime test. Pin the read so a serde rename / typo lands
        // red here.
        let toml_str = r#"
            [download]
            folder_structure_albums = "{album}/%Y"
            folder_structure_smart_folders = "{smart-folder}/by-year/%Y"
        "#;
        let toml: TomlConfig = toml::from_str(toml_str).unwrap();
        let cfg = Config::build(
            &default_globals(),
            &default_password(),
            default_sync(),
            Some(&toml),
        )
        .unwrap();
        assert_eq!(cfg.download.folder_structure_albums, "{album}/%Y");
        assert_eq!(
            cfg.download.folder_structure_smart_folders,
            "{smart-folder}/by-year/%Y"
        );
    }
    #[test]
    fn config_build_cli_overrides_toml_folder_structure_keys() {
        // CLI > TOML precedence on the new per-category template keys.
        let toml_str = r#"
            [download]
            folder_structure_albums = "{album}/from-toml"
            folder_structure_smart_folders = "{smart-folder}/from-toml"
        "#;
        let toml: TomlConfig = toml::from_str(toml_str).unwrap();

        let mut sync = default_sync();
        sync.config_overrides.folder_structure_albums = Some("{album}/from-cli".to_string());
        sync.config_overrides.folder_structure_smart_folders =
            Some("{smart-folder}/from-cli".to_string());
        let cfg =
            Config::build(&default_globals(), &default_password(), sync, Some(&toml)).unwrap();
        assert_eq!(cfg.download.folder_structure_albums, "{album}/from-cli");
        assert_eq!(
            cfg.download.folder_structure_smart_folders,
            "{smart-folder}/from-cli"
        );
    }
    #[test]
    fn test_build_hardcoded_default_when_both_absent() {
        let cfg = Config::build(
            &default_globals(),
            &default_password(),
            default_sync(),
            None,
        )
        .unwrap();
        assert_eq!(cfg.download.threads_num, 10);
        assert!(matches!(cfg.photos.raw_policy, RawPolicy::AsIs));
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
                        body.push_str(&format!("threads = {t}\n"));
                    }
                    Some(toml::from_str::<TomlConfig>(&body).unwrap())
                }
            };
            let mut sync = default_sync();
            sync.config_overrides.bandwidth_limit = case.cli;
            sync.config_overrides.threads = case.toml_cli_threads;
            let cfg = Config::build(&default_globals(), &default_password(), sync, toml.as_ref())
                .unwrap_or_else(|e| panic!("{}: build failed: {e}", case.name));
            assert_eq!(
                cfg.download.bandwidth_limit, case.want_limit,
                "{}",
                case.name
            );
            assert_eq!(cfg.download.threads_num, case.want_threads, "{}", case.name);
        }
    }
    #[test]
    fn build_bails_when_album_token_appears_in_smart_folders_template() {
        let mut sync = default_sync();
        sync.config_overrides.folder_structure_smart_folders = Some("{album}/%Y".to_string());
        let err = Config::build(&default_globals(), &default_password(), sync, None)
            .expect_err("`{album}` in --folder-structure-smart-folders must bail");
        let msg = err.to_string();
        assert!(
            msg.contains("--folder-structure-smart-folders")
                && msg.contains("--folder-structure-albums"),
            "error should name both flags: {msg}"
        );
    }
    #[test]
    fn build_bails_when_library_token_misplaced() {
        let mut sync = default_sync();
        sync.config_overrides.folder_structure_albums = Some("{album}/{library}".to_string());
        let err = Config::build(&default_globals(), &default_password(), sync, None)
            .expect_err("`{library}` not as first segment must bail");
        assert!(
            err.to_string().contains("`{library}` must be the first"),
            "{err}"
        );
    }
    #[test]
    fn build_accepts_library_album_pair_in_albums_template() {
        let mut sync = default_sync();
        sync.config_overrides.folder_structure_albums = Some("{library}/{album}/%Y".to_string());
        let cfg = Config::build(&default_globals(), &default_password(), sync, None)
            .expect("`{library}/{album}/...` is a valid albums template");
        assert_eq!(cfg.download.folder_structure_albums, "{library}/{album}/%Y");
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
            &default_password(),
            default_sync(),
            Some(&toml),
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
            [metadata]
            set_exif_datetime = true

            [filters]
            media = ["photos", "live-photos"]
        "#;
        let toml: TomlConfig = toml::from_str(toml_str).unwrap();
        let cfg = Config::build(
            &default_globals(),
            &default_password(),
            default_sync(),
            Some(&toml),
        )
        .unwrap();
        assert!(cfg.metadata.set_exif_datetime);
        assert!(cfg.filters.skip_videos);
    }
    #[cfg(feature = "xmp")]
    #[test]
    fn test_build_embed_xmp_and_sidecar_from_toml() {
        let toml_str = r#"
            [metadata]
            embed_xmp = true
            xmp_sidecar = true
        "#;
        let toml: TomlConfig = toml::from_str(toml_str).unwrap();
        let cfg = Config::build(
            &default_globals(),
            &default_password(),
            default_sync(),
            Some(&toml),
        )
        .unwrap();
        assert!(cfg.metadata.embed_xmp);
        assert!(cfg.metadata.xmp_sidecar);
    }
    #[cfg(feature = "xmp")]
    #[test]
    fn test_cli_embed_xmp_overrides_toml() {
        let toml_str = r#"
            [metadata]
            embed_xmp = true
        "#;
        let toml: TomlConfig = toml::from_str(toml_str).unwrap();
        let mut sync = default_sync();
        sync.config_overrides.embed_xmp = Some(false);
        let cfg =
            Config::build(&default_globals(), &default_password(), sync, Some(&toml)).unwrap();
        assert!(
            !cfg.metadata.embed_xmp,
            "--embed-xmp=false must override TOML embed_xmp = true"
        );
    }
    #[cfg(feature = "xmp")]
    #[test]
    fn test_embed_xmp_default_false_when_unset() {
        let cfg = Config::build(
            &default_globals(),
            &default_password(),
            default_sync(),
            None,
        )
        .unwrap();
        assert!(!cfg.metadata.embed_xmp);
        assert!(!cfg.metadata.xmp_sidecar);
    }
    #[test]
    fn test_build_cli_flag_overrides_toml_false() {
        let toml_str = r#"
            [filters]
            media = ["photos", "videos", "live-photos"]
        "#;
        let toml: TomlConfig = toml::from_str(toml_str).unwrap();
        let mut sync = default_sync();
        sync.config_overrides.skip_videos = Some(true);
        let cfg =
            Config::build(&default_globals(), &default_password(), sync, Some(&toml)).unwrap();
        assert!(cfg.filters.skip_videos);
    }
    #[test]
    fn test_build_cli_false_overrides_toml_true() {
        let toml_str = r#"
            [filters]
            media = ["photos", "live-photos"]
        "#;
        let toml: TomlConfig = toml::from_str(toml_str).unwrap();
        let mut sync = default_sync();
        sync.config_overrides.skip_videos = Some(false);
        let cfg =
            Config::build(&default_globals(), &default_password(), sync, Some(&toml)).unwrap();
        assert!(
            !cfg.filters.skip_videos,
            "CLI --skip-videos false should override TOML true"
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
            &default_password(),
            default_sync(),
            Some(&toml),
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
        let cfg = Config::build(&globals, &pw, default_sync(), Some(&toml)).unwrap();
        assert_eq!(cfg.auth.username, "toml@example.com");
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
            &default_password(),
            default_sync(),
            Some(&toml),
        )
        .unwrap();
        assert_eq!(cfg.auth.username, "u@example.com");
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
            &default_password(),
            default_sync(),
            Some(&toml),
        )
        .unwrap();
        assert_album_raw(&cfg, &["Favorites", "Vacation"]);
    }
    #[test]
    fn test_build_cli_albums_override_toml() {
        let toml_str = r#"
            [filters]
            albums = ["Favorites"]
        "#;
        let toml: TomlConfig = toml::from_str(toml_str).unwrap();
        let mut sync = default_sync();
        sync.config_overrides.albums = vec!["Screenshots".to_string()];
        let cfg =
            Config::build(&default_globals(), &default_password(), sync, Some(&toml)).unwrap();
        assert_album_raw(&cfg, &["Screenshots"]);
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
            &default_password(),
            default_sync(),
            Some(&toml),
        )
        .unwrap();
        assert_eq!(cfg.watch.interval, Some(1800));
        assert_eq!(cfg.watch.pid_file, Some(PathBuf::from("/run/test.pid")));
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
                &default_password(),
                default_sync(),
                Some(&toml),
            );
            assert!(result.is_err(), "interval {interval} should be rejected");
            assert!(
                result.unwrap_err().to_string().contains("watch interval"),
                "Error should mention watch interval"
            );
        }
    }
    #[test]
    fn test_build_max_retries_above_upper_bound_from_toml_rejected() {
        let toml_str = r#"
            [download.retry]
            per_transfer = 9999
        "#;
        let toml: TomlConfig = toml::from_str(toml_str).unwrap();
        let result = Config::build(
            &default_globals(),
            &default_password(),
            default_sync(),
            Some(&toml),
        );
        assert!(result.is_err(), "TOML per_transfer > 100 must be rejected");
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("per_transfer") && msg.contains("100"),
            "Error should mention per_transfer and the bound: {msg}"
        );
    }
    #[test]
    fn test_build_retry_clamp_accepts_upper_bound_from_toml() {
        let toml_str = r#"
            [download.retry]
            per_transfer = 100
        "#;
        let toml: TomlConfig = toml::from_str(toml_str).unwrap();
        let cfg = Config::build(
            &default_globals(),
            &default_password(),
            default_sync(),
            Some(&toml),
        )
        .expect("per_transfer=100 must be accepted");
        assert_eq!(cfg.retry.max_retries, 100);
        assert_eq!(cfg.retry.retry_delay_secs, 30);
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
        let result = Config::build(&globals, &pw, default_sync(), Some(&toml));
        assert!(result.is_err());
        assert!(
            result.unwrap_err().to_string().contains("username"),
            "Error should mention username"
        );
    }
    #[test]
    fn test_build_toml_password_rejected_nonempty() {
        let toml_str = r#"
            [auth]
            password = "secret"
        "#;
        let toml: TomlConfig = toml::from_str(toml_str).unwrap();
        let result = Config::build(
            &default_globals(),
            &default_password(),
            default_sync(),
            Some(&toml),
        );
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("`[auth].password`"),
            "Error should name the rejected field; got: {err}"
        );
        assert!(
            err.contains("kei password set"),
            "Error should point at the credential store; got: {err}"
        );
    }
    #[test]
    fn test_build_toml_password_rejected_empty() {
        let toml_str = r#"
            [auth]
            password = ""
        "#;
        let toml: TomlConfig = toml::from_str(toml_str).unwrap();
        let result = Config::build(
            &default_globals(),
            &default_password(),
            default_sync(),
            Some(&toml),
        );
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("`[auth].password`"),
            "Error should name the rejected field even for empty values"
        );
    }
    #[test]
    fn test_build_toml_password_rejected_even_with_cli_password() {
        // A CLI password does NOT rescue a TOML password; the TOML field is
        // rejected on its own.
        let toml_str = r#"
            [auth]
            password = "toml-pw"
        "#;
        let toml: TomlConfig = toml::from_str(toml_str).unwrap();
        let mut pw = default_password();
        pw.password = Some("cli-pw".to_string());
        let result = Config::build(&default_globals(), &pw, default_sync(), Some(&toml));
        assert!(result.is_err());
    }
    #[cfg(windows)]
    #[test]
    fn test_build_password_command_rejected_on_windows() {
        // `--password-command` requires `sh -c`, which isn't on a stock
        // Windows PATH. `Config::build` must reject at startup instead of
        // punting the failure to the first auth attempt.
        let mut pw = default_password();
        pw.password_command = Some("echo anything".to_string());
        let result = Config::build(&default_globals(), &pw, default_sync(), None);
        let err = result.unwrap_err().to_string();
        assert!(err.contains("not supported on Windows"), "{err}");
        assert!(err.contains("--password-file"), "{err}");
    }
    #[cfg(windows)]
    #[test]
    fn test_build_toml_password_command_rejected_on_windows() {
        let toml_str = r#"
            [auth]
            password_command = "echo anything"
        "#;
        let toml: TomlConfig = toml::from_str(toml_str).unwrap();
        let result = Config::build(
            &default_globals(),
            &default_password(),
            default_sync(),
            Some(&toml),
        );
        let err = result.unwrap_err().to_string();
        assert!(err.contains("not supported on Windows"), "{err}");
    }
    #[test]
    fn test_build_download_dir_from_cli() {
        let mut sync = default_sync();
        sync.config_overrides.download_dir = Some("/photos/new".to_string());
        let cfg = Config::build(&default_globals(), &default_password(), sync, None).unwrap();
        assert_eq!(cfg.download.directory, PathBuf::from("/photos/new"));
    }
    #[test]
    fn test_build_download_dir_cli_beats_toml() {
        let toml_str = r#"
            [download]
            directory = "/photos/toml"
        "#;
        let toml: TomlConfig = toml::from_str(toml_str).unwrap();
        let mut sync = default_sync();
        sync.config_overrides.download_dir = Some("/photos/cli".to_string());
        let cfg =
            Config::build(&default_globals(), &default_password(), sync, Some(&toml)).unwrap();
        assert_eq!(cfg.download.directory, PathBuf::from("/photos/cli"));
    }
    #[test]
    fn test_build_toml_directory_unchanged() {
        // The TOML key stays `[download].directory` - we didn't rename it,
        // only the CLI flag. Make sure nothing else broke.
        let toml_str = r#"
            [download]
            directory = "/photos/via-toml"
        "#;
        let toml: TomlConfig = toml::from_str(toml_str).unwrap();
        let cfg = Config::build(
            &default_globals(),
            &default_password(),
            default_sync(),
            Some(&toml),
        )
        .unwrap();
        assert_eq!(cfg.download.directory, PathBuf::from("/photos/via-toml"));
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
            &default_password(),
            default_sync(),
            Some(&toml),
        )
        .unwrap();
        assert!(cfg.filters.skip_created_before.is_some());
        assert!(cfg.filters.skip_created_after.is_some());
    }
    #[test]
    fn test_toml_auth_password_still_parses() {
        // The field stays on `TomlAuth` so `Config::build()` can detect and
        // reject it with a targeted migration message rather than a generic
        // "unknown field" error from serde's `deny_unknown_fields`.
        let toml_str = r#"
            [auth]
            password = "secret"
        "#;
        let config: TomlConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.auth.unwrap().password.as_deref(), Some("secret"));
    }
    #[test]
    fn test_toml_server_port_resolves_in_config() {
        let toml_str = r#"
            [auth]
            username = "user@example.com"
            [download]
            directory = "/photos"
            [server]
            port = 9090
        "#;
        let toml: TomlConfig = toml::from_str(toml_str).unwrap();
        let config = Config::build(
            &default_globals(),
            &default_password(),
            default_sync(),
            Some(&toml),
        )
        .unwrap();
        assert_eq!(config.server.port, 9090);
    }
    #[test]
    fn test_toml_server_port_zero_requests_ephemeral_bind() {
        let toml_str = r#"
            [auth]
            username = "user@example.com"
            [download]
            directory = "/photos"
            [server]
            port = 0
        "#;
        let toml: TomlConfig = toml::from_str(toml_str).unwrap();
        let config = Config::build(
            &default_globals(),
            &default_password(),
            default_sync(),
            Some(&toml),
        )
        .unwrap();
        assert_eq!(config.server.port, 0);
    }
    #[test]
    fn test_cli_http_port_overrides_toml() {
        let toml_str = r#"
            [auth]
            username = "user@example.com"
            [download]
            directory = "/photos"
            [server]
            port = 9090
        "#;
        let toml: TomlConfig = toml::from_str(toml_str).unwrap();
        let mut sync = default_sync();
        sync.config_overrides.http_port = Some(8080);
        let config =
            Config::build(&default_globals(), &default_password(), sync, Some(&toml)).unwrap();
        assert_eq!(config.server.port, 8080);
    }
    #[test]
    fn test_default_http_port() {
        // Without any explicit config, http_port should be 9090.
        let config = Config::build(
            &default_globals(),
            &default_password(),
            default_sync(),
            None,
        )
        .unwrap();
        assert_eq!(config.server.port, 9090);
    }
    #[test]
    fn test_default_http_bind_is_all_interfaces() {
        // The historical default. Kept so Docker's `-p 9090:9090` works out
        // of the box without an extra flag.
        let config = Config::build(
            &default_globals(),
            &default_password(),
            default_sync(),
            None,
        )
        .unwrap();
        assert_eq!(
            config.server.bind,
            std::net::IpAddr::V4(std::net::Ipv4Addr::new(0, 0, 0, 0)),
        );
    }
    #[test]
    fn test_http_bind_from_toml() {
        let toml_str = r#"
            [server]
            bind = "127.0.0.1"
        "#;
        let toml: TomlConfig = toml::from_str(toml_str).unwrap();
        let config = Config::build(
            &default_globals(),
            &default_password(),
            default_sync(),
            Some(&toml),
        )
        .unwrap();
        assert_eq!(
            config.server.bind,
            std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
        );
    }
    #[test]
    fn test_http_bind_cli_overrides_toml() {
        let toml_str = r#"
            [server]
            bind = "0.0.0.0"
        "#;
        let toml: TomlConfig = toml::from_str(toml_str).unwrap();
        let mut sync = default_sync();
        sync.config_overrides.http_bind = Some(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST));
        let config =
            Config::build(&default_globals(), &default_password(), sync, Some(&toml)).unwrap();
        assert_eq!(
            config.server.bind,
            std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
        );
    }
    #[test]
    fn test_http_bind_accepts_ipv6() {
        let toml_str = r#"
            [server]
            bind = "::1"
        "#;
        let toml: TomlConfig = toml::from_str(toml_str).unwrap();
        let config = Config::build(
            &default_globals(),
            &default_password(),
            default_sync(),
            Some(&toml),
        )
        .unwrap();
        assert_eq!(
            config.server.bind,
            std::net::IpAddr::V6(std::net::Ipv6Addr::LOCALHOST),
        );
    }
    #[test]
    fn test_http_bind_invalid_string_errors() {
        let toml_str = r#"
            [server]
            bind = "not-an-ip"
        "#;
        let toml: TomlConfig = toml::from_str(toml_str).unwrap();
        let err = Config::build(
            &default_globals(),
            &default_password(),
            default_sync(),
            Some(&toml),
        )
        .expect_err("invalid IP in [server] bind must error at build time");
        let msg = format!("{err}");
        assert!(msg.contains("bind"), "{msg}");
        assert!(msg.contains("not-an-ip"), "{msg}");
    }
    #[test]
    fn test_build_all_defaults_no_toml_exhaustive() {
        let cfg = Config::build(
            &default_globals(),
            &default_password(),
            default_sync(),
            None,
        )
        .unwrap();
        // Auth
        assert_eq!(cfg.auth.username, "u@example.com");
        assert!(cfg.auth.password.is_none());
        assert!(matches!(cfg.auth.domain, Domain::Com));
        assert!(cfg.auth.cookie_directory.ends_with("kei/cookies"));
        // Download
        assert_eq!(cfg.download.folder_structure, "%Y/%m/%d");
        assert_eq!(cfg.download.threads_num, 10);
        assert_eq!(cfg.download.temp_suffix, ".kei-tmp");
        assert!(!cfg.metadata.set_exif_datetime);
        assert!(!cfg.download.no_progress_bar);
        // Retry
        assert_eq!(cfg.retry.max_retries, 3);
        assert_eq!(cfg.retry.retry_delay_secs, 5);
        // Filters
        assert_eq!(
            cfg.filters.selection.libraries.to_raw(),
            vec!["primary".to_string()]
        );
        assert_album_raw(&cfg, &["all"]);
        assert!(
            cfg.filters.selection.unfiled,
            "v0.13 default: unfiled = true"
        );
        assert!(!cfg.filters.skip_videos);
        assert!(!cfg.filters.skip_photos);
        assert_eq!(cfg.photos.live_photo_mode, LivePhotoMode::Both);
        assert!(cfg.filters.recent.is_none());
        assert!(cfg.filters.skip_created_before.is_none());
        assert!(cfg.filters.skip_created_after.is_none());
        // Photos
        assert!(matches!(cfg.photos.resolution, PhotoResolution::Original));
        assert!(matches!(
            cfg.photos.live_resolution,
            LivePhotoResolution::Original
        ));
        assert!(matches!(
            cfg.photos.live_photo_mov_filename_policy,
            LivePhotoMovFilenamePolicy::Suffix
        ));
        assert!(matches!(cfg.photos.raw_policy, RawPolicy::AsIs));
        assert!(matches!(
            cfg.photos.file_match_policy,
            FileMatchPolicy::NameSizeDedupWithSuffix
        ));
        assert!(!cfg.photos.force_resolution);
        assert!(!cfg.photos.keep_unicode_in_filenames);
        // Watch
        assert!(cfg.watch.interval.is_none());
        assert!(!cfg.watch.notify_systemd);
        assert!(cfg.watch.pid_file.is_none());
        // Misc
        assert!(!cfg.runtime.dry_run);
        assert!(!cfg.runtime.only_print_filenames);
        assert!(!cfg.runtime.refresh_metadata);
        assert!(!cfg.runtime.repair_capture_timestamps);
        // Notifications
        assert!(cfg.notifications.script.is_none());
    }
    #[test]
    fn test_build_carries_capture_timestamp_repair_request() {
        let mut sync = default_sync();
        sync.refresh_metadata = true;
        sync.repair_capture_timestamps = true;
        let cfg = Config::build(&default_globals(), &default_password(), sync, None).unwrap();

        assert!(cfg.runtime.refresh_metadata);
        assert!(cfg.runtime.repair_capture_timestamps);
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
        let cfg = Config::build(&globals, &pw, default_sync(), Some(&toml)).unwrap();
        assert!(matches!(cfg.auth.domain, Domain::Com));
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
            &default_password(),
            default_sync(),
            Some(&toml),
        )
        .unwrap();
        assert!(matches!(cfg.auth.domain, Domain::Cn));
    }
    /// Escape backslashes for embedding a path in a TOML string literal.
    #[test]
    fn test_build_directory_tilde_expansion() {
        let toml_str = r#"
            [download]
            directory = "~/photos"
        "#;
        let toml: TomlConfig = toml::from_str(toml_str).unwrap();
        let cfg = Config::build(
            &default_globals(),
            &default_password(),
            default_sync(),
            Some(&toml),
        )
        .unwrap();
        if let Some(home) = dirs::home_dir() {
            assert_eq!(cfg.download.directory, home.join("photos"));
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
        sync.config_overrides.folder_structure = Some("%Y/%m/%d".to_string());
        let cfg =
            Config::build(&default_globals(), &default_password(), sync, Some(&toml)).unwrap();
        assert_eq!(cfg.download.folder_structure, "%Y/%m/%d");
    }
    #[test]
    fn test_build_temp_suffix_cli_overrides_toml() {
        let toml_str = r#"
            [download]
            temp_suffix = ".toml-tmp"
        "#;
        let toml: TomlConfig = toml::from_str(toml_str).unwrap();
        let mut sync = default_sync();
        sync.config_overrides.temp_suffix = Some(".cli-tmp".to_string());
        let cfg =
            Config::build(&default_globals(), &default_password(), sync, Some(&toml)).unwrap();
        assert_eq!(cfg.download.temp_suffix, ".cli-tmp");
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
            &default_password(),
            default_sync(),
            Some(&toml),
        )
        .unwrap();
        assert_eq!(cfg.download.temp_suffix, ".downloading");
    }
    #[test]
    fn test_build_max_retries_cli_overrides_toml() {
        let toml_str = r#"
            [download.retry]
            per_transfer = 5
        "#;
        let toml: TomlConfig = toml::from_str(toml_str).unwrap();
        let mut sync = default_sync();
        sync.config_overrides.max_retries = Some(10);
        let cfg =
            Config::build(&default_globals(), &default_password(), sync, Some(&toml)).unwrap();
        assert_eq!(cfg.retry.max_retries, 10);
    }
    #[test]
    fn test_build_max_download_attempts_default() {
        // Neither CLI nor TOML set: hardcoded fallback of 10 fires.
        let cfg = Config::build(
            &default_globals(),
            &default_password(),
            default_sync(),
            None,
        )
        .unwrap();
        assert_eq!(cfg.retry.max_download_attempts, 10);
    }
    #[test]
    fn test_build_max_download_attempts_from_toml() {
        // TOML-only: resolved value matches the TOML setting.
        let toml_str = r#"
            [download.retry]
            per_asset = 25
        "#;
        let toml: TomlConfig = toml::from_str(toml_str).unwrap();
        let cfg = Config::build(
            &default_globals(),
            &default_password(),
            default_sync(),
            Some(&toml),
        )
        .unwrap();
        assert_eq!(cfg.retry.max_download_attempts, 25);
    }
    #[test]
    fn test_build_max_download_attempts_cli_overrides_toml() {
        // CLI flag wins over TOML, mirroring every other resolved value.
        let toml_str = r#"
            [download.retry]
            per_asset = 25
        "#;
        let toml: TomlConfig = toml::from_str(toml_str).unwrap();
        let mut sync = default_sync();
        sync.config_overrides.max_download_attempts = Some(7);
        let cfg =
            Config::build(&default_globals(), &default_password(), sync, Some(&toml)).unwrap();
        assert_eq!(cfg.retry.max_download_attempts, 7);
    }
    #[test]
    fn test_build_max_download_attempts_zero_disables_cap() {
        // `0` is the documented "disable the cap" sentinel; resolution
        // must accept it through TOML the same as through CLI.
        let toml_str = r#"
            [download.retry]
            per_asset = 0
        "#;
        let toml: TomlConfig = toml::from_str(toml_str).unwrap();
        let cfg = Config::build(
            &default_globals(),
            &default_password(),
            default_sync(),
            Some(&toml),
        )
        .unwrap();
        assert_eq!(cfg.retry.max_download_attempts, 0);
    }
    #[test]
    fn test_build_resolution_cli_overrides_toml() {
        let toml_str = r#"
            [photos]
            resolution = "thumb"
        "#;
        let toml: TomlConfig = toml::from_str(toml_str).unwrap();
        let mut sync = default_sync();
        sync.config_overrides.resolution = Some(PhotoResolution::Medium);
        let cfg =
            Config::build(&default_globals(), &default_password(), sync, Some(&toml)).unwrap();
        assert!(matches!(cfg.photos.resolution, PhotoResolution::Medium));
    }
    #[test]
    fn test_build_resolution_from_toml() {
        let toml_str = r#"
            [photos]
            resolution = "thumb"
        "#;
        let toml: TomlConfig = toml::from_str(toml_str).unwrap();
        let cfg = Config::build(
            &default_globals(),
            &default_password(),
            default_sync(),
            Some(&toml),
        )
        .unwrap();
        assert!(matches!(cfg.photos.resolution, PhotoResolution::Thumb));
    }
    #[test]
    fn test_build_live_resolution_cli_overrides_toml() {
        let toml_str = r#"
            [photos]
            live_resolution = "thumb"
        "#;
        let toml: TomlConfig = toml::from_str(toml_str).unwrap();
        let mut sync = default_sync();
        sync.config_overrides.live_resolution = Some(LivePhotoResolution::Medium);
        let cfg =
            Config::build(&default_globals(), &default_password(), sync, Some(&toml)).unwrap();
        assert!(matches!(
            cfg.photos.live_resolution,
            LivePhotoResolution::Medium
        ));
    }
    #[test]
    fn test_build_live_resolution_from_toml() {
        let toml_str = r#"
            [photos]
            live_resolution = "thumb"
        "#;
        let toml: TomlConfig = toml::from_str(toml_str).unwrap();
        let cfg = Config::build(
            &default_globals(),
            &default_password(),
            default_sync(),
            Some(&toml),
        )
        .unwrap();
        assert!(matches!(
            cfg.photos.live_resolution,
            LivePhotoResolution::Thumb
        ));
    }
    #[test]
    fn test_build_edited_keeps_live_resolution_default() {
        let mut sync = default_sync();
        sync.config_overrides.resolution = Some(PhotoResolution::Original);
        sync.config_overrides.edited = Some(true);
        let cfg = Config::build(&default_globals(), &default_password(), sync, None).unwrap();
        assert!(matches!(
            cfg.photos.live_resolution,
            LivePhotoResolution::Original
        ));
    }
    #[test]
    fn test_build_live_resolution_explicit_overrides_edited_default() {
        let mut sync = default_sync();
        sync.config_overrides.resolution = Some(PhotoResolution::Original);
        sync.config_overrides.edited = Some(true);
        sync.config_overrides.live_resolution = Some(LivePhotoResolution::Original);
        let cfg = Config::build(&default_globals(), &default_password(), sync, None).unwrap();
        assert!(matches!(
            cfg.photos.live_resolution,
            LivePhotoResolution::Original
        ));
    }
    #[test]
    fn test_build_mov_filename_policy_cli_overrides_toml() {
        let toml_str = r#"
            [photos]
            live_photo_mov_filename_policy = "original"
        "#;
        let toml: TomlConfig = toml::from_str(toml_str).unwrap();
        let mut sync = default_sync();
        sync.config_overrides.live_photo_mov_filename_policy =
            Some(LivePhotoMovFilenamePolicy::Suffix);
        let cfg =
            Config::build(&default_globals(), &default_password(), sync, Some(&toml)).unwrap();
        assert!(matches!(
            cfg.photos.live_photo_mov_filename_policy,
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
            &default_password(),
            default_sync(),
            Some(&toml),
        )
        .unwrap();
        assert!(matches!(
            cfg.photos.live_photo_mov_filename_policy,
            LivePhotoMovFilenamePolicy::Original
        ));
    }
    #[test]
    fn test_build_raw_policy_cli_overrides_toml() {
        let toml_str = r#"
            [photos]
            raw_policy = "prefer-raw"
        "#;
        let toml: TomlConfig = toml::from_str(toml_str).unwrap();
        let mut sync = default_sync();
        sync.config_overrides.raw_policy = Some(RawPolicy::PreferJpeg);
        let cfg =
            Config::build(&default_globals(), &default_password(), sync, Some(&toml)).unwrap();
        assert!(matches!(cfg.photos.raw_policy, RawPolicy::PreferJpeg));
    }
    #[test]
    fn test_build_file_match_policy_cli_overrides_toml() {
        let toml_str = r#"
            [photos]
            file_match_policy = "name-id7"
        "#;
        let toml: TomlConfig = toml::from_str(toml_str).unwrap();
        let mut sync = default_sync();
        sync.config_overrides.file_match_policy = Some(FileMatchPolicy::NameSizeDedupWithSuffix);
        let cfg =
            Config::build(&default_globals(), &default_password(), sync, Some(&toml)).unwrap();
        assert!(matches!(
            cfg.photos.file_match_policy,
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
            &default_password(),
            default_sync(),
            Some(&toml),
        )
        .unwrap();
        assert!(matches!(
            cfg.photos.file_match_policy,
            FileMatchPolicy::NameId7
        ));
    }
    /// Defaults are wired so a caller passing all-`None` CLI args and no
    /// TOML lands on the documented defaults — this is the no-flags path
    /// both `sync` and `import-existing` follow when invoked bare.
    #[test]
    fn resolve_path_derivation_all_defaults() {
        let pd = resolve_path_derivation_fields(PathDerivationCliArgs::default(), None).unwrap();
        assert_eq!(pd.folder_structure, "%Y/%m/%d");
        assert_eq!(pd.resolution, PhotoResolution::Original);
        assert_eq!(pd.live_photo_mode, LivePhotoMode::Both);
        assert_eq!(pd.live_resolution, LivePhotoResolution::Original);
        assert_eq!(
            pd.live_photo_mov_filename_policy,
            LivePhotoMovFilenamePolicy::Suffix
        );
        assert_eq!(pd.raw_policy, RawPolicy::AsIs);
        assert_eq!(
            pd.file_match_policy,
            FileMatchPolicy::NameSizeDedupWithSuffix
        );
        assert!(!pd.force_resolution);
        assert!(!pd.keep_unicode_in_filenames);
    }
    /// `edited = true` is additive in v0.20: it does not rewrite the
    /// primary or live-photo resolution. Adjusted stills and videos are
    /// selected later as extra renditions by the download filter.
    #[test]
    fn resolve_path_derivation_edited_keeps_live_resolution_default() {
        let cli = PathDerivationCliArgs {
            resolution: Some(PhotoResolution::Original),
            edited: Some(true),
            ..Default::default()
        };
        let pd = resolve_path_derivation_fields(cli, None).unwrap();
        assert_eq!(pd.resolution, PhotoResolution::Original);
        assert_eq!(
            pd.live_resolution,
            LivePhotoResolution::Original,
            "edited extras must not rewrite live_resolution"
        );
    }
    /// Explicit live resolution still wins when edited extras are enabled.
    #[test]
    fn resolve_path_derivation_explicit_live_resolution_beats_toml() {
        let cli = PathDerivationCliArgs {
            resolution: Some(PhotoResolution::Original),
            edited: Some(true),
            live_resolution: Some(LivePhotoResolution::Original),
            ..Default::default()
        };
        let pd = resolve_path_derivation_fields(cli, None).unwrap();
        assert_eq!(pd.live_resolution, LivePhotoResolution::Original);
    }
    /// CLI > TOML > default. The resolver short-circuits on the first
    /// `Some`; this confirms we don't double-resolve and accidentally
    /// fall through to the TOML value when the CLI already chose.
    #[test]
    fn resolve_path_derivation_cli_beats_toml() {
        let toml: TomlConfig = toml::from_str(
            r#"
            [photos]
            resolution = "original"
            edited = true
            file_match_policy = "name-id7"
            force_resolution = true
            keep_unicode_in_filenames = true
            raw_policy = "prefer-raw"
            "#,
        )
        .unwrap();
        let cli = PathDerivationCliArgs {
            resolution: Some(PhotoResolution::Original),
            file_match_policy: Some(FileMatchPolicy::NameSizeDedupWithSuffix),
            force_resolution: Some(false),
            keep_unicode_in_filenames: Some(false),
            raw_policy: Some(RawPolicy::AsIs),
            ..Default::default()
        };
        let pd = resolve_path_derivation_fields(cli, Some(&toml)).unwrap();
        assert_eq!(pd.resolution, PhotoResolution::Original);
        assert_eq!(
            pd.file_match_policy,
            FileMatchPolicy::NameSizeDedupWithSuffix
        );
        assert!(!pd.force_resolution);
        assert!(!pd.keep_unicode_in_filenames);
        assert_eq!(pd.raw_policy, RawPolicy::AsIs);
    }
    #[test]
    fn config_build_uses_path_derivation_overlay_once() {
        let toml: TomlConfig = toml::from_str(
            r#"
            [download]
            folder_structure = "%Y/%m"
            folder_structure_albums = "{album}/toml"
            folder_structure_smart_folders = "{smart-folder}/toml"

            [photos]
            resolution = "medium"
            live_photo_mode = "skip"
            live_resolution = "thumb"
            raw_policy = "prefer-raw"
            "#,
        )
        .unwrap();
        let mut sync = default_sync();
        sync.config_overrides.folder_structure_albums = Some("{album}/cli".to_string());
        sync.config_overrides.live_photo_mode = Some(LivePhotoMode::ImageOnly);

        let cfg =
            Config::build(&default_globals(), &default_password(), sync, Some(&toml)).unwrap();

        assert_eq!(cfg.download.folder_structure, "%Y/%m");
        assert_eq!(cfg.download.folder_structure_albums, "{album}/cli");
        assert_eq!(
            cfg.download.folder_structure_smart_folders,
            "{smart-folder}/toml"
        );
        assert_eq!(cfg.photos.resolution, PhotoResolution::Medium);
        assert_eq!(cfg.photos.live_photo_mode, LivePhotoMode::ImageOnly);
        assert_eq!(cfg.photos.live_resolution, LivePhotoResolution::Thumb);
        assert_eq!(cfg.photos.raw_policy, RawPolicy::PreferRaw);
    }
    /// TOML wins when the CLI is silent. Pin this so anyone refactoring
    /// the resolver doesn't accidentally swallow TOML defaults.
    #[test]
    fn resolve_path_derivation_toml_when_cli_absent() {
        let toml: TomlConfig = toml::from_str(
            r#"
            [download]
            folder_structure = "%Y/%m"

            [photos]
            resolution = "original"
            edited = true
            live_photo_mode = "skip"
            file_match_policy = "name-id7"
            "#,
        )
        .unwrap();
        let pd =
            resolve_path_derivation_fields(PathDerivationCliArgs::default(), Some(&toml)).unwrap();
        assert_eq!(pd.folder_structure, "%Y/%m");
        assert_eq!(pd.resolution, PhotoResolution::Original);
        assert_eq!(pd.live_photo_mode, LivePhotoMode::Skip);
        assert_eq!(pd.file_match_policy, FileMatchPolicy::NameId7);
        // Edited extras do not rewrite the live-photo resolution.
        assert_eq!(pd.live_resolution, LivePhotoResolution::Original);
    }
    #[test]
    fn resolve_path_derivation_rejects_resolution_none_without_extras() {
        let toml: TomlConfig = toml::from_str(
            r#"
            [photos]
            resolution = "none"
            "#,
        )
        .unwrap();
        let err = resolve_path_derivation_fields(PathDerivationCliArgs::default(), Some(&toml))
            .expect_err("resolution none needs an extra");
        assert!(
            err.to_string().contains("requires [photos].edited"),
            "unexpected error: {err}"
        );
    }
    #[test]
    fn test_build_all_boolean_flags_from_toml() {
        // `media` replaces the old durable skip booleans. This keeps the
        // test anchored to the TOML-first filter surface while still checking
        // the derived runtime skip flags.
        let toml_str = r#"
            [metadata]
            set_exif_datetime = true

            [ui]
            progress_bar = false

            [filters]
            media = ["photos", "live-photos"]

            [photos]
            live_photo_mode = "skip"
            force_resolution = true
            keep_unicode_in_filenames = true

            [watch]
            notify_systemd = true
        "#;
        let toml: TomlConfig = toml::from_str(toml_str).unwrap();
        let cfg = Config::build(
            &default_globals(),
            &default_password(),
            default_sync(),
            Some(&toml),
        )
        .unwrap();
        assert!(cfg.metadata.set_exif_datetime);
        assert!(cfg.download.no_progress_bar);
        assert!(cfg.filters.skip_videos);
        assert!(!cfg.filters.skip_photos);
        assert_eq!(cfg.photos.live_photo_mode, LivePhotoMode::Skip);
        assert!(cfg.photos.force_resolution);
        assert!(cfg.photos.keep_unicode_in_filenames);
        assert!(cfg.watch.notify_systemd);
    }
    #[test]
    fn test_build_all_boolean_flags_cli_overrides() {
        // Programmatic skip overrides still feed the same media owner type
        // for tests and internal call sites.
        let mut sync = default_sync();
        sync.config_overrides.set_exif_datetime = Some(true);
        sync.no_progress_bar = true;
        sync.config_overrides.skip_videos = Some(true);
        sync.config_overrides.live_photo_mode = Some(LivePhotoMode::Skip);
        sync.config_overrides.force_resolution = Some(true);
        sync.config_overrides.keep_unicode_in_filenames = Some(true);
        sync.config_overrides.notify_systemd = Some(true);
        let cfg = Config::build(&default_globals(), &default_password(), sync, None).unwrap();
        assert!(cfg.metadata.set_exif_datetime);
        assert!(cfg.download.no_progress_bar);
        assert!(cfg.filters.skip_videos);
        assert!(!cfg.filters.skip_photos);
        assert_eq!(cfg.photos.live_photo_mode, LivePhotoMode::Skip);
        assert!(cfg.photos.force_resolution);
        assert!(cfg.photos.keep_unicode_in_filenames);
        assert!(cfg.watch.notify_systemd);
    }
    #[test]
    fn test_build_boolean_flags_false_in_toml_stays_false() {
        let toml_str = r#"
            [filters]
            media = ["photos", "videos", "live-photos"]
        "#;
        let toml: TomlConfig = toml::from_str(toml_str).unwrap();
        let cfg = Config::build(
            &default_globals(),
            &default_password(),
            default_sync(),
            Some(&toml),
        )
        .unwrap();
        assert!(!cfg.filters.skip_videos);
        assert!(!cfg.filters.skip_photos);
    }
    #[test]
    fn test_build_watch_interval_cli_overrides_toml() {
        let toml_str = r#"
            [watch]
            interval = 1800
        "#;
        let toml: TomlConfig = toml::from_str(toml_str).unwrap();
        let mut sync = default_sync();
        sync.config_overrides.watch_with_interval = Some(600);
        let cfg =
            Config::build(&default_globals(), &default_password(), sync, Some(&toml)).unwrap();
        assert_eq!(cfg.watch.interval, Some(600));
    }
    #[test]
    fn test_build_reconcile_cli_overrides_toml() {
        let toml_str = r#"
            [watch]
            reconcile_every_n_cycles = 24
        "#;
        let toml: TomlConfig = toml::from_str(toml_str).unwrap();
        let mut sync = default_sync();
        sync.config_overrides.reconcile_every_n_cycles = Some(6);
        let cfg =
            Config::build(&default_globals(), &default_password(), sync, Some(&toml)).unwrap();
        assert_eq!(cfg.watch.reconcile_every_n_cycles, Some(6));
    }
    #[test]
    fn test_build_reconcile_toml_only() {
        let toml_str = r#"
            [watch]
            reconcile_every_n_cycles = 12
        "#;
        let toml: TomlConfig = toml::from_str(toml_str).unwrap();
        let cfg = Config::build(
            &default_globals(),
            &default_password(),
            default_sync(),
            Some(&toml),
        )
        .unwrap();
        assert_eq!(cfg.watch.reconcile_every_n_cycles, Some(12));
    }
    // TOML accepts `reconcile_every_n_cycles = 0` as "off"; the resolved
    // config collapses it to `None` so the watch loop short-circuits.
    #[test]
    fn test_build_reconcile_toml_zero_is_off() {
        let toml_str = r#"
            [watch]
            reconcile_every_n_cycles = 0
        "#;
        let toml: TomlConfig = toml::from_str(toml_str).unwrap();
        let cfg = Config::build(
            &default_globals(),
            &default_password(),
            default_sync(),
            Some(&toml),
        )
        .unwrap();
        assert!(cfg.watch.reconcile_every_n_cycles.is_none());
    }
    #[test]
    fn test_build_reconcile_default_unset() {
        let cfg = Config::build(
            &default_globals(),
            &default_password(),
            default_sync(),
            None,
        )
        .unwrap();
        assert!(cfg.watch.reconcile_every_n_cycles.is_none());
    }
    #[test]
    fn test_build_notification_script_cli_overrides_toml() {
        let toml_str = r#"
            [notifications]
            script = "/toml/notify.sh"
        "#;
        let toml: TomlConfig = toml::from_str(toml_str).unwrap();
        let mut sync = default_sync();
        sync.config_overrides.notification_script = Some("/cli/notify.sh".to_string());
        let cfg =
            Config::build(&default_globals(), &default_password(), sync, Some(&toml)).unwrap();
        assert_eq!(
            cfg.notifications.script,
            Some(PathBuf::from("/cli/notify.sh"))
        );
    }
    #[test]
    fn test_build_notification_script_none_by_default() {
        let cfg = Config::build(
            &default_globals(),
            &default_password(),
            default_sync(),
            None,
        )
        .unwrap();
        assert!(cfg.notifications.script.is_none());
    }
    #[test]
    fn test_build_report_json_from_toml() {
        let toml_str = r#"
            [report]
            json = "/toml/run.json"
        "#;
        let toml: TomlConfig = toml::from_str(toml_str).unwrap();
        let cfg = Config::build(
            &default_globals(),
            &default_password(),
            default_sync(),
            Some(&toml),
        )
        .unwrap();
        assert_eq!(cfg.report.json, Some(PathBuf::from("/toml/run.json")));
    }
    #[test]
    fn test_build_report_json_cli_overrides_toml() {
        let toml_str = r#"
            [report]
            json = "/toml/run.json"
        "#;
        let toml: TomlConfig = toml::from_str(toml_str).unwrap();
        let mut sync = default_sync();
        sync.config_overrides.report_json = Some(PathBuf::from("/cli/run.json"));
        let cfg =
            Config::build(&default_globals(), &default_password(), sync, Some(&toml)).unwrap();
        assert_eq!(cfg.report.json, Some(PathBuf::from("/cli/run.json")));
    }
    #[test]
    fn test_build_report_json_none_by_default() {
        let cfg = Config::build(
            &default_globals(),
            &default_password(),
            default_sync(),
            None,
        )
        .unwrap();
        assert!(cfg.report.json.is_none());
    }
    #[test]
    fn test_build_recent_cli_overrides_toml() {
        let toml_str = r#"
            [filters]
            recent = 500
        "#;
        let toml: TomlConfig = toml::from_str(toml_str).unwrap();
        let mut sync = default_sync();
        sync.recent = Some(crate::cli::RecentLimit::Count(100));
        let cfg =
            Config::build(&default_globals(), &default_password(), sync, Some(&toml)).unwrap();
        assert_eq!(cfg.filters.recent, Some(100));
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
            &default_password(),
            default_sync(),
            Some(&toml),
        )
        .unwrap();
        assert_eq!(cfg.filters.recent, Some(500));
        assert_eq!(cfg.filters.recent_scope, crate::cli::RecentScope::Global);
    }
    #[test]
    fn test_build_recent_scope_cli_overrides_toml() {
        let toml_str = r#"
            [filters]
            recent = 500
            recent_scope = "per-filter"
        "#;
        let toml: TomlConfig = toml::from_str(toml_str).unwrap();
        let mut sync = default_sync();
        sync.recent = Some(crate::cli::RecentLimit::Count(100));
        sync.recent_scope = Some(crate::cli::RecentScope::Global);
        let cfg =
            Config::build(&default_globals(), &default_password(), sync, Some(&toml)).unwrap();
        assert_eq!(cfg.filters.recent, Some(100));
        assert_eq!(cfg.filters.recent_scope, crate::cli::RecentScope::Global);
    }
    #[test]
    fn test_build_recent_scope_from_toml() {
        let toml_str = r#"
            [filters]
            recent = 500
            recent_scope = "per-filter"
        "#;
        let toml: TomlConfig = toml::from_str(toml_str).unwrap();
        let cfg = Config::build(
            &default_globals(),
            &default_password(),
            default_sync(),
            Some(&toml),
        )
        .unwrap();
        assert_eq!(cfg.filters.recent, Some(500));
        assert_eq!(cfg.filters.recent_scope, crate::cli::RecentScope::PerFilter);
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
        let cfg =
            Config::build(&default_globals(), &default_password(), sync, Some(&toml)).unwrap();
        let before = cfg.filters.skip_created_before.unwrap();
        assert_eq!(
            before,
            CreatedDateFilter::CaptureDate(NaiveDate::from_ymd_opt(2023, 6, 1).unwrap())
        );
        let after = cfg.filters.skip_created_after.unwrap();
        assert_eq!(
            after,
            CreatedDateFilter::CaptureDate(NaiveDate::from_ymd_opt(2024, 6, 1).unwrap())
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
            &default_password(),
            default_sync(),
            Some(&toml),
        )
        .unwrap();
        let CreatedDateFilter::Instant(before) = cfg.filters.skip_created_before.unwrap() else {
            panic!("day interval must resolve to an instant");
        };
        let expected = chrono::Utc::now() - chrono::Duration::days(30);
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
            &default_password(),
            default_sync(),
            Some(&toml),
        );
        assert!(result.is_err());
    }
    #[cfg(feature = "xmp")]
    #[test]
    fn test_build_full_toml_all_sections() {
        let toml_str = r#"
            log_level = "warn"

            [auth]
            username = "full@example.com"
            domain = "cn"

            [download]
            directory = "/full/photos"
            folder_structure = "%Y"
            threads = 2
            temp_suffix = ".full-tmp"

            [metadata]
            set_exif_datetime = true

            [ui]
            progress_bar = false

            [download.retry]
            per_transfer = 1

            [filters]
            libraries = ["SharedSync-FULL"]
            albums = ["Album1"]
            media = ["photos", "live-photos"]
            recent = 50

            [photos]
            resolution = "medium"
            live_resolution = "thumb"
            live_photo_mov_filename_policy = "original"
            raw_policy = "prefer-jpeg"
            file_match_policy = "name-id7"
            force_resolution = true

            [watch]
            interval = 900
            pid_file = "/full/pid"
        "#;
        let toml: TomlConfig = toml::from_str(toml_str).unwrap();
        let cfg = Config::build(
            &default_globals(),
            &default_password(),
            default_sync(),
            Some(&toml),
        )
        .unwrap();
        // default_auth username overrides toml
        assert_eq!(cfg.auth.username, "u@example.com");
        assert!(cfg.auth.password.is_none());
        assert!(matches!(cfg.auth.domain, Domain::Cn));
        assert!(cfg.auth.cookie_directory.ends_with("kei/cookies"));
        assert_eq!(cfg.download.directory, PathBuf::from("/full/photos"));
        assert_eq!(cfg.download.folder_structure, "%Y");
        assert_eq!(cfg.download.threads_num, 2);
        assert_eq!(cfg.download.temp_suffix, ".full-tmp");
        assert!(cfg.metadata.set_exif_datetime);
        assert!(cfg.download.no_progress_bar);
        assert_eq!(cfg.retry.max_retries, 1);
        assert_eq!(cfg.retry.retry_delay_secs, 2);
        assert_eq!(
            cfg.filters.selection.libraries.to_raw(),
            vec!["SharedSync-FULL".to_string()]
        );
        assert_album_raw(&cfg, &["Album1"]);
        assert!(cfg.filters.skip_videos);
        assert_eq!(cfg.filters.recent, Some(50));
        assert!(matches!(cfg.photos.resolution, PhotoResolution::Medium));
        assert!(matches!(
            cfg.photos.live_resolution,
            LivePhotoResolution::Thumb
        ));
        assert!(matches!(
            cfg.photos.live_photo_mov_filename_policy,
            LivePhotoMovFilenamePolicy::Original
        ));
        assert!(matches!(cfg.photos.raw_policy, RawPolicy::PreferJpeg));
        assert!(matches!(
            cfg.photos.file_match_policy,
            FileMatchPolicy::NameId7
        ));
        assert!(cfg.photos.force_resolution);
        assert_eq!(cfg.watch.interval, Some(900));
        assert_eq!(cfg.watch.pid_file, Some(PathBuf::from("/full/pid")));
    }
    #[test]
    fn test_resolve_auth_all_from_toml() {
        // `[auth].password` is rejected in `Config::build()`, so resolve_auth
        // itself never reads it from TOML. Username and domain still flow through.
        let toml_str = r#"
            [auth]
            username = "toml@example.com"
            domain = "cn"
        "#;
        let toml: TomlConfig = toml::from_str(toml_str).unwrap();
        let globals = GlobalArgs {
            username: None,
            domain: None,
            data_dir: None,
        };
        let pw = crate::cli::PasswordArgs::default();
        let (username, password, domain, cookie_dir) = resolve_auth(&globals, &pw, Some(&toml));
        assert_eq!(username, "toml@example.com");
        assert!(password.is_none());
        assert!(matches!(domain, Domain::Cn));
        assert!(cookie_dir.ends_with("kei/cookies"));
    }
    #[test]
    fn test_resolve_auth_cli_overrides_all() {
        let toml_str = r#"
            [auth]
            username = "toml@example.com"
            domain = "cn"
        "#;
        let toml: TomlConfig = toml::from_str(toml_str).unwrap();
        let globals = GlobalArgs {
            username: Some("cli@example.com".to_string()),
            domain: Some(Domain::Com),
            data_dir: Some("/cli/data".to_string()),
        };
        let pw = crate::cli::PasswordArgs {
            password: Some("cli-pw".to_string()),
            ..crate::cli::PasswordArgs::default()
        };
        let (username, password, domain, cookie_dir) = resolve_auth(&globals, &pw, Some(&toml));
        assert_eq!(username, "cli@example.com");
        assert_eq!(password.as_deref(), Some("cli-pw"));
        assert!(matches!(domain, Domain::Com));
        assert_eq!(cookie_dir, PathBuf::from("/cli/data"));
    }
    #[test]
    fn test_resolve_auth_ignores_toml_password_field() {
        // Belt-and-braces: even if a TOML config somehow reaches resolve_auth
        // without passing through Config::build (e.g. a future caller),
        // resolve_auth itself must not surface the plaintext password.
        let toml_str = r#"
            [auth]
            password = "toml-pw"
        "#;
        let toml: TomlConfig = toml::from_str(toml_str).unwrap();
        let globals = GlobalArgs {
            username: None,
            domain: None,
            data_dir: None,
        };
        let pw = crate::cli::PasswordArgs::default();
        let (_, password, _, _) = resolve_auth(&globals, &pw, Some(&toml));
        assert!(
            password.is_none(),
            "resolve_auth must not read plaintext password from TOML"
        );
    }
    #[test]
    fn test_resolve_auth_defaults_when_both_absent() {
        let globals = GlobalArgs {
            username: None,
            domain: None,
            data_dir: None,
        };
        let pw = crate::cli::PasswordArgs::default();
        let (username, password, domain, cookie_dir) = resolve_auth(&globals, &pw, None);
        assert!(username.is_empty());
        assert!(password.is_none());
        assert!(matches!(domain, Domain::Com));
        assert!(cookie_dir.ends_with("kei/cookies"));
    }
    #[test]
    fn test_build_albums_empty_toml_empty_cli() {
        // Empty `albums = []` in TOML is equivalent to "no `--album` flag",
        // which the v0.13 default resolves to `All`.
        let toml_str = r#"
            [filters]
            albums = []
        "#;
        let toml: TomlConfig = toml::from_str(toml_str).unwrap();
        let cfg = Config::build(
            &default_globals(),
            &default_password(),
            default_sync(),
            Some(&toml),
        )
        .unwrap();
        assert_album_raw(&cfg, &["all"]);
        assert!(cfg.filters.selection.unfiled);
    }
    #[test]
    fn test_build_albums_no_toml_no_cli() {
        // v0.13 no-flag default: every user album plus an unfiled pass.
        let cfg = Config::build(
            &default_globals(),
            &default_password(),
            default_sync(),
            None,
        )
        .unwrap();
        assert_album_raw(&cfg, &["all"]);
        assert!(cfg.filters.selection.unfiled);
    }
    #[test]
    fn test_build_album_all_maps_to_all_variant() {
        let mut sync = default_sync();
        sync.config_overrides.albums = vec!["all".to_string()];
        let cfg = Config::build(&default_globals(), &default_password(), sync, None).unwrap();
        assert_album_raw(&cfg, &["all"]);
    }
    #[test]
    fn test_build_album_all_is_case_insensitive() {
        for raw in ["all", "ALL", "All", "aLL"] {
            let mut sync = default_sync();
            sync.config_overrides.albums = vec![raw.to_string()];
            let cfg = Config::build(&default_globals(), &default_password(), sync, None).unwrap();
            assert_eq!(
                cfg.filters.selection.albums.to_raw(),
                strings(&["all"]),
                "'{raw}' should resolve to AlbumSelector::All"
            );
        }
    }
    #[test]
    fn test_build_album_all_mixed_with_names_errors() {
        let mut sync = default_sync();
        sync.config_overrides.albums = vec!["all".to_string(), "Vacation".to_string()];
        let err = Config::build(&default_globals(), &default_password(), sync, None).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("`--album all` cannot be combined with album names"),
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
            &default_password(),
            default_sync(),
            Some(&toml),
        )
        .unwrap();
        assert_album_raw(&cfg, &["all"]);
    }
    #[test]
    fn test_build_default_is_all_with_album_template() {
        let mut sync = default_sync();
        sync.config_overrides.folder_structure_albums = Some("{album}/%Y/%m".to_string());
        let cfg = Config::build(&default_globals(), &default_password(), sync, None).unwrap();
        assert_album_raw(&cfg, &["all"]);
    }
    #[test]
    fn test_build_no_flag_default_is_all() {
        // v0.13: no `-a`, default `--folder-structure` -> `All`. The legacy
        // pre-v0.13 default was `LibraryOnly`; this test pins the new
        // contract so a regression flips it back.
        let mut sync = default_sync();
        sync.config_overrides.folder_structure = Some("%Y/%m/%d".to_string());
        let cfg = Config::build(&default_globals(), &default_password(), sync, None).unwrap();
        assert_album_raw(&cfg, &["all"]);
        assert!(cfg.filters.selection.unfiled);
    }
    #[test]
    fn test_build_album_named_preserved() {
        let mut sync = default_sync();
        sync.config_overrides.albums = vec!["Vacation".to_string(), "Trip".to_string()];
        let cfg = Config::build(&default_globals(), &default_password(), sync, None).unwrap();
        // Names are normalised through the v0.13 selector grammar, which uses
        // a BTreeSet for deterministic ordering — alphabetical, regardless of
        // CLI input order.
        assert_album_raw(&cfg, &["Trip", "Vacation"]);
    }
    #[test]
    fn test_build_album_inline_exclude_only_implies_all() {
        // `--album '!Family'` with no positive value resolves to "all minus
        // Family" via the new grammar; no `--album all` needed.
        let mut sync = default_sync();
        sync.config_overrides.albums = vec!["!Family".to_string()];
        let cfg = Config::build(&default_globals(), &default_password(), sync, None).unwrap();
        assert_album_raw(&cfg, &["all", "!Family"]);
    }
    #[test]
    fn test_build_album_all_with_inline_exclude() {
        let mut sync = default_sync();
        sync.config_overrides.albums = vec!["all".to_string(), "!Family".to_string()];
        let cfg = Config::build(&default_globals(), &default_password(), sync, None).unwrap();
        assert_album_raw(&cfg, &["all", "!Family"]);
    }
    #[test]
    fn test_build_album_named_with_inline_exclude() {
        let mut sync = default_sync();
        sync.config_overrides.albums = vec!["Vacation".to_string(), "!Family".to_string()];
        let cfg = Config::build(&default_globals(), &default_password(), sync, None).unwrap();
        assert_album_raw(&cfg, &["Vacation", "!Family"]);
    }
    #[test]
    fn test_build_album_none_sentinel_maps_to_library_only() {
        let mut sync = default_sync();
        sync.config_overrides.albums = vec!["none".to_string()];
        let cfg = Config::build(&default_globals(), &default_password(), sync, None).unwrap();
        assert_eq!(
            cfg.filters.selection.albums,
            crate::selection::AlbumSelector::None
        );
    }
    #[test]
    fn test_build_album_contradiction_bails() {
        let mut sync = default_sync();
        sync.config_overrides.albums = vec!["Vacation".to_string(), "!Vacation".to_string()];
        let err = Config::build(&default_globals(), &default_password(), sync, None).unwrap_err();
        assert!(
            err.to_string().contains("includes and excludes"),
            "unexpected error: {err}"
        );
    }
    #[test]
    fn test_build_album_none_mixed_with_names_bails() {
        let mut sync = default_sync();
        sync.config_overrides.albums = vec!["none".to_string(), "Vacation".to_string()];
        let err = Config::build(&default_globals(), &default_password(), sync, None).unwrap_err();
        assert!(
            err.to_string()
                .contains("`--album none` cannot be combined"),
            "unexpected error: {err}"
        );
    }
    #[test]
    fn test_build_album_token_rejected_mid_path() {
        let mut sync = default_sync();
        sync.config_overrides.folder_structure = Some("Photos/{album}/%Y".to_string());
        let err = Config::build(&default_globals(), &default_password(), sync, None).unwrap_err();
        assert!(
            err.to_string()
                .contains("`{album}` cannot be used in --folder-structure"),
            "unexpected error: {err}"
        );
    }
    #[test]
    fn test_build_album_token_rejected_after_date() {
        let mut sync = default_sync();
        sync.config_overrides.folder_structure = Some("%Y/{album}/%m".to_string());
        let err = Config::build(&default_globals(), &default_password(), sync, None).unwrap_err();
        assert!(
            err.to_string()
                .contains("`{album}` cannot be used in --folder-structure"),
            "unexpected error: {err}"
        );
    }
    #[test]
    fn test_build_album_token_rejected_as_trailing() {
        let mut sync = default_sync();
        sync.config_overrides.folder_structure = Some("%Y/%m/{album}".to_string());
        let err = Config::build(&default_globals(), &default_password(), sync, None).unwrap_err();
        assert!(
            err.to_string()
                .contains("`{album}` cannot be used in --folder-structure"),
            "unexpected error: {err}"
        );
    }
    #[test]
    fn test_build_album_token_rejected_duplicate() {
        let mut sync = default_sync();
        sync.config_overrides.folder_structure = Some("{album}/%Y/{album}".to_string());
        let err = Config::build(&default_globals(), &default_password(), sync, None).unwrap_err();
        assert!(
            err.to_string()
                .contains("`{album}` cannot be used in --folder-structure"),
            "unexpected error: {err}"
        );
    }
    #[test]
    fn test_build_no_album_token_no_migration() {
        // Without `{album}` in the template there is nothing to migrate;
        // both fields keep their resolved values.
        let mut sync = default_sync();
        sync.config_overrides.folder_structure = Some("%Y/%m".to_string());
        let cfg = Config::build(&default_globals(), &default_password(), sync, None).unwrap();
        assert_eq!(cfg.download.folder_structure, "%Y/%m");
        assert_eq!(cfg.download.folder_structure_albums, "{album}");
    }
    #[test]
    fn test_build_directory_cli_overrides_toml() {
        let toml_str = r#"
            [download]
            directory = "/toml/photos"
        "#;
        let toml: TomlConfig = toml::from_str(toml_str).unwrap();
        let mut sync = default_sync();
        sync.config_overrides.download_dir = Some("/cli/photos".to_string());
        let cfg =
            Config::build(&default_globals(), &default_password(), sync, Some(&toml)).unwrap();
        assert_eq!(cfg.download.directory, PathBuf::from("/cli/photos"));
    }
    #[test]
    fn test_build_passthrough_flags() {
        let mut sync = default_sync();
        sync.dry_run = true;
        let cfg = Config::build(&default_globals(), &default_password(), sync, None).unwrap();
        assert!(cfg.runtime.dry_run);
    }
    #[test]
    fn test_folder_structure_valid_tokens_accepted() {
        let mut sync = default_sync();
        sync.config_overrides.folder_structure = Some("%Y/%m/%d".to_string());
        let cfg = Config::build(&default_globals(), &default_password(), sync, None).unwrap();
        assert_eq!(cfg.download.folder_structure, "%Y/%m/%d");
    }
    #[test]
    fn test_folder_structure_all_tokens_accepted() {
        let mut sync = default_sync();
        sync.config_overrides.folder_structure = Some("%Y/%m/%d/%H/%M/%S".to_string());
        let cfg = Config::build(&default_globals(), &default_password(), sync, None).unwrap();
        assert_eq!(cfg.download.folder_structure, "%Y/%m/%d/%H/%M/%S");
    }
    #[test]
    fn test_folder_structure_none_bypasses_validation() {
        let mut sync = default_sync();
        sync.config_overrides.folder_structure = Some("none".to_string());
        let cfg = Config::build(&default_globals(), &default_password(), sync, None).unwrap();
        assert_eq!(cfg.download.folder_structure, "none");
    }
    #[test]
    fn test_folder_structure_strftime_tokens_accepted() {
        // Full strftime support: %B (month name), %X (locale time), etc. are valid
        let mut sync = default_sync();
        sync.config_overrides.folder_structure = Some("%Y/%B/%d".to_string());
        let cfg = Config::build(&default_globals(), &default_password(), sync, None).unwrap();
        assert_eq!(cfg.download.folder_structure, "%Y/%B/%d");
    }
    #[test]
    fn test_folder_structure_wrapped_format_accepted() {
        let mut sync = default_sync();
        sync.config_overrides.folder_structure = Some("{:%Y/%m/%d}".to_string());
        let cfg = Config::build(&default_globals(), &default_password(), sync, None).unwrap();
        assert_eq!(cfg.download.folder_structure, "{:%Y/%m/%d}");
    }
    #[test]
    fn test_config_build_captures_toml_friendly_true() {
        let toml_input = TomlConfig {
            data_dir: None,
            log_level: None,
            auth: None,
            download: None,
            filters: None,
            photos: None,
            import: None,
            metadata: None,
            watch: None,
            notifications: None,
            server: None,
            report: None,
            ui: Some(TomlUi {
                friendly: Some(true),
                progress_bar: None,
            }),
        };
        let cfg = Config::build(
            &default_globals(),
            &default_password(),
            default_sync(),
            Some(&toml_input),
        )
        .unwrap();
        assert_eq!(cfg.ui.friendly_request, Some(true));
        let round_tripped = cfg.to_toml();
        assert_eq!(
            round_tripped.ui.and_then(|u| u.friendly),
            Some(true),
            "to_toml must preserve the user's friendly = true preference"
        );
    }
    #[test]
    fn test_config_build_captures_toml_friendly_false() {
        let toml_input = TomlConfig {
            data_dir: None,
            log_level: None,
            auth: None,
            download: None,
            filters: None,
            photos: None,
            import: None,
            metadata: None,
            watch: None,
            notifications: None,
            server: None,
            report: None,
            ui: Some(TomlUi {
                friendly: Some(false),
                progress_bar: None,
            }),
        };
        let cfg = Config::build(
            &default_globals(),
            &default_password(),
            default_sync(),
            Some(&toml_input),
        )
        .unwrap();
        assert_eq!(cfg.ui.friendly_request, Some(false));
        assert_eq!(
            cfg.to_toml().ui.and_then(|u| u.friendly),
            Some(false),
            "to_toml must preserve the user's friendly = false opt-out"
        );
    }
    #[test]
    fn test_config_build_defaults_progress_bar_enabled() {
        let cfg = Config::build(
            &default_globals(),
            &default_password(),
            default_sync(),
            None,
        )
        .unwrap();
        assert!(
            !cfg.download.no_progress_bar,
            "progress bar should be enabled by default"
        );
        assert!(
            cfg.to_toml().ui.is_none(),
            "default progress-bar behavior should not create [ui]"
        );
    }
    #[test]
    fn test_config_build_captures_toml_progress_bar_false() {
        let parsed: TomlConfig = toml::from_str("[ui]\nprogress_bar = false\n").unwrap();
        let cfg = Config::build(
            &default_globals(),
            &default_password(),
            default_sync(),
            Some(&parsed),
        )
        .unwrap();
        assert!(
            cfg.download.no_progress_bar,
            "[ui].progress_bar = false should durably disable progress"
        );
        assert_eq!(
            cfg.to_toml().ui.and_then(|u| u.progress_bar),
            Some(false),
            "to_toml should round-trip the durable progress-bar opt-out"
        );
    }
    #[test]
    fn test_no_progress_bar_cli_overrides_toml_progress_bar_true() {
        let parsed: TomlConfig = toml::from_str("[ui]\nprogress_bar = true\n").unwrap();
        let mut sync = default_sync();
        sync.no_progress_bar = true;
        let cfg =
            Config::build(&default_globals(), &default_password(), sync, Some(&parsed)).unwrap();
        assert!(
            cfg.download.no_progress_bar,
            "--no-progress-bar should disable progress for this run even when TOML enables it"
        );
    }
    #[test]
    fn test_live_photo_mode_cli_overrides_toml() {
        let mut sync = default_sync();
        sync.config_overrides.live_photo_mode = Some(LivePhotoMode::ImageOnly);
        let toml_str = "[photos]\nlive_photo_mode = \"skip\"\n";
        let toml: TomlConfig = toml::from_str(toml_str).unwrap();
        let cfg =
            Config::build(&default_globals(), &default_password(), sync, Some(&toml)).unwrap();
        assert_eq!(cfg.photos.live_photo_mode, LivePhotoMode::ImageOnly);
    }
    #[test]
    fn test_live_photo_mode_from_toml() {
        let toml_str = "[photos]\nlive_photo_mode = \"video-only\"\n";
        let toml: TomlConfig = toml::from_str(toml_str).unwrap();
        let cfg = Config::build(
            &default_globals(),
            &default_password(),
            default_sync(),
            Some(&toml),
        )
        .unwrap();
        assert_eq!(cfg.photos.live_photo_mode, LivePhotoMode::VideoOnly);
    }
    #[test]
    fn test_filename_exclude_from_toml() {
        let toml_str = "[filters]\nfilename_exclude = [\"*.AAE\", \"*.TMP\"]\n";
        let toml: TomlConfig = toml::from_str(toml_str).unwrap();
        let cfg = Config::build(
            &default_globals(),
            &default_password(),
            default_sync(),
            Some(&toml),
        )
        .unwrap();
        let patterns: Vec<&str> = cfg
            .download
            .filename_exclude
            .iter()
            .map(|p| p.as_str())
            .collect();
        assert_eq!(patterns, vec!["*.AAE", "*.TMP"]);
    }
    #[test]
    fn test_filename_exclude_invalid_glob_rejected() {
        let mut sync = default_sync();
        sync.config_overrides.filename_exclude = vec!["[invalid".to_string()];
        let err = Config::build(&default_globals(), &default_password(), sync, None).unwrap_err();
        assert!(
            err.to_string()
                .contains("Invalid --filename-exclude pattern")
        );
    }
    fn assert_sf_named(
        sel: &crate::selection::SmartFolderSelector,
        want_in: &[&str],
        want_ex: &[&str],
    ) {
        let crate::selection::SmartFolderSelector::Named { included, excluded } = sel else {
            panic!("expected Named, got {sel:?}");
        };
        for n in want_in {
            assert!(included.contains(*n), "missing include {n}");
        }
        for n in want_ex {
            assert!(excluded.contains(*n), "missing exclude {n}");
        }
    }
    fn assert_sf_all(
        sel: &crate::selection::SmartFolderSelector,
        sensitive: bool,
        want_ex: &[&str],
    ) {
        let crate::selection::SmartFolderSelector::All {
            include_sensitive,
            excluded,
        } = sel
        else {
            panic!("expected All, got {sel:?}");
        };
        assert_eq!(*include_sensitive, sensitive, "include_sensitive mismatch");
        for n in want_ex {
            assert!(excluded.contains(*n), "missing exclude {n}");
        }
    }
    fn build_with_smart_folders(cli: Vec<&str>, toml_str: Option<&str>) -> Config {
        let mut sync = default_sync();
        sync.config_overrides.smart_folders = cli.iter().map(|s| (*s).to_string()).collect();
        let toml = toml_str.map(|s| toml::from_str::<TomlConfig>(s).unwrap());
        Config::build(&default_globals(), &default_password(), sync, toml.as_ref()).unwrap()
    }
    #[test]
    fn test_smart_folders_default_is_none() {
        let cfg = Config::build(
            &default_globals(),
            &default_password(),
            default_sync(),
            None,
        )
        .unwrap();
        assert_eq!(
            cfg.filters.selection.smart_folders,
            crate::selection::SmartFolderSelector::None
        );
    }
    #[test]
    fn test_smart_folders_from_cli() {
        let cfg = build_with_smart_folders(vec!["Favorites", "!Hidden"], None);
        assert_sf_named(
            &cfg.filters.selection.smart_folders,
            &["Favorites"],
            &["Hidden"],
        );
    }
    #[test]
    fn test_smart_folders_all_sentinel() {
        let cfg = build_with_smart_folders(vec!["all"], None);
        assert_sf_all(&cfg.filters.selection.smart_folders, false, &[]);
    }
    #[test]
    fn test_smart_folders_from_toml() {
        let cfg = build_with_smart_folders(
            vec![],
            Some("[filters]\nsmart_folders = [\"all-with-sensitive\", \"!Recently Deleted\"]\n"),
        );
        assert_sf_all(
            &cfg.filters.selection.smart_folders,
            true,
            &["Recently Deleted"],
        );
    }
    #[test]
    fn test_smart_folders_cli_overrides_toml() {
        let cfg = build_with_smart_folders(
            vec!["Favorites"],
            Some("[filters]\nsmart_folders = [\"Videos\"]\n"),
        );
        let crate::selection::SmartFolderSelector::Named { included, .. } =
            &cfg.filters.selection.smart_folders
        else {
            panic!(
                "expected Named, got {:?}",
                cfg.filters.selection.smart_folders
            );
        };
        assert!(included.contains("Favorites"));
        assert!(!included.contains("Videos"));
    }
    #[test]
    fn test_smart_folders_invalid_combination_bails() {
        let mut sync = default_sync();
        sync.config_overrides.smart_folders =
            vec!["all".to_string(), "all-with-sensitive".to_string()];
        let err = Config::build(&default_globals(), &default_password(), sync, None).unwrap_err();
        assert!(err.to_string().contains("cannot be used together"));
    }
    fn build_with_unfiled(cli: Option<bool>, toml_str: Option<&str>) -> Config {
        let mut sync = default_sync();
        sync.config_overrides.unfiled = cli;
        let toml = toml_str.map(|s| toml::from_str::<TomlConfig>(s).unwrap());
        Config::build(&default_globals(), &default_password(), sync, toml.as_ref()).unwrap()
    }
    #[test]
    fn test_unfiled_default_no_flags_is_true() {
        let cfg = build_with_unfiled(None, None);
        assert!(
            cfg.filters.selection.unfiled,
            "v0.13 default: unfiled = true"
        );
    }
    #[test]
    fn test_unfiled_default_with_named_albums_is_true() {
        // v0.13: --unfiled defaults to true regardless of --album. Named
        // albums get their own passes AND the unfiled pass runs alongside.
        // Pre-v0.13 behaviour was unfiled=false for named-album syncs; this
        // test pins the new contract.
        let mut sync = default_sync();
        sync.config_overrides.albums = vec!["Vacation".to_string()];
        let cfg = Config::build(&default_globals(), &default_password(), sync, None).unwrap();
        assert!(
            cfg.filters.selection.unfiled,
            "v0.13: --album Vacation alone should still default unfiled = true",
        );
    }
    #[test]
    fn test_unfiled_cli_true_explicit() {
        let cfg = build_with_unfiled(Some(true), None);
        assert!(cfg.filters.selection.unfiled);
    }
    #[test]
    fn test_unfiled_cli_false_explicit() {
        let cfg = build_with_unfiled(Some(false), None);
        assert!(!cfg.filters.selection.unfiled);
    }
    #[test]
    fn test_unfiled_false_disables_named_album_unfiled_pass() {
        // The user explicitly opts out of the unfiled pass when running a
        // named-album sync. Without this opt-out, v0.13's default would
        // run the Vacation pass AND the unfiled pass.
        let mut sync = default_sync();
        sync.config_overrides.albums = vec!["Vacation".to_string()];
        sync.config_overrides.unfiled = Some(false);
        let cfg = Config::build(&default_globals(), &default_password(), sync, None).unwrap();
        assert!(!cfg.filters.selection.unfiled);
    }
    #[test]
    fn test_unfiled_from_toml() {
        let cfg = build_with_unfiled(None, Some("[filters]\nunfiled = false\n"));
        assert!(!cfg.filters.selection.unfiled);
    }
    #[test]
    fn test_unfiled_cli_overrides_toml() {
        let cfg = build_with_unfiled(Some(true), Some("[filters]\nunfiled = false\n"));
        assert!(cfg.filters.selection.unfiled);
    }
    #[test]
    fn test_folder_structure_albums_default_is_album_token() {
        let cfg = Config::build(
            &default_globals(),
            &default_password(),
            default_sync(),
            None,
        )
        .unwrap();
        assert_eq!(
            cfg.download.folder_structure_albums,
            DEFAULT_FOLDER_STRUCTURE_ALBUMS
        );
    }
    #[test]
    fn test_folder_structure_smart_folders_default_is_smart_folder_token() {
        let cfg = Config::build(
            &default_globals(),
            &default_password(),
            default_sync(),
            None,
        )
        .unwrap();
        assert_eq!(
            cfg.download.folder_structure_smart_folders,
            DEFAULT_FOLDER_STRUCTURE_SMART_FOLDERS
        );
    }
    #[test]
    fn test_folder_structure_albums_from_cli() {
        let mut sync = default_sync();
        sync.config_overrides.folder_structure_albums = Some("{album}/%Y/%m".to_string());
        let cfg = Config::build(&default_globals(), &default_password(), sync, None).unwrap();
        assert_eq!(cfg.download.folder_structure_albums, "{album}/%Y/%m");
    }
    #[test]
    fn test_folder_structure_smart_folders_from_cli() {
        let mut sync = default_sync();
        sync.config_overrides.folder_structure_smart_folders =
            Some("{smart-folder}/%Y".to_string());
        let cfg = Config::build(&default_globals(), &default_password(), sync, None).unwrap();
        assert_eq!(
            cfg.download.folder_structure_smart_folders,
            "{smart-folder}/%Y"
        );
    }
    #[test]
    fn test_folder_structure_albums_from_toml() {
        let toml_str = "[download]\nfolder_structure_albums = \"{album}/%Y\"\n";
        let toml: TomlConfig = toml::from_str(toml_str).unwrap();
        let cfg = Config::build(
            &default_globals(),
            &default_password(),
            default_sync(),
            Some(&toml),
        )
        .unwrap();
        assert_eq!(cfg.download.folder_structure_albums, "{album}/%Y");
    }
    #[test]
    fn test_folder_structure_smart_folders_from_toml() {
        let toml_str = "[download]\nfolder_structure_smart_folders = \"{smart-folder}/%Y\"\n";
        let toml: TomlConfig = toml::from_str(toml_str).unwrap();
        let cfg = Config::build(
            &default_globals(),
            &default_password(),
            default_sync(),
            Some(&toml),
        )
        .unwrap();
        assert_eq!(
            cfg.download.folder_structure_smart_folders,
            "{smart-folder}/%Y"
        );
    }
    #[test]
    fn test_folder_structure_albums_cli_overrides_toml() {
        let mut sync = default_sync();
        sync.config_overrides.folder_structure_albums = Some("{album}/cli".to_string());
        let toml_str = "[download]\nfolder_structure_albums = \"{album}/toml\"\n";
        let toml: TomlConfig = toml::from_str(toml_str).unwrap();
        let cfg =
            Config::build(&default_globals(), &default_password(), sync, Some(&toml)).unwrap();
        assert_eq!(cfg.download.folder_structure_albums, "{album}/cli");
    }
    #[test]
    fn test_contradictory_date_filter_succeeds() {
        // A contradictory window is a warning, not an error.
        let mut sync = default_sync();
        sync.skip_created_before = Some("2025-06-01".to_string());
        sync.skip_created_after = Some("2025-01-01".to_string());
        let cfg = Config::build(&default_globals(), &default_password(), sync, None);
        assert!(
            cfg.is_ok(),
            "Contradictory date filters should warn, not error"
        );
        let cfg = cfg.unwrap();
        assert!(
            cfg.filters
                .skip_created_before
                .unwrap()
                .is_strictly_after(cfg.filters.skip_created_after.unwrap())
        );
    }
    #[test]
    fn test_filename_exclude_cli_overrides_toml() {
        let mut sync = default_sync();
        sync.config_overrides.filename_exclude = vec!["*.AAE".to_string()];
        let toml_str = "[filters]\nfilename_exclude = [\"*.TMP\"]\n";
        let toml: TomlConfig = toml::from_str(toml_str).unwrap();
        let cfg =
            Config::build(&default_globals(), &default_password(), sync, Some(&toml)).unwrap();
        let patterns: Vec<&str> = cfg
            .download
            .filename_exclude
            .iter()
            .map(|p| p.as_str())
            .collect();
        assert_eq!(patterns, vec!["*.AAE"]);
    }
    #[test]
    fn test_filename_exclude_falls_back_to_toml() {
        let toml_str = "[filters]\nfilename_exclude = [\"*.TMP\"]\n";
        let toml: TomlConfig = toml::from_str(toml_str).unwrap();
        let cfg = Config::build(
            &default_globals(),
            &default_password(),
            default_sync(),
            Some(&toml),
        )
        .unwrap();
        let patterns: Vec<&str> = cfg
            .download
            .filename_exclude
            .iter()
            .map(|p| p.as_str())
            .collect();
        assert_eq!(patterns, vec!["*.TMP"]);
    }
    #[test]
    fn resolve_library_defaults_to_primary_only() {
        let sel = resolve_library_selector(vec![], None).unwrap();
        assert_eq!(sel, crate::selection::LibrarySelector::default());
        assert_eq!(sel.to_raw(), vec!["primary".to_string()]);
    }
    #[test]
    fn resolve_library_primary_sentinel_round_trips() {
        let sel = resolve_library_selector(vec!["primary".to_string()], None).unwrap();
        assert!(sel.primary && !sel.shared_all && sel.named.is_empty());
    }
    #[test]
    fn resolve_library_cli_overrides_toml() {
        let toml_filters = TomlFilters {
            libraries: Some(vec!["SharedSync-FROM-TOML".to_string()]),
            ..Default::default()
        };
        let sel =
            resolve_library_selector(vec!["SharedSync-FROM-CLI".to_string()], Some(&toml_filters))
                .unwrap();
        assert_eq!(sel.to_raw(), vec!["SharedSync-FROM-CLI".to_string()]);
    }
    #[test]
    fn resolve_library_falls_back_to_toml_array() {
        let toml_filters = TomlFilters {
            libraries: Some(vec!["SharedSync-ABCD".to_string()]),
            ..Default::default()
        };
        let sel = resolve_library_selector(vec![], Some(&toml_filters)).unwrap();
        assert_eq!(sel.to_raw(), vec!["SharedSync-ABCD".to_string()]);
    }
    #[test]
    fn resolve_library_all_case_insensitive() {
        for sentinel in ["ALL", "All", "all"] {
            let sel = resolve_library_selector(vec![sentinel.to_string()], None).unwrap();
            assert_eq!(
                sel.to_raw(),
                vec!["all".to_string()],
                "sentinel: {sentinel}"
            );
        }
    }
    #[test]
    fn resolve_library_shared_alone_keeps_shared_only() {
        let sel = resolve_library_selector(vec!["shared".to_string()], None).unwrap();
        assert!(!sel.primary, "primary must stay off for `shared` alone");
        assert!(sel.shared_all);
        assert!(sel.named.is_empty());
    }
    #[test]
    fn resolve_library_multiple_named_keeps_both() {
        let sel = resolve_library_selector(
            vec!["SharedSync-AAAA".to_string(), "SharedSync-BBBB".to_string()],
            None,
        )
        .unwrap();
        assert_eq!(sel.named.len(), 2);
        assert!(!sel.primary);
    }
    #[test]
    fn resolve_library_exclusion_keeps_exclusion() {
        let sel = resolve_library_selector(vec!["!SharedSync-AAAA".to_string()], None).unwrap();
        assert!(sel.primary, "bare exclusion must lift category default");
        assert_eq!(sel.excluded.len(), 1);
    }
    #[test]
    fn resolve_library_named_zone_passes_through() {
        let sel = resolve_library_selector(vec!["SharedSync-ABCD1234".to_string()], None).unwrap();
        assert_eq!(sel.to_raw(), vec!["SharedSync-ABCD1234".to_string()]);
    }
    //
    // Pure policy via `resolve_notify_systemd` so the truth table is
    // testable without mutating the process environment (which would race
    // under parallel test execution).

    #[test]
    fn resolve_notify_systemd_cli_true_wins() {
        // CLI true: enabled regardless of socket presence.
        assert!(resolve_notify_systemd(Some(true), None, false));
        assert!(resolve_notify_systemd(Some(true), None, true));
        assert!(resolve_notify_systemd(Some(true), Some(false), true));
    }
    #[test]
    fn resolve_notify_systemd_cli_false_wins_even_under_systemd() {
        // Explicit CLI false is the escape hatch: user is under systemd
        // (NOTIFY_SOCKET set) but wants kei to stay silent.
        assert!(!resolve_notify_systemd(Some(false), None, true));
        assert!(!resolve_notify_systemd(Some(false), Some(true), true));
    }
    #[test]
    fn resolve_notify_systemd_toml_used_when_cli_absent() {
        assert!(resolve_notify_systemd(None, Some(true), false));
        assert!(!resolve_notify_systemd(None, Some(false), true));
    }
    #[test]
    fn resolve_notify_systemd_auto_detect_when_nothing_set() {
        // No explicit setting: follow the NOTIFY_SOCKET signal.
        assert!(resolve_notify_systemd(None, None, true));
        assert!(!resolve_notify_systemd(None, None, false));
    }
    #[test]
    fn test_smart_retry_delay_table() {
        assert_eq!(smart_retry_delay(0), 5);
        assert_eq!(smart_retry_delay(1), 2);
        assert_eq!(smart_retry_delay(2), 2);
        assert_eq!(smart_retry_delay(3), 5);
        assert_eq!(smart_retry_delay(4), 10);
        assert_eq!(smart_retry_delay(5), 10);
        assert_eq!(smart_retry_delay(6), 10);
        assert_eq!(smart_retry_delay(7), 30);
        assert_eq!(smart_retry_delay(25), 30);
        assert_eq!(smart_retry_delay(100), 30);
    }
    #[test]
    fn test_build_retry_delay_smart_default_from_max_retries() {
        // No explicit retry-delay anywhere; per_transfer=5 should pull the
        // 4..=6 bucket from the smart table (10s).
        let mut sync = default_sync();
        sync.config_overrides.max_retries = Some(5);
        let cfg = Config::build(&default_globals(), &default_password(), sync, None).unwrap();
        assert_eq!(cfg.retry.retry_delay_secs, 10);
    }
    #[test]
    fn test_build_retry_delay_smart_default_patient_bucket() {
        let mut sync = default_sync();
        sync.config_overrides.max_retries = Some(10);
        let cfg = Config::build(&default_globals(), &default_password(), sync, None).unwrap();
        assert_eq!(cfg.retry.retry_delay_secs, 30);
    }
    #[test]
    fn test_build_retry_delay_smart_default_fail_fast_bucket() {
        let mut sync = default_sync();
        sync.config_overrides.max_retries = Some(1);
        let cfg = Config::build(&default_globals(), &default_password(), sync, None).unwrap();
        assert_eq!(cfg.retry.retry_delay_secs, 2);
    }
    #[test]
    fn test_build_threads_cli_canonical() {
        let mut sync = default_sync();
        sync.config_overrides.threads = Some(7);
        let cfg = Config::build(&default_globals(), &default_password(), sync, None).unwrap();
        assert_eq!(cfg.download.threads_num, 7);
    }
    #[test]
    fn test_build_toml_threads_canonical() {
        let toml_str = r#"
            [download]
            threads = 16
        "#;
        let toml: TomlConfig = toml::from_str(toml_str).unwrap();
        let cfg = Config::build(
            &default_globals(),
            &default_password(),
            default_sync(),
            Some(&toml),
        )
        .unwrap();
        assert_eq!(cfg.download.threads_num, 16);
    }
    #[test]
    fn test_build_recent_count_populates_recent_field() {
        let mut sync = default_sync();
        sync.recent = Some(crate::cli::RecentLimit::Count(100));
        let cfg = Config::build(&default_globals(), &default_password(), sync, None).unwrap();
        assert_eq!(cfg.filters.recent, Some(100));
        assert!(
            cfg.filters.skip_created_before.is_none(),
            "Count form must not touch skip_created_before"
        );
    }
    #[test]
    fn test_build_recent_days_populates_skip_created_before() {
        let mut sync = default_sync();
        sync.recent = Some(crate::cli::RecentLimit::Days(30));
        let cfg = Config::build(&default_globals(), &default_password(), sync, None).unwrap();
        assert!(
            cfg.filters.recent.is_none(),
            "Days form must not populate recent (count-only field)"
        );
        assert!(
            cfg.filters.skip_created_before.is_some(),
            "Days form must populate skip_created_before cutoff"
        );
        // The cutoff should be ~30 days ago; give it a wide window to avoid
        // flakiness on slow CI.
        let CreatedDateFilter::Instant(cutoff) = cfg.filters.skip_created_before.unwrap() else {
            panic!("recent day window must resolve to an instant");
        };
        let now = chrono::Utc::now();
        let delta = now.signed_duration_since(cutoff);
        assert!(
            delta.num_days() >= 29 && delta.num_days() <= 31,
            "cutoff should be ~30 days ago; got {} days",
            delta.num_days()
        );
    }
    #[test]
    fn test_build_recent_days_conflicts_with_skip_created_before() {
        let mut sync = default_sync();
        sync.recent = Some(crate::cli::RecentLimit::Days(30));
        sync.skip_created_before = Some("2024-01-01".to_string());
        let err = Config::build(&default_globals(), &default_password(), sync, None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("--recent 30d"), "{err}");
        assert!(err.contains("--skip-created-before"), "{err}");
        assert!(err.contains("Pick one"), "{err}");
    }
    #[test]
    fn test_build_recent_count_orthogonal_with_skip_created_before() {
        // Count and skip_created_before are orthogonal - take the N most
        // recent assets, filtered to those after the cutoff.
        let mut sync = default_sync();
        sync.recent = Some(crate::cli::RecentLimit::Count(100));
        sync.skip_created_before = Some("2024-01-01".to_string());
        let cfg = Config::build(&default_globals(), &default_password(), sync, None).unwrap();
        assert_eq!(cfg.filters.recent, Some(100));
        assert!(cfg.filters.skip_created_before.is_some());
    }
    #[test]
    fn test_build_recent_days_from_toml() {
        let toml_str = r#"
            [filters]
            recent = "14d"
        "#;
        let toml: TomlConfig = toml::from_str(toml_str).unwrap();
        let cfg = Config::build(
            &default_globals(),
            &default_password(),
            default_sync(),
            Some(&toml),
        )
        .unwrap();
        assert!(cfg.filters.recent.is_none());
        assert!(cfg.filters.skip_created_before.is_some());
    }
    #[test]
    fn test_build_recent_count_from_toml_integer() {
        let toml_str = r#"
            [filters]
            recent = 250
        "#;
        let toml: TomlConfig = toml::from_str(toml_str).unwrap();
        let cfg = Config::build(
            &default_globals(),
            &default_password(),
            default_sync(),
            Some(&toml),
        )
        .unwrap();
        assert_eq!(cfg.filters.recent, Some(250));
    }
    #[test]
    fn test_build_recent_scope_without_recent_rejected() {
        let toml_str = r#"
            [filters]
            recent_scope = "per-filter"
        "#;
        let toml: TomlConfig = toml::from_str(toml_str).unwrap();
        let err = Config::build(
            &default_globals(),
            &default_password(),
            default_sync(),
            Some(&toml),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("recent_scope"), "{err}");
        assert!(err.contains("recent"), "{err}");
    }
    #[test]
    fn test_build_recent_scope_with_days_rejected() {
        let toml_str = r#"
            [filters]
            recent = "14d"
            recent_scope = "per-filter"
        "#;
        let toml: TomlConfig = toml::from_str(toml_str).unwrap();
        let err = Config::build(
            &default_globals(),
            &default_password(),
            default_sync(),
            Some(&toml),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("recent_scope"), "{err}");
        assert!(err.contains("count-form"), "{err}");
    }
    #[test]
    fn test_build_recent_days_conflicts_with_toml_skip_created_before() {
        // CLI Days form should also conflict with a TOML skip_created_before.
        let toml_str = r#"
            [filters]
            skip_created_before = "2024-01-01"
        "#;
        let toml: TomlConfig = toml::from_str(toml_str).unwrap();
        let mut sync = default_sync();
        sync.recent = Some(crate::cli::RecentLimit::Days(7));
        let err = Config::build(&default_globals(), &default_password(), sync, Some(&toml))
            .unwrap_err()
            .to_string();
        assert!(err.contains("--recent 7d"), "{err}");
    }
    #[test]
    fn test_build_live_photos_only_with_live_skip_rejected() {
        let toml_str = r#"
            [filters]
            media = ["live-photos"]

            [photos]
            live_photo_mode = "skip"
        "#;
        let toml: TomlConfig = toml::from_str(toml_str).unwrap();
        let err = Config::build(
            &default_globals(),
            &default_password(),
            default_sync(),
            Some(&toml),
        )
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("would download nothing"),
            "error should explain the outcome; got: {err}"
        );
        assert!(
            err.contains("live_photo_mode"),
            "error should name the conflicting field; got: {err}"
        );
    }
    #[test]
    fn test_build_empty_media_rejected() {
        let toml_str = r#"
            [filters]
            media = []
        "#;
        let toml: TomlConfig = toml::from_str(toml_str).unwrap();
        let err = Config::build(
            &default_globals(),
            &default_password(),
            default_sync(),
            Some(&toml),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("[filters].media cannot be empty"), "{err}");
    }
    #[test]
    fn test_build_duplicate_media_rejected() {
        let toml_str = r#"
            [filters]
            media = ["photos", "photos"]
        "#;
        let toml: TomlConfig = toml::from_str(toml_str).unwrap();
        let err = Config::build(
            &default_globals(),
            &default_password(),
            default_sync(),
            Some(&toml),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("duplicate `photos`"), "{err}");
    }
    #[test]
    fn test_build_legacy_skip_media_toml_keys_rejected() {
        for key in ["skip_videos", "skip_photos"] {
            let toml_str = format!("[filters]\n{key} = true\n");
            let err = toml::from_str::<TomlConfig>(&toml_str).unwrap_err();
            assert!(err.to_string().contains(&format!("unknown field `{key}`")));
        }
    }
    #[test]
    fn test_build_skip_videos_and_photos_with_live_skip_rejected() {
        let mut sync = default_sync();
        sync.config_overrides.skip_videos = Some(true);
        sync.config_overrides.skip_photos = Some(true);
        sync.config_overrides.live_photo_mode = Some(LivePhotoMode::Skip);
        let err = Config::build(&default_globals(), &default_password(), sync, None)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("would download nothing"),
            "error should explain the outcome; got: {err}"
        );
        assert!(
            err.contains("live_photo_mode"),
            "error should name the conflicting field; got: {err}"
        );
    }
    #[test]
    fn test_build_skip_videos_and_photos_with_image_only_allowed() {
        let mut sync = default_sync();
        sync.config_overrides.skip_videos = Some(true);
        sync.config_overrides.skip_photos = Some(true);
        sync.config_overrides.live_photo_mode = Some(LivePhotoMode::ImageOnly);
        let cfg = Config::build(&default_globals(), &default_password(), sync, None).unwrap();
        assert_eq!(cfg.filters.media.to_kinds(), vec![MediaKind::LivePhotos]);
        assert_eq!(cfg.photos.live_photo_mode, LivePhotoMode::ImageOnly);
    }
    #[test]
    fn test_build_skip_videos_and_photos_with_video_only_allowed() {
        // Obscure but legitimate: user wants only Live Photo MOV
        // companions. video-only mode keeps the MOV while both skip flags
        // drop everything else. Must not error.
        let mut sync = default_sync();
        sync.config_overrides.skip_videos = Some(true);
        sync.config_overrides.skip_photos = Some(true);
        sync.config_overrides.live_photo_mode = Some(LivePhotoMode::VideoOnly);
        let cfg = Config::build(&default_globals(), &default_password(), sync, None).unwrap();
        assert!(cfg.filters.skip_videos);
        assert!(cfg.filters.skip_photos);
        assert_eq!(cfg.photos.live_photo_mode, LivePhotoMode::VideoOnly);
    }
    #[test]
    fn test_build_skip_videos_and_photos_with_both_allowed() {
        // Default live-photo-mode is Both. With both skip flags set, Live
        // Photo MOVs still download (skip_videos targets pure videos, not
        // Live Photo video companions). Must not error.
        let mut sync = default_sync();
        sync.config_overrides.skip_videos = Some(true);
        sync.config_overrides.skip_photos = Some(true);
        sync.config_overrides.live_photo_mode = Some(LivePhotoMode::Both);
        let cfg = Config::build(&default_globals(), &default_password(), sync, None).unwrap();
        assert_eq!(cfg.photos.live_photo_mode, LivePhotoMode::Both);
    }
    #[test]
    fn test_build_skip_videos_alone_ok() {
        let mut sync = default_sync();
        sync.config_overrides.skip_videos = Some(true);
        let cfg = Config::build(&default_globals(), &default_password(), sync, None).unwrap();
        assert!(cfg.filters.skip_videos);
        assert!(!cfg.filters.skip_photos);
    }
    #[test]
    fn test_build_skip_photos_alone_ok() {
        let mut sync = default_sync();
        sync.config_overrides.skip_photos = Some(true);
        let cfg = Config::build(&default_globals(), &default_password(), sync, None).unwrap();
        assert!(cfg.filters.skip_photos);
        assert!(!cfg.filters.skip_videos);
    }
    #[test]
    fn test_build_media_from_toml() {
        let toml_str = r#"
            [filters]
            media = ["videos", "live-photos"]
        "#;
        let toml: TomlConfig = toml::from_str(toml_str).unwrap();
        let cfg = Config::build(
            &default_globals(),
            &default_password(),
            default_sync(),
            Some(&toml),
        )
        .unwrap();
        assert_eq!(
            cfg.filters.media.to_kinds(),
            vec![MediaKind::Videos, MediaKind::LivePhotos]
        );
        assert!(cfg.filters.skip_photos);
        assert!(!cfg.filters.skip_videos);
    }
}
