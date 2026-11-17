use super::{
    EDIT_ONE, EDIT_TWO, EditedNaming, ORIGINAL, album_directory_options, assert_bytes,
    cycle_options, db_count, records, server,
};

#[tokio::test]
async fn album_directory_exclusive_binding_renames_future_template_copies() {
    for (template, prefix) in [("{album}", "media"), ("archive/{album}", "media/archive")] {
        let server = server().await;
        let root = tempfile::tempdir().unwrap();
        let mut provider = records(&server, None);
        let mut initial = album_directory_options("Old");
        initial.folder_structure = template;
        cycle_options(
            root.path(),
            provider.clone(),
            false,
            EditedNaming::Suffix,
            initial,
        )
        .await;
        std::fs::write(
            root.path().join(prefix).join("Old/user.txt"),
            b"owned directory user tree",
        )
        .unwrap();
        provider.extend(super::second_date_records(&server));
        let mut options = album_directory_options("Old");
        options.folder_structure = "future/{album}";
        let (result, _) = cycle_options(
            root.path(),
            provider.clone(),
            false,
            EditedNaming::Suffix,
            options,
        )
        .await;
        assert!(result.db_sync_token_advance_safe, "{:?}", result.stats);
        assert_bytes(&root.path().join(prefix).join("Old/IMG_0501.JPG"), ORIGINAL);
        assert_bytes(&root.path().join("media/future/Old/IMG_0502.JPG"), ORIGINAL);
        assert!(!root.path().join("media/future/Old/IMG_0501.JPG").exists());
        std::fs::write(
            root.path().join("media/future/Old/unclaimed.txt"),
            b"unclaimed user tree",
        )
        .unwrap();
        for name in ["New", "Again"] {
            for quiet in [false, true, true] {
                let mut options = album_directory_options(name);
                options.folder_structure = "later/{album}";
                options.expected_due_selection = quiet.then_some(false);
                let (result, _) = cycle_options(
                    root.path(),
                    provider.clone(),
                    quiet,
                    EditedNaming::Suffix,
                    options,
                )
                .await;
                assert!(result.db_sync_token_advance_safe, "{:?}", result.stats);
                assert_bytes(
                    &root
                        .path()
                        .join(prefix)
                        .join(format!("{name}/IMG_0501.JPG")),
                    ORIGINAL,
                );
                assert_bytes(
                    &root.path().join(prefix).join(format!("{name}/user.txt")),
                    b"owned directory user tree",
                );
                assert_bytes(
                    &root
                        .path()
                        .join(format!("media/future/{name}/IMG_0502.JPG")),
                    ORIGINAL,
                );
                assert_bytes(
                    &root.path().join("media/future/Old/unclaimed.txt"),
                    b"unclaimed user tree",
                );
                assert!(!root.path().join("media/later").exists());
            }
        }
    }
}

