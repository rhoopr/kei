#![allow(
    clippy::print_stdout,
    reason = "CLI subcommand whose primary purpose is to print reset status to stdout"
)]

use crate::config;
use crate::state;
use crate::state::StateDb;

/// Run the reset-state command.
pub(crate) async fn run_reset_state(
    yes: bool,
    globals: &config::GlobalArgs,
    toml: Option<&config::TomlConfig>,
) -> anyhow::Result<()> {
    let db_path = super::super::get_db_path(globals, toml)?;

    if !db_path.exists() {
        println!("No state database found at {}", db_path.display());
        return Ok(());
    }

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
pub(crate) async fn run_reset_sync_token(
    globals: &config::GlobalArgs,
    toml: Option<&config::TomlConfig>,
) -> anyhow::Result<()> {
    let db_path = super::super::get_db_path(globals, toml)?;

    if !db_path.exists() {
        println!("No state database found at {}", db_path.display());
        return Ok(());
    }

    let db = state::SqliteStateDb::open(&db_path).await?;
    db.set_metadata("db_sync_token", "").await?;
    let cleared = db.delete_metadata_by_prefix("sync_token:").await?;
    println!(
        "Cleared sync tokens ({} zone token{} + db token). Next sync will do a full enumeration.",
        cleared,
        if cleared == 1 { "" } else { "s" }
    );

    Ok(())
}
