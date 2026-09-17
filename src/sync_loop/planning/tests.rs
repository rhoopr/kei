use std::sync::Arc;

use crate::commands::{
    CollectionContext, PassScope, collection_libraries, pass_scope_for_zone, zone_name_set,
};
use crate::sync_cycle::LibraryState;
use crate::sync_loop::count_passes;
use crate::sync_loop::planning::{
    SHARED_LIBRARY_NOTICE_CHECK_TTL_SECS, find_multi_library_commingle_flags,
    maybe_notify_shared_libraries, refresh_needed_library_plans,
    shared_library_notice_recently_checked, should_notify_shared_libraries,
    warn_if_multi_library_paths_commingle,
};
use crate::sync_loop::test_support::{make_library_state, make_state_db};

fn all_libraries() -> crate::selection::LibrarySelector {
    crate::selection::parse_library_selector(&["all".to_string()]).unwrap()
}

fn shared_zone() -> crate::selection::LibrarySelector {
    crate::selection::parse_library_selector(&["SharedSync-ABCD1234".to_string()]).unwrap()
}

fn selection_with_smart_folder(
    libraries: crate::selection::LibrarySelector,
    unfiled: bool,
) -> crate::selection::Selection {
    use crate::selection::{AlbumSelector, Selection, SmartFolderSelector};
    Selection {
        albums: AlbumSelector::None,
        albums_explicit: false,
        smart_folders: SmartFolderSelector::Named {
            included: std::collections::BTreeSet::from(["Hidden".to_string()]),
            excluded: std::collections::BTreeSet::new(),
        },
        smart_folders_explicit: true,
        libraries,
        unfiled,
    }
}

fn test_library(zone_name: &str) -> crate::icloud::photos::PhotoLibrary {
    crate::icloud::photos::PhotoLibrary::new_stub_with_zone(
        Box::new(crate::test_helpers::MockPhotosSession::new()),
        zone_name,
    )
}

#[test]
fn run_sync_scope_planning_shared_only_smart_folder_widens_zone_scope() {
    let selection = selection_with_smart_folder(
        crate::selection::parse_library_selector(&["shared".to_string()]).unwrap(),
        false,
    );
    let primary = test_library("PrimarySync");
    let shared = test_library("SharedSync-ABCD1234");
    let selected_libraries = vec![shared.clone()];
    let all_libraries = vec![primary.clone(), shared.clone()];

    let collection = collection_libraries(&selection, &selected_libraries, &all_libraries);
    let selected_zones = zone_name_set(&selected_libraries);
    let collection_zones = zone_name_set(collection);

    let primary_scope = pass_scope_for_zone(
        &selection,
        primary.zone_name(),
        &selected_zones,
        &collection_zones,
    );
    let shared_scope = pass_scope_for_zone(
        &selection,
        shared.zone_name(),
        &selected_zones,
        &collection_zones,
    );

    assert!(
        primary_scope.include_smart_folders,
        "explicit smart-folder selection should widen pass planning beyond the library selector"
    );
    assert!(
        shared_scope.include_smart_folders,
        "run_sync planning should schedule smart-folder passes for selected shared zone"
    );
}

#[test]
fn run_sync_scope_planning_primary_only_still_filters_unfiled() {
    let selection = selection_with_smart_folder(
        crate::selection::parse_library_selector(&["primary".to_string()]).unwrap(),
        true,
    );
    let primary = test_library("PrimarySync");
    let shared = test_library("SharedSync-ABCD1234");
    let selected_libraries = vec![primary.clone()];
    let all_libraries = vec![primary.clone(), shared.clone()];

    let collection = collection_libraries(&selection, &selected_libraries, &all_libraries);
    let selected_zones = zone_name_set(&selected_libraries);
    let collection_zones = zone_name_set(collection);

    let primary_scope = pass_scope_for_zone(
        &selection,
        primary.zone_name(),
        &selected_zones,
        &collection_zones,
    );
    let shared_scope = pass_scope_for_zone(
        &selection,
        shared.zone_name(),
        &selected_zones,
        &collection_zones,
    );

    assert!(
        primary_scope.include_smart_folders,
        "run_sync planning should keep smart-folder passes in the selected primary zone"
    );
    assert!(
        shared_scope.include_smart_folders,
        "explicit smart-folder selection should widen scope to shared zones too"
    );
    assert!(
        primary_scope.include_unfiled,
        "selected primary zone should keep unfiled pass when unfiled=true"
    );
    assert!(
        !shared_scope.include_unfiled,
        "library selector should still filter unfiled passes to selected zones"
    );
}

