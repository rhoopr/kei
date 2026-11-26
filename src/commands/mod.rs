mod config_cmd;
mod doctor;
mod import;
mod list;
mod login;
mod manifest;
mod migrate_state;
mod password;
pub(crate) mod reconcile;
mod reset;
mod service;
mod status;
mod verify;

pub(crate) use config_cmd::run_config_show;
pub(crate) use doctor::run_doctor;
pub(crate) use import::adopt_startup_archive;
pub(crate) use import::run_import_existing;
#[cfg(test)]
pub(crate) use import::{ImportRunOptions, import_assets};
pub(crate) use list::run_list;
pub(crate) use login::run_login;
pub(crate) use manifest::run_manifest;
pub(crate) use migrate_state::run_migrate_state;
pub(crate) use password::run_password;
pub(crate) use reconcile::run_reconcile;
pub(crate) use reset::{run_reset_session, run_reset_state, run_reset_sync_token};
pub(crate) use service::{
    AlbumPass, AlbumPlan, CollectionContext, MAX_REAUTH_ATTEMPTS, PassKind, PassScope,
    attempt_reauth, build_collection_context_for_sync, build_photos_params, collection_libraries,
    init_photos_service, pass_scope_for_zone, resolve_cross_zone_libraries_for_album_hydration,
    resolve_libraries, resolve_passes_for_scope, wait_and_retry_2fa, zone_name_set,
};
#[cfg(test)]
pub(crate) use status::backup_status_line;
pub(crate) use status::run_status;
pub(crate) use verify::run_verify;

/// Open the configured account's sync state for an offline command,
/// upgrading a previous-engine state file first if needed. Returns the
/// state's location and the state, or `None` when the account has none.
pub(crate) async fn core_state(
    globals: &crate::config::GlobalArgs,
    toml: Option<&crate::config::TomlConfig>,
) -> anyhow::Result<(
    crate::sync_core::access::Location,
    Option<crate::sync_core::State>,
)> {
    let (username, _, domain, cookie_dir) =
        crate::config::resolve_auth(globals, &crate::cli::PasswordArgs::default(), toml)?;
    if username.is_empty() {
        anyhow::bail!("Set your iCloud username with ICLOUD_USERNAME or [auth].username.");
    }
    let location =
        crate::sync_core::access::Location::resolve(&cookie_dir, &username, domain.as_str())
            .await?;
    let owner = crate::state::db::account::AccountOwner::configured(&username, domain.as_str());
    let directory = download_directory(toml);
    let state = crate::sync_core::access::open(&location, &owner, directory.as_deref()).await?;
    Ok((location, state))
}

/// The configured download directory, if the config file sets one.
pub(crate) fn download_directory(
    toml: Option<&crate::config::TomlConfig>,
) -> Option<std::path::PathBuf> {
    toml.and_then(|t| t.download.as_ref())
        .and_then(|download| download.directory.as_deref())
        .filter(|directory| !directory.is_empty())
        .map(crate::config::expand_tilde)
}

/// Maximum per-section listing size for the diagnostic subcommands
/// (`kei status`, `kei verify`, `kei reconcile`). Listings beyond this
/// cap print a tail line via [`print_truncation_tail`]; summary counts
/// always reflect the true totals. Matches the `FAILED_ASSETS_CAP` used
/// by `sync_report.json` so operators see a consistent amount of detail
/// across the diagnostic surfaces.
pub(crate) const LISTING_CAP: usize = 200;

/// Print the standard "... and N more" tail when a listing was capped.
/// No-op when `total <= shown`. Callers decide whether to emit a leading
/// blank line based on their surrounding section structure.
#[allow(
    clippy::print_stdout,
    reason = "CLI subcommand output; callers already opt into print_stdout at their module level"
)]
pub(crate) fn print_truncation_tail(total: usize, shown: usize) {
    if total > shown {
        println!(
            "... and {} more (listing capped at {LISTING_CAP})",
            total - shown
        );
    }
}
