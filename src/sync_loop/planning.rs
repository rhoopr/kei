//! Library pass refresh, shared-library notices, and path warnings.

use crate::commands::{
    CollectionContext, build_collection_context, collection_libraries, pass_scope_for_zone,
    resolve_cross_zone_libraries_for_album_hydration, resolve_passes_for_scope, zone_name_set,
};
use crate::sync_cycle::{LibraryState, sync_token_key as make_sync_token_key};
use crate::{config, state};

/// State-DB metadata key for the first-sync shared-library notice. Bumping
/// the version suffix (e.g. `_v2`) re-fires the notice for every existing
/// data dir the next time it's used.
const SHARED_LIBRARY_NOTICE_KEY: &str = "shared_library_notice_shown_v1";

const SHARED_LIBRARY_NOTICE_CHECKED_KEY: &str = "shared_library_notice_checked_at_v1";

const SHARED_LIBRARY_NOTICE_CHECK_TTL_SECS: i64 = 24 * 60 * 60;

/// Given the user's library selector, the count of iCloud shared libraries
/// on the account, and whether the notice has already fired, return the
/// warning message to emit, or `None` if no notice is warranted.
///
/// Pure function so the policy is unit-testable without mocking the
/// `PhotosService` or the state DB. The I/O wrapper lives in
/// [`maybe_notify_shared_libraries`].
fn should_notify_shared_libraries(
    selector: &crate::selection::LibrarySelector,
    shared_count: usize,
    already_notified: bool,
) -> Option<String> {
    if already_notified || shared_count == 0 {
        return None;
    }
    // Only users on the `primary`-only default see the notice. Anyone who
    // explicitly picked a different shape (`shared`, `all`, named zones,
    // exclusions) has already made a deliberate choice.
    if selector != &crate::selection::LibrarySelector::default() {
        return None;
    }
    let (word, verb) = if shared_count == 1 {
        ("library", "is")
    } else {
        ("libraries", "are")
    };
    Some(format!(
        "Detected {shared_count} iCloud shared {word} on this account; only the primary \
         library {verb} being synced. To include shared libraries too, set \
         `[filters] libraries = [\"all\"]` in config.toml. \
         Run `kei list libraries` to enumerate every zone."
    ))
}

fn shared_library_notice_recently_checked(checked_at: Option<&str>, now_ts: i64) -> bool {
    let Some(checked_at) = checked_at.and_then(|value| value.parse::<i64>().ok()) else {
        return false;
    };
    now_ts.saturating_sub(checked_at) < SHARED_LIBRARY_NOTICE_CHECK_TTL_SECS
}

/// Probe + warning for users on the `PrimarySync` default who also have
/// shared libraries. The notice marker (stored in the state DB's `metadata`
/// table under [`SHARED_LIBRARY_NOTICE_KEY`]) is set after the notice fires.
/// A separate short-lived negative cache records "no shared libraries seen"
/// so accounts without shared libraries do not pay a shared-zone listing on
/// every one-shot sync. The probe and marker writes are best-effort: failures
/// degrade to `tracing::debug!` and skip without breaking the sync.
pub(super) async fn maybe_notify_shared_libraries(
    selector: &crate::selection::LibrarySelector,
    photos_service: &mut crate::icloud::photos::PhotosService,
    state_db: Option<&dyn state::SyncTokenStore>,
) {
    let Some(db) = state_db else {
        tracing::debug!("shared-library notice: no state DB available; skipping uncached probe");
        return;
    };

    let already_notified = match db.get_metadata(SHARED_LIBRARY_NOTICE_KEY).await {
        Ok(Some(_)) => true,
        Ok(None) => false,
        Err(e) => {
            tracing::debug!(
                error = %e,
                "shared-library notice: metadata read failed; skipping"
            );
            return;
        }
    };
    if already_notified {
        return;
    }

    // Skip the probe when the user explicitly picked a non-default library;
    // they've already opted in or out. `should_notify_shared_libraries`
    // repeats this check defensively.
    if selector != &crate::selection::LibrarySelector::default() {
        return;
    }

    match db.get_metadata(SHARED_LIBRARY_NOTICE_CHECKED_KEY).await {
        Ok(checked_at) => {
            if shared_library_notice_recently_checked(
                checked_at.as_deref(),
                chrono::Utc::now().timestamp(),
            ) {
                tracing::debug!("shared-library notice: recent no-shared check cached");
                return;
            }
        }
        Err(e) => {
            tracing::debug!(
                error = %e,
                "shared-library notice: checked-at read failed; probing"
            );
        }
    }

    let shared_count = match photos_service.fetch_shared_libraries().await {
        Ok(map) => map.len(),
        Err(e) => {
            tracing::debug!(
                error = %e,
                "shared-library notice: enumeration failed; skipping"
            );
            return;
        }
    };

    let Some(msg) = should_notify_shared_libraries(selector, shared_count, already_notified) else {
        if shared_count == 0
            && let Err(e) = db
                .set_metadata(
                    SHARED_LIBRARY_NOTICE_CHECKED_KEY,
                    &chrono::Utc::now().timestamp().to_string(),
                )
                .await
        {
            tracing::debug!(
                error = %e,
                "shared-library notice: failed to persist no-shared check marker"
            );
        }
        return;
    };
    tracing::warn!(message = %msg, "Shared library notice");

    if let Err(e) = db.set_metadata(SHARED_LIBRARY_NOTICE_KEY, "1").await {
        tracing::debug!(
            error = %e,
            "shared-library notice: failed to persist marker"
        );
    }
}