#[test]
fn notice_suppressed_when_already_shown() {
    // The marker overrides everything: even a user with 5 shared libraries
    // and the default selection gets no notice on re-entry.
    assert!(should_notify_shared_libraries(&primary(), 5, true).is_none());
}

#[test]
fn notice_suppressed_when_no_shared_libraries() {
    assert!(should_notify_shared_libraries(&primary(), 0, false).is_none());
}

#[test]
fn notice_suppressed_when_user_picked_all() {
    // Anyone who explicitly set all libraries has already opted in;
    // nothing to tell them.
    assert!(should_notify_shared_libraries(&all_libraries(), 3, false).is_none());
}

#[test]
fn notice_suppressed_when_user_picked_shared_zone_explicitly() {
    // A user who configured `SharedSync-ABCD1234` has also made
    // a choice; don't second-guess them.
    assert!(should_notify_shared_libraries(&shared_zone(), 3, false).is_none());
}

#[test]
fn notice_fires_with_singular_wording_for_one_library() {
    let msg = should_notify_shared_libraries(&primary(), 1, false).unwrap();
    assert!(
        msg.contains("1 iCloud shared library"),
        "singular 'library' wording expected; got: {msg}"
    );
    assert!(
        msg.contains("is being synced"),
        "singular verb 'is' expected; got: {msg}"
    );
    // The guidance is what the notice is for - it must name the config
    // key, the CLI flag, and the discovery subcommand.
    assert!(
        msg.contains("[filters] libraries = [\"all\"]"),
        "TOML guidance missing: {msg}"
    );
    assert!(
        !msg.contains("--library all"),
        "CLI guidance should be gone: {msg}"
    );
    assert!(
        msg.contains("kei list libraries"),
        "discovery guidance missing: {msg}"
    );
}

#[test]
fn notice_fires_with_plural_wording_for_multiple_libraries() {
    let msg = should_notify_shared_libraries(&primary(), 3, false).unwrap();
    assert!(
        msg.contains("3 iCloud shared libraries"),
        "plural 'libraries' wording expected; got: {msg}"
    );
    assert!(
        msg.contains("are being synced"),
        "plural verb 'are' expected; got: {msg}"
    );
}

#[test]
fn notice_suppressed_when_both_user_opted_out_and_already_notified() {
    // Belt-and-braces: every suppression condition stacks correctly.
    assert!(should_notify_shared_libraries(&all_libraries(), 0, true).is_none());
}

#[test]
fn shared_library_notice_recent_check_uses_ttl() {
    let now = 1_800_000_000;
    assert!(shared_library_notice_recently_checked(
        Some(&(now - 60).to_string()),
        now
    ));
    assert!(!shared_library_notice_recently_checked(
        Some(&(now - SHARED_LIBRARY_NOTICE_CHECK_TTL_SECS - 1).to_string()),
        now
    ));
    assert!(!shared_library_notice_recently_checked(
        Some("not-a-ts"),
        now
    ));
}

#[derive(Clone)]
struct CountingSharedLibrarySession {
    shared_calls: Arc<std::sync::atomic::AtomicUsize>,
}

#[async_trait::async_trait]
impl crate::icloud::photos::PhotosSession for CountingSharedLibrarySession {
    async fn post(
        &self,
        url: &str,
        _body: String,
        _headers: &[(&str, &str)],
    ) -> anyhow::Result<serde_json::Value> {
        if url.contains("/shared/zones/list") {
            self.shared_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            return Ok(serde_json::json!({"zones": []}));
        }
        anyhow::bail!("unexpected URL: {url}")
    }

    fn clone_box(&self) -> Box<dyn crate::icloud::photos::PhotosSession> {
        Box::new(self.clone())
    }
}

fn shared_notice_service(
    calls: Arc<std::sync::atomic::AtomicUsize>,
) -> crate::icloud::photos::PhotosService {
    crate::icloud::photos::PhotosService::for_testing(
        Box::new(CountingSharedLibrarySession {
            shared_calls: calls,
        }),
        std::collections::HashMap::new(),
    )
}