#[cfg(unix)]
#[tokio::test]
async fn album_directory_native_root_preserves_native_publication_authority() {
    use std::os::unix::ffi::OsStringExt;
    let server = server().await;
    let temp = tempfile::tempdir().unwrap();
    let root = temp
        .path()
        .join(std::ffi::OsString::from_vec(b"native-\xff".to_vec()));
    std::fs::create_dir(&root).unwrap();
    let provider = records(&server, None);
    for (name, quiet) in [("Old", false), ("New", false), ("New", true), ("New", true)] {
        let (result, _) = cycle_options(
            &root,
            provider.clone(),
            quiet,
            EditedNaming::Suffix,
            album_directory_options(name),
        )
        .await;
        assert!(result.db_sync_token_advance_safe, "{:?}", result.stats);
        let path = root.join("media").join(name).join("IMG_0501.JPG");
        assert_bytes(&path, ORIGINAL);
        let conn = rusqlite::Connection::open(root.join("state.db")).unwrap();
        let expected = crate::state::db::provider_selection::SelectionPath::from_path(&path)
            .key()
            .unwrap();
        assert!(conn.query_row::<bool,_,_>("SELECT EXISTS(SELECT 1 FROM provider_active_destinations WHERE path=?1 AND current_publication=1)", [expected], |row| row.get(0)).unwrap());
    }
    assert!(!root.join("media/Old").exists());
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn album_directory_additional_routes_recover_after_installation() {
    let server = server().await;
    let root = tempfile::tempdir().unwrap();
    let mut provider = records(&server, None);
    cycle_options(
        root.path(),
        provider.clone(),
        false,
        EditedNaming::Suffix,
        album_directory_options("Old"),
    )
    .await;
    std::fs::write(
        root.path().join("media/Old/user.txt"),
        b"retained user tree",
    )
    .unwrap();
    provider.extend(super::second_date_records(&server));
    let mut options = album_directory_options("Old");
    options.folder_structure = "future/{album}";
    cycle_options(
        root.path(),
        provider.clone(),
        false,
        EditedNaming::Suffix,
        options,
    )
    .await;
    std::fs::write(
        root.path().join("fixture-owner"),
        b"kei-synthetic-process-death",
    )
    .unwrap();
    std::fs::write(
        root.path().join("additional-projection-fixture"),
        b"external album copy",
    )
    .unwrap();
    super::kill_album_child(root.path(), "album-additional-directory-published").await;
    for quiet in [false, true, true] {
        let mut options = album_directory_options("New");
        options.folder_structure = "later/{album}";
        options.expected_due_selection = quiet.then_some(false);
        let (result, _) = cycle_options(
            root.path(),
            provider.clone(),
            quiet,
            EditedNaming::Suffix,
            options,
        )
        .await;
        assert!(result.db_sync_token_advance_safe, "{:?}", result.stats);
        assert_bytes(&root.path().join("media/New/IMG_0501.JPG"), ORIGINAL);
        assert_bytes(
            &root.path().join("media/New/user.txt"),
            b"retained user tree",
        );
        assert_bytes(&root.path().join("media/future/New/IMG_0502.JPG"), ORIGINAL);
        assert!(!root.path().join("media/later").exists());
    }
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn album_directory_interrupted_move_follows_a_second_provider_rename() {
    for resumed_name in ["Third", "Old"] {
        for (template, point, naming) in [
            ("{album}", "album-directory-planned", EditedNaming::Primary),
            ("{album}", "album-directory-durable", EditedNaming::Primary),
            (
                "%Y/photos-{album}",
                "album-projection-planned",
                EditedNaming::Suffix,
            ),
            (
                "%Y/photos-{album}",
                "album-projection-directory-published",
                EditedNaming::Suffix,
            ),
        ] {
            let server = server().await;
            let root = tempfile::tempdir().unwrap();
            let provider = records(&server, None);
            let mut options = album_directory_options("Old");
            options.folder_structure = template;
            cycle_options(root.path(), provider.clone(), false, naming, options).await;
            std::fs::write(
                root.path().join("fixture-owner"),
                b"kei-synthetic-process-death",
            )
            .unwrap();
            if template.starts_with('%') {
                std::fs::write(root.path().join("projection-fixture"), b"projection").unwrap();
            }
            super::kill_album_child(root.path(), point).await;
            for quiet in [false, true, true] {
                let mut options = album_directory_options(resumed_name);
                options.folder_structure = template;
                options.expected_due_selection = quiet.then_some(false);
                let (result, _) =
                    cycle_options(root.path(), provider.clone(), quiet, naming, options).await;
                assert!(
                    result.db_sync_token_advance_safe,
                    "{point}: {:?}",
                    result.stats
                );
                let leaf = if resumed_name == "Old" && !template.starts_with('%') {
                    "Old (1)"
                } else {
                    resumed_name
                };
                let destination = if template.starts_with('%') {
                    format!("media/2023/photos-{leaf}/IMG_0501.JPG")
                } else {
                    format!("media/{leaf}/IMG_0501.JPG")
                };
                assert_bytes(&root.path().join(destination), ORIGINAL);
                assert_eq!(
                    db_count(
                        root.path(),
                        "SELECT COUNT(*) FROM primary_layout_operations WHERE policy='album-directory-v1' AND phase NOT IN ('committed','cancelled')"
                    ),
                    0,
                    "{point}/{resumed_name}: recovery must finish its journal"
                );
            }
        }
    }
}

#[tokio::test]
async fn album_directory_native_move_accepts_new_current_resource() {
    let server = server().await;
    let root = tempfile::tempdir().unwrap();
    cycle_options(
        root.path(),
        records(&server, Some(("edit-one", EDIT_ONE))),
        false,
        EditedNaming::Suffix,
        album_directory_options("Old"),
    )
    .await;
    let current = records(&server, Some(("edit-two", EDIT_TWO)));
    for quiet in [false, true, true] {
        let (result, _) = cycle_options(
            root.path(),
            current.clone(),
            quiet,
            EditedNaming::Suffix,
            album_directory_options("New"),
        )
        .await;
        assert!(result.db_sync_token_advance_safe, "{:?}", result.stats);
        assert_bytes(&root.path().join("media/New/IMG_0501_edited.JPG"), EDIT_TWO);
    }
    assert_eq!(
        db_count(
            root.path(),
            "SELECT COUNT(*) FROM provider_active_destinations WHERE preserved_local IS NOT NULL"
        ),
        0
    );
}

#[tokio::test]
async fn album_directory_preserved_native_copy_stays_out_of_user_entry_moves() {
    let server = server().await;
    let root = tempfile::tempdir().unwrap();
    cycle_options(
        root.path(),
        records(&server, Some(("edit-one", EDIT_ONE))),
        false,
        EditedNaming::Suffix,
        album_directory_options("Old"),
    )
    .await;
    let old = root.path().join("media/Old/IMG_0501_edited.JPG");
    std::fs::write(&old, b"preserved native local edit").unwrap();
    let current = records(&server, Some(("edit-two", EDIT_TWO)));
    let (preserved, _) = cycle_options(
        root.path(),
        current.clone(),
        false,
        EditedNaming::Suffix,
        album_directory_options("Old"),
    )
    .await;
    assert!(
        preserved.db_sync_token_advance_safe,
        "{:?}",
        preserved.stats
    );
    assert!(
        db_count(
            root.path(),
            "SELECT COUNT(*) FROM provider_active_destinations WHERE preserved_local IS NOT NULL"
        ) > 0
    );
    for quiet in [false, true, true] {
        let (result, _) = cycle_options(
            root.path(),
            current.clone(),
            quiet,
            EditedNaming::Suffix,
            album_directory_options("New"),
        )
        .await;
        assert!(result.db_sync_token_advance_safe, "{:?}", result.stats);
        assert_bytes(&old, b"preserved native local edit");
        assert_bytes(&root.path().join("media/New/IMG_0501_edited.JPG"), EDIT_TWO);
    }
}

#[tokio::test]
async fn album_directory_landed_handoff_preserves_changes_and_second_rename() {
    for replay_name in ["New", "Third"] {
        let server = server().await;
        let root = tempfile::tempdir().unwrap();
        let provider = records(&server, Some(("edit-one", EDIT_ONE)));
        cycle_options(
            root.path(),
            provider.clone(),
            false,
            EditedNaming::Suffix,
            album_directory_options("Old"),
        )
        .await;
        let conn = rusqlite::Connection::open(root.path().join("state.db")).unwrap();
        conn.execute_batch("CREATE TRIGGER stop_new_selection BEFORE INSERT ON provider_active_generations BEGIN SELECT RAISE(ABORT,'selection fault after directory landing'); END;").unwrap();
        drop(conn);
        let attempted = super::try_cycle_options(
            root.path(),
            provider.clone(),
            false,
            EditedNaming::Suffix,
            album_directory_options("New"),
        )
        .await;
        if let Ok((result, _)) = attempted {
            assert!(!result.db_sync_token_advance_safe);
        }
        assert!(!root.path().join("media/Old").exists());
        let landed = root.path().join("media/New/IMG_0501_edited.JPG");
        assert_bytes(&landed, EDIT_ONE);
        assert!(
            db_count(
                root.path(),
                "SELECT COUNT(*) FROM primary_layout_members WHERE json_extract(CAST(evidence AS TEXT),'$.native.settled')=0"
            ) > 0
        );
        std::fs::write(&landed, b"user changed landed media").unwrap();
        let user_file = root.path().join("media/New/user.txt");
        std::fs::write(&user_file, b"user retained entry").unwrap();
        let conn = rusqlite::Connection::open(root.path().join("state.db")).unwrap();
        conn.execute_batch("DROP TRIGGER stop_new_selection")
            .unwrap();
        drop(conn);
        for quiet in [false, true, true] {
            let (result, _) = cycle_options(
                root.path(),
                provider.clone(),
                quiet,
                EditedNaming::Suffix,
                album_directory_options(replay_name),
            )
            .await;
            assert!(
                result.db_sync_token_advance_safe,
                "{replay_name}: {:?}",
                result.stats
            );
            assert_bytes(&landed, b"user changed landed media");
            assert_bytes(&user_file, b"user retained entry");
            if replay_name == "Third" {
                assert_bytes(
                    &root.path().join("media/Third/IMG_0501_edited.JPG"),
                    EDIT_ONE,
                );
            }
        }
    }
}

#[tokio::test]
async fn album_directory_mixed_projection_retains_independent_smart_copy() {
    let server = server().await;
    let root = tempfile::tempdir().unwrap();
    let provider = records(&server, None);
    for (name, quiet) in [("Old", false), ("New", false), ("New", true), ("New", true)] {
        let mut options = album_directory_options(name);
        options.folder_structure = "%Y/{album}";
        options.shared_folder_structure = Some("%Y/Old");
        let (result, _) = cycle_options(
            root.path(),
            provider.clone(),
            quiet,
            EditedNaming::Suffix,
            options,
        )
        .await;
        assert!(result.db_sync_token_advance_safe, "{:?}", result.stats);
        let conn = rusqlite::Connection::open(root.path().join("state.db")).unwrap();
        let paths: Vec<String> = conn.prepare("SELECT DISTINCT compat_path FROM provider_active_destinations WHERE current_publication=1 ORDER BY compat_path").unwrap().query_map([], |row| row.get(0)).unwrap().collect::<Result<_,_>>().unwrap();
        assert!(
            paths.iter().any(|path| path.contains("/2023/Old/")),
            "{paths:?}"
        );
        if name == "New" {
            assert!(
                paths.iter().any(|path| path.contains("/2023/New/")),
                "{paths:?}"
            );
        }
        for path in paths {
            assert_bytes(std::path::Path::new(&path), ORIGINAL);
        }
    }
}

#[tokio::test]
async fn album_directory_native_move_settles_current_authority_once() {
    let server = server().await;
    let root = tempfile::tempdir().unwrap();
    let initial = records(&server, Some(("edit-one", EDIT_ONE)));
    let (first, _) = cycle_options(
        root.path(),
        initial.clone(),
        false,
        EditedNaming::Suffix,
        album_directory_options("Old"),
    )
    .await;
    assert!(first.db_sync_token_advance_safe, "{:?}", first.stats);
    let old = root.path().join("media/Old/IMG_0501_edited.JPG");
    assert_bytes(&old, EDIT_ONE);
    let current = root.path().join("media/New/IMG_0501_edited.JPG");
    let (renamed, _) = cycle_options(
        root.path(),
        initial.clone(),
        false,
        EditedNaming::Suffix,
        album_directory_options("New"),
    )
    .await;
    assert!(renamed.db_sync_token_advance_safe, "{:?}", renamed.stats);
    assert!(!old.exists());
    assert_bytes(&current, EDIT_ONE);
    let conn = rusqlite::Connection::open(root.path().join("state.db")).unwrap();
    let authority: String = conn.query_row("SELECT compat_path FROM provider_active_destinations WHERE version_size='adjusted' AND current_publication=1", [], |row| row.get(0)).unwrap();
    assert_eq!(authority, current.to_str().unwrap());
    assert_eq!(
        db_count(
            root.path(),
            "SELECT COUNT(*) FROM primary_layout_members WHERE json_extract(CAST(evidence AS TEXT),'$.native.settled')=0"
        ),
        0
    );
    drop(conn);

    let changed = records(&server, Some(("edit-two", EDIT_TWO)));
    let (updated, _) = cycle_options(
        root.path(),
        changed.clone(),
        false,
        EditedNaming::Suffix,
        album_directory_options("New"),
    )
    .await;
    assert!(updated.db_sync_token_advance_safe, "{:?}", updated.stats);
    assert_bytes(&current, EDIT_TWO);
    std::fs::write(&current, EDIT_ONE).unwrap();
    for quiet in [false, true, true] {
        let (result, _) = cycle_options(
            root.path(),
            changed.clone(),
            quiet,
            EditedNaming::Suffix,
            album_directory_options("New"),
        )
        .await;
        assert!(result.db_sync_token_advance_safe, "{:?}", result.stats);
        assert_bytes(&current, EDIT_ONE);
        assert_bytes(&root.path().join("media/New/IMG_0501.JPG"), ORIGINAL);
    }
    assert!(
        db_count(
            root.path(),
            "SELECT COUNT(*) FROM provider_active_destinations WHERE version_size='adjusted' AND preserved_local IS NOT NULL"
        ) > 0
    );
}
