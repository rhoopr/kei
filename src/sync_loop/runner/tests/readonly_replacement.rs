//! Normal-cycle protection for stale read-only replacement journals.
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;

use crate::state::{MetadataRewriteStore, SqliteStateDb};
use crate::sync_loop::test_support::{
    make_run_cycle_config, make_run_cycle_download_config_builder, make_run_cycle_library_state,
    make_shared_session_for_run_cycle,
};
use crate::{download, state};

fn journal_path(target: &Path) -> PathBuf {
    let digest =
        data_encoding::HEXLOWER.encode(&Sha256::digest(target.file_name().unwrap().as_bytes()));
    target.with_file_name(format!(".kei-replace-{digest}"))
}

fn seed_journal(target: &Path, part: &Path, original: &[u8], replacement: &[u8]) {
    crate::download::file::seed_test_replacement_journal(target, part, original, replacement)
        .unwrap();
}

async fn cycle(
    media: &Path,
    database: &Path,
    session: &crate::auth::SharedSession,
) -> crate::sync_cycle::CycleResult {
    let db = Arc::new(SqliteStateDb::open(database).await.unwrap());
    let library =
        make_run_cycle_library_state("PrimarySync", "sync_token:PrimarySync", "zone-token-next");
    let build = make_run_cycle_download_config_builder(media, db.clone());
    let config = make_run_cycle_config();
    Box::pin(crate::sync_cycle::run_cycle(
        &[&library],
        &config,
        Some(db.as_ref()),
        false,
        &build,
        download::DownloadControls::download_hidden(),
        session,
        &CancellationToken::new(),
    ))
    .await
    .unwrap()
}

#[tokio::test]
async fn stale_readonly_marker_cycle_preserves_protected_journal_and_checkpoint_across_reopen() {
    let root = tempfile::tempdir().unwrap();
    let media = root.path().join("media");
    std::fs::create_dir(&media).unwrap();
    let protected_target = media.join("protected.JPG");
    let protected_part = media.join("protected.part");
    let protected_sidecar = media.join("protected.JPG.xmp");
    let protected_sidecar_part = media.join("protected.xmp.part");
    let healthy_target = media.join("healthy.JPG");
    let healthy_part = media.join("healthy.part");
    seed_journal(
        &protected_target,
        &protected_part,
        b"protected-original",
        b"protected-replacement",
    );
    seed_journal(
        &protected_sidecar,
        &protected_sidecar_part,
        b"sidecar-original",
        b"sidecar-replacement",
    );
    seed_journal(
        &healthy_target,
        &healthy_part,
        b"healthy-original",
        b"healthy-replacement",
    );
    let database = root.path().join("state.db");
    {
        let db = SqliteStateDb::open(&database).await.unwrap();
        let wrong_marker = crate::state::db::primary_layout::path_key(&healthy_target).unwrap();
        db.acquire_lock("seed stale readonly marker")
            .unwrap()
            .execute(
                "INSERT INTO asset_metadata_paths
                 (library,id,version_size,local_path,provider_checksum,local_checksum,source_checksum,preserved_path_key)
                 VALUES ('PrimarySync','stale','original',?1,'provider','local','source',?2)",
                rusqlite::params![protected_target.to_str().unwrap(), wrong_marker],
            )
            .unwrap();
        db.set_metadata("sync_token:PrimarySync", "zone-token-prior")
            .await
            .unwrap();
    }

    for round in 0..2 {
        let (_session_dir, session) = make_shared_session_for_run_cycle().await;
        let result = cycle(&media, &database, &session).await;
        assert_eq!(
            result.failed_count, 2,
            "round {round}: both protected journals are retained and reported"
        );
        assert_eq!(result.stats.exif_failures, 2);
        let db = SqliteStateDb::open(&database).await.unwrap();
        assert_eq!(
            db.get_metadata("sync_token:PrimarySync").await.unwrap(),
            Some("zone-token-next".into()),
            "round {round}: the protected journal does not roll the checkpoint back or churn it"
        );
        assert!(journal_path(&protected_target).exists());
        assert!(journal_path(&protected_sidecar).exists());
        assert!(!journal_path(&healthy_target).exists());
        assert_eq!(
            std::fs::read(&protected_target).unwrap(),
            b"protected-original"
        );
        assert_eq!(
            std::fs::read(&protected_part).unwrap(),
            b"protected-replacement"
        );
        assert_eq!(
            std::fs::read(&protected_sidecar).unwrap(),
            b"sidecar-original"
        );
        assert_eq!(
            std::fs::read(&protected_sidecar_part).unwrap(),
            b"sidecar-replacement"
        );
        assert_eq!(std::fs::read(&healthy_target).unwrap(), b"healthy-original");
        assert_eq!(
            std::fs::read(&healthy_part).unwrap(),
            b"healthy-replacement"
        );
        assert!(
            db.protected_legacy_paths()
                .await
                .unwrap()
                .iter()
                .any(|path| path == &protected_target)
        );
        assert!(
            db.protected_legacy_paths()
                .await
                .unwrap()
                .iter()
                .any(|path| path == &protected_sidecar)
        );
        assert_eq!(
            db.source_metadata_recovery(&protected_target)
                .await
                .unwrap(),
            state::db::SourceMetadataRecovery::ReadOnly
        );
    }
}

