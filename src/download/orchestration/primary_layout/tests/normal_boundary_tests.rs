//! Normal-sync boundaries around existing layout ownership.
use super::{
    DownloadConfig, EditedNaming, PREPARED, durable_state, prepared_layout,
    prepared_layout_with_refresh, recover,
};
use crate::download::baseline_matching::BaselineMatch;
use crate::download::planner::TaskPlanner;
use crate::state::SqliteStateDb;
use crate::test_helpers::TestPhotoAsset;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn v027_absent_managed_claim_is_uncertain_for_same_and_other_identity_after_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state.sqlite");
    let root = dir.path().join("media");
    std::fs::create_dir(&root).unwrap();
    let db = Arc::new(SqliteStateDb::open(&state).await.unwrap());
    let op = prepared_layout(&db, &root, "claimed.JPG").await;
    let destination = op.members[0].destination.as_ref().unwrap().to_path();
    let stage = op.members[0].stage.as_ref().unwrap().to_path();
    assert!(!destination.exists());
    let before = durable_state(&db);
    drop(db);
    for _ in 0..2 {
        let db = Arc::new(SqliteStateDb::open(&state).await.unwrap());
        let config = DownloadConfig {
            directory: Arc::from(root.as_path()),
            folder_structure: String::new(),
            edited_naming: EditedNaming::Primary,
            primary_layout_active: false,
            primary_layout_preview: false,
            state_db: Some(db.clone()),
            ..DownloadConfig::test_default()
        };
        for identity in ["claimed.JPG", "another-child"] {
            let asset = TestPhotoAsset::new(identity)
                .filename("claimed.JPG")
                .orig_size(PREPARED.len() as u64)
                .orig_checksum("synthetic-provider-checksum")
                .build();
            let mut planner = TaskPlanner::for_download(config.state_db.as_deref())
                .await
                .unwrap();
            let plan = planner.plan_download_asset(&asset, &config).await.unwrap();
            assert_eq!(plan.tasks.len(), 1);
            assert!(
                matches!(
                    planner
                        .match_existing(&asset, &config, &plan.tasks[0])
                        .await,
                    BaselineMatch::Uncertain(_)
                ),
                "an absent managed slot remains owned, including for its own current identity"
            );
            assert!(
                !destination.exists(),
                "matching cannot publish into the old claim"
            );
            assert_eq!(std::fs::read(&stage).unwrap(), PREPARED);
            assert!(
                durable_state(&db) == before,
                "matching must not adopt, release or replace the retained operation"
            );
        }
    }
}

#[tokio::test]
async fn v027_prepared_layout_recovery_finishes_without_current_pass_or_layout() {
    for phase in ["prepared", "preserved"] {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("state.sqlite");
        let root = dir.path().join("old-root");
        std::fs::create_dir(&root).unwrap();
        let old = root.join("untouched.JPG");
        std::fs::write(&old, b"existing unrelated media").unwrap();
        let old_mtime = std::fs::metadata(&old).unwrap().modified().unwrap();
        let db = Arc::new(SqliteStateDb::open(&state).await.unwrap());
        let mut op = prepared_layout(&db, &root, "retained.JPG").await;
        if phase == "preserved" {
            op.phase = phase.into();
            db.save_primary_layout(op.clone()).await.unwrap();
        }
        let destination = op.members[0].destination.as_ref().unwrap().to_path();
        drop(db);
        let mut completed = None;
        let mut mtime = None;
        for _ in 0..2 {
            let db = Arc::new(SqliteStateDb::open(&state).await.unwrap());
            let config = DownloadConfig {
                directory: dir.path().join("new-root").into(),
                folder_structure: "changed-template".into(),
                state_db: Some(db.clone()),
                ..DownloadConfig::test_default()
            };
            recover(
                &reqwest::Client::new(),
                &[],
                &config,
                &CancellationToken::new(),
            )
            .await
            .unwrap();
            assert_eq!(std::fs::read(&destination).unwrap(), PREPARED);
            assert!(
                db.primary_layout_operations(op.source_library.clone())
                    .await
                    .unwrap()
                    .is_empty()
            );
            let current = durable_state(&db);
            let current_mtime = std::fs::metadata(&destination).unwrap().modified().unwrap();
            if let Some(previous) = &completed {
                assert!(&current == previous);
            }
            if let Some(previous) = mtime {
                assert_eq!(current_mtime, previous);
            }
            completed = Some(current);
            mtime = Some(current_mtime);
            assert_eq!(std::fs::read(&old).unwrap(), b"existing unrelated media");
            assert_eq!(
                std::fs::metadata(&old).unwrap().modified().unwrap(),
                old_mtime
            );
            assert!(!config.directory.exists());
        }
    }
}

#[tokio::test]
async fn v027_publishing_layout_recovery_finishes_recorded_operation_without_current_replan() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state.sqlite");
    let root = dir.path().join("old-root");
    std::fs::create_dir(&root).unwrap();
    let db = Arc::new(SqliteStateDb::open(&state).await.unwrap());
    let mut op = prepared_layout(&db, &root, "publishing.JPG").await;
    // The existing operation has durable prepared bytes and crosses its
    // publication boundary; reopening simulates stopping before its first link.
    // This fixture has no preexisting member to preserve; the real prepared
    // file receipt is already stored. Traverse the owner's required preserved
    // phase rather than skipping its publication precondition.
    assert!(
        op.members
            .iter()
            .all(|member| member.old.is_none() && member.prepared.is_some())
    );
    op.phase = "preserved".into();
    db.save_primary_layout(op.clone()).await.unwrap();
    op.phase = "publishing".into();
    db.save_primary_layout(op.clone()).await.unwrap();
    let destination = op.members[0].destination.as_ref().unwrap().to_path();
    drop(db);
    let mut completed = None;
    let mut published_mtime = None;
    for _ in 0..2 {
        let db = Arc::new(SqliteStateDb::open(&state).await.unwrap());
        let config = DownloadConfig {
            directory: dir.path().join("new-root").into(),
            folder_structure: "changed-template".into(),
            state_db: Some(db.clone()),
            ..DownloadConfig::test_default()
        };
        recover(
            &reqwest::Client::new(),
            &[],
            &config,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(std::fs::read(&destination).unwrap(), PREPARED);
        assert!(
            db.primary_layout_operations(op.source_library.clone())
                .await
                .unwrap()
                .is_empty()
        );
        let rows = durable_state(&db);
        let mtime = std::fs::metadata(&destination).unwrap().modified().unwrap();
        if let Some(previous) = &completed {
            assert!(
                &rows == previous,
                "completed operation changed on quiet reopen"
            );
        }
        if let Some(previous) = published_mtime {
            assert_eq!(mtime, previous);
        }
        completed = Some(rows);
        published_mtime = Some(mtime);
        assert!(
            !config.directory.exists(),
            "completion cannot begin a new current-config layout"
        );
    }
}