#[tokio::test]
async fn shared_library_notice_skips_uncached_dry_run_probe_without_state_db() {
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut service = shared_notice_service(Arc::clone(&calls));

    maybe_notify_shared_libraries(&primary(), &mut service, None).await;

    assert_eq!(
        calls.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "without a state DB the shared-library probe cannot be cached, so dry-run should not pay the API call"
    );
}

#[tokio::test]
async fn shared_library_notice_caches_no_shared_libraries() {
    let db = make_state_db();
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));

    let mut first = shared_notice_service(Arc::clone(&calls));
    maybe_notify_shared_libraries(&primary(), &mut first, Some(db.as_ref())).await;
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);

    let mut second = shared_notice_service(Arc::clone(&calls));
    maybe_notify_shared_libraries(&primary(), &mut second, Some(db.as_ref())).await;

    assert_eq!(
        calls.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "fresh no-shared marker should suppress the next shared-zone listing"
    );
}

// ── find_multi_library_commingle_flags ───────────────────────────
//
// Multi-library sync without a `{library}` token lets same-named
// assets from different zones share an on-disk namespace. The
// `file_match_policy` keeps writes from silently overwriting (the
// default policy adds a `-N` suffix on collision), but the user
// probably wanted zone-disjoint trees. `warn_if_*` emits a startup
// warning; these tests pin the underlying `find_*` truth table.

/// Build a Selection that activates every pass kind. The default
/// `LibrarySelector` is fine — the guard reads only `albums`,
/// `smart_folders`, `unfiled` from the Selection.
fn selection_all_passes_active() -> crate::selection::Selection {
    use crate::selection::{AlbumSelector, LibrarySelector, Selection, SmartFolderSelector};
    Selection {
        albums: AlbumSelector::All {
            excluded: std::collections::BTreeSet::new(),
        },
        albums_explicit: true,
        smart_folders: SmartFolderSelector::All {
            include_sensitive: false,
            excluded: std::collections::BTreeSet::new(),
        },
        smart_folders_explicit: true,
        libraries: LibrarySelector::default(),
        unfiled: true,
    }
}

/// Build a Selection that activates only the unfiled pass.
fn selection_unfiled_only() -> crate::selection::Selection {
    use crate::selection::{AlbumSelector, LibrarySelector, Selection, SmartFolderSelector};
    Selection {
        albums: AlbumSelector::None,
        albums_explicit: false,
        smart_folders: SmartFolderSelector::None,
        smart_folders_explicit: false,
        libraries: LibrarySelector::default(),
        unfiled: true,
    }
}

fn commingle_test_states(
    count: usize,
    selection: &crate::selection::Selection,
) -> Vec<LibraryState> {
    use crate::commands::PassKind;
    use crate::selection::{AlbumSelector, SmartFolderSelector};

    let pass_scope = PassScope {
        include_albums: !matches!(selection.albums, AlbumSelector::None),
        include_smart_folders: !matches!(selection.smart_folders, SmartFolderSelector::None),
        include_unfiled: selection.unfiled,
    };
    (0..count)
        .map(|idx| {
            let zone_name = if idx == 0 {
                "PrimarySync".to_string()
            } else {
                format!("SharedSync-{:08X}", idx)
            };
            let library = crate::icloud::photos::PhotoLibrary::new_stub_with_zone(
                Box::new(crate::test_helpers::MockPhotosSession::new()),
                &zone_name,
            );
            LibraryState {
                library,
                cross_zone_libraries: Vec::new(),
                pass_scope,
                zone_name: zone_name.clone(),
                sync_token_key: format!("sync_token:{zone_name}"),
                plan: crate::commands::AlbumPlan {
                    passes: [
                        pass_scope
                            .include_albums
                            .then(|| make_pass("album", PassKind::Album)),
                        pass_scope
                            .include_smart_folders
                            .then(|| make_pass("smart-folder", PassKind::SmartFolder)),
                        pass_scope
                            .include_unfiled
                            .then(|| make_pass("unfiled", PassKind::Unfiled)),
                    ]
                    .into_iter()
                    .flatten()
                    .collect(),
                },
                plan_is_stale: false,
                plan_needs_refresh: false,
            }
        })
        .collect()
}

#[test]
fn find_multi_library_commingle_flags_short_circuits_under_two_libraries() {
    // Zero or one library never flags any template, regardless of
    // template content or active-pass selection.
    let sel = selection_all_passes_active();
    let states0 = commingle_test_states(0, &sel);
    assert!(
        find_multi_library_commingle_flags(&states0, "%Y/%m/%d", "{album}", "{smart-folder}")
            .is_empty()
    );
    let states1 = commingle_test_states(1, &sel);
    assert!(
        find_multi_library_commingle_flags(&states1, "%Y/%m/%d", "{album}", "{smart-folder}")
            .is_empty()
    );
}

