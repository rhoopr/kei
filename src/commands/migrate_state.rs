#![allow(clippy::print_stdout, reason = "CLI migration status")]

use crate::{auth, cli, config, state};

/// Adoption is deliberately separate from sync and requires an explicit source
/// and ownership attestation. Cached authentication is never adoption evidence.
pub(crate) async fn run_migrate_state(
    args: cli::MigrateStateArgs,
    globals: &config::GlobalArgs,
    toml: Option<&config::TomlConfig>,
    input_mode: crate::InputMode,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        args.confirm_ownership,
        "Legacy adoption requires --confirm-ownership; a filename cannot prove ownership."
    );
    let (username, password, domain, directory) =
        config::resolve_auth(globals, &args.password, toml);
    anyhow::ensure!(
        !username.is_empty(),
        "Set your iCloud username with ICLOUD_USERNAME or [auth].username."
    );
    let destination = directory.join(format!(
        "{}.db",
        crate::account::namespace(&username, domain.as_str())
    ));
    anyhow::ensure!(
        matches!(tokio::fs::symlink_metadata(&destination).await, Err(error) if error.kind() == std::io::ErrorKind::NotFound),
        "The account destination already exists; adoption never overwrites or merges state."
    );
    anyhow::ensure!(
        tokio::fs::metadata(&args.legacy_db).await?.is_file(),
        "The explicit legacy database must be an existing file."
    );
    // Coordinate with older sync/login processes without changing an existing
    // legacy lock's contents or generation. Operators must also stop old offline
    // state commands; the SQLite backup detects commits during copying.
    let legacy_lock = args.legacy_db.parent().unwrap_or(&directory).join(format!(
        "{}.lock",
        auth::session::sanitize_username(&username)
    ));
    let _legacy_guard = if tokio::fs::try_exists(&legacy_lock).await? {
        use fs4::fs_std::FileExt;
        let file = tokio::fs::File::open(&legacy_lock).await?.into_std().await;
        anyhow::ensure!(
            file.try_lock_exclusive()?,
            "A legacy kei process is active; stop it before adoption."
        );
        Some(file)
    } else {
        None
    };
    let password_provider = super::super::make_provider_from_auth(
        &args.password,
        password,
        &username,
        &directory,
        domain.as_str(),
        toml,
        input_mode,
    );
    let authenticated = auth::authenticate_fresh_for_adoption(
        &directory,
        &username,
        &password_provider,
        domain.as_str(),
        args.code.as_deref(),
        input_mode,
    )
    .await?;
    let owner = state::db::account::AccountOwner::authenticated(
        &username,
        domain.as_str(),
        &authenticated.data,
    )?;
    state::db::account::adopt_legacy(
        &args.legacy_db,
        &destination,
        &owner,
        &username,
        domain.as_str(),
    )
    .await?;
    println!(
        "Legacy state adopted into {}. All legacy files are preserved. Legacy encrypted credentials are not copied; re-save them with `kei password set` if needed.",
        destination.display()
    );
    Ok(())
}