fn recorded_recovery_pass() -> crate::commands::AlbumPass {
    #[derive(Clone, Debug)]
    struct NoProvider;
    #[async_trait::async_trait]
    impl crate::icloud::photos::PhotosSession for NoProvider {
        async fn post(
            &self,
            _url: &str,
            _body: String,
            _headers: &[(&str, &str)],
        ) -> anyhow::Result<serde_json::Value> {
            panic!("recorded recovery cannot query or replan current provider state")
        }
        fn clone_box(&self) -> Box<dyn crate::icloud::photos::PhotosSession> {
            Box::new(self.clone())
        }
    }
    crate::commands::AlbumPass {
        kind: crate::commands::PassKind::Unfiled,
        album: crate::download::orchestration::test_support::album_with_session(
            "PrimarySync",
            "",
            Box::new(NoProvider),
        ),
        exclude_ids: Default::default(),
    }
}

#[tokio::test]
async fn v027_wrapper_prepared_layout_completes_across_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state.sqlite");
    let root = dir.path().join("old-root");
    std::fs::create_dir(&root).unwrap();
    let db = Arc::new(SqliteStateDb::open(&state).await.unwrap());
    let op = prepared_layout(&db, &root, "retained.JPG").await;
    let destination = op.members[0].destination.as_ref().unwrap().to_path();
    drop(db);
    let mut completed = None;
    for _ in 0..2 {
        let db = Arc::new(SqliteStateDb::open(&state).await.unwrap());
        let config = DownloadConfig {
            directory: dir.path().join("new-root").into(),
            folder_structure: "changed-template".into(),
            state_db: Some(db.clone()),
            ..DownloadConfig::test_default()
        };
        let result = crate::download::recover_recorded_sources(
            &reqwest::Client::new(),
            &[recorded_recovery_pass()],
            Arc::new(config.clone()),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(result.complete);
        assert_eq!(std::fs::read(&destination).unwrap(), PREPARED);
        let current = durable_state(&db);
        if let Some(previous) = &completed {
            assert!(&current == previous);
        }
        completed = Some(current);
        assert!(!config.directory.exists());
    }
}

#[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
#[tokio::test]
async fn embedded_attempt_cleanup_retains_unknown_entry_and_only_removes_owned_input() {
    let root = tempfile::tempdir().unwrap();
    let original = root.path().join("original.jpg");
    std::fs::write(&original, b"synthetic owned input").unwrap();
    let stage = root.path().join("stages/target.jpg");
    let scratch = super::super::EmbeddedScratch::create(root.path(), &stage, None)
        .await
        .unwrap();
    let source = crate::download::file::primary_layout::snapshot(root.path(), &original)
        .await
        .unwrap()
        .unwrap();
    let input_path = scratch.input.path().to_path_buf();
    let input = crate::download::file::primary_layout::copy_verified_between_roots(
        root.path(),
        &original,
        &scratch.copy_root,
        &input_path,
        &source,
    )
    .await
    .unwrap();
    let marker = input_path.parent().unwrap().join("unknown-marker");
    std::fs::write(&marker, b"keep this entry").unwrap();
    assert!(scratch.cleanup(&input).await.is_err());
    assert_eq!(std::fs::read(&marker).unwrap(), b"keep this entry");
    assert!(!input_path.exists());
    assert!(!stage.exists());
    assert_eq!(std::fs::read(&original).unwrap(), b"synthetic owned input");
    std::fs::remove_file(&marker).unwrap();
    std::fs::remove_dir(marker.parent().unwrap()).unwrap();
}

