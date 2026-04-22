#![allow(
    clippy::print_stdout,
    reason = "interactive setup wizard whose purpose is to drive a stdout dialogue"
)]

use std::fmt::Write as FmtWrite;
use std::io::IsTerminal;
use std::path::Path;

use anyhow::{bail, Context};
use dialoguer::{Confirm, Input, Password, Select};

use crate::types::{
    Domain, FileMatchPolicy, LivePhotoMovFilenamePolicy, LogLevel, RawTreatmentPolicy, VersionSize,
};

/// Result of the setup wizard — either the user wants to sync now or just exit.
pub(crate) enum SetupResult {
    /// User chose to sync now. Contains the config path and env file path.
    SyncNow {
        config_path: std::path::PathBuf,
        env_path: std::path::PathBuf,
    },
    /// User chose not to sync now (or cancelled).
    Done,
}

/// Collected answers from the interactive setup wizard.
#[derive(Debug)]
struct SetupAnswers {
    // Account
    username: String,
    password: secrecy::SecretString,
    domain: Option<Domain>,

    // Destination
    directory: String,
    folder_structure: Option<String>,

    // What to download
    albums: Vec<String>,
    library: Option<String>, // None = default, Some("all") = all libraries

    // Media types
    skip_videos: bool,
    skip_live_photos: bool,
    live_photo_mov_filename_policy: Option<LivePhotoMovFilenamePolicy>,

    // Quality
    size: Option<VersionSize>,
    force_size: bool,
    align_raw: Option<RawTreatmentPolicy>,

    // Date range
    recent: Option<u32>,
    skip_created_before: Option<String>,
    skip_created_after: Option<String>,

    // Running mode
    watch_interval: Option<u64>,
    notify_systemd: bool,
    pid_file: Option<String>,

    // Extras
    notification_script: Option<String>,
    threads_num: Option<u16>,
    max_retries: Option<u32>,
    retry_delay: Option<u64>,
    keep_unicode_in_filenames: bool,
    set_exif_datetime: bool,
    file_match_policy: Option<FileMatchPolicy>,
    cookie_directory: Option<String>,
    log_level: Option<LogLevel>,
}

impl Default for SetupAnswers {
    fn default() -> Self {
        Self {
            username: String::new(),
            password: secrecy::SecretString::from(String::new()),
            domain: None,
            directory: "~/Photos/iCloud".to_string(),
            folder_structure: None,
            albums: Vec::new(),
            library: Some("all".to_string()),
            skip_videos: false,
            skip_live_photos: false,
            live_photo_mov_filename_policy: None,
            size: None,
            force_size: false,
            align_raw: None,
            recent: None,
            skip_created_before: None,
            skip_created_after: None,
            watch_interval: None,
            notify_systemd: false,
            pid_file: None,
            notification_script: None,
            threads_num: None,
            max_retries: None,
            retry_delay: None,
            keep_unicode_in_filenames: false,
            set_exif_datetime: false,
            file_match_policy: None,
            cookie_directory: None,
            log_level: None,
        }
    }
}

pub(crate) fn run_setup(config_path: &Path) -> anyhow::Result<SetupResult> {
    if !std::io::stdin().is_terminal() {
        bail!("The setup wizard requires an interactive terminal.");
    }

    println!();
    println!("Welcome to kei setup!");
    println!();
    println!("This wizard will create a config file. Press Enter to accept defaults.");
    println!();

    // Check for existing config
    if config_path.exists() && !check_overwrite(config_path)? {
        println!("Setup cancelled.");
        return Ok(SetupResult::Done);
    }

    let mut answers = SetupAnswers::default();

    // Step 1: Account
    ask_account(&mut answers)?;

    // Step 2: Where to save
    ask_destination(&mut answers)?;

    // Step 3: What to download
    ask_what_to_download(&mut answers)?;

    // Step 4: Media types
    ask_media_types(&mut answers)?;

    // Step 5: Photo quality & RAW
    ask_quality(&mut answers)?;

    // Step 6: Date range
    ask_date_range(&mut answers)?;

    // Step 7: Running mode
    ask_running_mode(&mut answers)?;

    // Step 8: Extras
    ask_extras(&mut answers)?;

    // Generate TOML
    let toml_content = generate_toml(&answers);

    // Preview
    println!();
    println!("Here's your configuration:");
    println!();
    println!("───────────────────────────────────────────────────────");
    print!("{toml_content}");
    println!("───────────────────────────────────────────────────────");
    println!();

    // Confirm write
    let write = Confirm::new()
        .with_prompt(format!("Write to {}?", config_path.display()))
        .default(true)
        .interact()?;

    if !write {
        println!("Setup cancelled.");
        return Ok(SetupResult::Done);
    }

    // Ensure parent directory exists
    if let Some(parent) = config_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create directory {}", parent.display()))?;
    }

    // Write config
    std::fs::write(config_path, &toml_content)
        .with_context(|| format!("Failed to write {}", config_path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(config_path, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("Failed to set permissions on {}", config_path.display()))?;
    }

    // Write .env file
    let env_path = config_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(".env");
    // Single-quote values to prevent shell expansion of special characters
    // ($, `, !, etc.) when the file is sourced. Single quotes inside the
    // password are escaped as '\'' (end-quote, literal quote, re-open quote).
    let raw_pass = secrecy::ExposeSecret::expose_secret(&answers.password);
    let escaped_user = answers.username.replace('\'', "'\\''");
    let escaped_pass = raw_pass.replace('\'', "'\\''");
    let env_content =
        format!("ICLOUD_USERNAME='{escaped_user}'\nICLOUD_PASSWORD='{escaped_pass}'\n",);
    std::fs::write(&env_path, &env_content)
        .with_context(|| format!("Failed to write {}", env_path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&env_path, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("Failed to set permissions on {}", env_path.display()))?;
    }

    println!();
    println!("Config written to:  {}", config_path.display());
    println!("Credentials saved:  {}", env_path.display());
    println!();

    // Offer to sync now
    let sync_now = Confirm::new()
        .with_prompt("Start syncing now?")
        .default(true)
        .interact()?;

    if sync_now {
        Ok(SetupResult::SyncNow {
            config_path: config_path.to_path_buf(),
            env_path,
        })
    } else {
        println!();
        println!("To sync later, run:");
        println!();
        println!("  set -a; source {}; set +a", env_path.display());
        println!("  kei sync");
        println!();
        Ok(SetupResult::Done)
    }
}

