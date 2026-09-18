mod config_cmd;
mod doctor;
mod import;
mod list;
mod login;
mod manifest;
mod password;
pub(crate) mod reconcile;
mod reset;
mod service;
mod status;
mod verify;

pub(crate) use config_cmd::run_config_show;
pub(crate) use doctor::run_doctor;
pub(crate) use import::run_import_existing;
#[cfg(test)]
pub(crate) use import::{ImportRunOptions, import_assets};
pub(crate) use list::run_list;
pub(crate) use login::run_login;
pub(crate) use manifest::run_manifest;
pub(crate) use password::run_password;
pub(crate) use reconcile::run_reconcile;
pub(crate) use reset::{run_reset_session, run_reset_state, run_reset_sync_token};
pub(crate) use service::{
    AlbumPass, AlbumPlan, CollectionContext, MAX_REAUTH_ATTEMPTS, PassKind, PassScope,
    attempt_reauth, build_collection_context, collection_libraries, init_photos_service,
    pass_scope_for_zone, resolve_cross_zone_libraries_for_album_hydration, resolve_libraries,
    resolve_passes_for_scope, wait_and_retry_2fa, zone_name_set,
};
#[cfg(test)]
pub(crate) use status::backup_status_line;
pub(crate) use status::run_status;
pub(crate) use verify::run_verify;

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