#[test]
fn find_multi_library_commingle_flags_accepts_library_token_in_active_template_only() {
    // CG-7 contract: when every active template carries `{library}`,
    // multi-library is safe. Inactive templates are irrelevant -
    // their pass kind doesn't run.
    let all = selection_all_passes_active();
    let all_states = commingle_test_states(2, &all);
    assert!(
        find_multi_library_commingle_flags(
            &all_states,
            "{library}/%Y/%m/%d",
            "{library}/{album}",
            "{library}/{smart-folder}",
        )
        .is_empty(),
        "every active template carries `{{library}}` - no commingle"
    );

    // When only the unfiled pass is active, the unfiled template is
    // the only one that needs `{library}`. The album / smart-folder
    // templates can be anything because no pass reads them.
    let unfiled = selection_unfiled_only();
    let unfiled_states = commingle_test_states(2, &unfiled);
    assert!(
        find_multi_library_commingle_flags(
            &unfiled_states,
            "{library}/%Y/%m/%d",
            "{album}",
            "{smart-folder}",
        )
        .is_empty(),
        "only unfiled active and its template has `{{library}}`"
    );
}

#[test]
fn find_multi_library_commingle_flags_reports_per_active_pass() {
    // CG-6 contract: only *active* passes whose templates lack
    // `{library}` show up in the missing-flags list.
    //
    // Scenario: --folder-structure-albums '{library}/{album}' (token
    // present, active) + --folder-structure-smart-folders
    // '{smart-folder}' (no token, active because --smart-folder all)
    // + --unfiled false (inactive).
    use crate::selection::{AlbumSelector, LibrarySelector, Selection, SmartFolderSelector};
    let sel = Selection {
        albums: AlbumSelector::All {
            excluded: std::collections::BTreeSet::new(),
        },
        albums_explicit: true,
        smart_folders: SmartFolderSelector::All {
            include_sensitive: false,
            excluded: std::collections::BTreeSet::new(),
        },
        smart_folders_explicit: true,
        libraries: LibrarySelector::default(),
        unfiled: false,
    };
    let states = commingle_test_states(2, &sel);
    let missing = find_multi_library_commingle_flags(
        &states,
        "%Y/%m/%d",
        "{library}/{album}",
        "{smart-folder}",
    );
    assert_eq!(
        missing,
        vec!["--folder-structure-smart-folders"],
        "only the active smart-folder pass with `{{library}}`-less template should be listed"
    );

    // Negative: same templates but smart-folder pass disabled. No
    // active pass lacks `{library}`, so the list is empty.
    let sel_no_smart = Selection {
        albums: AlbumSelector::All {
            excluded: std::collections::BTreeSet::new(),
        },
        albums_explicit: true,
        smart_folders: SmartFolderSelector::None,
        smart_folders_explicit: false,
        libraries: LibrarySelector::default(),
        unfiled: false,
    };
    let states_no_smart = commingle_test_states(2, &sel_no_smart);
    assert!(
        find_multi_library_commingle_flags(
            &states_no_smart,
            "%Y/%m/%d",
            "{library}/{album}",
            "{smart-folder}",
        )
        .is_empty(),
        "smart-folder inactive - its `{{library}}`-less template is irrelevant"
    );
}

#[test]
fn find_multi_library_commingle_flags_ignores_visibility_only_libraries() {
    use crate::commands::PassKind;
    let mut states = commingle_test_states(2, &selection_all_passes_active());
    states[1].plan = crate::commands::AlbumPlan { passes: Vec::new() };
    states[1].pass_scope = PassScope {
        include_albums: true,
        include_smart_folders: true,
        include_unfiled: true,
    };
    states[0].plan = crate::commands::AlbumPlan {
        passes: vec![make_pass("Album A", PassKind::Album)],
    };
    states[0].pass_scope = PassScope {
        include_albums: true,
        include_smart_folders: false,
        include_unfiled: false,
    };
    let missing =
        find_multi_library_commingle_flags(&states, "%Y/%m/%d", "{album}", "{smart-folder}");
    assert!(
        missing.is_empty(),
        "only one library has active passes - visibility-only zones must not trigger commingle warnings"
    );
}