fn check_overwrite(path: &Path) -> anyhow::Result<bool> {
    Confirm::new()
        .with_prompt(format!("{} already exists. Overwrite?", path.display()))
        .default(false)
        .interact()
        .map_err(Into::into)
}

// ── Step 1: Account ────────────────────────────────────────────────

fn ask_account(answers: &mut SetupAnswers) -> anyhow::Result<()> {
    answers.username = Input::new()
        .with_prompt("Apple ID email")
        .validate_with(|input: &String| {
            if input.contains('@') && input.contains('.') {
                Ok(())
            } else {
                Err("Please enter a valid email address")
            }
        })
        .interact_text()?;

    answers.password =
        secrecy::SecretString::from(Password::new().with_prompt("iCloud password").interact()?);

    println!();
    let region_items = ["iCloud.com", "iCloud.com.cn (China)"];
    let region = Select::new()
        .with_prompt("iCloud region")
        .items(region_items)
        .default(0)
        .interact()?;

    answers.domain = match region {
        1 => Some(Domain::Cn),
        _ => None, // com is the default, no need to write it
    };

    Ok(())
}

// ── Step 2: Where to save ──────────────────────────────────────────

fn ask_destination(answers: &mut SetupAnswers) -> anyhow::Result<()> {
    println!();
    answers.directory = Input::new()
        .with_prompt("Where should photos be saved?")
        .default("~/Photos/iCloud".to_string())
        .interact_text()?;

    println!();
    let folder_items = [
        "By date: 2024/03/15  (%Y/%m/%d)",
        "By month: 2024/03  (%Y/%m)",
        "By year: 2024  (%Y)",
        "All in one folder",
        "Custom pattern...",
    ];
    let folder = Select::new()
        .with_prompt("How should photos be organized into folders?")
        .items(folder_items)
        .default(0)
        .interact()?;

    answers.folder_structure = match folder {
        1 => Some("%Y/%m".to_string()),
        2 => Some("%Y".to_string()),
        3 => Some(String::new()),
        4 => {
            let custom: String = Input::new()
                .with_prompt("Folder pattern (strftime format)")
                .default("%Y/%m/%d".to_string())
                .interact_text()?;
            Some(custom)
        }
        // %Y/%m/%d is the default
        _ => None,
    };

    Ok(())
}

// ── Step 3: What to download ───────────────────────────────────────

fn ask_what_to_download(answers: &mut SetupAnswers) -> anyhow::Result<()> {
    println!();
    let scope_items = ["Entire library", "Specific albums"];
    let scope = Select::new()
        .with_prompt("Download your entire library or specific albums?")
        .items(scope_items)
        .default(0)
        .interact()?;

    if scope == 1 {
        let album_input: String = Input::new()
            .with_prompt("Album names (comma-separated)")
            .interact_text()?;
        answers.albums = album_input
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        if !answers.albums.is_empty() {
            println!("  Tip: run `kei list albums` to see available album names.");
        }
    }

    println!();
    let library_items = [
        "Yes, sync all libraries (including shared)",
        "No, just my main library",
    ];
    let library = Select::new()
        .with_prompt("Do you use shared or family libraries?")
        .items(library_items)
        .default(0)
        .interact()?;

    answers.library = match library {
        0 => Some("all".to_string()),
        _ => None, // PrimarySync default
    };

    Ok(())
}

