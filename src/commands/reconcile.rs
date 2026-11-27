//! `kei reconcile`: owe again saved copies whose file is missing, and
//! confirm damage to one copy so the next sync replaces exactly those bytes.
//! It never deletes or changes files.

#![allow(
    clippy::print_stdout,
    reason = "CLI subcommand whose primary purpose is to print a reconcile report to stdout"
)]

use crate::cli;
use crate::config;

use super::{LISTING_CAP, print_truncation_tail};

pub(crate) async fn run_reconcile(
    args: cli::ReconcileArgs,
    globals: &config::GlobalArgs,
    toml: Option<&config::TomlConfig>,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        args.action.is_none() || args.inspect_path.is_none(),
        "Integrity inspection cannot be combined with a reconcile subcommand"
    );
    if args.action.is_some() {
        anyhow::bail!(
            "Legacy file trust is no longer supported. Run kei sync. Existing trust records and media were not changed."
        );
    }
    let (location, state) = super::core_state(globals, toml).await?;
    let Some(state) = state else {
        anyhow::ensure!(
            args.inspect_path.is_none(),
            "Integrity inspection requires an existing owned state database"
        );
        println!("No state database found at {}", location.current.display());
        println!("Run a sync first to create the database.");
        return Ok(());
    };
    let directory = super::state_directory(toml, &state).await?.ok_or_else(|| {
        anyhow::anyhow!(
            "Set [download].directory in the config file to reconcile downloaded files."
        )
    })?;

    if let Some(path) = args.inspect_path {
        return inspect(&state, &directory, &path, args.confirm_damage.as_deref()).await;
    }

    let missing = crate::sync_core::missing_saved_copies(&state, &directory).await?;
    // Carried-over copies with no file are never taken over, so the next
    // sync downloads them as new copies; list them with the rest.
    let mut carried = Vec::new();
    for copy in state.list_imported().await? {
        if tokio::fs::symlink_metadata(directory.join(&copy.path))
            .await
            .is_err()
        {
            carried.push(copy.path);
        }
    }
    let total = missing.len() + carried.len();
    for path in missing.iter().chain(carried.iter()).take(LISTING_CAP) {
        println!("MISSING: {}", directory.join(path).display());
    }
    print_truncation_tail(total, total.min(LISTING_CAP));
    if args.dry_run {
        println!("Dry run: {total} missing files would be downloaded again by the next sync.");
        return Ok(());
    }
    for path in &missing {
        if let Some(row) = state.file_at(path).await? {
            state.owe_again(row.id, None).await?;
        }
    }
    println!("{total} missing files will be downloaded again by the next sync.");
    Ok(())
}

/// Print one copy's fingerprint and, when confirmed, owe it again so the
/// next sync replaces exactly these bytes.
async fn inspect(
    state: &crate::sync_core::State,
    directory: &std::path::Path,
    path: &std::path::Path,
    confirm: Option<&str>,
) -> anyhow::Result<()> {
    let path = std::path::absolute(path)?;
    let relative = path
        .strip_prefix(directory)
        .map_err(|_outside| {
            anyhow::anyhow!("{} is outside the download directory", path.display())
        })?
        .components()
        .map(|part| part.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/");
    let row = state.file_at(&relative).await?.ok_or_else(|| {
        anyhow::anyhow!("Path has no current independently verified destination receipt")
    })?;
    let (size, sha256) = crate::download::core_io::hash_file(&path)
        .await?
        .ok_or_else(|| anyhow::anyhow!("{} does not exist", path.display()))?;
    let token = format!("{sha256}:{size}");
    println!("Path: {}", path.display());
    println!("Fingerprint: {token}");
    println!(
        "Recorded: {}",
        row.sha256.as_deref().unwrap_or("no checksum recorded")
    );
    let Some(confirmed) = confirm else {
        return Ok(());
    };
    anyhow::ensure!(
        confirmed == token,
        "File changed since inspection; confirmation refused"
    );
    anyhow::ensure!(
        row.sha256.as_deref() != Some(sha256.as_str()),
        "File matches its recorded local checksum; no damage to confirm"
    );
    state.owe_again(row.id, Some(&sha256)).await?;
    println!("Damage confirmed. The next sync replaces this copy if it still has these bytes.");
    Ok(())
}
