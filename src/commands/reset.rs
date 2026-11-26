#![allow(
    clippy::print_stdout,
    reason = "CLI subcommand whose primary purpose is to print reset status to stdout"
)]

use crate::auth;
use crate::cli;
use crate::config;
use crate::state::db::account::AccountOwner;
use crate::sync_core::access::{self, Location};

/// Run the reset-state command: delete the account's state, previous and
/// current, after checking it belongs to this account.
pub(crate) async fn run_reset_state(
    yes: bool,
    globals: &config::GlobalArgs,
    toml: Option<&config::TomlConfig>,
    input_mode: crate::InputMode,
) -> anyhow::Result<()> {
    let (username, _, domain, cookie_dir) =
        config::resolve_auth(globals, &cli::PasswordArgs::default(), toml)?;
    if username.is_empty() {
        anyhow::bail!("Set your iCloud username with ICLOUD_USERNAME or [auth].username.");
    }
    let location = Location::resolve(&cookie_dir, &username, domain.as_str()).await?;
    if !location.exists() {
        println!("No state database found at {}", location.current.display());
        return Ok(());
    }
    let owner = AccountOwner::configured(&username, domain.as_str());
    access::check_existing(&location, &owner).await?;

    if !yes {
        use std::io::Write;
        if !input_mode.can_prompt() {
            anyhow::bail!(
                "`{}` needs `--yes` when stdin is not a terminal. The state database was not deleted.",
                cli::command_for_config("reset state", globals.config_source.recovery_path())
            );
        }
        println!("This will delete the state database at:");
        for path in [&location.current, &location.legacy] {
            if path.exists() {
                println!("  {}", path.display());
            }
        }
        println!();
        print!("Are you sure? [y/N] ");
        std::io::stdout().flush()?;

        let mut input = String::new();
        std::io::stdin().read_line(&mut input)?;
        if !input.trim().eq_ignore_ascii_case("y") {
            println!("Cancelled.");
            return Ok(());
        }
    }

    access::delete(&location, &owner).await?;
    println!("State database deleted.");
    Ok(())
}

/// Run the reset-sync-token command.
///
/// `yes` skips the confirmation prompt. Without it, prompts on a TTY and
/// errors under non-interactive use, mirroring `reset state`. Forgetting the
/// saved positions makes the next sync list every library again, which on a
/// 100k-asset library is slow; the confirmation keeps a typo from
/// triggering that work by accident. Saved files and their records stay.
pub(crate) async fn run_reset_sync_token(
    yes: bool,
    globals: &config::GlobalArgs,
    toml: Option<&config::TomlConfig>,
    input_mode: crate::InputMode,
) -> anyhow::Result<()> {
    let (location, state) = super::core_state(globals, toml).await?;
    let Some(state) = state else {
        println!("No state database found at {}", location.current.display());
        return Ok(());
    };

    if !yes {
        use std::io::Write;
        if !input_mode.can_prompt() {
            anyhow::bail!(
                "`{}` needs `--yes` when stdin is not a terminal. The next sync will re-enumerate every asset, which can take a long time on a large library.",
                cli::command_for_config("reset sync-token", globals.config_source.recovery_path())
            );
        }
        println!("This will clear stored sync tokens at:");
        println!("  {}", location.current.display());
        println!();
        println!("Next sync will re-enumerate every asset.");
        print!("Are you sure? [y/N] ");
        std::io::stdout().flush()?;

        let mut input = String::new();
        std::io::stdin().read_line(&mut input)?;
        if !input.trim().eq_ignore_ascii_case("y") {
            println!("Cancelled.");
            return Ok(());
        }
    }

    let cleared = state.forget_positions().await?;
    println!(
        "Cleared sync tokens ({cleared} zone token{}). Next sync will do a full enumeration.",
        if cleared == 1 { "" } else { "s" }
    );
    Ok(())
}

/// Run the reset-session command: discard the local session so the next
/// `kei login` starts from a clean slate.
///
/// `yes` skips the confirmation prompt. Without it, prompts on a TTY and
/// errors under non-interactive use, mirroring `reset sync-token`.
/// Discarding the session (including trust tokens) forces the next
/// `kei login` to run the full password + 2FA flow; the stored password
/// and the state database are kept.
pub(crate) async fn run_reset_session(
    yes: bool,
    globals: &config::GlobalArgs,
    toml: Option<&config::TomlConfig>,
) -> anyhow::Result<()> {
    // Reset never authenticates; resolve_auth only reads the password from
    // CLI/env args, so empty PasswordArgs are fine here.
    let password_args = cli::PasswordArgs::default();
    let (username, _, domain, cookie_directory) =
        config::resolve_auth(globals, &password_args, toml)?;

    if username.is_empty() {
        anyhow::bail!("Set your iCloud username with ICLOUD_USERNAME or [auth].username.");
    }

    let state_guard =
        auth::session::SessionStateGuard::acquire(&cookie_directory, &username, domain.as_str())
            .await?;
    let files = state_guard.files();
    let existing: Vec<_> = files.iter().filter(|p| p.exists()).collect();

    // Nothing to remove: report and exit before the lock and the `--yes`
    // guard, mirroring the no-DB early return of the other reset subcommands.
    if existing.is_empty() {
        println!(
            "No local session found in {} — nothing to reset.",
            cookie_directory.display()
        );
        return Ok(());
    }

    if !yes {
        use std::io::IsTerminal;
        use std::io::Write;
        if !std::io::stdin().is_terminal() {
            anyhow::bail!(
                "`{}` needs `--yes` when stdin is not a terminal. This discards the local session and trust tokens, and the next login will require password authentication and 2FA again.",
                cli::command_for_config("reset session", globals.config_source.recovery_path())
            );
        }
        println!("This will discard the local iCloud session (including trust tokens):");
        for path in &existing {
            println!("  {}", path.display());
        }
        println!();
        println!("The stored password and the state database are kept.");
        print!("Are you sure? [y/N] ");
        std::io::stdout().flush()?;

        let mut input = String::new();
        std::io::stdin().read_line(&mut input)?;
        if !input.trim().eq_ignore_ascii_case("y") {
            println!("Cancelled.");
            return Ok(());
        }
    }

    let removed = state_guard.discard().await?;
    for path in &removed {
        println!("Removed {}", path.display());
    }
    println!(
        "Session reset. Run `{}` to authenticate again.",
        cli::command_for_config("login", globals.config_source.recovery_path())
    );

    Ok(())
}