pub(super) async fn refresh_needed_library_plans(
    library_states: &mut [LibraryState],
    selection: &crate::selection::Selection,
    collection_context: &CollectionContext,
    changed_zones: Option<&rustc_hash::FxHashSet<String>>,
    consecutive_album_refresh_failures: &mut u32,
) {
    for lib_state in library_states {
        if !lib_state.plan_needs_refresh {
            continue;
        }
        if changed_zones.is_some_and(|zones| !zones.contains(&lib_state.zone_name)) {
            continue;
        }

        // Re-resolve albums per-library to discover newly created iCloud albums.
        // Full sync resolves unfiled album-member exclusions in the download
        // phase; incremental/cleanup paths resolve them before planning tasks.
        // This refresh is intentionally delayed until a selected zone has
        // changes so quiet watch cycles avoid the album-listing traffic.
        match resolve_passes_for_scope(
            &lib_state.library,
            selection,
            lib_state.pass_scope,
            collection_context,
            &lib_state.cross_zone_libraries,
        )
        .await
        {
            Ok(refreshed) => {
                lib_state.plan = refreshed;
                lib_state.plan_is_stale = false;
                lib_state.plan_needs_refresh = false;
                *consecutive_album_refresh_failures = 0;
            }
            Err(e) => {
                *consecutive_album_refresh_failures += 1;
                lib_state.plan_is_stale = true;
                lib_state.plan_needs_refresh = true;
                if *consecutive_album_refresh_failures >= 3 {
                    tracing::error!(
                        zone = %lib_state.zone_name,
                        error = %e,
                        consecutive_failures = *consecutive_album_refresh_failures,
                        "Repeated album refresh failures, reusing previous set"
                    );
                } else {
                    tracing::warn!(
                        zone = %lib_state.zone_name,
                        error = %e,
                        "Failed to refresh albums, reusing previous set"
                    );
                }
            }
        }
    }
}

/// Identify active-pass templates that lack `{library}` when multiple
/// libraries are selected. Returns a sorted list of CLI flag names whose
/// templates would let same-named assets from different zones land in the
/// same on-disk path. Empty list means the multi-library plan is unambiguous.
///
/// The check is per-pass-kind: each *active* template (one whose pass kind
/// will actually run under the current Selection) must contain `{library}`.
pub(crate) fn count_passes(plan: &crate::commands::AlbumPlan) -> (usize, usize, bool) {
    use crate::commands::PassKind;
    let mut album = 0;
    let mut smart_folder = 0;
    let mut unfiled = false;
    for pass in &plan.passes {
        match pass.kind {
            PassKind::Album => album += 1,
            PassKind::SmartFolder => smart_folder += 1,
            PassKind::Unfiled => unfiled = true,
        }
    }
    (album, smart_folder, unfiled)
}