#[tokio::test]
async fn embedded_private_copy_crosses_confined_roots_without_adopting_collision() {
    let archive = tempfile::tempdir().unwrap();
    let scratch = tempfile::tempdir().unwrap();
    let original = archive.path().join("original.jpg");
    let input = scratch.path().join("input.jpg");
    std::fs::write(&original, b"synthetic source").unwrap();
    let source = crate::download::file::primary_layout::snapshot(archive.path(), &original)
        .await
        .unwrap()
        .unwrap();
    let copied = crate::download::file::primary_layout::copy_verified_between_roots(
        archive.path(),
        &original,
        scratch.path(),
        &input,
        &source,
    )
    .await
    .unwrap();
    assert_ne!(copied.identity, source.identity);
    assert_eq!(std::fs::read(&input).unwrap(), b"synthetic source");
    let denied = scratch.path().join("outside-source.jpg");
    assert!(
        crate::download::file::primary_layout::copy_verified_between_roots(
            scratch.path(),
            &original,
            scratch.path(),
            &denied,
            &source,
        )
        .await
        .is_err()
    );
    assert!(!denied.exists());
    assert!(
        crate::download::file::primary_layout::copy_verified_between_roots(
            archive.path(),
            &original,
            scratch.path(),
            &input,
            &source
        )
        .await
        .is_err()
    );
    assert_eq!(std::fs::read(&original).unwrap(), b"synthetic source");
    assert_eq!(std::fs::read(&input).unwrap(), b"synthetic source");
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn embedded_planning_scratch_rejects_replaced_archive_root() {
    let parent = tempfile::tempdir().unwrap();
    let archive = parent.path().join("archive");
    let displaced = parent.path().join("displaced");
    std::fs::create_dir(&archive).unwrap();
    let stage = archive.join("stage.jpg");
    let stage_cap = crate::fs_util::ConfinedPath::open(
        &archive,
        &stage,
        crate::fs_util::ConfinedParents::Existing,
    )
    .unwrap();
    std::fs::rename(&archive, &displaced).unwrap();
    std::fs::create_dir(&archive).unwrap();
    std::fs::write(archive.join("foreign-marker"), b"foreign bytes").unwrap();
    assert!(
        super::super::EmbeddedScratch::create(&archive, &stage, Some(stage_cap))
            .await
            .is_err()
    );
    assert_eq!(std::fs::read_dir(&archive).unwrap().count(), 1);
    assert_eq!(std::fs::read_dir(&displaced).unwrap().count(), 0);
    assert_eq!(
        std::fs::read(archive.join("foreign-marker")).unwrap(),
        b"foreign bytes"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn embedded_final_copy_rejects_replaced_archive_root() {
    let parent = tempfile::tempdir().unwrap();
    let archive = parent.path().join("archive");
    let displaced = parent.path().join("displaced");
    let scratch = tempfile::tempdir().unwrap();
    std::fs::create_dir(&archive).unwrap();
    let stage = archive.join("stage.jpg");
    let output = scratch.path().join("output.jpg");
    std::fs::write(&output, b"synthetic toolkit output").unwrap();
    let expected = crate::download::file::primary_layout::snapshot(scratch.path(), &output)
        .await
        .unwrap()
        .unwrap();
    let destination = crate::fs_util::ConfinedPath::open(
        &archive,
        &stage,
        crate::fs_util::ConfinedParents::Existing,
    )
    .unwrap();
    std::fs::rename(&archive, &displaced).unwrap();
    std::fs::create_dir(&archive).unwrap();
    let marker = archive.join("foreign-marker");
    std::fs::write(&marker, b"foreign bytes").unwrap();
    assert!(
        crate::download::file::primary_layout::copy_verified_to_confined(
            scratch.path(),
            &output,
            destination,
            &expected
        )
        .await
        .is_err()
    );
    assert!(!stage.exists());
    assert!(!displaced.join("stage.jpg").exists());
    assert_eq!(std::fs::read(marker).unwrap(), b"foreign bytes");
}

#[cfg(windows)]
#[test]
fn embedded_stage_capability_prevents_archive_parent_rename() {
    let parent = tempfile::tempdir().unwrap();
    let archive = parent.path().join("archive");
    let displaced = parent.path().join("displaced");
    std::fs::create_dir(&archive).unwrap();
    let stage = archive.join("stage.jpg");
    let mut cap = crate::fs_util::ConfinedPath::open(
        &archive,
        &stage,
        crate::fs_util::ConfinedParents::Existing,
    )
    .unwrap();
    cap.pin_windows_namespace().unwrap();
    assert!(std::fs::rename(&archive, &displaced).is_err());
    assert!(archive.is_dir());
    assert!(!displaced.exists());
    drop(cap);
    std::fs::rename(&archive, &displaced).unwrap();
}

#[cfg(windows)]
#[test]
fn embedded_stage_capability_prevents_nested_parent_rename() {
    let parent = tempfile::tempdir().unwrap();
    let archive = parent.path().join("archive");
    let nested = archive.join("nested");
    let displaced = archive.join("displaced");
    std::fs::create_dir_all(&nested).unwrap();
    let stage = nested.join("stage.jpg");
    let mut cap = crate::fs_util::ConfinedPath::open(
        &archive,
        &stage,
        crate::fs_util::ConfinedParents::Existing,
    )
    .unwrap();
    cap.pin_windows_namespace().unwrap();
    assert!(std::fs::rename(&nested, &displaced).is_err());
    assert!(nested.is_dir());
    assert!(!displaced.exists());
    drop(cap);
    std::fs::rename(&nested, &displaced).unwrap();
}

#[cfg(windows)]
#[test]
fn embedded_stage_capability_prevents_cross_process_parent_rename() {
    use std::process::Command;
    if let Some(from) = std::env::var_os("KEI_TEST_RENAME_FROM") {
        let to = std::env::var_os("KEI_TEST_RENAME_TO").unwrap();
        let blocked = std::env::var_os("KEI_TEST_RENAME_BLOCKED").unwrap() == "yes";
        assert_eq!(std::fs::rename(from, to).is_err(), blocked);
        return;
    }
    let parent = tempfile::tempdir().unwrap();
    let archive = parent.path().join("archive");
    let displaced = parent.path().join("displaced");
    std::fs::create_dir(&archive).unwrap();
    let stage = archive.join("stage.jpg");
    let mut cap = crate::fs_util::ConfinedPath::open(
        &archive,
        &stage,
        crate::fs_util::ConfinedParents::Existing,
    )
    .unwrap();
    cap.pin_windows_namespace().unwrap();
    let try_rename = |blocked: bool| {
        Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "download::orchestration::primary_layout::tests::normal_boundary_tests::embedded_stage_capability_prevents_cross_process_parent_rename", "--nocapture"])
            .env("KEI_TEST_RENAME_FROM", &archive)
            .env("KEI_TEST_RENAME_TO", &displaced)
            .env("KEI_TEST_RENAME_BLOCKED", if blocked { "yes" } else { "no" })
            .status().unwrap().success()
    };
    assert!(try_rename(true));
    assert!(archive.is_dir());
    assert!(!displaced.exists());
    drop(cap);
    assert!(try_rename(false));
    assert!(!archive.exists());
    assert!(displaced.is_dir());
}

#[cfg(all(feature = "xmp", unix))]
#[tokio::test]
async fn embedded_sidecar_rejects_replaced_archive_after_planning() {
    let parent = tempfile::tempdir().unwrap();
    let archive = parent.path().join("archive");
    let displaced = parent.path().join("displaced");
    std::fs::create_dir(&archive).unwrap();
    let stage = archive.join("stage.jpg");
    std::fs::write(&stage, b"synthetic prepared stage").unwrap();
    let stage_cap = crate::fs_util::ConfinedPath::open(
        &archive,
        &stage,
        crate::fs_util::ConfinedParents::Existing,
    )
    .unwrap();
    let target = stage_cap.sibling(&archive.join("stage.jpg.xmp")).unwrap();
    let write = crate::download::metadata::MetadataWrite {
        rating: Some(3),
        ..Default::default()
    };
    std::fs::rename(&archive, &displaced).unwrap();
    std::fs::create_dir(&archive).unwrap();
    let marker = archive.join("foreign-marker");
    std::fs::write(&marker, b"foreign bytes").unwrap();
    let result = crate::download::metadata::create_layout_sidecar(
        &archive, target, None, &write, ".kei-tmp",
    )
    .await;
    assert!(result.is_err());
    assert!(!archive.join("stage.jpg.xmp").exists());
    assert!(!displaced.join("stage.jpg.xmp").exists());
    assert_eq!(std::fs::read(marker).unwrap(), b"foreign bytes");
}

#[cfg(feature = "xmp")]
#[tokio::test]
async fn v027_recorded_prepared_refresh_replays_metadata_and_preserves_sidecar() {
    use crate::state::DownloadStateStore as _;
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state.sqlite");
    let root = dir.path().join("old-root");
    std::fs::create_dir(&root).unwrap();
    let db = Arc::new(SqliteStateDb::open(&state).await.unwrap());
    let media = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/data/media/metadata.jpg"
    ));
    let asset = TestPhotoAsset::new("refresh-owned")
        .filename("refresh.JPG")
        .item_type("public.jpeg")
        .orig_file_type("public.jpeg")
        .orig_size(media.len() as u64)
        .orig_checksum("synthetic-provider-checksum")
        .orig_url("http://127.0.0.1/unused")
        .favorite(true)
        .build()
        .with_source_zone("PrimarySync".into());
    let mut config = DownloadConfig {
        directory: Arc::from(root.as_path()),
        folder_structure: String::new(),
        edited_naming: EditedNaming::Primary,
        refresh_metadata: true,
        state_db: Some(db.clone()),
        ..DownloadConfig::test_default()
    };
    config.metadata.xmp_sidecar = true;
    let derived = crate::download::filter::derive_expected_paths(&asset, &config).remove(0);
    std::fs::write(&derived.path, media).unwrap();
    let old_sidecar = b"<x:xmpmeta xmlns:x='adobe:ns:meta/'><rdf:RDF xmlns:rdf='http://www.w3.org/1999/02/22-rdf-syntax-ns#'><rdf:Description rdf:about='' xmlns:xmp='http://ns.adobe.com/xap/1.0/' xmp:Rating='4'/></rdf:RDF></x:xmpmeta>";
    std::fs::write(super::sidecar(&derived.path), old_sidecar).unwrap();
    let task = super::super::task_for(
        &asset,
        &config,
        &derived.url,
        &derived.checksum,
        derived.size,
        derived.version_size,
        &derived.path,
    );
    let record = crate::download::planner::pending_record_for_task(&config, &asset, &task);
    db.upsert_seen(&record).await.unwrap();
    let checksum = super::digest(media);
    db.mark_verified_download(
        "PrimarySync",
        asset.state_id(),
        derived.version_size.as_str(),
        &derived.path,
        &checksum,
        Some(&checksum),
        false,
    )
    .await
    .unwrap();
    let checked_config = config.check_ownership().await.unwrap();
    drop(config);
    let config = checked_config;
    assert!(config.allows_existing_mutation());
    let session = super::LayoutSession::load(db.as_ref()).await.unwrap();
    let mut op = super::plan(
        &asset,
        &config,
        &session,
        None,
        super::family(&asset, &config).unwrap(),
        super::decision(&asset, &config).unwrap(),
        true,
    )
    .await
    .unwrap();
    assert!(op.refresh_metadata);
    super::super::prepare(
        &reqwest::Client::new(),
        &asset,
        &config,
        &session,
        &mut op,
        &CancellationToken::new(),
        &mut super::LayoutOutcome::default(),
    )
    .await
    .unwrap();
    assert_eq!(op.phase, "planned");
    assert!(
        op.members
            .iter()
            .all(|member| member.stage.is_none() || member.prepared.is_some())
    );
    let preserved = op.members[0].preserved_path.as_ref().unwrap().to_path();
    db.acquire_lock("inject prepared phase failure").unwrap().execute_batch(
        "CREATE TRIGGER fail_refresh_prepared BEFORE UPDATE OF phase ON primary_layout_operations WHEN NEW.phase='prepared' BEGIN SELECT RAISE(FAIL,'injected refresh preparation failure'); END"
    ).unwrap();
    op.phase = "prepared".into();
    assert!(db.save_primary_layout(op.clone()).await.is_err());
    let recorded = db
        .primary_layout_operations("PrimarySync".into())
        .await
        .unwrap();
    assert_eq!(recorded.len(), 1);
    assert_eq!(recorded[0].phase, "planned");
    assert!(recorded[0].refresh_metadata);
    assert!(
        recorded[0]
            .members
            .iter()
            .all(|member| member.stage.is_none() || member.prepared.is_some())
    );
    assert_eq!(std::fs::read(&derived.path).unwrap(), media);
    assert_eq!(
        std::fs::read(super::sidecar(&derived.path)).unwrap(),
        old_sidecar
    );
    db.acquire_lock("replace preparation failure with commit failure")
        .unwrap()
        .execute_batch(
            "DROP TRIGGER fail_refresh_prepared;
             CREATE TRIGGER fail_refresh_binding BEFORE INSERT ON primary_layout_bindings
             BEGIN SELECT RAISE(FAIL,'injected refresh binding failure'); END",
        )
        .unwrap();
    drop(config);
    assert_eq!(Arc::strong_count(&db), 1);
    drop(db);

    // Reopen without the one-shot option. A failed catalogue commit must retain
    // the actual displaced bytes and operation evidence for the next recovery.
    let db = Arc::new(SqliteStateDb::open(&state).await.unwrap());
    let retry_config = DownloadConfig {
        directory: dir.path().join("new-root").into(),
        state_db: Some(db.clone()),
        ..DownloadConfig::test_default()
    };
    assert!(!retry_config.refresh_metadata);
    let error = recover(
        &reqwest::Client::new(),
        &[recorded_recovery_pass()],
        &retry_config,
        &CancellationToken::new(),
    )
    .await
    .unwrap_err();
    assert!(format!("{error:#}").contains("injected refresh binding failure"));
    let held = crate::download::recover_recorded_sources(
        &reqwest::Client::new(),
        &[recorded_recovery_pass()],
        Arc::new(retry_config),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert!(!held.complete);
    assert!(held.stats.failed > 0);
    assert_eq!(std::fs::read(&derived.path).unwrap(), media);
    assert_eq!(std::fs::read(&preserved).unwrap(), media);
    assert_eq!(
        std::fs::read(super::sidecar(&preserved)).unwrap(),
        old_sidecar
    );
    let held_operations = db
        .primary_layout_operations("PrimarySync".into())
        .await
        .unwrap();
    assert_eq!(held_operations.len(), 1);
    assert!(held_operations[0].refresh_metadata);
    assert_eq!(held_operations[0].phase, "publishing");
    assert!(held_operations[0].members[0].installed.is_some());
    assert!(held_operations[0].members[0].preserved.is_some());
    assert!(
        db.primary_layout_binding(op.family.clone())
            .await
            .unwrap()
            .is_none()
    );
    db.acquire_lock("clear refresh binding failure")
        .unwrap()
        .execute_batch("DROP TRIGGER fail_refresh_binding")
        .unwrap();
    drop(db);
    let mut previous = None;
    for _ in 0..2 {
        let db = Arc::new(SqliteStateDb::open(&state).await.unwrap());
        let config = DownloadConfig {
            directory: dir.path().join("new-root").into(),
            state_db: Some(db.clone()),
            ..DownloadConfig::test_default()
        };
        assert!(!config.refresh_metadata);
        let result = crate::download::recover_recorded_sources(
            &reqwest::Client::new(),
            &[recorded_recovery_pass()],
            Arc::new(config),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(result.complete);
        assert_eq!(result.stats.failed, 0);
        assert_eq!(std::fs::read(&derived.path).unwrap(), media);
        assert_eq!(std::fs::read(&preserved).unwrap(), media);
        let new_sidecar = std::fs::read(super::sidecar(&derived.path)).unwrap();
        assert!(
            std::str::from_utf8(&new_sidecar)
                .unwrap()
                .contains("Rating>5")
        );
        assert_eq!(
            std::fs::read(super::sidecar(&preserved)).unwrap(),
            old_sidecar
        );
        let manifest = db.get_manifest_assets().await.unwrap();
        let preserved_files: Vec<_> = manifest
            .iter()
            .flat_map(|row| &row.preserved_files)
            .collect();
        assert_eq!(preserved_files.len(), 1);
        let receipt = preserved_files[0];
        assert_eq!(receipt.operation, op.operation);
        assert_eq!(receipt.family, op.family);
        assert_eq!(receipt.previous_generation, op.generation);
        assert_eq!(receipt.phase, "committed");
        assert_eq!(receipt.native_path.to_path(), preserved);
        assert_eq!(receipt.provider_checksum, "synthetic-provider-checksum");
        assert_eq!(receipt.local_checksum, super::digest(media));
        assert_eq!(
            receipt.source_checksum.as_deref(),
            Some(super::digest(media).as_str())
        );
        assert_eq!(
            receipt.sidecar_checksum.as_deref(),
            Some(super::digest(old_sidecar).as_str())
        );
        assert_eq!(receipt.size_bytes, media.len() as u64);
        let binding = db
            .primary_layout_binding(op.family.clone())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(binding.generation, op.generation + 1);
        assert_eq!(binding.child, op.child);
        assert!(
            db.get_pending_metadata_rewrites(100)
                .await
                .unwrap()
                .is_empty()
        );
        assert!(
            db.primary_layout_operations("PrimarySync".into())
                .await
                .unwrap()
                .is_empty()
        );
        let snapshot = durable_state(&db);
        if let Some(previous) = &previous {
            assert_eq!(&snapshot, previous);
        }
        previous = Some(snapshot);
        assert!(!dir.path().join("new-root").exists());
    }
}

#[tokio::test]
async fn v027_recorded_refresh_intent_survives_failed_commit_and_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state.sqlite");
    let root = dir.path().join("old-root");
    std::fs::create_dir(&root).unwrap();
    let db = Arc::new(SqliteStateDb::open(&state).await.unwrap());
    let op = prepared_layout_with_refresh(&db, &root, "refresh.JPG", "PrimarySync", true).await;
    assert!(op.refresh_metadata);
    let stage = op.members[0].stage.as_ref().unwrap().to_path();
    let destination = op.members[0].destination.as_ref().unwrap().to_path();
    db.acquire_lock("inject recorded refresh commit failure").unwrap().execute_batch(
        "CREATE TRIGGER fail_recorded_refresh_commit BEFORE INSERT ON primary_layout_bindings BEGIN SELECT RAISE(FAIL,'injected recorded refresh commit failure'); END"
    ).unwrap();
    drop(db);
    let db = Arc::new(SqliteStateDb::open(&state).await.unwrap());
    let config = DownloadConfig {
        directory: dir.path().join("new-root").into(),
        state_db: Some(db.clone()),
        ..DownloadConfig::test_default()
    };
    assert!(!config.refresh_metadata);
    let held = crate::download::recover_recorded_sources(
        &reqwest::Client::new(),
        &[recorded_recovery_pass()],
        Arc::new(config),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert!(!held.complete);
    assert!(held.stats.failed > 0);
    let pending = db
        .primary_layout_operations("PrimarySync".into())
        .await
        .unwrap();
    assert_eq!(pending.len(), 1);
    assert!(
        pending[0].refresh_metadata,
        "recorded refresh intent must survive the failed commit"
    );
    assert_eq!(std::fs::read(&destination).unwrap(), PREPARED);
    db.acquire_lock("clear recorded refresh failure")
        .unwrap()
        .execute_batch("DROP TRIGGER fail_recorded_refresh_commit")
        .unwrap();
    drop(db);
    let mut settled = None;
    for _ in 0..2 {
        let db = Arc::new(SqliteStateDb::open(&state).await.unwrap());
        let config = DownloadConfig {
            directory: dir.path().join("new-root").into(),
            state_db: Some(db.clone()),
            ..DownloadConfig::test_default()
        };
        assert!(!config.refresh_metadata);
        let result = crate::download::recover_recorded_sources(
            &reqwest::Client::new(),
            &[recorded_recovery_pass()],
            Arc::new(config),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(result.complete);
        assert_eq!(result.stats.failed, 0);
        assert_eq!(std::fs::read(&destination).unwrap(), PREPARED);
        assert!(
            db.primary_layout_operations("PrimarySync".into())
                .await
                .unwrap()
                .is_empty()
        );
        let snapshot = durable_state(&db);
        if let Some(previous) = &settled {
            assert_eq!(&snapshot, previous);
        }
        settled = Some(snapshot);
        assert!(!dir.path().join("new-root").exists());
    }
    assert!(!stage.exists());
}

#[tokio::test]
async fn v027_wrapper_publishing_layout_completes_across_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state.sqlite");
    let root = dir.path().join("old-root");
    std::fs::create_dir(&root).unwrap();
    let db = Arc::new(SqliteStateDb::open(&state).await.unwrap());
    let mut op = prepared_layout(&db, &root, "publishing.JPG").await;
    // The existing operation has durable prepared bytes and crosses its
    // publication boundary; reopening simulates stopping before its first link.
    // This fixture has no preexisting member to preserve; the real prepared
    // file receipt is already stored. Traverse the owner's required preserved
    // phase rather than skipping its publication precondition.
    assert!(
        op.members
            .iter()
            .all(|member| member.old.is_none() && member.prepared.is_some())
    );
    op.phase = "preserved".into();
    db.save_primary_layout(op.clone()).await.unwrap();
    op.phase = "publishing".into();
    db.save_primary_layout(op.clone()).await.unwrap();
    let destination = op.members[0].destination.as_ref().unwrap().to_path();
    drop(db);
    let mut completed = None;
    let mut published_mtime = None;
    for _ in 0..2 {
        let db = Arc::new(SqliteStateDb::open(&state).await.unwrap());
        let config = DownloadConfig {
            directory: dir.path().join("new-root").into(),
            folder_structure: "changed-template".into(),
            state_db: Some(db.clone()),
            ..DownloadConfig::test_default()
        };
        let result = crate::download::recover_recorded_sources(
            &reqwest::Client::new(),
            &[recorded_recovery_pass()],
            Arc::new(config.clone()),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(result.complete);
        assert_eq!(result.stats.failed, 0);
        assert_eq!(std::fs::read(&destination).unwrap(), PREPARED);
        assert!(
            db.primary_layout_operations(op.source_library.clone())
                .await
                .unwrap()
                .is_empty()
        );
        let rows = durable_state(&db);
        let mtime = std::fs::metadata(&destination).unwrap().modified().unwrap();
        if let Some(previous) = &completed {
            assert!(
                &rows == previous,
                "completed operation changed on quiet reopen"
            );
        }
        if let Some(previous) = published_mtime {
            assert_eq!(mtime, previous);
        }
        completed = Some(rows);
        published_mtime = Some(mtime);
        assert!(
            !config.directory.exists(),
            "completion cannot begin a new current-config layout"
        );
    }
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn v027_wrapper_invalid_old_source_journal_stays_pending_across_reopen() {
    use sha2::{Digest, Sha256};
    use std::os::unix::ffi::OsStrExt;
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state.sqlite");
    let root = dir.path().join("old-root");
    std::fs::create_dir(&root).unwrap();
    let source = root.join("old.JPG");
    std::fs::write(&source, PREPARED).unwrap();
    let original_mtime = std::fs::metadata(&source).unwrap().modified().unwrap();
    let journal = root.join(format!(
        ".kei-replace-{}",
        data_encoding::HEXLOWER.encode(&Sha256::digest(source.file_name().unwrap().as_bytes()))
    ));
    std::fs::create_dir(&journal).unwrap();
    std::fs::write(journal.join("manifest.json"), b"retained invalid journal").unwrap();
    let db = SqliteStateDb::open(&state).await.unwrap();
    db.upsert_seen(
        &crate::test_helpers::TestAssetRecord::new("old-child")
            .filename("old.JPG")
            .size(PREPARED.len() as u64)
            .build(),
    )
    .await
    .unwrap();
    let checksum = data_encoding::HEXLOWER.encode(&Sha256::digest(PREPARED));
    db.mark_downloaded(
        "PrimarySync",
        "old-child",
        "original",
        &source,
        &checksum,
        None,
    )
    .await
    .unwrap();
    drop(db);
    for _ in 0..2 {
        let db = Arc::new(SqliteStateDb::open(&state).await.unwrap());
        let before = durable_state(&db);
        let config = DownloadConfig {
            directory: dir.path().join("new-root").into(),
            state_db: Some(db.clone()),
            ..DownloadConfig::test_default()
        };
        let result = crate::download::recover_recorded_sources(
            &reqwest::Client::new(),
            &[recorded_recovery_pass()],
            Arc::new(config.clone()),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(!result.complete);
        assert_eq!(result.stats.failed, 1);
        assert!(durable_state(&db) == before);
        assert_eq!(std::fs::read(&source).unwrap(), PREPARED);
        assert_eq!(
            std::fs::metadata(&source).unwrap().modified().unwrap(),
            original_mtime
        );
        assert_eq!(
            std::fs::read(journal.join("manifest.json")).unwrap(),
            b"retained invalid journal"
        );
        assert!(!config.directory.exists());
    }
}

#[tokio::test]
async fn v027_wrapper_publication_failure_retains_owner_then_recovers_after_reopen() {
    for cancelled in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("state.sqlite");
        let root = dir.path().join("old-root");
        std::fs::create_dir(&root).unwrap();
        let db = Arc::new(SqliteStateDb::open(&state).await.unwrap());
        let mut op = prepared_layout(&db, &root, "interrupted.JPG").await;
        op.phase = "preserved".into();
        db.save_primary_layout(op.clone()).await.unwrap();
        op.phase = "publishing".into();
        db.save_primary_layout(op.clone()).await.unwrap();
        let destination = op.members[0].destination.as_ref().unwrap().to_path();
        if !cancelled {
            db.acquire_lock("inject recorded publication finalization failure").unwrap()
                .execute_batch("CREATE TEMP TRIGGER reject_layout_commit BEFORE UPDATE OF phase ON primary_layout_operations WHEN NEW.phase='committed' BEGIN SELECT RAISE(FAIL, 'injected layout commit failure'); END;")
                .unwrap();
        }
        let config = DownloadConfig {
            directory: dir.path().join("new-root").into(),
            state_db: Some(db.clone()),
            ..DownloadConfig::test_default()
        };
        let token = CancellationToken::new();
        if cancelled {
            token.cancel();
        }
        let result = crate::download::recover_recorded_sources(
            &reqwest::Client::new(),
            &[recorded_recovery_pass()],
            Arc::new(config.clone()),
            token,
        )
        .await
        .unwrap();
        assert!(!result.complete);
        assert_eq!(result.stats.failed, 1);
        assert!(
            !db.primary_layout_operations(op.source_library.clone())
                .await
                .unwrap()
                .is_empty()
        );
        assert!(!config.directory.exists());
        drop(config);
        drop(db);
        let reopened = Arc::new(SqliteStateDb::open(&state).await.unwrap());
        let config = DownloadConfig {
            directory: dir.path().join("new-root").into(),
            state_db: Some(reopened.clone()),
            ..DownloadConfig::test_default()
        };
        let result = crate::download::recover_recorded_sources(
            &reqwest::Client::new(),
            &[recorded_recovery_pass()],
            Arc::new(config.clone()),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(result.complete);
        assert_eq!(std::fs::read(&destination).unwrap(), PREPARED);
        assert!(
            reopened
                .primary_layout_operations(op.source_library)
                .await
                .unwrap()
                .is_empty()
        );
        assert!(!config.directory.exists());
    }
}

#[tokio::test]
async fn v027_recorded_recovery_rejects_changed_stage_or_newer_provider_snapshot() {
    for change in ["stage", "provider", "metadata", "foreign-target"] {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("state.sqlite");
        let root = dir.path().join("old-root");
        std::fs::create_dir(&root).unwrap();
        let db = Arc::new(SqliteStateDb::open(&state).await.unwrap());
        let op = prepared_layout(&db, &root, "retained.JPG").await;
        let stage = op.members[0].stage.as_ref().unwrap().to_path();
        let destination = op.members[0].destination.as_ref().unwrap().to_path();
        if change == "stage" {
            std::fs::write(&stage, b"foreign preparation").unwrap();
        } else if change == "foreign-target" {
            std::fs::write(&destination, b"unowned current file").unwrap();
        } else {
            let mut observed = op.members[0].record.clone().unwrap();
            if change == "provider" {
                observed.checksum = "newer-provider-generation".into();
            } else {
                let metadata = Arc::make_mut(&mut observed.metadata);
                metadata.title = Some("new current title".into());
                metadata.metadata_hash = Some(metadata.compute_hash());
            }
            db.upsert_seen(&observed).await.unwrap();
        }
        let before = durable_state(&db);
        let stage_bytes = std::fs::read(&stage).unwrap();
        let stage_mtime = std::fs::metadata(&stage).unwrap().modified().unwrap();
        drop(db);
        for _ in 0..2 {
            let db = Arc::new(SqliteStateDb::open(&state).await.unwrap());
            let config = DownloadConfig {
                directory: dir.path().join("new-root").into(),
                state_db: Some(db.clone()),
                ..DownloadConfig::test_default()
            };
            let result = crate::download::recover_recorded_sources(
                &reqwest::Client::new(),
                &[recorded_recovery_pass()],
                Arc::new(config.clone()),
                CancellationToken::new(),
            )
            .await
            .unwrap();
            assert!(!result.complete);
            assert_eq!(result.stats.failed, 1);
            // Foreign-target publication may advance its journal phase, but it
            // cannot overwrite that target or lose its owned prepared bytes.
            if change != "foreign-target" {
                assert!(durable_state(&db) == before);
            }
            assert_eq!(std::fs::read(&stage).unwrap(), stage_bytes);
            assert_eq!(
                std::fs::metadata(&stage).unwrap().modified().unwrap(),
                stage_mtime
            );
            if change == "foreign-target" {
                assert_eq!(
                    std::fs::read(&destination).unwrap(),
                    b"unowned current file"
                );
            } else {
                assert!(!destination.exists());
            }
            assert!(!config.directory.exists());
        }
    }
}

#[tokio::test]
async fn v027_planned_journal_requires_every_recorded_preparation() {
    for has_receipt in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("state.sqlite");
        let root = dir.path().join("old-root");
        std::fs::create_dir(&root).unwrap();
        let db = Arc::new(SqliteStateDb::open(&state).await.unwrap());
        let mut op =
            prepared_layout_with_refresh(&db, &root, "planned.JPG", "PrimarySync", true).await;
        assert!(op.refresh_metadata);
        let destination = op.members[0].destination.as_ref().unwrap().to_path();
        let stage = op.members[0].stage.as_ref().unwrap().to_path();
        // Reproduce the durable prefix before prepare saves its final phase:
        // bytes may exist both before and after their per-member receipt.
        op.phase = "planned".into();
        if !has_receipt {
            op.members[0].prepared = None;
        }
        {
            let conn = db
                .acquire_lock("replay interrupted planned journal")
                .unwrap();
            let tx = conn.unchecked_transaction().unwrap();
            let mut header = op.clone();
            header.members.clear();
            tx.execute(
                "UPDATE primary_layout_operations SET phase='planned',header=?1 WHERE operation=?2",
                rusqlite::params![serde_json::to_vec(&header).unwrap(), op.operation],
            )
            .unwrap();
            tx.execute(
                "UPDATE primary_layout_members SET evidence=?1 WHERE operation=?2 AND member=0",
                rusqlite::params![serde_json::to_vec(&op.members[0]).unwrap(), op.operation],
            )
            .unwrap();
            tx.commit().unwrap();
        }
        let before = durable_state(&db);
        drop(db);
        for _ in 0..2 {
            let db = Arc::new(SqliteStateDb::open(&state).await.unwrap());
            let config = DownloadConfig {
                directory: dir.path().join("new-root").into(),
                state_db: Some(db.clone()),
                ..DownloadConfig::test_default()
            };
            let result = crate::download::recover_recorded_sources(
                &reqwest::Client::new(),
                &[recorded_recovery_pass()],
                Arc::new(config.clone()),
                CancellationToken::new(),
            )
            .await
            .unwrap();
            assert_eq!(result.complete, has_receipt);
            if has_receipt {
                assert_eq!(std::fs::read(&destination).unwrap(), PREPARED);
            } else {
                assert!(durable_state(&db) == before);
                assert_eq!(std::fs::read(&stage).unwrap(), PREPARED);
                assert!(!destination.exists());
            }
            assert!(!config.directory.exists());
        }
    }
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn v027_recorded_journal_recovers_each_publication_crash() {
    use std::os::unix::process::ExitStatusExt;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};
    struct Worker(std::process::Child);
    impl Drop for Worker {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    for point in [
        "layout-recovery-prepared",
        "layout-preserved",
        "layout-member-published",
        "layout-committed",
    ] {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("fixture-owner"),
            b"kei-synthetic-process-death",
        )
        .unwrap();
        let state = dir.path().join("state.sqlite");
        let root = dir.path().join("old-root");
        std::fs::create_dir(&root).unwrap();
        let old = root.join("untouched.JPG");
        std::fs::write(&old, b"old unchanged media").unwrap();
        let old_mtime = std::fs::metadata(&old).unwrap().modified().unwrap();
        let db = Arc::new(SqliteStateDb::open(&state).await.unwrap());
        let op = prepared_layout(&db, &root, "interrupted.JPG").await;
        let destination = op.members[0].destination.as_ref().unwrap().to_path();
        drop(db);
        let log = std::fs::File::create(dir.path().join("worker.log")).unwrap();
        let mut worker = Worker(Command::new(std::env::current_exe().unwrap())
            .args(["--exact",
                "download::orchestration::primary_layout::tests::normal_boundary_tests::v027_recorded_journal_death_worker",
                "--ignored", "--nocapture"])
            .env("KEI_TEST_PROCESS_DEATH_ROOT", dir.path())
            .env("KEI_TEST_PROCESS_DEATH_POINT", point)
            .stdout(Stdio::from(log.try_clone().unwrap())).stderr(Stdio::from(log))
            .spawn().unwrap());
        let deadline = Instant::now() + Duration::from_secs(45);
        while !dir.path().join("ready").exists() {
            assert!(
                worker.0.try_wait().unwrap().is_none(),
                "worker exited: {}",
                std::fs::read_to_string(dir.path().join("worker.log")).unwrap()
            );
            assert!(
                Instant::now() < deadline,
                "journal worker timeout at {point}"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(
            std::fs::read_to_string(dir.path().join("ready")).unwrap(),
            point
        );
        worker.0.kill().unwrap();
        assert_eq!(worker.0.wait().unwrap().signal(), Some(9));
        let mut completed = None;
        let mut mtime = None;
        for _ in 0..2 {
            let db = Arc::new(SqliteStateDb::open(&state).await.unwrap());
            let config = DownloadConfig {
                directory: dir.path().join("new-root").into(),
                state_db: Some(db.clone()),
                ..DownloadConfig::test_default()
            };
            let result = crate::download::recover_recorded_sources(
                &reqwest::Client::new(),
                &[recorded_recovery_pass()],
                Arc::new(config.clone()),
                CancellationToken::new(),
            )
            .await
            .unwrap();
            assert!(result.complete);
            assert_eq!(std::fs::read(&destination).unwrap(), PREPARED);
            let current = durable_state(&db);
            let current_mtime = std::fs::metadata(&destination).unwrap().modified().unwrap();
            if let Some(previous) = &completed {
                assert!(&current == previous);
            }
            if let Some(previous) = mtime {
                assert_eq!(current_mtime, previous);
            }
            completed = Some(current);
            mtime = Some(current_mtime);
            assert_eq!(std::fs::read(&old).unwrap(), b"old unchanged media");
            assert_eq!(
                std::fs::metadata(&old).unwrap().modified().unwrap(),
                old_mtime
            );
            assert!(!config.directory.exists());
        }
    }
}

#[cfg(target_os = "linux")]
#[tokio::test]
#[ignore = "spawned by v027_recorded_journal_recovers_each_publication_crash"]
async fn v027_recorded_journal_death_worker() {
    let root = std::path::PathBuf::from(std::env::var_os("KEI_TEST_PROCESS_DEATH_ROOT").unwrap());
    assert_eq!(
        std::fs::read(root.join("fixture-owner")).unwrap(),
        b"kei-synthetic-process-death"
    );
    let db = Arc::new(
        SqliteStateDb::open(&root.join("state.sqlite"))
            .await
            .unwrap(),
    );
    let config = DownloadConfig {
        directory: root.join("new-root").into(),
        state_db: Some(db),
        ..DownloadConfig::test_default()
    };
    crate::download::recover_recorded_sources(
        &reqwest::Client::new(),
        &[recorded_recovery_pass()],
        Arc::new(config),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    panic!("journal process-death point did not execute");
}

#[tokio::test]
async fn v027_recorded_journal_commit_cannot_replace_observation_arriving_after_publication() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state.sqlite");
    let root = dir.path().join("old-root");
    std::fs::create_dir(&root).unwrap();
    let db = Arc::new(SqliteStateDb::open(&state).await.unwrap());
    let op = prepared_layout(&db, &root, "interrupted.JPG").await;
    let record = op.members[0].record.as_ref().unwrap();
    db.upsert_seen(record).await.unwrap();
    let destination = op.members[0].destination.as_ref().unwrap().to_path();
    db.acquire_lock("inject observation after journal publication")
        .unwrap()
        .execute_batch(
            "CREATE TEMP TRIGGER observe_after_install AFTER UPDATE ON primary_layout_members
         WHEN json_type(CAST(NEW.evidence AS TEXT),'$.installed')='object'
         BEGIN UPDATE assets SET checksum='new-after-publication'; END;",
        )
        .unwrap();
    let config = DownloadConfig {
        directory: dir.path().join("new-root").into(),
        state_db: Some(db.clone()),
        ..DownloadConfig::test_default()
    };
    let result = crate::download::recover_recorded_sources(
        &reqwest::Client::new(),
        &[recorded_recovery_pass()],
        Arc::new(config),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert!(!result.complete);
    assert_eq!(result.stats.failed, 1);
    assert_eq!(
        std::fs::read(&destination).unwrap(),
        PREPARED,
        "the guard is repeated after actual recorded publication"
    );
    let mtime = std::fs::metadata(&destination).unwrap().modified().unwrap();
    assert!(
        db.primary_layout_binding(op.family.clone())
            .await
            .unwrap()
            .is_none()
    );
    drop(db);
    for _ in 0..2 {
        let db = Arc::new(SqliteStateDb::open(&state).await.unwrap());
        let before = durable_state(&db);
        let checksum: String = db
            .acquire_lock("read retained newer observation")
            .unwrap()
            .query_row(
                "SELECT checksum FROM assets WHERE id=?1",
                [&op.child],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(checksum, "new-after-publication");
        let config = DownloadConfig {
            directory: dir.path().join("new-root").into(),
            state_db: Some(db.clone()),
            ..DownloadConfig::test_default()
        };
        let result = crate::download::recover_recorded_sources(
            &reqwest::Client::new(),
            &[recorded_recovery_pass()],
            Arc::new(config),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(!result.complete);
        assert!(durable_state(&db) == before);
        assert_eq!(std::fs::read(&destination).unwrap(), PREPARED);
        assert_eq!(
            std::fs::metadata(&destination).unwrap().modified().unwrap(),
            mtime
        );
    }
}
