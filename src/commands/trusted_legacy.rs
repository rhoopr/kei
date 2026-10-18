//! Offline, explicit local-byte acceptance. No authentication or provider client.
#![allow(
    clippy::print_stdout,
    reason = "CLI command reports explicit local acceptance to stdout"
)]
use crate::{cli, config, download::trusted_legacy, state};
use anyhow::Context;
use tokio_util::sync::CancellationToken;

pub(crate) async fn run(
    args: cli::TrustedLegacyArgs,
    globals: &config::GlobalArgs,
    toml: Option<&config::TomlConfig>,
) -> anyhow::Result<()> {
    let config = config::Config::build(
        globals,
        &cli::PasswordArgs::default(),
        cli::SyncArgs::default(),
        toml,
    )?;
    let _guard = crate::auth::session::SessionStateGuard::acquire(
        &config.auth.cookie_directory,
        &config.auth.username,
        config.auth.domain.as_str(),
    )
    .await?;
    let path = crate::get_db_path(globals, toml).await?;
    let owner = crate::get_account_owner(globals, toml);
    let cancel = CancellationToken::new();
    let signal_cancel = cancel.clone();
    let signal = tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            signal_cancel.cancel();
        }
    });
    let result = execute(args, &config, &path, &owner, &cancel).await;
    signal.abort();
    result
}

async fn execute(
    args: cli::TrustedLegacyArgs,
    config: &config::Config,
    path: &std::path::Path,
    owner: &state::db::account::AccountOwner,
    cancel: &CancellationToken,
) -> anyhow::Result<()> {
    match args.action {
        cli::TrustedLegacyAction::Plan { library, output } => {
            let db = state::SqliteStateDb::open_owned_read_only(path, owner).await?;
            let plan = trusted_legacy::plan(
                &db,
                &library,
                &config.download.directory,
                &config.download.temp_suffix,
                config.download.legacy_preservation_allow_hardlinks,
                cancel,
            )
            .await?;
            let bytes = serde_json::to_vec_pretty(&plan)?;
            anyhow::ensure!(bytes.len() <= 4 * 1024 * 1024, "Plan exceeds 4 MiB");
            write_plan(&output, &bytes)?;
            println!(
                "Planned {} Trusted Legacy Files. Review the plan before apply. Provider verification remains unknown.",
                plan.targets.len()
            );
        }
        cli::TrustedLegacyAction::Apply { plan } => {
            use std::io::Read;
            let file = std::fs::File::open(plan)?;
            let mut bytes = Vec::new();
            file.take(4 * 1024 * 1024 + 1).read_to_end(&mut bytes)?;
            anyhow::ensure!(bytes.len() <= 4 * 1024 * 1024, "Plan exceeds 4 MiB");
            let plan =
                serde_json::from_slice(&bytes).context("Invalid trusted legacy file plan")?;
            // Validate ownership/schema before any writable open.
            drop(state::SqliteStateDb::open_owned_read_only(path, owner).await?);
            let db = state::SqliteStateDb::open_owned(path, owner).await?;
            let count = trusted_legacy::apply(
                &db,
                plan,
                &config.download.directory,
                &config.download.temp_suffix,
                config.download.legacy_preservation_allow_hardlinks,
                cancel,
            )
            .await?;
            println!(
                "Accepted {count} Trusted Legacy Files for legacy migration. Source receipts remain unchanged; fresh provider coverage is still required."
            );
        }
    }
    Ok(())
}

fn write_plan(path: &std::path::Path, bytes: &[u8]) -> anyhow::Result<()> {
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}