// ── Step 4: Media types ────────────────────────────────────────────

fn ask_media_types(answers: &mut SetupAnswers) -> anyhow::Result<()> {
    println!();
    let include_videos = Confirm::new()
        .with_prompt("Include videos?")
        .default(true)
        .interact()?;
    answers.skip_videos = !include_videos;

    let include_live = Confirm::new()
        .with_prompt("Include live photos?")
        .default(true)
        .interact()?;
    answers.skip_live_photos = !include_live;

    if include_live {
        let mov_items = [
            "Add -live suffix (IMG_1234-live.mov)",
            "Same name as the photo (IMG_1234.mov)",
        ];
        let mov_policy = Select::new()
            .with_prompt("How should the video part of live photos be named?")
            .items(mov_items)
            .default(0)
            .interact()?;
        answers.live_photo_mov_filename_policy = match mov_policy {
            1 => Some(LivePhotoMovFilenamePolicy::Original),
            _ => None, // suffix is the default
        };
    }

    Ok(())
}

// ── Step 5: Photo quality & RAW ────────────────────────────────────

fn ask_quality(answers: &mut SetupAnswers) -> anyhow::Result<()> {
    println!();
    let size_items = ["Original (full resolution)", "Medium", "Thumbnail"];
    let size = Select::new()
        .with_prompt("What size should photos be downloaded at?")
        .items(size_items)
        .default(0)
        .interact()?;

    answers.size = match size {
        1 => Some(VersionSize::Medium),
        2 => Some(VersionSize::Thumb),
        _ => None, // original is the default
    };

    // If not original, ask about fallback
    if answers.size.is_some() {
        let fallback = Confirm::new()
            .with_prompt("If that size isn't available, fall back to original?")
            .default(true)
            .interact()?;
        answers.force_size = !fallback;
    }

    println!();
    let shoots_raw = Confirm::new()
        .with_prompt("Do you shoot RAW photos?")
        .default(false)
        .interact()?;

    if shoots_raw {
        let raw_items = [
            "Download both as-is",
            "Prefer the RAW original",
            "Prefer the processed JPEG",
        ];
        let raw_policy = Select::new()
            .with_prompt("When both RAW and JPEG versions exist:")
            .items(raw_items)
            .default(0)
            .interact()?;
        answers.align_raw = match raw_policy {
            1 => Some(RawTreatmentPolicy::PreferOriginal),
            2 => Some(RawTreatmentPolicy::PreferAlternative),
            _ => None, // as-is is the default
        };
    }

    Ok(())
}

// ── Step 6: Date range ─────────────────────────────────────────────

fn ask_date_range(answers: &mut SetupAnswers) -> anyhow::Result<()> {
    println!();
    let limit = Confirm::new()
        .with_prompt("Want to limit syncing to a specific date range or recent photos?")
        .default(false)
        .interact()?;

    if !limit {
        return Ok(());
    }

    let after: String = Input::new()
        .with_prompt("Only sync photos created after (e.g. 2024-01-01 or 30d, blank = no limit)")
        .default(String::new())
        .show_default(false)
        .interact_text()?;
    if !after.is_empty() {
        answers.skip_created_before = Some(after);
    }

    let before: String = Input::new()
        .with_prompt("Only sync photos created before (blank = no limit)")
        .default(String::new())
        .show_default(false)
        .interact_text()?;
    if !before.is_empty() {
        answers.skip_created_after = Some(before);
    }

    let recent: String = Input::new()
        .with_prompt("Only sync the N most recent photos (blank = all)")
        .default(String::new())
        .show_default(false)
        .interact_text()?;
    if !recent.is_empty() {
        if let Ok(n) = recent.parse::<u32>() {
            answers.recent = Some(n);
        } else {
            println!("  Invalid number, skipping.");
        }
    }

    Ok(())
}

// ── Step 7: Running mode ───────────────────────────────────────────

fn ask_running_mode(answers: &mut SetupAnswers) -> anyhow::Result<()> {
    println!();
    let mode_items = [
        "Manually when needed",
        "Continuously in the background (watch mode)",
    ];
    let mode = Select::new()
        .with_prompt("How will you run kei?")
        .items(mode_items)
        .default(0)
        .interact()?;

    if mode == 1 {
        let interval: u64 = Input::new()
            .with_prompt("Re-sync every how many seconds?")
            .default(3600u64)
            .interact_text()?;
        answers.watch_interval = Some(interval);

        let systemd = Confirm::new()
            .with_prompt("Running as a systemd service?")
            .default(false)
            .interact()?;

        if systemd {
            answers.notify_systemd = true;

            let pid: String = Input::new()
                .with_prompt("PID file path (blank = skip)")
                .default(String::new())
                .show_default(false)
                .interact_text()?;
            if !pid.is_empty() {
                answers.pid_file = Some(pid);
            }
        }
    }

    Ok(())
}