#[test]
fn find_multi_library_commingle_flags_reports_all_missing_when_every_active_template_lacks_token() {
    let sel = selection_all_passes_active();
    let states = commingle_test_states(2, &sel);
    let missing =
        find_multi_library_commingle_flags(&states, "%Y/%m/%d", "{album}", "{smart-folder}");
    assert_eq!(
        missing,
        vec![
            "--folder-structure",
            "--folder-structure-albums",
            "--folder-structure-smart-folders",
        ],
        "every active template lacks `{{library}}` - all three should be listed",
    );
}

#[test]
fn find_multi_library_commingle_flags_reports_with_none_folder_structure_too() {
    // `none` (date hierarchy disabled) is the worst-case commingle:
    // every asset lands directly in the download dir. Still surfaces
    // the unfiled flag so the user knows the namespace is shared.
    let sel = selection_all_passes_active();
    let states = commingle_test_states(5, &sel);
    let missing = find_multi_library_commingle_flags(&states, "none", "{album}", "{smart-folder}");
    assert!(missing.contains(&"--folder-structure"));
}

#[test]
fn find_multi_library_commingle_flags_short_circuits_when_no_passes_active() {
    // --album none + --smart-folder none + --unfiled false: every
    // pass is disabled, resolve_passes returns an empty plan, no
    // path is ever rendered, so multi-library can't commingle even
    // without `{library}` in any template.
    use crate::selection::{AlbumSelector, LibrarySelector, Selection, SmartFolderSelector};
    let sel = Selection {
        albums: AlbumSelector::None,
        albums_explicit: true,
        smart_folders: SmartFolderSelector::None,
        smart_folders_explicit: false,
        libraries: LibrarySelector::default(),
        unfiled: false,
    };
    let states = commingle_test_states(3, &sel);
    assert!(
        find_multi_library_commingle_flags(&states, "%Y/%m/%d", "{album}", "{smart-folder}")
            .is_empty(),
        "no active passes - find_* must report empty"
    );
}

/// CG-6 (2026-05-03 test review): the existing suite asserts the
/// *return value* of `find_multi_library_commingle_flags`, but
/// nothing pins the contract that `warn_if_multi_library_paths_commingle`
/// emits the `library_count` and `missing` lists as structured tracing
/// fields. A future refactor that drops the named args (or replaces
/// them with a positional message) would silently lose operator
/// visibility into commingle scenarios. Pin the field shape so the
/// regression is loud.
#[test]
fn warn_if_multi_library_paths_commingle_emits_structured_fields() {
    let (capture, _guard) = crate::test_helpers::TracingCapture::install();
    let sel = selection_all_passes_active();
    let states = commingle_test_states(3, &sel);
    warn_if_multi_library_paths_commingle(&states, "%Y/%m/%d", "{album}", "{smart-folder}");
    let events = capture.events();
    let warn = events
        .iter()
        .find(|event| {
            event.level == tracing::Level::WARN
                && event
                    .message()
                    .is_some_and(|msg| msg.starts_with("Multi-library sync"))
        })
        .unwrap_or_else(|| panic!("missing commingle warning event: {events:?}"));
    assert_eq!(warn.field("library_count"), Some("3"));
    let missing = warn.field("missing").expect("missing field");
    for flag in [
        "--folder-structure",
        "--folder-structure-albums",
        "--folder-structure-smart-folders",
    ] {
        assert!(
            missing.contains(flag),
            "missing field should include {flag}, got {missing}"
        );
    }
}

/// CG-6 negative: when `{library}` is present in every active
/// template, the warn must NOT fire. Catches the inverse mutation
/// (warn fires unconditionally).
#[tracing_test::traced_test]
#[test]
fn warn_if_multi_library_paths_commingle_silent_when_no_commingle() {
    let sel = selection_all_passes_active();
    let states = commingle_test_states(3, &sel);
    warn_if_multi_library_paths_commingle(
        &states,
        "{library}/%Y/%m/%d",
        "{library}/{album}",
        "{library}/{smart-folder}",
    );
    assert!(
        !logs_contain("library_count="),
        "warn must not fire when every active template carries `{{library}}`"
    );
}

// ── count_passes ────────────────────────────────────────────────────
//
// Pass tally feeds the per-library `Sync plan for library` info line.
// The numbers map directly to API surface (one enumeration per album /
// smart-folder pass + one for unfiled), so a regression that drops a
// category would silently mislead the operator.

