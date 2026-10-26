//! Normal-sync boundaries around existing layout ownership.
use super::{DownloadConfig, EditedNaming, PREPARED, durable_state, prepared_layout, recover};
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
        let mut op = prepared_layout(&db, &root, "planned.JPG").await;
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