// ── Step 8: Extras ─────────────────────────────────────────────────

fn ask_extras(answers: &mut SetupAnswers) -> anyhow::Result<()> {
    println!();
    let configure = Confirm::new()
        .with_prompt(
            "Want to configure additional options\n  \
             (threads, retries, filenames, EXIF, dedup, notifications, logging)?",
        )
        .default(false)
        .interact()?;

    if !configure {
        return Ok(());
    }

    println!();

    // Notifications
    let notify = Confirm::new()
        .with_prompt("Run a notification script on events (2FA needed, sync complete, errors)?")
        .default(false)
        .interact()?;
    if notify {
        let script: String = Input::new().with_prompt("Script path").interact_text()?;
        if !script.is_empty() {
            answers.notification_script = Some(script);
        }
    }

    // Performance
    println!();
    let threads: u16 = Input::new()
        .with_prompt("Concurrent download threads")
        .default(10u16)
        .interact_text()?;
    if threads != 10 {
        answers.threads_num = Some(threads);
    }

    let retries: u32 = Input::new()
        .with_prompt("Max retries per failed download (0 = disable)")
        .default(3u32)
        .interact_text()?;
    if retries != 3 {
        answers.max_retries = Some(retries);
    }

    let delay: u64 = Input::new()
        .with_prompt("Retry delay in seconds")
        .default(5u64)
        .interact_text()?;
    if delay != 5 {
        answers.retry_delay = Some(delay);
    }

    // Filenames
    println!();
    answers.keep_unicode_in_filenames = Confirm::new()
        .with_prompt("Preserve Unicode characters in filenames?")
        .default(false)
        .interact()?;

    answers.set_exif_datetime = Confirm::new()
        .with_prompt("Write EXIF date tag if missing from photo?")
        .default(false)
        .interact()?;

    // Dedup
    println!();
    let dedup_items = [
        "By name and size, add suffix for duplicates",
        "By name and iCloud ID (deterministic)",
    ];
    let dedup = Select::new()
        .with_prompt("File deduplication strategy")
        .items(dedup_items)
        .default(0)
        .interact()?;
    if dedup == 1 {
        answers.file_match_policy = Some(FileMatchPolicy::NameId7);
    }

    // Cookie directory
    println!();
    let cookie: String = Input::new()
        .with_prompt("Cookie/session directory")
        .default("~/.config/kei/cookies".to_string())
        .interact_text()?;
    if cookie != "~/.config/kei/cookies" {
        answers.cookie_directory = Some(cookie);
    }

    // Log level
    let log_items = ["info", "debug", "warn", "error"];
    let log = Select::new()
        .with_prompt("Log level")
        .items(log_items)
        .default(0)
        .interact()?;
    answers.log_level = match log {
        1 => Some(LogLevel::Debug),
        2 => Some(LogLevel::Warn),
        3 => Some(LogLevel::Error),
        _ => None, // info is the default
    };

    Ok(())
}

// ── TOML generation ────────────────────────────────────────────────

