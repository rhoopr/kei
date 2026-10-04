#![allow(
    clippy::print_stdout,
    reason = "interactive setup wizard whose purpose is to drive a stdout dialogue"
)]

use std::fmt::Write as FmtWrite;
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use console::Style;
use dialoguer::{Confirm, Input, Password, Select};
use indicatif::ProgressBar;
use secrecy::ExposeSecret;

use crate::credential::{CredentialBackend, CredentialStore};
use crate::types::{
    Domain, FileMatchPolicy, LivePhotoMode, LivePhotoMovFilenamePolicy, LogLevel, PhotoResolution,
    RawPolicy,
};

/// Result of the setup wizard — either the user wants to sync now or just exit.
#[derive(Debug)]
pub(crate) enum SetupResult {
    /// User chose to sync now. Contains the config path and any one-shot
    /// password needed for explicit `.env` fallback.
    SyncNow {
        config_path: PathBuf,
        one_shot_password: Option<secrecy::SecretString>,
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
    secret_source: SetupSecretSource,

    // Destination. `folder_structure` is the unfiled-pass template;
    // `folder_structure_albums` is the v0.13 per-album template. The wizard
    // sets both together when the user picks a date hierarchy so album passes
    // get the same layout as the unfiled pass (matches v0.12 behavior).
    directory: String,
    folder_structure: Option<String>,
    folder_structure_albums: Option<String>,

    // What to download
    albums: Vec<String>,
    /// v0.13+ array form. Empty = use kei's default (`primary`).
    /// `["all"]` = every library (PrimarySync + every shared zone).
    libraries: Vec<String>,
    /// v0.13+ smart-folder selector (Favorites, Hidden, etc.). Empty = default
    /// (`none`); non-empty = emit `[filters].smart_folders`.
    smart_folders: Vec<String>,
    /// Optional smart-folder pass template for `[download].folder_structure_smart_folders`.
    /// `None` leaves the runtime default (`{smart-folder}`).
    folder_structure_smart_folders: Option<String>,
    /// `Some(false)` emits `[filters].unfiled = false` (used when the user
    /// picks specific albums and doesn't also want every other photo).
    /// `None` keeps the v0.13 default (`true`).
    unfiled: Option<bool>,
    /// v0.13 `[filters].filename_exclude` patterns; empty = don't emit.
    filename_exclude: Vec<String>,

    // Media types
    skip_videos: bool,
    /// `Some(_)` emits `[photos].live_photo_mode = "..."`; `None` keeps the
    /// `Both` default. Replaces the wizard's old binary skip prompt.
    live_photo_mode: Option<LivePhotoMode>,
    live_photo_mov_filename_policy: Option<LivePhotoMovFilenamePolicy>,

    // Quality
    resolution: Option<PhotoResolution>,
    force_resolution: bool,
    raw_policy: Option<RawPolicy>,

    // Date range
    recent: Option<u32>,
    skip_created_before: Option<String>,
    skip_created_after: Option<String>,

    // Running mode
    watch_interval: Option<u64>,
    notify_systemd: bool,
    pid_file: Option<String>,
    /// `[watch].reconcile_every_n_cycles`; only meaningful in watch mode.
    reconcile_every_n_cycles: Option<u64>,

    // Extras
    notification_script: Option<String>,
    threads_num: Option<u16>,
    max_retries: Option<u32>,
    /// Raw user input string (e.g. `"10MB"`); validated by the config layer's
    /// `parse_bandwidth_limit` on next sync. Empty = no limit.
    bandwidth_limit: Option<String>,
    keep_unicode_in_filenames: bool,
    set_exif_datetime: bool,
    #[cfg(feature = "xmp")]
    embed_xmp: bool,
    #[cfg(feature = "xmp")]
    xmp_sidecar: bool,
    file_match_policy: Option<FileMatchPolicy>,
    /// Top-level `data_dir` in the emitted TOML.
    data_dir: Option<String>,
    log_level: Option<LogLevel>,
    /// `[ui].friendly`. `None` keeps the section out of the emitted TOML so
    /// the runtime default-on-for-TTY policy applies. `Some(false)` lets a
    /// user opt out at setup time without having to remember the flag later.
    ui_friendly: Option<bool>,
}

impl Default for SetupAnswers {
    fn default() -> Self {
        Self {
            username: String::new(),
            password: secrecy::SecretString::from(String::new()),
            domain: None,
            secret_source: SetupSecretSource::CredentialStore,
            directory: "~/Photos/iCloud".to_string(),
            folder_structure: None,
            folder_structure_albums: None,
            albums: Vec::new(),
            libraries: vec!["all".to_string()],
            smart_folders: Vec::new(),
            folder_structure_smart_folders: None,
            unfiled: None,
            filename_exclude: Vec::new(),
            skip_videos: false,
            live_photo_mode: None,
            live_photo_mov_filename_policy: None,
            resolution: None,
            force_resolution: false,
            raw_policy: None,
            recent: None,
            skip_created_before: None,
            skip_created_after: None,
            watch_interval: None,
            notify_systemd: false,
            pid_file: None,
            reconcile_every_n_cycles: None,
            notification_script: None,
            threads_num: None,
            max_retries: None,
            bandwidth_limit: None,
            keep_unicode_in_filenames: false,
            set_exif_datetime: false,
            #[cfg(feature = "xmp")]
            embed_xmp: false,
            #[cfg(feature = "xmp")]
            xmp_sidecar: false,
            file_match_policy: None,
            data_dir: None,
            log_level: None,
            ui_friendly: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum SetupSecretSource {
    CredentialStore,
    PasswordFile(String),
    PasswordCommand(String),
    EnvFile,
}

impl SetupSecretSource {
    fn needs_password_prompt(&self) -> bool {
        matches!(self, Self::CredentialStore | Self::EnvFile)
    }
}

#[derive(Debug)]
struct SetupWriteResult {
    credential_backend: Option<CredentialBackend>,
    env_path: Option<PathBuf>,
}

// ── Delight helpers ──────────────────────────────────────────────

/// Print a dimmed section divider for visual structure between wizard steps.
/// Run `f` with a ticking spinner showing `msg`. Clears the spinner when done.
fn spinner_for<R>(msg: &'static str, f: impl FnOnce() -> R) -> R {
    let spinner = ProgressBar::new_spinner();
    spinner.set_message(msg);
    spinner.enable_steady_tick(std::time::Duration::from_millis(80));
    let result = f();
    spinner.finish_and_clear();
    result
}

fn section_header(text: &str) {
    let dim = Style::new().dim();
    println!();
    println!("{}", dim.apply_to(format!("  ── {text} ──")));
}

/// Print a line prefixed with a green bold checkmark.
fn check(text: &str) {
    let mark = Style::new().green().bold();
    println!("{} {text}", mark.apply_to("✓"));
}

/// Dim every comment line in a generated TOML string so the active
/// keys stand out. Returns a new String with ANSI escapes injected.
fn dim_comments(toml: &str) -> String {
    let dim = Style::new().dim();
    toml.lines()
        .map(|line| {
            if line.trim_start().starts_with('#') {
                dim.apply_to(line).to_string()
            } else if line.trim_start().starts_with('[') {
                Style::new().cyan().bold().apply_to(line).to_string()
            } else {
                line.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn print_toml_preview(toml_content: &str) {
    println!();
    println!("───────────────────────────────────────────────────────");
    print!("{}", dim_comments(toml_content));
    println!("───────────────────────────────────────────────────────");
    println!();
}

pub(crate) fn run_setup(
    config_path: &Path,
    input_mode: crate::InputMode,
) -> anyhow::Result<SetupResult> {
    if !input_mode.can_prompt() {
        bail!("The setup wizard needs an interactive terminal.");
    }

    println!();
    println!("Hey! Let's get kei set up.");
    println!();
    println!("A few quick questions, then you'll be syncing.");
    println!("Press Enter to accept a default — it's always a safe bet.");
    println!();

    // Check for existing config
    if config_path.exists() && !check_overwrite(config_path)? {
        println!("Setup cancelled.");
        return Ok(SetupResult::Done);
    }

    let mut answers = SetupAnswers::default();

    // Step 1: Account
    section_header("Account");
    ask_account_identity(&mut answers)?;
    check(&format!("Glad you're here, {}.", answers.username));

    // Step 2: Secrets
    section_header("Secrets");
    ask_secret_source(&mut answers)?;
    if answers.secret_source.needs_password_prompt() {
        ask_account_password(&mut answers)?;
    }

    // Step 3: Where to save
    section_header("Where to save");
    ask_destination(&mut answers)?;

    // Step 4: What to download
    section_header("What to sync");
    ask_what_to_download(&mut answers)?;
    if !answers.albums.is_empty() {
        check("Keeping it focused.");
    }

    // Step 5: Media types
    section_header("Media types");
    ask_media_types(&mut answers)?;

    // Step 6: Photo quality & RAW
    section_header("Quality");
    ask_quality(&mut answers)?;

    // Step 7: Date range
    section_header("Date range");
    ask_date_range(&mut answers)?;

    // Step 8: Running mode
    section_header("Running mode");
    ask_running_mode(&mut answers)?;
    if answers.watch_interval.is_some() {
        check("Set-and-forget. Kei will keep an eye on things.");
    }

    // Step 9: Extras
    section_header("Extras");
    ask_extras(&mut answers)?;

    apply_library_scoped_templates_for_all_libraries(&mut answers);

    // Generate TOML with a brief spinner so the user sees something happening.
    let toml_content: String = spinner_for("Building your config...", || generate_toml(&answers));
    check("All done.");
    println!();

    // The Select offers "Show again" so users on small terminals can
    // re-read the config after it scrolled past.
    let line_count = toml_content.lines().count();
    print_toml_preview(&toml_content);
    loop {
        let action_items = [
            format!("Write to {}", config_path.display()),
            format!("Show full configuration again ({line_count} lines)"),
            "Cancel and exit without writing".to_string(),
        ];
        let action = Select::new()
            .with_prompt("Look good?")
            .items(&action_items)
            .default(0)
            .interact()?;
        match action {
            0 => break,
            1 => print_toml_preview(&toml_content),
            _ => {
                println!("Setup cancelled.");
                return Ok(SetupResult::Done);
            }
        }
    }

    // Write files with a spinner for visual closure.
    let write_result = spinner_for("Writing your config...", || {
        write_setup_files(config_path, &toml_content, &answers)
    })?;

    check("Config saved.");
    println!();
    println!("  Config  →  {}", config_path.display());
    print_secret_summary(&answers, &write_result);
    println!();
    let bold = Style::new().bold();
    println!("{}", bold.apply_to("You're all set."));
    if answers.watch_interval.is_some() {
        println!();
        println!(
            "  {} to run continuously in the background.",
            bold.apply_to(format!("kei install --config {}", config_path.display()))
        );
    }
    println!();

    // Offer to sync now
    let sync_now = Confirm::new()
        .with_prompt("Want to kick off your first sync right now?")
        .default(true)
        .interact()?;

    if sync_now {
        let one_shot_password = match answers.secret_source {
            SetupSecretSource::EnvFile => Some(answers.password),
            _ => None,
        };
        Ok(SetupResult::SyncNow {
            config_path: config_path.to_path_buf(),
            one_shot_password,
        })
    } else {
        println!();
        println!("No rush. When you're ready:");
        println!();
        if let Some(env_path) = &write_result.env_path {
            print_load_env_snippet(env_path);
        }
        println!("  {}", sync_command_for_config(config_path));
        println!();
        Ok(SetupResult::Done)
    }
}

fn apply_library_scoped_templates_for_all_libraries(answers: &mut SetupAnswers) {
    if answers.libraries.as_slice() != ["all"] {
        return;
    }

    answers.folder_structure = Some(library_template_or_default(
        answers.folder_structure.as_deref(),
        "%Y/%m/%d",
    ));
}

fn ensure_library_token(template: &str) -> String {
    if template.contains("{library}") {
        return template.to_string();
    }
    if template.is_empty() {
        return "{library}".to_string();
    }
    format!("{{library}}/{template}")
}

fn library_template_or_default(template: Option<&str>, default: &str) -> String {
    ensure_library_token(template.unwrap_or(default))
}

fn single_quoted_shell(value: &str) -> String {
    format!("'{}'", shell_single_quote_escape(value))
}

fn sync_command_for_config(config_path: &Path) -> String {
    let path = config_path.to_string_lossy();
    format!("kei --config {} sync", single_quoted_shell(&path))
}

/// Print the right "load .env into this shell" command for the user's shell.
/// Detects the shell from `$SHELL`. Defaults to the bash/zsh form because
/// it's the most common on Linux and macOS; calls out fish explicitly because
/// `set -a` doesn't exist there and the bash snippet would silently no-op.
fn print_load_env_snippet(env_path: &Path) {
    let shell = std::env::var("SHELL").unwrap_or_default();
    let shell_name = Path::new(&shell)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("");
    let env_display = env_path.display();
    match shell_name {
        "fish" => {
            // fish doesn't have `set -a`, and the .env file's single-quoted
            // values would be passed through verbatim by a naive
            // `(cat | string split)` loop. Cleanest reliable one-liner: have
            // bash do the parsing, then exec fish so the inherited env
            // carries through.
            println!("  # fish: load .env via a bash subshell, then continue in fish");
            println!("  bash -c 'set -a; source {env_display}; set +a; exec fish'");
        }
        _ => {
            // bash / zsh / sh / dash / ksh: POSIX `set -a` + source.
            println!("  set -a; source {env_display}; set +a");
        }
    }
}

fn write_setup_files(
    config_path: &Path,
    toml_content: &str,
    answers: &SetupAnswers,
) -> anyhow::Result<SetupWriteResult> {
    write_setup_files_with_store(
        config_path,
        toml_content,
        answers,
        |username, config_dir, pw| {
            CredentialStore::new(
                username,
                config_dir,
                answers.domain.unwrap_or(Domain::Com).as_str(),
            )
            .store(pw)
        },
    )
}

fn write_setup_files_with_store(
    config_path: &Path,
    toml_content: &str,
    answers: &SetupAnswers,
    store_password: impl FnOnce(&str, &Path, &str) -> anyhow::Result<CredentialBackend>,
) -> anyhow::Result<SetupWriteResult> {
    if let Some(parent) = config_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("Could not create directory {}", parent.display()))?;
    }

    let (credential_backend, write_env) = match &answers.secret_source {
        SetupSecretSource::CredentialStore => {
            let config_dir = credential_store_dir(answers);
            let backend = store_password(
                &answers.username,
                &config_dir,
                answers.password.expose_secret(),
            )?;
            (Some(backend), false)
        }
        SetupSecretSource::PasswordFile(_) | SetupSecretSource::PasswordCommand(_) => (None, false),
        SetupSecretSource::EnvFile => (None, true),
    };
    write_config_file(config_path, toml_content)?;
    let env_path = if write_env {
        Some(write_env_file(config_path, answers)?)
    } else {
        None
    };

    Ok(SetupWriteResult {
        credential_backend,
        env_path,
    })
}

fn write_config_file(config_path: &Path, toml_content: &str) -> anyhow::Result<()> {
    std::fs::write(config_path, toml_content)
        .with_context(|| format!("Could not write {}", config_path.display()))?;
    #[cfg(unix)]
    set_restricted_permissions(config_path)?;
    Ok(())
}

fn credential_store_dir(answers: &SetupAnswers) -> PathBuf {
    match &answers.data_dir {
        Some(dir) => crate::config::expand_tilde(dir),
        None => crate::config::default_cookie_dir(),
    }
}

fn write_env_file(config_path: &Path, answers: &SetupAnswers) -> anyhow::Result<PathBuf> {
    let env_path = config_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(".env");
    let escaped_user = shell_single_quote_escape(&answers.username);
    let escaped_pass = shell_single_quote_escape(answers.password.expose_secret());
    let env_content =
        format!("ICLOUD_USERNAME='{escaped_user}'\nICLOUD_PASSWORD='{escaped_pass}'\n",);
    std::fs::write(&env_path, &env_content)
        .with_context(|| format!("Could not write {}", env_path.display()))?;
    #[cfg(unix)]
    set_restricted_permissions(&env_path)?;
    Ok(env_path)
}

fn shell_single_quote_escape(value: &str) -> String {
    value.replace('\'', "'\\''")
}

#[cfg(unix)]
fn set_restricted_permissions(path: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .with_context(|| format!("Could not set secure permissions on {}", path.display()))
}

fn print_secret_summary(answers: &SetupAnswers, write_result: &SetupWriteResult) {
    for line in secret_summary_lines(answers, write_result) {
        println!("{line}");
    }
}

fn secret_summary_lines(answers: &SetupAnswers, write_result: &SetupWriteResult) -> Vec<String> {
    match &answers.secret_source {
        SetupSecretSource::CredentialStore => {
            let backend = write_result
                .credential_backend
                .map(CredentialBackend::as_str)
                .unwrap_or("credential-store");
            vec![
                format!("  Secrets →  {backend} backend"),
                "             Use `kei password backend` to check it or `kei password set` to change it."
                    .to_string(),
            ]
        }
        SetupSecretSource::PasswordFile(path) => {
            vec![format!("  Secrets →  password file: {path}")]
        }
        SetupSecretSource::PasswordCommand(_) => {
            vec!["  Secrets →  password command from config".to_string()]
        }
        SetupSecretSource::EnvFile => {
            if let Some(env_path) = &write_result.env_path {
                vec![format!("  Secrets →  {}", env_path.display())]
            } else {
                Vec::new()
            }
        }
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

fn ask_account_identity(answers: &mut SetupAnswers) -> anyhow::Result<()> {
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

fn ask_account_password(answers: &mut SetupAnswers) -> anyhow::Result<()> {
    // `with_confirmation` re-prompts on mismatch in-place, so a typo costs
    // one extra round, not a broken config that fails the next sync.
    answers.password = secrecy::SecretString::from(
        Password::new()
            .with_prompt("iCloud password")
            .with_confirmation(
                "Re-enter password to confirm",
                "Passwords didn't match, try again",
            )
            .interact()?,
    );

    Ok(())
}

// ── Step 2: Where to save ──────────────────────────────────────────

fn ask_destination(answers: &mut SetupAnswers) -> anyhow::Result<()> {
    println!();
    answers.directory = Input::new()
        .with_prompt("Where should photos be saved?")
        .default("~/Photos/iCloud".to_string())
        .interact_text()?;

    // Data directory for sessions, state DB, credentials, health. The default
    // is right for almost everyone, so it's offered here as a single line with
    // an obvious skip, not gated behind the "additional options?" extras prompt.
    let data_dir: String = Input::new()
        .with_prompt(
            "App data directory (advanced — leave default unless you know you need to change this)",
        )
        .default("~/.config/kei".to_string())
        .interact_text()?;
    if data_dir != "~/.config/kei" {
        answers.data_dir = Some(data_dir);
    }

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

    // The unfiled-pass template (`folder_structure`) and the per-album
    // template (`folder_structure_albums`) are independent in v0.13.
    // To match user intent ("organize photos by date") and v0.12 layout
    // behavior, when the user picks a date hierarchy we apply the same
    // template under each album folder by setting
    // `folder_structure_albums = "{album}/<template>"`. The default
    // `{album}` (flat per-album folder) stays for "All in one folder",
    // since collapsing the date hierarchy to nothing inside per-album
    // folders is what the user implicitly asked for.
    let unfiled_template: Option<String> = match folder {
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

    let date_template_for_albums: Option<&str> = match folder {
        0 => Some("%Y/%m/%d"),
        1 => Some("%Y/%m"),
        2 => Some("%Y"),
        // 3 ("all in one folder") and 4 (custom) intentionally leave the
        // album template at the v0.13 default `{album}`. For 4 the user
        // can edit the generated TOML if they want a custom album layout
        // too; offering yet another wizard prompt here clutters the flow.
        _ => None,
    };
    answers.folder_structure_albums = date_template_for_albums.map(|t| format!("{{album}}/{t}"));
    answers.folder_structure = unfiled_template;

    Ok(())
}

// ── Step 3: Secrets ────────────────────────────────────────────────

fn ask_secret_source(answers: &mut SetupAnswers) -> anyhow::Result<()> {
    println!();
    let use_store = Confirm::new()
        .with_prompt("Store the password in kei's credential store?")
        .default(true)
        .interact()?;
    if use_store {
        answers.secret_source = SetupSecretSource::CredentialStore;
        println!(
            "  Kei will try the OS keyring first, then encrypted file storage if the keyring is unavailable."
        );
        return Ok(());
    }

    let secret_items = [
        "Use an existing password file",
        "Use a password command",
        "Write a local .env file next to the config (last resort)",
    ];
    let choice = Select::new()
        .with_prompt("How should kei read the password?")
        .items(secret_items)
        .default(0)
        .interact()?;
    answers.secret_source = match choice {
        0 => {
            let path: String = Input::new()
                .with_prompt("Password file path")
                .validate_with(|input: &String| {
                    if input.trim().is_empty() {
                        Err("Password file path cannot be empty.")
                    } else {
                        Ok(())
                    }
                })
                .interact_text()?;
            SetupSecretSource::PasswordFile(path.trim().to_string())
        }
        1 => {
            let command: String = Input::new()
                .with_prompt("Password command")
                .validate_with(|input: &String| {
                    if input.trim().is_empty() {
                        Err("Password command cannot be empty.")
                    } else {
                        Ok(())
                    }
                })
                .interact_text()?;
            SetupSecretSource::PasswordCommand(command.trim().to_string())
        }
        _ => SetupSecretSource::EnvFile,
    };

    Ok(())
}

// ── Step 4: What to download ───────────────────────────────────────

fn ask_what_to_download(answers: &mut SetupAnswers) -> anyhow::Result<()> {
    println!();
    let scope_items = ["Entire library", "Specific albums"];
    let scope = Select::new()
        .with_prompt("Download your entire library or specific albums?")
        .items(scope_items)
        .default(0)
        .interact()?;

    if scope == 1 {
        // One album per prompt; blank input ends the loop. We can't comma-split
        // the input because `--album` / `[filters].albums` no longer split
        // (v0.13), so a single comma-separated entry would silently break any
        // album whose name contains a comma.
        println!("  Enter one album per line. Press Enter on a blank line to finish.");
        println!("  Names are case-sensitive and must match iCloud exactly. If you're unsure,");
        println!("  cancel now (Ctrl+C), run `kei list albums`, and re-run `kei config setup`.");
        loop {
            let prompt = if answers.albums.is_empty() {
                "Album name".to_string()
            } else {
                format!(
                    "Album name ({} so far, blank to finish)",
                    answers.albums.len()
                )
            };
            let name: String = Input::new()
                .with_prompt(prompt)
                .default(String::new())
                .show_default(false)
                .allow_empty(true)
                .interact_text()?;
            let trimmed = name.trim();
            if trimmed.is_empty() {
                break;
            }
            answers.albums.push(trimmed.to_string());
        }

        // Default sourced from runtime so the wizard stays truthful if
        // `unfiled_default()` ever changes. Emit explicitly so the generated
        // config records the user's intended album/unfiled scope.
        if !answers.albums.is_empty() {
            println!();
            let also_unfiled = Confirm::new()
                .with_prompt("Also download photos that aren't in any of these albums?")
                .default(crate::config::unfiled_default())
                .interact()?;
            answers.unfiled = Some(also_unfiled);
        }
    } else {
        // scope == 0 ("entire library") -- albums default to `all`. Set
        // unfiled explicitly so the generated config records the full
        // library scope.
        answers.unfiled = Some(true);
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

    answers.libraries = match library {
        0 => vec!["all".to_string()],
        _ => Vec::new(), // empty = use kei's default (`primary`)
    };

    Ok(())
}

// ── Step 4: Media types ────────────────────────────────────────────

fn ask_media_types(answers: &mut SetupAnswers) -> anyhow::Result<()> {
    println!();
    let include_videos = Confirm::new()
        .with_prompt("Include videos along with photos?")
        .default(true)
        .interact()?;
    answers.skip_videos = !include_videos;

    // Four-way choice (matches the runtime `LivePhotoMode` enum). The old
    // wizard only exposed Both vs Skip, hiding the image-only / video-only
    // modes that the CLI surface and TOML accept.
    let live_items = [
        "Both: image + video (default)",
        "Image only: skip the .mov video file",
        "Video only: skip the still image",
        "Skip live photos entirely",
    ];
    let live_choice = Select::new()
        .with_prompt("How should live photos be downloaded?")
        .items(live_items)
        .default(0)
        .interact()?;
    answers.live_photo_mode = match live_choice {
        1 => Some(LivePhotoMode::ImageOnly),
        2 => Some(LivePhotoMode::VideoOnly),
        3 => Some(LivePhotoMode::Skip),
        // `Both` is the default; leave as None so the emitted TOML keeps the
        // commented hint instead of an explicit assignment.
        _ => None,
    };

    // The .mov filename policy only matters when both image and video land
    // on disk together. Image-only / video-only / skip leave the .mov on its
    // own (or absent), so the suffix-vs-original choice is moot.
    let download_both_parts = matches!(live_choice, 0);
    if download_both_parts {
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

    answers.resolution = match size {
        1 => Some(PhotoResolution::Medium),
        2 => Some(PhotoResolution::Thumb),
        _ => None, // original is the default
    };

    // If not original, ask about fallback
    if answers.resolution.is_some() {
        let fallback = Confirm::new()
            .with_prompt("If that size isn't available, fall back to original?")
            .default(true)
            .interact()?;
        answers.force_resolution = !fallback;
    }

    println!();
    let shoots_raw = Confirm::new()
        .with_prompt("Do you shoot RAW?")
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
        answers.raw_policy = match raw_policy {
            1 => Some(RawPolicy::PreferRaw),
            2 => Some(RawPolicy::PreferJpeg),
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

    answers.skip_created_before = date_prompt("Only sync photos created after")?;
    answers.skip_created_after = date_prompt("Only sync photos created before")?;

    let recent: String = Input::new()
        .with_prompt(
            "Only consider the N most-recent assets per selected library/album/smart-folder pass (blank = all)",
        )
        .default(String::new())
        .show_default(false)
        .allow_empty(true)
        .validate_with(|s: &String| parse_positive_or_blank::<u32>(s).map(|_| ()))
        .interact_text()?;
    if let Ok(Some(n)) = parse_positive_or_blank::<u32>(&recent) {
        answers.recent = Some(n);
    }

    Ok(())
}

/// One of the two date-range prompts (`skip_created_before` /
/// `skip_created_after`). Returns the trimmed value or `None` for blank.
/// The `(ISO date ...)` suffix is uniform across both prompts.
fn date_prompt(label: &str) -> anyhow::Result<Option<String>> {
    let prompt = format!("{label} (ISO date, datetime, or Nd interval; blank = no limit)");
    let raw: String = Input::new()
        .with_prompt(prompt)
        .default(String::new())
        .show_default(false)
        .allow_empty(true)
        .validate_with(|s: &String| validate_date_or_blank(s))
        .interact_text()?;
    let trimmed = raw.trim();
    Ok(if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    })
}

/// Accept blank or anything `config::parse_created_date_filter` parses cleanly,
/// so a typo here surfaces with the same error the runtime would print.
fn validate_date_or_blank(s: &str) -> Result<(), String> {
    if s.trim().is_empty() {
        return Ok(());
    }
    crate::config::parse_created_date_filter(s.trim())
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// Parse a wizard input that should be either blank or a strictly-positive
/// integer. Returns the parsed value (or `None` for blank), so callers don't
/// re-parse what dialoguer's `validate_with` already validated. `"0"` is
/// rejected because every caller treats blank (not zero) as "off".
fn parse_positive_or_blank<T: std::str::FromStr>(s: &str) -> Result<Option<T>, String> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    if trimmed == "0" {
        return Err("Enter a number greater than zero, or leave blank to disable.".to_string());
    }
    trimmed
        .parse::<T>()
        .map(Some)
        .map_err(|_e| "Enter a positive integer, or leave blank.".to_string())
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
        // Range mirrors the runtime parser at `src/cli.rs::SyncArgs::watch_with_interval`.
        let interval: u64 = Input::new()
            .with_prompt("Re-sync every how many seconds (60..=86400)")
            .default(3600u64)
            .validate_with(|n: &u64| -> Result<(), String> {
                if (60..=86400).contains(n) {
                    Ok(())
                } else {
                    Err("Enter a value between 60 and 86400 seconds.".to_string())
                }
            })
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
                .allow_empty(true)
                .interact_text()?;
            if !pid.trim().is_empty() {
                answers.pid_file = Some(pid.trim().to_string());
            }
        }

        // Read-only walk: logs missing files, never re-downloads or marks
        // failed. Blank/0 = off; opt-in is intentional.
        let reconcile: String = Input::new()
            .with_prompt(
                "Reconcile (read-only walk for missing local files) every N watch cycles (blank = off)",
            )
            .default(String::new())
            .show_default(false)
            .allow_empty(true)
            .validate_with(|s: &String| parse_positive_or_blank::<u64>(s).map(|_| ()))
            .interact_text()?;
        if let Ok(Some(n)) = parse_positive_or_blank::<u64>(&reconcile) {
            answers.reconcile_every_n_cycles = Some(n);
        }
    }

    Ok(())
}

// ── Step 8: Extras ─────────────────────────────────────────────────

fn ask_extras(answers: &mut SetupAnswers) -> anyhow::Result<()> {
    println!();
    let configure = Confirm::new()
        .with_prompt("Configure advanced options?")
        .default(false)
        .interact()?;

    if !configure {
        return Ok(());
    }

    println!();

    // Smart folders (Favorites, Hidden, Recently Deleted, etc.). v0.13 added
    // these as a first-class selector; default is `none`. One name per prompt
    // line for the same reason as the album loop.
    let include_smart = Confirm::new()
        .with_prompt("Include smart folders (Favorites, Hidden, Recently Deleted, etc.)?")
        .default(false)
        .interact()?;
    if include_smart {
        let smart_scope_items = [
            "All visible smart folders (Favorites, etc., excludes Hidden + Recently Deleted)",
            "All including Hidden + Recently Deleted",
            "Specific smart folders by name",
        ];
        let smart_scope = Select::new()
            .with_prompt("Which smart folders?")
            .items(smart_scope_items)
            .default(0)
            .interact()?;
        match smart_scope {
            0 => answers.smart_folders = vec!["all".to_string()],
            1 => answers.smart_folders = vec!["all-with-sensitive".to_string()],
            _ => {
                // Source-of-truth for the available names; stays in sync if
                // any are added or renamed.
                let known: Vec<&'static str> =
                    crate::icloud::photos::smart_folders::smart_folder_names().collect();
                println!(
                    "  Enter one smart-folder name per line. Press Enter on a blank line to finish."
                );
                println!("  Names are case-sensitive. Available smart folders:");
                println!("  {}.", known.join(", "));
                loop {
                    let name: String = Input::new()
                        .with_prompt("Smart folder name (blank to finish)")
                        .default(String::new())
                        .show_default(false)
                        .allow_empty(true)
                        .interact_text()?;
                    let trimmed = name.trim();
                    if trimmed.is_empty() {
                        break;
                    }
                    answers.smart_folders.push(trimmed.to_string());
                }
            }
        }
    }

    // Notifications
    println!();
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

    // Performance. Ranges mirror runtime config validation so the wizard
    // rejects values the sync path would also reject.
    println!();
    let threads: u16 = Input::new()
        .with_prompt("Concurrent download threads (1..=64)")
        .default(10u16)
        .validate_with(|n: &u16| -> Result<(), String> {
            if (1..=64).contains(n) {
                Ok(())
            } else {
                Err("Enter a value between 1 and 64.".to_string())
            }
        })
        .interact_text()?;
    if threads != 10 {
        answers.threads_num = Some(threads);
    }

    let retries: u32 = Input::new()
        .with_prompt("Max retries per failed download (0..=100, 0 = disable)")
        .default(3u32)
        .validate_with(|n: &u32| -> Result<(), String> {
            if (0..=100).contains(n) {
                Ok(())
            } else {
                Err("Enter a value between 0 and 100.".to_string())
            }
        })
        .interact_text()?;
    if retries != 3 {
        answers.max_retries = Some(retries);
    }

    let bandwidth: String = Input::new()
        .with_prompt("Bandwidth limit (e.g. 10MB, 500K; blank = no limit)")
        .default(String::new())
        .show_default(false)
        .interact_text()?;
    if !bandwidth.trim().is_empty() {
        // Validate at the same place the runtime config does, so a typo here
        // surfaces immediately instead of on the next sync.
        match crate::cli::parse_bandwidth_limit(bandwidth.trim()) {
            Ok(_) => answers.bandwidth_limit = Some(bandwidth.trim().to_string()),
            Err(e) => println!("  Invalid bandwidth limit ({e}), skipping."),
        }
    }

    // Exclusions: filename glob patterns applied across every pass.
    println!();
    let exclude = Confirm::new()
        .with_prompt("Exclude files matching glob patterns (e.g. IMG_screenshot*.png)?")
        .default(false)
        .interact()?;
    if exclude {
        println!("  Enter one pattern per line. Press Enter on a blank line to finish.");
        loop {
            let pat: String = Input::new()
                .with_prompt("Pattern (blank to finish)")
                .default(String::new())
                .show_default(false)
                .allow_empty(true)
                .interact_text()?;
            let trimmed = pat.trim();
            if trimmed.is_empty() {
                break;
            }
            answers.filename_exclude.push(trimmed.to_string());
        }
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

    #[cfg(feature = "xmp")]
    {
        answers.embed_xmp = Confirm::new()
            .with_prompt("Embed XMP metadata (rating, GPS, description) into photos?")
            .default(false)
            .interact()?;

        answers.xmp_sidecar = Confirm::new()
            .with_prompt("Write a sidecar `.xmp` file alongside each photo?")
            .default(false)
            .interact()?;
    }

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

    // Log level
    println!();
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
    // `fmt::Write for String` cannot fail (the `core` impl is unreachable),
    // so wrap the `?`-propagating body in an IIFE and unwrap once at the
    // boundary instead of polluting the public signature with a dead error
    // channel.
    #[allow(
        clippy::expect_used,
        reason = "fmt::Write for String is infallible; the IIFE only carries `?` so writeln! calls compile cleanly"
    )]
    (|| -> Result<String, std::fmt::Error> {
        let mut out = String::with_capacity(2048);

        writeln!(out, "# kei configuration")?;
        writeln!(out, "# Generated by: kei setup")?;
        writeln!(out)?;

        // Top-level keys (data_dir, log_level)
        match &answers.data_dir {
            Some(dir) => writeln!(out, "data_dir = \"{}\"", escape_toml_string(dir))?,
            None => writeln!(out, "# data_dir = \"~/.config/kei\"")?,
        };
        match answers.log_level {
            Some(level) => writeln!(out, "log_level = \"{}\"", log_level_str(level))?,
            None => writeln!(out, "# log_level = \"warn\"")?,
        };

        // [auth]
        writeln!(out)?;
        writeln!(out, "[auth]")?;
        writeln!(
            out,
            "username = \"{}\"",
            escape_toml_string(&answers.username)
        )?;
        match &answers.secret_source {
            SetupSecretSource::CredentialStore => {
                writeln!(
                    out,
                    "# Password is stored in kei's credential store, not in this file."
                )?;
                writeln!(
                    out,
                    "# Use `kei password backend` to check it or `kei password set` to change it."
                )?;
            }
            SetupSecretSource::PasswordFile(path) => {
                writeln!(out, "password_file = \"{}\"", escape_toml_string(path))?;
            }
            SetupSecretSource::PasswordCommand(command) => {
                writeln!(
                    out,
                    "password_command = \"{}\"",
                    escape_toml_string(command)
                )?;
            }
            SetupSecretSource::EnvFile => {
                writeln!(
                    out,
                    "# Password is stored in .env file, not here (last-resort fallback)."
                )?;
            }
        }
        match answers.domain {
            Some(domain) => writeln!(out, "domain = \"{}\"", domain.as_str())?,
            None => writeln!(out, "# domain = \"com\"")?,
        };

        // [download]
        writeln!(out)?;
        writeln!(out, "[download]")?;
        writeln!(
            out,
            "directory = \"{}\"",
            escape_toml_string(&answers.directory)
        )?;
        // `folder_structure` is the unfiled-pass template (every photo not in
        // any user album / smart folder). The two per-category templates
        // below default to flat per-category folders; the wizard sets
        // `folder_structure_albums` to mirror the chosen date hierarchy when
        // appropriate (see ask_destination).
        match &answers.folder_structure {
            Some(fs) => writeln!(out, "folder_structure = \"{}\"", escape_toml_string(fs))?,
            None => writeln!(out, "# folder_structure = \"%Y/%m/%d\"")?,
        };
        match &answers.folder_structure_albums {
            Some(fs) => writeln!(
                out,
                "folder_structure_albums = \"{}\"",
                escape_toml_string(fs)
            )?,
            None => writeln!(out, "# folder_structure_albums = \"{{album}}\"")?,
        };
        match &answers.folder_structure_smart_folders {
            Some(fs) => writeln!(
                out,
                "folder_structure_smart_folders = \"{}\"",
                escape_toml_string(fs)
            )?,
            None => writeln!(
                out,
                "# folder_structure_smart_folders = \"{{smart-folder}}\""
            )?,
        };
        match answers.threads_num {
            Some(n) => writeln!(out, "threads = {n}")?,
            None => writeln!(out, "# threads = 10")?,
        };
        match &answers.bandwidth_limit {
            Some(b) => writeln!(out, "bandwidth_limit = \"{}\"", escape_toml_string(b))?,
            None => writeln!(
                out,
                "# bandwidth_limit = \"10MB\"  # blank/comment = no limit"
            )?,
        };
        writeln!(out, "# temp_suffix = \".kei-tmp\"")?;

        // [download.retry]
        writeln!(out)?;
        writeln!(out, "[download.retry]")?;
        match answers.max_retries {
            Some(n) => writeln!(out, "per_transfer = {n}")?,
            None => writeln!(out, "# per_transfer = 3")?,
        };

        // [metadata]
        writeln!(out)?;
        writeln!(out, "[metadata]")?;
        if answers.set_exif_datetime {
            writeln!(out, "set_exif_datetime = true")?;
        } else {
            writeln!(out, "# set_exif_datetime = false")?;
        }
        writeln!(out, "# set_exif_rating = false")?;
        writeln!(out, "# set_exif_gps = false")?;
        writeln!(out, "# set_exif_description = false")?;
        #[cfg(feature = "xmp")]
        {
            if answers.embed_xmp {
                writeln!(out, "embed_xmp = true")?;
            } else {
                writeln!(out, "# embed_xmp = false")?;
            }
            if answers.xmp_sidecar {
                writeln!(out, "xmp_sidecar = true")?;
            } else {
                writeln!(out, "# xmp_sidecar = false")?;
            }
        }

        // [filters]
        writeln!(out)?;
        writeln!(out, "[filters]")?;
        if answers.libraries.is_empty() {
            writeln!(out, "# libraries = [\"primary\"]")?;
        } else {
            let library_strs: Vec<String> = answers
                .libraries
                .iter()
                .map(|l| format!("\"{}\"", escape_toml_string(l)))
                .collect();
            writeln!(out, "libraries = [{}]", library_strs.join(", "))?;
        }
        if answers.albums.is_empty() {
            writeln!(out, "# albums = [\"all\"]")?;
        } else {
            let album_strs: Vec<String> = answers
                .albums
                .iter()
                .map(|a| format!("\"{}\"", escape_toml_string(a)))
                .collect();
            writeln!(out, "albums = [{}]", album_strs.join(", "))?;
        }
        if answers.smart_folders.is_empty() {
            writeln!(out, "# smart_folders = [\"none\"]")?;
        } else {
            let sf_strs: Vec<String> = answers
                .smart_folders
                .iter()
                .map(|s| format!("\"{}\"", escape_toml_string(s)))
                .collect();
            writeln!(out, "smart_folders = [{}]", sf_strs.join(", "))?;
        }
        match answers.unfiled {
            Some(false) => writeln!(out, "unfiled = false")?,
            Some(true) => writeln!(out, "unfiled = true")?,
            None => writeln!(out, "# unfiled = true")?,
        };
        if answers.filename_exclude.is_empty() {
            writeln!(out, "# filename_exclude = []")?;
        } else {
            let pat_strs: Vec<String> = answers
                .filename_exclude
                .iter()
                .map(|p| format!("\"{}\"", escape_toml_string(p)))
                .collect();
            writeln!(out, "filename_exclude = [{}]", pat_strs.join(", "))?;
        }
        if answers.skip_videos {
            writeln!(out, "media = [\"photos\", \"live-photos\"]")?;
        } else {
            writeln!(out, "# media = [\"photos\", \"videos\", \"live-photos\"]")?;
        }
        // `live_photo_mode` is emitted in the [photos] section below.
        match answers.recent {
            Some(n) => writeln!(out, "recent = {n}")?,
            None => writeln!(
                out,
                "# recent = 0  # 0 = all; cap applies per selected library/album/smart-folder pass"
            )?,
        };
        match &answers.skip_created_before {
            Some(d) => writeln!(out, "skip_created_before = \"{}\"", escape_toml_string(d))?,
            None => writeln!(out, "# skip_created_before = \"\"")?,
        };
        match &answers.skip_created_after {
            Some(d) => writeln!(out, "skip_created_after = \"{}\"", escape_toml_string(d))?,
            None => writeln!(out, "# skip_created_after = \"\"")?,
        };

        // [photos]
        writeln!(out)?;
        writeln!(out, "[photos]")?;
        match answers.resolution {
            Some(size) => writeln!(out, "resolution = \"{}\"", version_size_str(size))?,
            None => writeln!(out, "# resolution = \"original\"")?,
        };
        writeln!(out, "# edited = false")?;
        writeln!(out, "# alternative = false")?;
        writeln!(out, "# live_resolution = \"original\"")?;
        match answers.live_photo_mode {
            Some(mode) => writeln!(out, "live_photo_mode = \"{}\"", live_photo_mode_str(mode))?,
            None => writeln!(out, "# live_photo_mode = \"both\"")?,
        };
        match answers.live_photo_mov_filename_policy {
            Some(p) => writeln!(
                out,
                "live_photo_mov_filename_policy = \"{}\"",
                mov_policy_str(p)
            )?,
            None => writeln!(out, "# live_photo_mov_filename_policy = \"suffix\"")?,
        };
        match answers.raw_policy {
            Some(p) => writeln!(out, "raw_policy = \"{}\"", raw_policy_str(p))?,
            None => writeln!(out, "# raw_policy = \"as-is\"")?,
        };
        match answers.file_match_policy {
            Some(p) => writeln!(out, "file_match_policy = \"{}\"", file_match_str(p))?,
            None => writeln!(out, "# file_match_policy = \"name-size-dedup-with-suffix\"")?,
        };
        if answers.force_resolution {
            writeln!(out, "force_resolution = true")?;
        } else {
            writeln!(out, "# force_resolution = false")?;
        }
        if answers.keep_unicode_in_filenames {
            writeln!(out, "keep_unicode_in_filenames = true")?;
        } else {
            writeln!(out, "# keep_unicode_in_filenames = false")?;
        }

        // [watch]
        writeln!(out)?;
        writeln!(out, "[watch]")?;
        match answers.watch_interval {
            Some(n) => writeln!(out, "interval = {n}")?,
            None => writeln!(out, "# interval = 3600")?,
        };
        if answers.notify_systemd {
            writeln!(out, "notify_systemd = true")?;
        } else {
            writeln!(out, "# notify_systemd = false")?;
        }
        match &answers.pid_file {
            Some(p) => writeln!(out, "pid_file = \"{}\"", escape_toml_string(p))?,
            None => writeln!(out, "# pid_file = \"\"")?,
        };
        match answers.reconcile_every_n_cycles {
            Some(n) => writeln!(out, "reconcile_every_n_cycles = {n}")?,
            None => writeln!(out, "# reconcile_every_n_cycles = 0  # 0/unset = off")?,
        };

        // [notifications]
        writeln!(out)?;
        writeln!(out, "[notifications]")?;
        match &answers.notification_script {
            Some(s) => writeln!(out, "script = \"{}\"", escape_toml_string(s))?,
            None => writeln!(out, "# script = \"/path/to/script.sh\"")?,
        }

        // [server] - HTTP/Prometheus metrics endpoint. Replaces the removed
        // [metrics] section. Hint-only; no wizard prompt.
        writeln!(out)?;
        writeln!(out, "[server]")?;
        writeln!(out, "# port = 9090")?;
        writeln!(out, "# bind = \"127.0.0.1\"")?;

        // [report] - per-run JSON report destination. Hint-only.
        writeln!(out)?;
        writeln!(out, "[report]")?;
        writeln!(out, "# json = \"/path/to/last-run.json\"")?;

        // [ui] - friendly terminal progress and summaries. Default-on-for-TTY
        // at runtime, so we only emit an active line when the user explicitly
        // opted out; otherwise the section is hint-only.
        writeln!(out)?;
        writeln!(out, "[ui]")?;
        match answers.ui_friendly {
            Some(false) => writeln!(out, "friendly = false")?,
            Some(true) => writeln!(out, "friendly = true")?,
            None => writeln!(out, "# friendly = true  # default on TTY")?,
        }

        Ok(out)
    })()
    .expect("formatting into a String is infallible")
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

fn version_size_str(size: PhotoResolution) -> &'static str {
    match size {
        PhotoResolution::None => "none",
        PhotoResolution::Original => "original",
        PhotoResolution::Medium => "medium",
        PhotoResolution::Thumb => "thumb",
    }
}

fn mov_policy_str(policy: LivePhotoMovFilenamePolicy) -> &'static str {
    match policy {
        LivePhotoMovFilenamePolicy::Suffix => "suffix",
        LivePhotoMovFilenamePolicy::Original => "original",
    }
}

fn live_photo_mode_str(mode: LivePhotoMode) -> &'static str {
    // Must match the kebab-case rename in `LivePhotoMode`'s `Deserialize`
    // (`#[serde(rename_all = "kebab-case")]`).
    match mode {
        LivePhotoMode::Both => "both",
        LivePhotoMode::ImageOnly => "image-only",
        LivePhotoMode::VideoOnly => "video-only",
        LivePhotoMode::Skip => "skip",
    }
}

fn raw_policy_str(policy: RawPolicy) -> &'static str {
    match policy {
        RawPolicy::AsIs => "as-is",
        RawPolicy::PreferRaw => "prefer-raw",
        RawPolicy::PreferJpeg => "prefer-jpeg",
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
    use crate::types::LivePhotoMode;

    #[test]
    fn dim_comments_preserves_toml_text_while_styling_comments_and_sections() {
        let rendered = dim_comments("# comment\n[auth]\nusername = \"u\"");

        assert!(rendered.contains("# comment"));
        assert!(rendered.contains("[auth]"));
        assert!(rendered.contains("username = \"u\""));
        assert_eq!(rendered.lines().count(), 3);
    }

    #[test]
    fn setup_secret_source_password_prompt_policy_is_explicit() {
        assert!(SetupSecretSource::CredentialStore.needs_password_prompt());
        assert!(SetupSecretSource::EnvFile.needs_password_prompt());
        assert!(
            !SetupSecretSource::PasswordFile("/run/secrets/icloud".to_string())
                .needs_password_prompt()
        );
        assert!(
            !SetupSecretSource::PasswordCommand("op read item kei".to_string())
                .needs_password_prompt()
        );
    }

    #[test]
    fn apply_library_scoped_templates_sets_library_safe_defaults_for_all_libraries() {
        let mut answers = SetupAnswers {
            username: "user@example.com".to_string(),
            password: secrecy::SecretString::from("secret"),
            directory: "~/Photos/iCloud".to_string(),
            libraries: vec!["all".to_string()],
            folder_structure_albums: Some("{album}/%Y/%m/%d".to_string()),
            ..Default::default()
        };

        apply_library_scoped_templates_for_all_libraries(&mut answers);

        assert_eq!(
            answers.folder_structure.as_deref(),
            Some("{library}/%Y/%m/%d")
        );
        assert_eq!(
            answers.folder_structure_albums.as_deref(),
            Some("{album}/%Y/%m/%d")
        );
        assert_eq!(answers.folder_structure_smart_folders.as_deref(), None);
    }

    #[test]
    fn apply_library_scoped_templates_does_not_mutate_primary_only_setup() {
        let mut answers = SetupAnswers {
            username: "user@example.com".to_string(),
            password: secrecy::SecretString::from("secret"),
            directory: "~/Photos/iCloud".to_string(),
            libraries: Vec::new(),
            ..Default::default()
        };

        apply_library_scoped_templates_for_all_libraries(&mut answers);

        assert_eq!(answers.folder_structure, None);
        assert_eq!(answers.folder_structure_albums, None);
        assert_eq!(answers.folder_structure_smart_folders, None);
    }

    #[test]
    fn apply_library_scoped_templates_avoids_double_prefixing() {
        let mut answers = SetupAnswers {
            username: "user@example.com".to_string(),
            password: secrecy::SecretString::from("secret"),
            directory: "~/Photos/iCloud".to_string(),
            libraries: vec!["all".to_string()],
            folder_structure: Some("{library}/%Y/%m".to_string()),
            folder_structure_albums: Some("{album}".to_string()),
            folder_structure_smart_folders: Some("{smart-folder}".to_string()),
            ..Default::default()
        };

        apply_library_scoped_templates_for_all_libraries(&mut answers);

        assert_eq!(answers.folder_structure.as_deref(), Some("{library}/%Y/%m"));
        assert_eq!(answers.folder_structure_albums.as_deref(), Some("{album}"));
        assert_eq!(
            answers.folder_structure_smart_folders.as_deref(),
            Some("{smart-folder}")
        );
    }

    #[test]
    fn sync_command_for_config_shell_quotes_paths() {
        assert_eq!(
            sync_command_for_config(Path::new("/tmp/kei config.toml")),
            "kei --config '/tmp/kei config.toml' sync"
        );
        assert_eq!(
            sync_command_for_config(Path::new("/tmp/rob's-config.toml")),
            "kei --config '/tmp/rob'\\''s-config.toml' sync"
        );
    }

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
        // Libraries should be set to ["all"] (v0.13 array form, not the
        // removed singular `library` key).
        assert!(toml.contains("libraries = [\"all\"]"));
        assert!(!toml.contains("library = \"all\""));
        // Password should NOT be in the TOML
        assert!(!toml.contains("secret"));
        // Defaults should be commented out
        assert!(toml.contains("# resolution = \"original\""));
        assert!(toml.contains("# threads = 10"));
        assert!(toml.contains("# log_level = \"warn\""));
        assert!(toml.contains("# data_dir = \"~/.config/kei\""));
        assert!(toml.contains("Password is stored in kei's credential store"));
        assert!(toml.contains("kei password backend"));
        assert!(!toml.contains("password_file ="));
        assert!(!toml.contains("password_command ="));
        assert!(!toml.contains("ICLOUD_PASSWORD"));
        // Removed v0.20 keys must not appear in the generated config.
        assert!(!toml.contains("cookie_directory"));
        assert!(!toml.contains("skip_live_photos"));
    }

    #[test]
    fn test_generate_toml_password_file_secret_source() {
        let answers = SetupAnswers {
            username: "user@example.com".to_string(),
            password: secrecy::SecretString::from("hunter2"),
            secret_source: SetupSecretSource::PasswordFile("/run/secrets/icloud".to_string()),
            directory: "~/Photos/iCloud".to_string(),
            ..Default::default()
        };
        let toml_str = generate_toml(&answers);
        assert!(toml_str.contains("password_file = \"/run/secrets/icloud\""));
        assert!(!toml_str.contains("hunter2"));

        let parsed: TomlConfig = toml::from_str(&toml_str).unwrap();
        let auth = parsed.auth.unwrap();
        assert_eq!(auth.password_file.as_deref(), Some("/run/secrets/icloud"));
        assert!(auth.password.is_none());
    }

    #[test]
    fn test_generate_toml_password_command_secret_source() {
        let answers = SetupAnswers {
            username: "user@example.com".to_string(),
            password: secrecy::SecretString::from("hunter2"),
            secret_source: SetupSecretSource::PasswordCommand("op read item".to_string()),
            directory: "~/Photos/iCloud".to_string(),
            ..Default::default()
        };
        let toml_str = generate_toml(&answers);
        assert!(toml_str.contains("password_command = \"op read item\""));
        assert!(!toml_str.contains("hunter2"));

        let parsed: TomlConfig = toml::from_str(&toml_str).unwrap();
        let auth = parsed.auth.unwrap();
        assert_eq!(auth.password_command.as_deref(), Some("op read item"));
        assert!(auth.password.is_none());
    }

    #[test]
    fn test_password_prompt_only_runs_for_setup_owned_secrets() {
        assert!(SetupSecretSource::CredentialStore.needs_password_prompt());
        assert!(SetupSecretSource::EnvFile.needs_password_prompt());
        assert!(
            !SetupSecretSource::PasswordFile("/run/secrets/icloud".to_string())
                .needs_password_prompt()
        );
        assert!(
            !SetupSecretSource::PasswordCommand("op read item".to_string()).needs_password_prompt()
        );
    }

    // ── [ui] section emission ───────────────────────────────────────
    //
    // The wizard's friendly question is the only opt-out path baked into
    // the TOML. Default answers (skipped or "yes") leave `[ui].friendly`
    // commented so the runtime keeps freedom to flip the default; an
    // explicit "no" must persist as `friendly = false`.

    #[test]
    fn test_generate_toml_default_leaves_ui_friendly_commented() {
        let answers = SetupAnswers {
            username: "user@example.com".to_string(),
            password: secrecy::SecretString::from("secret"),
            directory: "~/Photos/iCloud".to_string(),
            ..Default::default()
        };
        let toml_str = generate_toml(&answers);
        assert!(
            toml_str.contains("[ui]"),
            "[ui] section header must always render so users see the option"
        );
        assert!(
            toml_str.contains("# friendly = true"),
            "default-yes answer must leave the friendly key commented; got:\n{toml_str}"
        );
        // Round-trip: parser sees no preference.
        let parsed: TomlConfig = toml::from_str(&toml_str).unwrap();
        assert_eq!(parsed.ui.and_then(|u| u.friendly), None);
    }

    #[test]
    fn test_generate_toml_opt_out_emits_friendly_false() {
        let answers = SetupAnswers {
            username: "user@example.com".to_string(),
            password: secrecy::SecretString::from("secret"),
            directory: "~/Photos/iCloud".to_string(),
            ui_friendly: Some(false),
            ..Default::default()
        };
        let toml_str = generate_toml(&answers);
        assert!(
            toml_str.contains("\nfriendly = false"),
            "explicit opt-out must emit an active `friendly = false` line; got:\n{toml_str}"
        );
        let parsed: TomlConfig = toml::from_str(&toml_str).unwrap();
        assert_eq!(
            parsed.ui.and_then(|u| u.friendly),
            Some(false),
            "round-trip parse must preserve the opt-out"
        );
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
        assert_eq!(filters.libraries.as_deref(), Some(&["all".to_string()][..]));
    }

    #[test]
    fn test_write_setup_files_stores_password_and_skips_env_file() {
        let dir = tempfile::tempdir().unwrap();
        let data_dir = dir.path().join("data");
        let config_path = dir.path().join("config.toml");
        let answers = SetupAnswers {
            username: "user@example.com".to_string(),
            password: secrecy::SecretString::from("secret"),
            data_dir: Some(data_dir.display().to_string()),
            directory: "~/Photos/iCloud".to_string(),
            ..Default::default()
        };
        let toml_str = generate_toml(&answers);

        let result = write_setup_files_with_store(
            &config_path,
            &toml_str,
            &answers,
            |username, credential_dir, password| {
                assert_eq!(username, "user@example.com");
                assert_eq!(credential_dir, data_dir.as_path());
                assert_eq!(password, "secret");
                Ok(CredentialBackend::Keyring)
            },
        )
        .unwrap();

        assert_eq!(result.credential_backend, Some(CredentialBackend::Keyring));
        assert!(result.env_path.is_none());
        assert!(!dir.path().join(".env").exists());
        let summary = secret_summary_lines(&answers, &result);
        assert_eq!(summary[0], "  Secrets →  keyring backend");
        assert!(
            summary[1].contains("kei password backend"),
            "summary should point at credential inspection: {summary:?}"
        );
        assert!(
            summary[1].contains("kei password set"),
            "summary should point at credential replacement: {summary:?}"
        );
        let written = std::fs::read_to_string(&config_path).unwrap();
        assert!(!written.contains("secret"));
        let parsed: TomlConfig = toml::from_str(&written).unwrap();
        assert!(parsed.auth.unwrap().password.is_none());
    }

    #[test]
    fn test_write_setup_files_reports_encrypted_file_backend() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        let answers = SetupAnswers {
            username: "user@example.com".to_string(),
            password: secrecy::SecretString::from("secret"),
            directory: "~/Photos/iCloud".to_string(),
            ..Default::default()
        };
        let toml_str = generate_toml(&answers);

        let result = write_setup_files_with_store(
            &config_path,
            &toml_str,
            &answers,
            |_username, _credential_dir, password| {
                assert_eq!(password, "secret");
                Ok(CredentialBackend::EncryptedFile)
            },
        )
        .unwrap();

        assert_eq!(
            result.credential_backend,
            Some(CredentialBackend::EncryptedFile)
        );
        assert!(result.env_path.is_none());

        let summary = secret_summary_lines(&answers, &result);
        assert_eq!(summary[0], "  Secrets →  encrypted-file backend");
        assert!(
            summary[1].contains("kei password backend"),
            "summary should point at credential inspection: {summary:?}"
        );
        assert!(
            summary[1].contains("kei password set"),
            "summary should point at credential replacement: {summary:?}"
        );
    }

    #[test]
    fn test_write_setup_files_does_not_write_config_when_credential_store_fails() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        let answers = SetupAnswers {
            username: "user@example.com".to_string(),
            password: secrecy::SecretString::from("secret"),
            directory: "~/Photos/iCloud".to_string(),
            ..Default::default()
        };
        let toml_str = generate_toml(&answers);

        let err = write_setup_files_with_store(
            &config_path,
            &toml_str,
            &answers,
            |_username, _credential_dir, _password| anyhow::bail!("store failed"),
        )
        .unwrap_err();

        assert!(
            err.to_string().contains("store failed"),
            "expected store failure, got {err}"
        );
        assert!(
            !config_path.exists(),
            "config must not be committed unless the selected credential store accepts the password"
        );
    }

    #[test]
    fn test_write_setup_files_env_fallback_writes_env_not_toml_password() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        let answers = SetupAnswers {
            username: "user@example.com".to_string(),
            password: secrecy::SecretString::from("secret"),
            secret_source: SetupSecretSource::EnvFile,
            directory: "~/Photos/iCloud".to_string(),
            ..Default::default()
        };
        let toml_str = generate_toml(&answers);

        let result = write_setup_files_with_store(
            &config_path,
            &toml_str,
            &answers,
            |_username, _credential_dir, _password| {
                panic!("env fallback must not touch credential storage");
            },
        )
        .unwrap();

        let env_path = result.env_path.expect("env fallback should write .env");
        assert_eq!(env_path, dir.path().join(".env"));
        let written = std::fs::read_to_string(&config_path).unwrap();
        assert!(!written.contains("secret"));
        let env_content = std::fs::read_to_string(&env_path).unwrap();
        assert!(env_content.contains("ICLOUD_USERNAME='user@example.com'"));
        assert!(env_content.contains("ICLOUD_PASSWORD='secret'"));
    }

    #[test]
    fn test_generate_toml_full() {
        let answers = SetupAnswers {
            username: "user@example.com".to_string(),
            password: secrecy::SecretString::from("secret"),
            domain: Some(Domain::Cn),
            secret_source: SetupSecretSource::CredentialStore,
            directory: "~/photos".to_string(),
            folder_structure: Some("%Y/%m".to_string()),
            folder_structure_albums: Some("{album}/%Y/%m".to_string()),
            albums: vec!["Favorites".to_string(), "Vacation".to_string()],
            libraries: vec!["all".to_string()],
            smart_folders: vec!["all".to_string()],
            folder_structure_smart_folders: None,
            unfiled: Some(false),
            filename_exclude: vec!["IMG_screenshot*.png".to_string()],
            skip_videos: true,
            live_photo_mode: Some(LivePhotoMode::Both),
            live_photo_mov_filename_policy: Some(LivePhotoMovFilenamePolicy::Original),
            resolution: Some(PhotoResolution::Medium),
            force_resolution: true,
            raw_policy: Some(RawPolicy::PreferRaw),
            recent: Some(100),
            skip_created_before: Some("2024-01-01".to_string()),
            skip_created_after: Some("2025-01-01".to_string()),
            watch_interval: Some(1800),
            notify_systemd: true,
            pid_file: Some("/var/run/kei.pid".to_string()),
            reconcile_every_n_cycles: Some(24),
            notification_script: Some("/usr/local/bin/notify.sh".to_string()),
            threads_num: Some(4),
            max_retries: Some(5),
            bandwidth_limit: Some("10MB".to_string()),
            keep_unicode_in_filenames: true,
            set_exif_datetime: true,
            #[cfg(feature = "xmp")]
            embed_xmp: true,
            #[cfg(feature = "xmp")]
            xmp_sidecar: false,
            file_match_policy: Some(FileMatchPolicy::NameId7),
            data_dir: Some("~/.kei".to_string()),
            log_level: Some(LogLevel::Debug),
            ui_friendly: Some(false),
        };
        let toml_str = generate_toml(&answers);

        // All user-set values should be uncommented
        assert!(toml_str.contains("domain = \"cn\""));
        assert!(toml_str.contains("folder_structure = \"%Y/%m\""));
        assert!(toml_str.contains("folder_structure_albums = \"{album}/%Y/%m\""));
        assert!(toml_str.contains("albums = [\"Favorites\", \"Vacation\"]"));
        assert!(toml_str.contains("libraries = [\"all\"]"));
        assert!(toml_str.contains("smart_folders = [\"all\"]"));
        assert!(toml_str.contains("unfiled = false"));
        assert!(toml_str.contains("filename_exclude = [\"IMG_screenshot*.png\"]"));
        assert!(toml_str.contains("media = [\"photos\", \"live-photos\"]"));
        assert!(toml_str.contains("resolution = \"medium\""));
        // live_photo_mode = "both" is the default; emitting it explicitly is
        // also fine, but the test above sets `Some(Both)` so we expect it.
        assert!(toml_str.contains("live_photo_mode = \"both\""));
        assert!(toml_str.contains("force_resolution = true"));
        assert!(toml_str.contains("raw_policy = \"prefer-raw\""));
        assert!(toml_str.contains("recent = 100"));
        assert!(toml_str.contains("interval = 1800"));
        assert!(toml_str.contains("notify_systemd = true"));
        assert!(toml_str.contains("reconcile_every_n_cycles = 24"));
        assert!(toml_str.contains("threads = 4"));
        assert!(toml_str.contains("bandwidth_limit = \"10MB\""));
        assert!(toml_str.contains("file_match_policy = \"name-id7\""));
        assert!(toml_str.contains("log_level = \"debug\""));
        assert!(toml_str.contains("set_exif_datetime = true"));
        #[cfg(feature = "xmp")]
        {
            assert!(toml_str.contains("embed_xmp = true"));
            // `xmp_sidecar = false` here means it's commented out, not active.
            assert!(toml_str.contains("# xmp_sidecar = false"));
        }
        assert!(toml_str.contains("keep_unicode_in_filenames = true"));
        assert!(toml_str.contains("data_dir = \"~/.kei\""));
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
            secret_source: SetupSecretSource::CredentialStore,
            directory: "/data/photos".to_string(),
            folder_structure: Some("%Y-%m".to_string()),
            folder_structure_albums: Some("{album}/%Y-%m".to_string()),
            albums: vec!["A".to_string()],
            libraries: Vec::new(),
            smart_folders: vec!["Favorites".to_string()],
            folder_structure_smart_folders: None,
            unfiled: Some(false),
            filename_exclude: vec!["*.tmp".to_string()],
            skip_videos: true,
            live_photo_mode: Some(LivePhotoMode::Skip),
            live_photo_mov_filename_policy: Some(LivePhotoMovFilenamePolicy::Original),
            resolution: Some(PhotoResolution::Thumb),
            force_resolution: true,
            raw_policy: Some(RawPolicy::PreferJpeg),
            recent: Some(50),
            skip_created_before: Some("30d".to_string()),
            skip_created_after: Some("2025-06-01".to_string()),
            watch_interval: Some(600),
            notify_systemd: true,
            pid_file: Some("/tmp/pid".to_string()),
            reconcile_every_n_cycles: Some(48),
            notification_script: Some("/bin/notify".to_string()),
            threads_num: Some(2),
            max_retries: Some(0),
            bandwidth_limit: Some("1Mi".to_string()),
            keep_unicode_in_filenames: true,
            set_exif_datetime: true,
            #[cfg(feature = "xmp")]
            embed_xmp: true,
            #[cfg(feature = "xmp")]
            xmp_sidecar: true,
            file_match_policy: Some(FileMatchPolicy::NameId7),
            data_dir: Some("/var/lib/kei".to_string()),
            log_level: Some(LogLevel::Error),
            ui_friendly: Some(false),
        };
        let toml_str = generate_toml(&answers);
        let parsed: TomlConfig = toml::from_str(&toml_str)
            .unwrap_or_else(|e| panic!("Failed to parse: {e}\n\n{toml_str}"));

        assert_eq!(parsed.data_dir.as_deref(), Some("/var/lib/kei"));

        let auth = parsed.auth.unwrap();
        assert_eq!(auth.username.as_deref(), Some("test@icloud.com"));
        assert_eq!(auth.domain, Some(Domain::Cn));
        let dl = parsed.download.unwrap();
        assert_eq!(dl.directory.as_deref(), Some("/data/photos"));
        assert_eq!(dl.folder_structure.as_deref(), Some("%Y-%m"));
        assert_eq!(dl.folder_structure_albums.as_deref(), Some("{album}/%Y-%m"));
        assert_eq!(dl.threads, Some(2));
        assert_eq!(dl.bandwidth_limit.as_deref(), Some("1Mi"));
        let metadata = parsed.metadata.unwrap();
        assert_eq!(metadata.set_exif_datetime, Some(true));
        #[cfg(feature = "xmp")]
        {
            assert_eq!(metadata.embed_xmp, Some(true));
            assert_eq!(metadata.xmp_sidecar, Some(true));
        }
        let retry = dl.retry.unwrap();
        assert_eq!(retry.per_transfer, Some(0));

        let filters = parsed.filters.unwrap();
        assert_eq!(filters.albums.as_deref(), Some(&["A".to_string()][..]));
        assert_eq!(
            filters.smart_folders.as_deref(),
            Some(&["Favorites".to_string()][..])
        );
        assert_eq!(filters.unfiled, Some(false));
        assert_eq!(
            filters.filename_exclude.as_deref(),
            Some(&["*.tmp".to_string()][..])
        );
        assert_eq!(
            filters.media.as_deref(),
            Some(
                &[
                    crate::config::MediaKind::Photos,
                    crate::config::MediaKind::LivePhotos,
                ][..]
            )
        );
        assert!(
            filters.libraries.is_none(),
            "empty libraries vec must produce a comment, not an array"
        );
        assert_eq!(filters.recent, Some(crate::cli::RecentLimit::Count(50)));
        assert_eq!(filters.skip_created_before.as_deref(), Some("30d"));
        assert_eq!(filters.skip_created_after.as_deref(), Some("2025-06-01"));

        let photos = parsed.photos.unwrap();
        assert_eq!(photos.resolution, Some(PhotoResolution::Thumb));
        assert_eq!(photos.force_resolution, Some(true));
        assert_eq!(photos.raw_policy, Some(RawPolicy::PreferJpeg));
        assert_eq!(
            photos.live_photo_mov_filename_policy,
            Some(LivePhotoMovFilenamePolicy::Original)
        );
        assert_eq!(photos.live_photo_mode, Some(LivePhotoMode::Skip));
        assert_eq!(photos.file_match_policy, Some(FileMatchPolicy::NameId7));
        assert_eq!(photos.keep_unicode_in_filenames, Some(true));

        let watch = parsed.watch.unwrap();
        assert_eq!(watch.interval, Some(600));
        assert_eq!(watch.notify_systemd, Some(true));
        assert_eq!(watch.pid_file.as_deref(), Some("/tmp/pid"));
        assert_eq!(watch.reconcile_every_n_cycles, Some(48));

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

    /// Single source of truth: the wizard must never emit any removed TOML key.
    /// Add future removed keys to `REMOVED_KEYS` so wizard authors get a CI
    /// failure pointing them at the right field.
    #[test]
    fn test_wizard_never_emits_removed_keys() {
        // Cover both "default" answers (most users) and "every option set"
        // answers (everything the wizard can possibly emit).
        let cases: Vec<SetupAnswers> = vec![
            SetupAnswers {
                username: "u@e.com".to_string(),
                password: secrecy::SecretString::from("p"),
                directory: "~/Photos".to_string(),
                ..Default::default()
            },
            SetupAnswers {
                username: "u@e.com".to_string(),
                password: secrecy::SecretString::from("p"),
                directory: "/photos".to_string(),
                albums: vec!["A".to_string()],
                libraries: Vec::new(),
                smart_folders: vec!["all".to_string()],
                folder_structure_smart_folders: None,
                unfiled: Some(false),
                filename_exclude: vec!["*.tmp".to_string()],
                skip_videos: true,
                live_photo_mode: Some(LivePhotoMode::Skip),
                live_photo_mov_filename_policy: Some(LivePhotoMovFilenamePolicy::Original),
                resolution: Some(PhotoResolution::Thumb),
                force_resolution: true,
                raw_policy: Some(RawPolicy::PreferJpeg),
                bandwidth_limit: Some("10MB".to_string()),
                reconcile_every_n_cycles: Some(24),
                set_exif_datetime: true,
                #[cfg(feature = "xmp")]
                embed_xmp: true,
                #[cfg(feature = "xmp")]
                xmp_sidecar: true,
                file_match_policy: Some(FileMatchPolicy::NameId7),
                data_dir: Some("/var/lib/kei".to_string()),
                log_level: Some(LogLevel::Error),
                ..Default::default()
            },
        ];

        // Removed keys must not appear in wizard output. Match
        // `key = ` (with the equals sign) so we don't false-positive on
        // comment hints or substring matches inside another key.
        const REMOVED_KEYS: &[(&str, &str)] = &[
            (
                "cookie_directory =",
                "[auth].cookie_directory -> top-level data_dir",
            ),
            (
                "library =",
                "[filters].library (singular) -> [filters].libraries (array)",
            ),
            (
                "album =",
                "[filters].album (singular) -> [filters].albums (array)",
            ),
            (
                "exclude_albums =",
                "[filters].exclude_albums -> [filters].albums with !name entries",
            ),
            (
                "skip_live_photos =",
                "[filters].skip_live_photos -> [photos].live_photo_mode",
            ),
            ("skip_videos =", "[filters].skip_videos -> [filters].media"),
            ("skip_photos =", "[filters].skip_photos -> [filters].media"),
            (
                "threads_num =",
                "[download].threads_num -> [download].threads",
            ),
        ];

        for answers in &cases {
            let toml_str = generate_toml(answers);
            // Strip comment lines so the substring search only inspects
            // active assignments, not the `# foo = ...` hint comments.
            let active: String = toml_str
                .lines()
                .filter(|l| !l.trim_start().starts_with('#'))
                .collect::<Vec<_>>()
                .join("\n");
            for (needle, msg) in REMOVED_KEYS {
                assert!(
                    !active.contains(needle),
                    "wizard emitted removed key `{needle}` ({msg}); full output:\n{toml_str}"
                );
            }
        }
    }

    /// "Specific albums" + the user wants only those albums (declined the
    /// "also download photos not in any of these albums?" prompt) -> wizard
    /// must emit `unfiled = false`. The pre-fix wizard implicitly relied on
    /// v0.13's `unfiled = true` default and pulled every other photo into
    /// the unfiled pass.
    #[test]
    fn test_specific_albums_with_unfiled_disabled_emits_unfiled_false() {
        let answers = SetupAnswers {
            username: "u@e.com".to_string(),
            password: secrecy::SecretString::from("p"),
            directory: "/d".to_string(),
            albums: vec!["Vacation".to_string()],
            unfiled: Some(false),
            ..Default::default()
        };
        let toml_str = generate_toml(&answers);
        assert!(toml_str.contains("unfiled = false"));

        let parsed: TomlConfig =
            toml::from_str(&toml_str).expect("generated TOML must parse cleanly");
        assert_eq!(parsed.filters.unwrap().unfiled, Some(false));
    }

    /// "Specific albums" + the user wants those AND every other photo
    /// (accepted the unfiled prompt) -> wizard emits `unfiled = true`
    /// explicitly so the generated config records that selection.
    #[test]
    fn test_specific_albums_with_unfiled_enabled_emits_unfiled_true() {
        let answers = SetupAnswers {
            username: "u@e.com".to_string(),
            password: secrecy::SecretString::from("p"),
            directory: "/d".to_string(),
            albums: vec!["Vacation".to_string()],
            unfiled: Some(true),
            ..Default::default()
        };
        let toml_str = generate_toml(&answers);
        assert!(toml_str.contains("unfiled = true"));

        let parsed: TomlConfig =
            toml::from_str(&toml_str).expect("generated TOML must parse cleanly");
        assert_eq!(parsed.filters.unwrap().unfiled, Some(true));
    }

    /// When the user picks a date hierarchy, the wizard must emit a
    /// `folder_structure_albums = "{album}/<template>"` so album passes share
    /// the same date layout as the unfiled pass. The v0.13 default for the
    /// per-album template is the flat `{album}` (no date), which silently
    /// changes the on-disk layout for v0.12 users who only set
    /// `--folder-structure %Y/%m/%d`.
    #[test]
    fn test_date_template_lifts_into_folder_structure_albums() {
        let answers = SetupAnswers {
            username: "u@e.com".to_string(),
            password: secrecy::SecretString::from("p"),
            directory: "/d".to_string(),
            // Match what ask_destination sets for the "By month" choice.
            folder_structure: Some("%Y/%m".to_string()),
            folder_structure_albums: Some("{album}/%Y/%m".to_string()),
            ..Default::default()
        };
        let toml_str = generate_toml(&answers);
        assert!(toml_str.contains("folder_structure = \"%Y/%m\""));
        assert!(toml_str.contains("folder_structure_albums = \"{album}/%Y/%m\""));

        let parsed: TomlConfig = toml::from_str(&toml_str).expect("must parse");
        let dl = parsed.download.unwrap();
        assert_eq!(dl.folder_structure.as_deref(), Some("%Y/%m"));
        assert_eq!(dl.folder_structure_albums.as_deref(), Some("{album}/%Y/%m"));
    }

    /// When the user picks "All in one folder", the wizard emits
    /// `folder_structure = ""` (empty unfiled template) and intentionally
    /// leaves `folder_structure_albums` unset so albums keep their flat
    /// per-album folder default. Collapsing per-album folders into a single
    /// flat directory is rarely what the user wanted from "all in one".
    #[test]
    fn test_all_in_one_folder_does_not_set_folder_structure_albums() {
        let answers = SetupAnswers {
            username: "u@e.com".to_string(),
            password: secrecy::SecretString::from("p"),
            directory: "/d".to_string(),
            folder_structure: Some(String::new()),
            folder_structure_albums: None,
            ..Default::default()
        };
        let toml_str = generate_toml(&answers);
        // Empty template emits as `folder_structure = ""`, not `none`.
        assert!(toml_str.contains("folder_structure = \"\""));
        let active: String = toml_str
            .lines()
            .filter(|l| !l.trim_start().starts_with('#'))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!active.contains("folder_structure_albums ="));
        assert!(toml_str.contains("# folder_structure_albums = \"{album}\""));
    }

    /// The wizard must be able to emit every `LivePhotoMode` value the
    /// runtime accepts. The pre-fix wizard only exposed Both vs Skip, hiding
    /// `image-only` and `video-only` from interactive setup.
    #[test]
    fn test_live_photo_mode_emits_every_runtime_variant() {
        for (mode, expected) in [
            (LivePhotoMode::Both, "live_photo_mode = \"both\""),
            (LivePhotoMode::ImageOnly, "live_photo_mode = \"image-only\""),
            (LivePhotoMode::VideoOnly, "live_photo_mode = \"video-only\""),
            (LivePhotoMode::Skip, "live_photo_mode = \"skip\""),
        ] {
            let answers = SetupAnswers {
                username: "u@e.com".to_string(),
                password: secrecy::SecretString::from("p"),
                directory: "/d".to_string(),
                live_photo_mode: Some(mode),
                ..Default::default()
            };
            let toml_str = generate_toml(&answers);
            assert!(
                toml_str.contains(expected),
                "expected `{expected}` for mode {mode:?}; got:\n{toml_str}"
            );
            let parsed: TomlConfig = toml::from_str(&toml_str).expect("must parse");
            assert_eq!(parsed.photos.unwrap().live_photo_mode, Some(mode));
        }
    }

    /// Smart-folder selector emission, including the v0.13 grammar
    /// (`all`, `all-with-sensitive`, named entries, `!exclusion`).
    #[test]
    fn test_smart_folders_emission() {
        let answers = SetupAnswers {
            username: "u@e.com".to_string(),
            password: secrecy::SecretString::from("p"),
            directory: "/d".to_string(),
            smart_folders: vec![
                "all".to_string(),
                "!Hidden".to_string(),
                "Recently Saved".to_string(),
            ],
            ..Default::default()
        };
        let toml_str = generate_toml(&answers);
        assert!(toml_str.contains("smart_folders = [\"all\", \"!Hidden\", \"Recently Saved\"]"));
        let parsed: TomlConfig = toml::from_str(&toml_str).expect("must parse");
        let sf = parsed.filters.unwrap().smart_folders.unwrap();
        assert_eq!(sf, vec!["all", "!Hidden", "Recently Saved"]);
    }

    /// `[metrics]` was removed in v0.20. The wizard must emit `[server]`
    /// instead and never name `[metrics]` even as a hint.
    #[test]
    fn test_wizard_emits_server_section_not_metrics() {
        let answers = SetupAnswers {
            username: "u@e.com".to_string(),
            password: secrecy::SecretString::from("p"),
            directory: "/d".to_string(),
            ..Default::default()
        };
        let toml_str = generate_toml(&answers);
        assert!(toml_str.contains("[server]"));
        assert!(toml_str.contains("# port = 9090"));
        assert!(
            !toml_str.contains("[metrics]"),
            "wizard must not name the removed [metrics] section; got:\n{toml_str}"
        );
    }

    #[test]
    fn test_generate_toml_enum_values() {
        // Verify each enum serializes to the correct TOML string that
        // the config parser expects.
        assert_eq!(version_size_str(PhotoResolution::Original), "original");
        assert_eq!(version_size_str(PhotoResolution::Medium), "medium");
        assert_eq!(version_size_str(PhotoResolution::Thumb), "thumb");

        assert_eq!(raw_policy_str(RawPolicy::AsIs), "as-is");
        assert_eq!(raw_policy_str(RawPolicy::PreferRaw), "prefer-raw");
        assert_eq!(raw_policy_str(RawPolicy::PreferJpeg), "prefer-jpeg");

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

        assert_eq!(live_photo_mode_str(LivePhotoMode::Both), "both");
        assert_eq!(live_photo_mode_str(LivePhotoMode::ImageOnly), "image-only");
        assert_eq!(live_photo_mode_str(LivePhotoMode::VideoOnly), "video-only");
        assert_eq!(live_photo_mode_str(LivePhotoMode::Skip), "skip");

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

        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        let answers = SetupAnswers {
            username: "test@example.com".to_string(),
            password: secrecy::SecretString::from("secret"),
            secret_source: SetupSecretSource::EnvFile,
            directory: "~/Photos/iCloud".to_string(),
            ..Default::default()
        };
        let toml_str = generate_toml(&answers);
        let result = write_setup_files_with_store(
            &config_path,
            &toml_str,
            &answers,
            |_username, _credential_dir, _password| {
                panic!("env fallback must not touch credential storage");
            },
        )
        .unwrap();
        let env_path = result.env_path.unwrap();

        // Verify permissions
        let metadata = std::fs::metadata(&env_path).unwrap();
        let mode = metadata.permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o600,
            "expected mode 0o600 (owner rw only), got {mode:#o}"
        );

        // Verify content
        let content = std::fs::read_to_string(&env_path).unwrap();
        assert!(content.contains("ICLOUD_USERNAME='test@example.com'"));
        assert!(content.contains("ICLOUD_PASSWORD='secret'"));
    }