/// Template strings whose pass is disabled (e.g. `folder_structure_smart_folders`
/// when `--smart-folder none`) don't render any path so they don't need
/// `{library}` to keep the sync safe.
fn find_multi_library_commingle_flags(
    library_states: &[LibraryState],
    folder_structure: &str,
    folder_structure_albums: &str,
    folder_structure_smart_folders: &str,
) -> Vec<&'static str> {
    if library_states.len() < 2 {
        return Vec::new();
    }

    let mut active_unfiled_libraries = 0usize;
    let mut active_album_libraries = 0usize;
    let mut active_smart_folder_libraries = 0usize;
    for state in library_states {
        let (album_passes, smart_folder_passes, has_unfiled_pass) = count_passes(&state.plan);
        if has_unfiled_pass {
            active_unfiled_libraries += 1;
        }
        if album_passes > 0 {
            active_album_libraries += 1;
        }
        if smart_folder_passes > 0 {
            active_smart_folder_libraries += 1;
        }
    }

    // All passes disabled - resolve_passes returns an empty plan, no path
    // ever renders, multi-library can't commingle.
    if active_unfiled_libraries == 0
        && active_album_libraries == 0
        && active_smart_folder_libraries == 0
    {
        return Vec::new();
    }

    let mut missing: Vec<&'static str> = Vec::new();
    if active_unfiled_libraries > 1 && !folder_structure.contains("{library}") {
        missing.push("--folder-structure");
    }
    if active_album_libraries > 1 && !folder_structure_albums.contains("{library}") {
        missing.push("--folder-structure-albums");
    }
    if active_smart_folder_libraries > 1 && !folder_structure_smart_folders.contains("{library}") {
        missing.push("--folder-structure-smart-folders");
    }
    missing
}

/// Emit a startup warning when multi-library paths commingle. Informational:
/// the run continues, and `file_match_policy` (default
/// `name-size-dedup-with-suffix`) keeps two libraries from silently
/// overwriting each other -- collisions land at `<name>-1.<ext>` rather
/// than overwriting. The warning surfaces the namespace ambiguity so the
/// user can add `{library}` to their templates if they want zone-disjoint
/// trees.
fn warn_if_multi_library_paths_commingle(
    library_states: &[LibraryState],
    folder_structure: &str,
    folder_structure_albums: &str,
    folder_structure_smart_folders: &str,
) {
    let missing = find_multi_library_commingle_flags(
        library_states,
        folder_structure,
        folder_structure_albums,
        folder_structure_smart_folders,
    );
    if missing.is_empty() {
        return;
    }
    let library_count = library_states.len();
    tracing::warn!(
        library_count,
        missing = ?missing,
        "Multi-library sync: active template(s) lack `{{library}}`; same-named \
         assets from different zones will share an on-disk namespace. \
         File-match policy keeps writes from overwriting (collisions get a \
         `-N` suffix), but cross-library files end up interleaved. Add \
         `{{library}}` to each listed template for zone-disjoint trees."
    );
}

/// Resolve selected library passes and report the active plan and path warnings.
pub(super) async fn resolve_library_plans(
    photos_service: &mut crate::icloud::photos::PhotosService,
    libraries: &[crate::icloud::photos::PhotoLibrary],
    config: &config::Config,
) -> anyhow::Result<(Vec<LibraryState>, CollectionContext)> {
    let all_libraries = photos_service.all_libraries().await?;
    let cross_zone_libraries =
        resolve_cross_zone_libraries_for_album_hydration(&config.filters.selection, async {
            Ok::<_, anyhow::Error>(all_libraries.clone())
        })
        .await?;

    let collection_libraries =
        collection_libraries(&config.filters.selection, libraries, &all_libraries);
    let collection_context =
        build_collection_context(&config.filters.selection, collection_libraries).await?;
    let selected_zones = zone_name_set(libraries);
    let collection_zones = zone_name_set(collection_libraries);

    let mut library_states: Vec<LibraryState> = Vec::with_capacity(all_libraries.len());
    for library in &all_libraries {
        let zone_name = library.zone_name().to_string();
        let pass_scope = pass_scope_for_zone(
            &config.filters.selection,
            zone_name.as_str(),
            &selected_zones,
            &collection_zones,
        );
        if pass_scope.is_empty() {
            continue;
        }

        let sync_token_key = make_sync_token_key(&zone_name);
        let plan = resolve_passes_for_scope(
            library,
            &config.filters.selection,
            pass_scope,
            &collection_context,
            &cross_zone_libraries,
        )
        .await?;
        let (album_passes, smart_folder_passes, unfiled) = super::count_passes(&plan);
        tracing::info!(
            zone = %zone_name,
            album_passes,
            smart_folder_passes,
            unfiled,
            "Sync plan for library"
        );
        library_states.push(LibraryState {
            library: library.clone(),
            cross_zone_libraries: cross_zone_libraries.clone(),
            pass_scope,
            zone_name,
            sync_token_key,
            plan,
            plan_is_stale: false,
            plan_needs_refresh: false,
        });
    }
    warn_if_multi_library_paths_commingle(
        &library_states,
        &config.download.folder_structure,
        &config.download.folder_structure_albums,
        &config.download.folder_structure_smart_folders,
    );
    Ok((library_states, collection_context))
}

#[cfg(test)]
mod tests;