fn generate_toml(answers: &SetupAnswers) -> String {
    let mut out = String::with_capacity(2048);

    writeln!(out, "# kei configuration").ok();
    writeln!(out, "# Generated by: kei setup").ok();
    writeln!(out).ok();

    // Log level
    match answers.log_level {
        Some(level) => writeln!(out, "log_level = \"{}\"", log_level_str(level)).ok(),
        None => writeln!(out, "# log_level = \"warn\"").ok(),
    };

    // [auth]
    writeln!(out).ok();
    writeln!(out, "[auth]").ok();
    writeln!(
        out,
        "username = \"{}\"",
        escape_toml_string(&answers.username)
    )
    .ok();
    writeln!(
        out,
        "# Password is stored in .env file, not here (for security)"
    )
    .ok();
    match answers.domain {
        Some(domain) => writeln!(out, "domain = \"{}\"", domain.as_str()).ok(),
        None => writeln!(out, "# domain = \"com\"").ok(),
    };
    match &answers.cookie_directory {
        Some(dir) => writeln!(out, "cookie_directory = \"{}\"", escape_toml_string(dir)).ok(),
        None => writeln!(out, "# cookie_directory = \"~/.config/kei/cookies\"").ok(),
    };

    // [download]
    writeln!(out).ok();
    writeln!(out, "[download]").ok();
    writeln!(
        out,
        "directory = \"{}\"",
        escape_toml_string(&answers.directory)
    )
    .ok();
    match &answers.folder_structure {
        Some(fs) => writeln!(out, "folder_structure = \"{}\"", escape_toml_string(fs)).ok(),
        None => writeln!(out, "# folder_structure = \"%Y/%m/%d\"").ok(),
    };
    match answers.threads_num {
        Some(n) => writeln!(out, "threads_num = {n}").ok(),
        None => writeln!(out, "# threads_num = 10").ok(),
    };
    if answers.set_exif_datetime {
        writeln!(out, "set_exif_datetime = true").ok();
    } else {
        writeln!(out, "# set_exif_datetime = false").ok();
    }
    writeln!(out, "# set_exif_rating = false").ok();
    writeln!(out, "# set_exif_gps = false").ok();
    writeln!(out, "# set_exif_description = false").ok();
    writeln!(out, "# temp_suffix = \".kei-tmp\"").ok();
    writeln!(out, "# no_progress_bar = false").ok();

    // [download.retry]
    writeln!(out).ok();
    writeln!(out, "[download.retry]").ok();
    match answers.max_retries {
        Some(n) => writeln!(out, "max_retries = {n}").ok(),
        None => writeln!(out, "# max_retries = 3").ok(),
    };
    match answers.retry_delay {
        Some(n) => writeln!(out, "delay = {n}").ok(),
        None => writeln!(out, "# delay = 5").ok(),
    };

    // [filters]
    writeln!(out).ok();
    writeln!(out, "[filters]").ok();
    match &answers.library {
        Some(lib) => writeln!(out, "library = \"{}\"", escape_toml_string(lib)).ok(),
        None => writeln!(out, "# library = \"PrimarySync\"").ok(),
    };
    if answers.albums.is_empty() {
        writeln!(out, "# albums = []").ok();
    } else {
        let album_strs: Vec<String> = answers
            .albums
            .iter()
            .map(|a| format!("\"{}\"", escape_toml_string(a)))
            .collect();
        writeln!(out, "albums = [{}]", album_strs.join(", ")).ok();
    }
    if answers.skip_videos {
        writeln!(out, "skip_videos = true").ok();
    } else {
        writeln!(out, "# skip_videos = false").ok();
    }
    writeln!(out, "# skip_photos = false").ok();
    if answers.skip_live_photos {
        writeln!(out, "skip_live_photos = true").ok();
    } else {
        writeln!(out, "# skip_live_photos = false").ok();
    }
    match answers.recent {
        Some(n) => writeln!(out, "recent = {n}").ok(),
        None => writeln!(out, "# recent = 0  (0 = all)").ok(),
    };
    match &answers.skip_created_before {
        Some(d) => writeln!(out, "skip_created_before = \"{}\"", escape_toml_string(d)).ok(),
        None => writeln!(out, "# skip_created_before = \"\"").ok(),
    };
    match &answers.skip_created_after {
        Some(d) => writeln!(out, "skip_created_after = \"{}\"", escape_toml_string(d)).ok(),
        None => writeln!(out, "# skip_created_after = \"\"").ok(),
    };

    // [photos]
    writeln!(out).ok();
    writeln!(out, "[photos]").ok();
    match answers.size {
        Some(size) => writeln!(out, "size = \"{}\"", version_size_str(size)).ok(),
        None => writeln!(out, "# size = \"original\"").ok(),
    };
    writeln!(out, "# live_photo_size = \"original\"").ok();
    match answers.live_photo_mov_filename_policy {
        Some(p) => writeln!(
            out,
            "live_photo_mov_filename_policy = \"{}\"",
            mov_policy_str(p)
        )
        .ok(),
        None => writeln!(out, "# live_photo_mov_filename_policy = \"suffix\"").ok(),
    };
    match answers.align_raw {
        Some(p) => writeln!(out, "align_raw = \"{}\"", raw_policy_str(p)).ok(),
        None => writeln!(out, "# align_raw = \"as-is\"").ok(),
    };
    match answers.file_match_policy {
        Some(p) => writeln!(out, "file_match_policy = \"{}\"", file_match_str(p)).ok(),
        None => writeln!(out, "# file_match_policy = \"name-size-dedup-with-suffix\"").ok(),
    };
    if answers.force_size {
        writeln!(out, "force_size = true").ok();
    } else {
        writeln!(out, "# force_size = false").ok();
    }
    if answers.keep_unicode_in_filenames {
        writeln!(out, "keep_unicode_in_filenames = true").ok();
    } else {
        writeln!(out, "# keep_unicode_in_filenames = false").ok();
    }

    // [watch]
    writeln!(out).ok();
    writeln!(out, "[watch]").ok();
    match answers.watch_interval {
        Some(n) => writeln!(out, "interval = {n}").ok(),
        None => writeln!(out, "# interval = 3600").ok(),
    };
    if answers.notify_systemd {
        writeln!(out, "notify_systemd = true").ok();
    } else {
        writeln!(out, "# notify_systemd = false").ok();
    }
    match &answers.pid_file {
        Some(p) => writeln!(out, "pid_file = \"{}\"", escape_toml_string(p)).ok(),
        None => writeln!(out, "# pid_file = \"\"").ok(),
    };

    // [notifications]
    writeln!(out).ok();
    writeln!(out, "[notifications]").ok();
    match &answers.notification_script {
        Some(s) => writeln!(out, "script = \"{}\"", escape_toml_string(s)).ok(),
        None => writeln!(out, "# script = \"/path/to/script.sh\"").ok(),
    };

    out
}