    #[test]
    fn secret_summary_lines_covers_every_secret_source() {
        let env_dir = tempfile::tempdir().unwrap();
        let env_path = env_dir.path().join(".env");

        for (secret_source, write_result, expected) in [
            (
                SetupSecretSource::CredentialStore,
                SetupWriteResult {
                    credential_backend: None,
                    env_path: None,
                },
                vec![
                    "  Secrets →  credential-store backend".to_string(),
                    "             Use `kei password backend` to check it or `kei password set` to change it."
                        .to_string(),
                ],
            ),
            (
                SetupSecretSource::PasswordFile("/run/secrets/icloud".to_string()),
                SetupWriteResult {
                    credential_backend: None,
                    env_path: None,
                },
                vec!["  Secrets →  password file: /run/secrets/icloud".to_string()],
            ),
            (
                SetupSecretSource::PasswordCommand("op read item kei".to_string()),
                SetupWriteResult {
                    credential_backend: None,
                    env_path: None,
                },
                vec!["  Secrets →  password command from config".to_string()],
            ),
            (
                SetupSecretSource::EnvFile,
                SetupWriteResult {
                    credential_backend: None,
                    env_path: Some(env_path.clone()),
                },
                vec![format!("  Secrets →  {}", env_path.display())],
            ),
        ] {
            let answers = SetupAnswers {
                username: "user@example.com".to_string(),
                password: secrecy::SecretString::from("secret"),
                secret_source,
                directory: "/photos".to_string(),
                ..Default::default()
            };

            assert_eq!(
                secret_summary_lines(&answers, &write_result),
                expected,
                "unexpected secret summary for {:?}",
                answers.secret_source
            );
        }
    }

