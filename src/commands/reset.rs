#![allow(
    clippy::print_stdout,
    reason = "CLI subcommand whose primary purpose is to print reset status to stdout"
)]

use crate::auth;
use crate::cli;
use crate::config;
use crate::state;

/// Run the reset-state command.
pub(crate) async fn run_reset_state(
    yes: bool,
    globals: &config::GlobalArgs,
    toml: Option<&config::TomlConfig>,
) -> anyhow::Result<()> {
    let db_path = super::super::get_db_path(globals, toml).await?;

    if !db_path.exists() {
        println!("No state database found at {}", db_path.display());
        return Ok(());
    }

    let _ownership_check = state::SqliteStateDb::open_owned_read_only(
        &db_path,
        &super::super::get_account_owner(globals, toml),
    )
    .await?;
    drop(_ownership_check);

    if !yes {
        use std::io::Write;
        println!("This will delete the state database at:");
        println!("  {}", db_path.display());
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

    tokio::fs::remove_file(&db_path).await?;
    println!("State database deleted.");

    // Also remove WAL and SHM files if they exist
    let wal_path = db_path.with_extension("db-wal");
    let shm_path = db_path.with_extension("db-shm");
    let _ = tokio::fs::remove_file(&wal_path).await;
    let _ = tokio::fs::remove_file(&shm_path).await;

    Ok(())
}

/// Run the reset-sync-token command.
///
/// `yes` skips the confirmation prompt. Without it, prompts on a TTY and
/// errors under non-interactive use, mirroring `reset state`. Clearing the
/// sync token forces the next sync to do a full enumeration of every asset,
/// which on a 100k-asset library is slow and chats up Apple's API; the
/// confirmation is here to keep a typo (or muscle memory after `reset state`)
/// from triggering that work by accident.
pub(crate) async fn run_reset_sync_token(
    yes: bool,
    globals: &config::GlobalArgs,
    toml: Option<&config::TomlConfig>,
    input_mode: crate::InputMode,
) -> anyhow::Result<()> {
    let db_path = super::super::get_db_path(globals, toml).await?;

    if !db_path.exists() {
        println!("No state database found at {}", db_path.display());
        return Ok(());
    }

    if !yes {
        use std::io::Write;
        if !input_mode.can_prompt() {
            anyhow::bail!(
                "`kei reset sync-token` needs `--yes` when stdin is not a terminal. The next sync will re-enumerate every asset, which can take a long time on a large library."
            );
        }
        println!("This will clear stored sync tokens at:");
        println!("  {}", db_path.display());
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

    let db =
        state::SqliteStateDb::open_owned(&db_path, &super::super::get_account_owner(globals, toml))
            .await?;
    db.set_metadata("db_sync_token", "").await?;
    let cleared = db.delete_metadata_by_prefix("sync_token:").await?;
    let scoped_cleared = db.delete_scoped_db_sync_tokens().await?;
    println!(
        "Cleared sync tokens ({} zone token{} + db token + {} scoped db token{}). Next sync will do a full enumeration.",
        cleared,
        if cleared == 1 { "" } else { "s" },
        scoped_cleared,
        if scoped_cleared == 1 { "" } else { "s" }
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
        config::resolve_auth(globals, &password_args, toml);

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
                "`kei reset session` needs `--yes` when stdin is not a terminal. This discards the local session and trust tokens, and the next `kei login` will run the full password + 2FA flow again."
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
    println!("Session reset. Run `kei login` to authenticate again.");

    Ok(())
}