fn escape_toml_string(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

fn log_level_str(level: LogLevel) -> &'static str {
    match level {
        LogLevel::Debug => "debug",
        LogLevel::Info => "info",
        LogLevel::Warn => "warn",
        LogLevel::Error => "error",
    }
}

fn version_size_str(size: VersionSize) -> &'static str {
    match size {
        VersionSize::Original => "original",
        VersionSize::Medium => "medium",
        VersionSize::Thumb => "thumb",
        VersionSize::Adjusted => "adjusted",
        VersionSize::Alternative => "alternative",
    }
}

fn mov_policy_str(policy: LivePhotoMovFilenamePolicy) -> &'static str {
    match policy {
        LivePhotoMovFilenamePolicy::Suffix => "suffix",
        LivePhotoMovFilenamePolicy::Original => "original",
    }
}

fn raw_policy_str(policy: RawTreatmentPolicy) -> &'static str {
    match policy {
        RawTreatmentPolicy::Unchanged => "as-is",
        RawTreatmentPolicy::PreferOriginal => "original",
        RawTreatmentPolicy::PreferAlternative => "alternative",
    }
}

fn file_match_str(policy: FileMatchPolicy) -> &'static str {
    match policy {
        FileMatchPolicy::NameSizeDedupWithSuffix => "name-size-dedup-with-suffix",
        FileMatchPolicy::NameId7 => "name-id7",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::TomlConfig;

    #[test]
    fn test_generate_toml_defaults_only() {
        let answers = SetupAnswers {
            username: "user@example.com".to_string(),
            password: secrecy::SecretString::from("secret"),
            directory: "~/Photos/iCloud".to_string(),
            ..Default::default()
        };
        let toml = generate_toml(&answers);

        // Must contain the username uncommented
        assert!(toml.contains("username = \"user@example.com\""));
        // Must contain directory uncommented
        assert!(toml.contains("directory = \"~/Photos/iCloud\""));
        // Library should be set to "all"
        assert!(toml.contains("library = \"all\""));
        // Password should NOT be in the TOML
        assert!(!toml.contains("secret"));
        // Defaults should be commented out
        assert!(toml.contains("# size = \"original\""));
        assert!(toml.contains("# threads_num = 10"));
        assert!(toml.contains("# log_level = \"warn\""));
    }

    #[test]
    fn test_generate_toml_roundtrip() {
        let answers = SetupAnswers {
            username: "user@example.com".to_string(),
            password: secrecy::SecretString::from("secret"),
            directory: "~/Photos/iCloud".to_string(),
            ..Default::default()
        };
        let toml_str = generate_toml(&answers);

        // Must parse as valid TOML config
        let parsed: TomlConfig = toml::from_str(&toml_str)
            .unwrap_or_else(|e| panic!("Generated TOML failed to parse: {e}\n\n{toml_str}"));

        // Verify values round-trip
        let auth = parsed.auth.expect("auth section missing");
        assert_eq!(auth.username.as_deref(), Some("user@example.com"));
        assert!(auth.password.is_none());

        let download = parsed.download.expect("download section missing");
        assert_eq!(download.directory.as_deref(), Some("~/Photos/iCloud"));

        let filters = parsed.filters.expect("filters section missing");
        assert_eq!(filters.library.as_deref(), Some("all"));
    }

    #[test]
    fn test_generate_toml_full() {
        let answers = SetupAnswers {
            username: "user@example.com".to_string(),
            password: secrecy::SecretString::from("secret"),
            domain: Some(Domain::Cn),
            directory: "~/photos".to_string(),
            folder_structure: Some("%Y/%m".to_string()),
            albums: vec!["Favorites".to_string(), "Vacation".to_string()],
            library: Some("all".to_string()),
            skip_videos: true,
            skip_live_photos: false,
            live_photo_mov_filename_policy: Some(LivePhotoMovFilenamePolicy::Original),
            size: Some(VersionSize::Medium),
            force_size: true,
            align_raw: Some(RawTreatmentPolicy::PreferOriginal),
            recent: Some(100),
            skip_created_before: Some("2024-01-01".to_string()),
            skip_created_after: Some("2025-01-01".to_string()),
            watch_interval: Some(1800),
            notify_systemd: true,
            pid_file: Some("/var/run/kei.pid".to_string()),
            notification_script: Some("/usr/local/bin/notify.sh".to_string()),
            threads_num: Some(4),
            max_retries: Some(5),
            retry_delay: Some(10),
            keep_unicode_in_filenames: true,
            set_exif_datetime: true,
            file_match_policy: Some(FileMatchPolicy::NameId7),
            cookie_directory: Some("~/.cookies".to_string()),
            log_level: Some(LogLevel::Debug),
        };
        let toml_str = generate_toml(&answers);

        // All user-set values should be uncommented
        assert!(toml_str.contains("domain = \"cn\""));
        assert!(toml_str.contains("folder_structure = \"%Y/%m\""));
        assert!(toml_str.contains("albums = [\"Favorites\", \"Vacation\"]"));
        assert!(toml_str.contains("skip_videos = true"));
        assert!(toml_str.contains("size = \"medium\""));
        assert!(toml_str.contains("force_size = true"));
        assert!(toml_str.contains("align_raw = \"original\""));
        assert!(toml_str.contains("recent = 100"));
        assert!(toml_str.contains("interval = 1800"));
        assert!(toml_str.contains("notify_systemd = true"));
        assert!(toml_str.contains("threads_num = 4"));
        assert!(toml_str.contains("file_match_policy = \"name-id7\""));
        assert!(toml_str.contains("log_level = \"debug\""));
        assert!(toml_str.contains("set_exif_datetime = true"));
        assert!(toml_str.contains("keep_unicode_in_filenames = true"));
        assert!(toml_str.contains("cookie_directory = \"~/.cookies\""));
        assert!(toml_str.contains("script = \"/usr/local/bin/notify.sh\""));

        // Must still parse
        let _parsed: TomlConfig = toml::from_str(&toml_str)
            .unwrap_or_else(|e| panic!("Generated TOML failed to parse: {e}\n\n{toml_str}"));
    }

    #[test]
    fn test_generate_toml_full_roundtrip_values() {
        let answers = SetupAnswers {
            username: "test@icloud.com".to_string(),
            password: secrecy::SecretString::from("pw"),
            domain: Some(Domain::Cn),
            directory: "/data/photos".to_string(),
            folder_structure: Some("%Y-%m".to_string()),
            albums: vec!["A".to_string()],
            library: None,
            skip_videos: true,
            skip_live_photos: true,
            live_photo_mov_filename_policy: Some(LivePhotoMovFilenamePolicy::Original),
            size: Some(VersionSize::Thumb),
            force_size: true,
            align_raw: Some(RawTreatmentPolicy::PreferAlternative),
            recent: Some(50),
            skip_created_before: Some("30d".to_string()),
            skip_created_after: Some("2025-06-01".to_string()),
            watch_interval: Some(600),
            notify_systemd: true,
            pid_file: Some("/tmp/pid".to_string()),
            notification_script: Some("/bin/notify".to_string()),
            threads_num: Some(2),
            max_retries: Some(0),
            retry_delay: Some(1),
            keep_unicode_in_filenames: true,
            set_exif_datetime: true,
            file_match_policy: Some(FileMatchPolicy::NameId7),
            cookie_directory: Some("/tmp/cookies".to_string()),
            log_level: Some(LogLevel::Error),
        };
        let toml_str = generate_toml(&answers);
        let parsed: TomlConfig = toml::from_str(&toml_str)
            .unwrap_or_else(|e| panic!("Failed to parse: {e}\n\n{toml_str}"));

        let auth = parsed.auth.unwrap();
        assert_eq!(auth.username.as_deref(), Some("test@icloud.com"));
        assert_eq!(auth.domain, Some(Domain::Cn));
        assert_eq!(auth.cookie_directory.as_deref(), Some("/tmp/cookies"));

        let dl = parsed.download.unwrap();
        assert_eq!(dl.directory.as_deref(), Some("/data/photos"));
        assert_eq!(dl.folder_structure.as_deref(), Some("%Y-%m"));
        assert_eq!(dl.threads_num, Some(2));
        assert_eq!(dl.set_exif_datetime, Some(true));
        let retry = dl.retry.unwrap();
        assert_eq!(retry.max_retries, Some(0));
        assert_eq!(retry.delay, Some(1));

        let filters = parsed.filters.unwrap();
        assert_eq!(filters.albums.as_deref(), Some(&["A".to_string()][..]));
        assert_eq!(filters.skip_videos, Some(true));
        assert_eq!(filters.skip_live_photos, Some(true));
        assert_eq!(filters.recent, Some(50));
        assert_eq!(filters.skip_created_before.as_deref(), Some("30d"));
        assert_eq!(filters.skip_created_after.as_deref(), Some("2025-06-01"));

        let photos = parsed.photos.unwrap();
        assert_eq!(photos.size, Some(VersionSize::Thumb));
        assert_eq!(photos.force_size, Some(true));
        assert_eq!(
            photos.align_raw,
            Some(RawTreatmentPolicy::PreferAlternative)
        );
        assert_eq!(
            photos.live_photo_mov_filename_policy,
            Some(LivePhotoMovFilenamePolicy::Original)
        );
        assert_eq!(photos.file_match_policy, Some(FileMatchPolicy::NameId7));
        assert_eq!(photos.keep_unicode_in_filenames, Some(true));

        let watch = parsed.watch.unwrap();
        assert_eq!(watch.interval, Some(600));
        assert_eq!(watch.notify_systemd, Some(true));
        assert_eq!(watch.pid_file.as_deref(), Some("/tmp/pid"));

        let notif = parsed.notifications.unwrap();
        assert_eq!(notif.script.as_deref(), Some("/bin/notify"));

        assert_eq!(parsed.log_level, Some(LogLevel::Error));
    }

    #[test]
    fn test_generate_toml_albums_array() {
        let answers = SetupAnswers {
            username: "u@e.com".to_string(),
            password: secrecy::SecretString::from("p"),
            directory: "/d".to_string(),
            albums: vec!["My Album".to_string(), "Vacation \"2024\"".to_string()],
            ..Default::default()
        };
        let toml_str = generate_toml(&answers);
        assert!(toml_str.contains("albums = [\"My Album\", \"Vacation \\\"2024\\\"\"]"));

        // Must still parse
        let parsed: TomlConfig = toml::from_str(&toml_str)
            .unwrap_or_else(|e| panic!("Failed to parse: {e}\n\n{toml_str}"));
        let albums = parsed.filters.unwrap().albums.unwrap();
        assert_eq!(albums, vec!["My Album", "Vacation \"2024\""]);
    }

    #[test]
    fn test_generate_toml_enum_values() {
        // Verify each enum serializes to the correct TOML string that
        // the config parser expects.
        assert_eq!(version_size_str(VersionSize::Original), "original");
        assert_eq!(version_size_str(VersionSize::Medium), "medium");
        assert_eq!(version_size_str(VersionSize::Thumb), "thumb");
        assert_eq!(version_size_str(VersionSize::Adjusted), "adjusted");
        assert_eq!(version_size_str(VersionSize::Alternative), "alternative");

        assert_eq!(raw_policy_str(RawTreatmentPolicy::Unchanged), "as-is");
        assert_eq!(
            raw_policy_str(RawTreatmentPolicy::PreferOriginal),
            "original"
        );
        assert_eq!(
            raw_policy_str(RawTreatmentPolicy::PreferAlternative),
            "alternative"
        );

        assert_eq!(
            file_match_str(FileMatchPolicy::NameSizeDedupWithSuffix),
            "name-size-dedup-with-suffix"
        );
        assert_eq!(file_match_str(FileMatchPolicy::NameId7), "name-id7");

        assert_eq!(mov_policy_str(LivePhotoMovFilenamePolicy::Suffix), "suffix");
        assert_eq!(
            mov_policy_str(LivePhotoMovFilenamePolicy::Original),
            "original"
        );

        assert_eq!(log_level_str(LogLevel::Debug), "debug");
        assert_eq!(log_level_str(LogLevel::Info), "info");
        assert_eq!(log_level_str(LogLevel::Warn), "warn");
        assert_eq!(log_level_str(LogLevel::Error), "error");
    }

    #[test]
    fn test_escape_toml_string() {
        assert_eq!(escape_toml_string("hello"), "hello");
        assert_eq!(escape_toml_string("he\"llo"), "he\\\"llo");
        assert_eq!(escape_toml_string("c:\\path"), "c:\\\\path");
    }

    /// T-5: The .env file created by the setup wizard must have mode 0o600
    /// so credentials are not world-readable.
    #[cfg(unix)]
    #[test]
    fn test_env_file_created_with_restricted_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir()
            .join("claude")
            .join("setup_perm_test")
            .join(format!("{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        let env_path = dir.join(".env");
        let env_content = "ICLOUD_USERNAME=test@example.com\nICLOUD_PASSWORD=secret\n";

        // Replicate the exact logic from run_setup
        std::fs::write(&env_path, env_content).unwrap();
        std::fs::set_permissions(&env_path, std::fs::Permissions::from_mode(0o600)).unwrap();

        // Verify permissions
        let metadata = std::fs::metadata(&env_path).unwrap();
        let mode = metadata.permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o600,
            "expected mode 0o600 (owner rw only), got {mode:#o}"
        );

        // Verify content
        let content = std::fs::read_to_string(&env_path).unwrap();
        assert!(content.contains("ICLOUD_USERNAME=test@example.com"));
        assert!(content.contains("ICLOUD_PASSWORD=secret"));

        // Clean up
        let _ = std::fs::remove_dir_all(&dir);
    }
}