    #[test]
    fn shell_single_quote_escape_handles_embedded_quotes() {
        assert_eq!(shell_single_quote_escape("plain"), "plain");
        assert_eq!(shell_single_quote_escape("don't"), "don'\\''t");
        assert_eq!(shell_single_quote_escape("a'b'c"), "a'\\''b'\\''c");
    }

    #[test]
    fn credential_store_dir_uses_data_dir_or_cookie_default() {
        let custom = SetupAnswers {
            data_dir: Some("/var/lib/kei".to_string()),
            ..Default::default()
        };
        assert_eq!(credential_store_dir(&custom), PathBuf::from("/var/lib/kei"));

        let default = SetupAnswers::default();
        assert_eq!(
            credential_store_dir(&default),
            crate::config::default_cookie_dir()
        );
    }

    #[test]
    fn write_setup_files_with_external_secret_source_writes_only_config() {
        for secret_source in [
            SetupSecretSource::PasswordFile("/run/secrets/icloud".to_string()),
            SetupSecretSource::PasswordCommand("op read item kei".to_string()),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let config_path = dir.path().join("config.toml");
            let answers = SetupAnswers {
                username: "user@example.com".to_string(),
                password: secrecy::SecretString::from("secret"),
                secret_source,
                ..Default::default()
            };

            let result = write_setup_files_with_store(
                &config_path,
                "[auth]\nusername = \"user@example.com\"\n",
                &answers,
                |_username, _dir, _pw| {
                    panic!("external secret sources must not write to credential store")
                },
            )
            .expect("setup files");

            assert_eq!(result.credential_backend, None);
            assert_eq!(result.env_path, None);
            assert!(config_path.exists());
            assert!(!dir.path().join(".env").exists());
        }
    }