#[tokio::test]
async fn retained_protected_journal_preserves_checkpoint_on_failed_promotion_across_reopen() {
    let root = tempfile::tempdir().unwrap();
    let media = root.path().join("media");
    std::fs::create_dir(&media).unwrap();
    let target = media.join("protected.JPG");
    let part = media.join("protected.part");
    seed_journal(
        &target,
        &part,
        b"protected-original",
        b"protected-replacement",
    );
    let database = root.path().join("state.db");
    {
        let db = SqliteStateDb::open(&database).await.unwrap();
        let marker = crate::state::db::primary_layout::path_key(&target).unwrap();
        db.acquire_lock("seed checkpoint marker")
            .unwrap()
            .execute(
                "INSERT INTO asset_metadata_paths
                 (library,id,version_size,local_path,provider_checksum,local_checksum,source_checksum,preserved_path_key)
                 VALUES ('PrimarySync','stale','original',?1,'provider','local','source',?2)",
                rusqlite::params![target.to_str().unwrap(), marker],
            )
            .unwrap();
        db.set_metadata("sync_token:PrimarySync", "zone-token-prior")
            .await
            .unwrap();
        db.acquire_lock("inject checkpoint failure")
            .unwrap()
            .execute_batch(
                "CREATE TRIGGER fail_checkpoint_update BEFORE UPDATE ON metadata
                 WHEN NEW.key='sync_token:PrimarySync'
                 BEGIN SELECT RAISE(FAIL,'synthetic checkpoint failure'); END;
                 CREATE TRIGGER fail_checkpoint_insert BEFORE INSERT ON metadata
                 WHEN NEW.key='sync_token:PrimarySync'
                 BEGIN SELECT RAISE(FAIL,'synthetic checkpoint failure'); END;",
            )
            .unwrap();
    }
    let (_session_dir, session) = make_shared_session_for_run_cycle().await;
    let (trace, _guard) = crate::test_helpers::TracingCapture::install();
    let failed = cycle(&media, &database, &session).await;
    assert!(failed.failed_count > 0, "{failed:?}");
    let db = SqliteStateDb::open(&database).await.unwrap();
    assert!(
        trace.events().iter().any(|event| event
            .fields
            .values()
            .any(|value| value.contains("synthetic checkpoint failure"))),
        "the synthetic checkpoint write failure must have fired"
    );
    assert_eq!(
        db.get_metadata("sync_token:PrimarySync").await.unwrap(),
        Some("zone-token-prior".into()),
        "a failed checkpoint promotion must retain the prior cursor"
    );
    assert!(journal_path(&target).exists());
    assert_eq!(std::fs::read(&target).unwrap(), b"protected-original");
    assert_eq!(std::fs::read(&part).unwrap(), b"protected-replacement");
    db.acquire_lock("remove checkpoint failure")
        .unwrap()
        .execute_batch(
            "DROP TRIGGER fail_checkpoint_update;
             DROP TRIGGER fail_checkpoint_insert;",
        )
        .unwrap();
    drop(db);
    let (_session_dir, session) = make_shared_session_for_run_cycle().await;
    let retried = cycle(&media, &database, &session).await;
    assert_eq!(retried.failed_count, 1, "{retried:?}");
    let db = SqliteStateDb::open(&database).await.unwrap();
    assert_eq!(
        db.get_metadata("sync_token:PrimarySync").await.unwrap(),
        Some("zone-token-next".into()),
        "the repaired cycle advances only after the checkpoint write succeeds"
    );
    assert!(journal_path(&target).exists());
    assert_eq!(std::fs::read(&target).unwrap(), b"protected-original");
    assert_eq!(std::fs::read(&part).unwrap(), b"protected-replacement");
}