fn make_pass(name: &str, kind: crate::commands::PassKind) -> crate::commands::AlbumPass {
    crate::commands::AlbumPass {
        kind,
        album: crate::icloud::photos::PhotoAlbum::stub_for_test(std::sync::Arc::from(name)),
        exclude_ids: std::sync::Arc::new(rustc_hash::FxHashSet::default()),
    }
}

#[test]
fn count_passes_empty_plan_is_all_zero() {
    let plan = crate::commands::AlbumPlan { passes: Vec::new() };
    assert_eq!(count_passes(&plan), (0, 0, false));
}

#[test]
fn count_passes_tallies_each_kind_independently() {
    use crate::commands::PassKind;
    let plan = crate::commands::AlbumPlan {
        passes: vec![
            make_pass("Vacation", PassKind::Album),
            make_pass("Family", PassKind::Album),
            make_pass("Favorites", PassKind::SmartFolder),
            make_pass("PrimarySync", PassKind::Unfiled),
        ],
    };
    assert_eq!(count_passes(&plan), (2, 1, true));
}

#[test]
fn count_passes_unfiled_only_returns_zero_album_zero_smart_folder() {
    use crate::commands::PassKind;
    let plan = crate::commands::AlbumPlan {
        passes: vec![make_pass("PrimarySync", PassKind::Unfiled)],
    };
    assert_eq!(count_passes(&plan), (0, 0, true));
}

#[tokio::test]
async fn refresh_needed_library_plans_filters_to_changed_zones() {
    let mut states = vec![
        make_library_state("PrimarySync", "sync_token:PrimarySync"),
        make_library_state("SharedSync-ABCD", "sync_token:SharedSync-ABCD"),
    ];
    for state in &mut states {
        state.plan_needs_refresh = true;
    }
    let mut changed_zones = rustc_hash::FxHashSet::default();
    changed_zones.insert("PrimarySync".to_string());
    let selection = crate::selection::Selection {
        albums: crate::selection::AlbumSelector::None,
        albums_explicit: false,
        smart_folders: crate::selection::SmartFolderSelector::None,
        smart_folders_explicit: false,
        libraries: crate::selection::LibrarySelector::default(),
        unfiled: false,
    };
    let collection_context = CollectionContext {
        collection_album_names: std::collections::BTreeSet::new(),
        selected_smart_folder_names: Vec::new(),
    };
    let mut failures = 0;

    refresh_needed_library_plans(
        &mut states,
        &selection,
        &collection_context,
        Some(&changed_zones),
        &mut failures,
    )
    .await;

    assert!(
        !states[0].plan_needs_refresh,
        "changed zone should refresh before syncing"
    );
    assert!(
        states[1].plan_needs_refresh,
        "unchanged zone must not refresh albums on this cycle"
    );
    assert_eq!(failures, 0);
}

#[tokio::test]
async fn refresh_needed_library_plans_without_zone_filter_refreshes_every_stale_plan() {
    let mut states = vec![
        make_library_state("PrimarySync", "sync_token:PrimarySync"),
        make_library_state("SharedSync-ABCD", "sync_token:SharedSync-ABCD"),
    ];
    for state in &mut states {
        state.plan_needs_refresh = true;
        state.plan_is_stale = true;
    }
    let selection = crate::selection::Selection {
        albums: crate::selection::AlbumSelector::None,
        albums_explicit: false,
        smart_folders: crate::selection::SmartFolderSelector::None,
        smart_folders_explicit: false,
        libraries: all_libraries(),
        unfiled: false,
    };
    let collection_context = CollectionContext {
        collection_album_names: std::collections::BTreeSet::new(),
        selected_smart_folder_names: Vec::new(),
    };
    let mut failures = 2;

    refresh_needed_library_plans(
        &mut states,
        &selection,
        &collection_context,
        None,
        &mut failures,
    )
    .await;

    assert!(
        states.iter().all(|state| !state.plan_needs_refresh),
        "every stale plan should refresh when no changed-zone precheck filtered the cycle"
    );
    assert!(
        states.iter().all(|state| !state.plan_is_stale),
        "successful refresh should clear stale-plan token gates"
    );
    assert_eq!(failures, 0);
}

fn primary() -> crate::selection::LibrarySelector {
    crate::selection::LibrarySelector::default()
}