    // ── Numeric / date wizard-input validators ──────────────────────

    #[test]
    fn validate_date_or_blank_accepts_empty() {
        assert!(validate_date_or_blank("").is_ok());
        assert!(validate_date_or_blank("   ").is_ok());
    }

    #[test]
    fn validate_date_or_blank_accepts_iso_date() {
        assert!(validate_date_or_blank("2025-01-02").is_ok());
        assert!(validate_date_or_blank("2025-01-02T14:30:00").is_ok());
    }

    #[test]
    fn validate_date_or_blank_accepts_relative_interval() {
        assert!(validate_date_or_blank("30d").is_ok());
        assert!(validate_date_or_blank("1d").is_ok());
    }

    #[test]
    fn validate_date_or_blank_rejects_garbage() {
        for bad in ["2024-13-99", "tomorrow", "30dx", "abc", "999"] {
            assert!(
                validate_date_or_blank(bad).is_err(),
                "bad input {bad:?} should have been rejected"
            );
        }
    }

    #[test]
    fn parse_positive_or_blank_blank_is_none() {
        assert_eq!(parse_positive_or_blank::<u32>("").unwrap(), None);
        assert_eq!(parse_positive_or_blank::<u64>("   ").unwrap(), None);
    }

    #[test]
    fn parse_positive_or_blank_accepts_positive() {
        assert_eq!(parse_positive_or_blank::<u32>("1").unwrap(), Some(1));
        assert_eq!(parse_positive_or_blank::<u32>("100").unwrap(), Some(100));
        assert_eq!(
            parse_positive_or_blank::<u32>("4294967295").unwrap(),
            Some(u32::MAX)
        );
        assert_eq!(parse_positive_or_blank::<u64>("3600").unwrap(), Some(3600));
    }

    #[test]
    fn parse_positive_or_blank_rejects_zero() {
        let err = parse_positive_or_blank::<u32>("0").unwrap_err();
        assert!(err.contains("greater than zero"), "got: {err}");
    }

    #[test]
    fn parse_positive_or_blank_rejects_garbage() {
        for bad in ["abc", "-1", "1.5", "100000000000"] {
            assert!(
                parse_positive_or_blank::<u32>(bad).is_err(),
                "bad input {bad:?} should have been rejected"
            );
        }
    }
}
