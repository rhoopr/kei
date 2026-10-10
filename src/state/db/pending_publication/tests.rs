use std::fs;
use std::sync::Arc;

use sha2::{Digest, Sha256};
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

use crate::state::{
    AssetRecord, AssetVerificationState, MediaType, RetryErrorRetention, SqliteStateDb,
    VersionSizeKey,
};

async fn fixture() -> (TempDir, SqliteStateDb, AssetRecord) {
    let dir = TempDir::new().unwrap();
    let db = SqliteStateDb::open(&dir.path().join("state.db"))
        .await
        .unwrap();
    let now = chrono::Utc::now();
    let record = AssetRecord::new_pending(
        Arc::from("PrimarySync"),
        "L1".to_owned(),
        VersionSizeKey::LiveOriginal,
        "provider-a".to_owned(),
        "movie.MOV".to_owned(),
        now,
        Some(now),
        5,
        MediaType::LivePhotoVideo,
    );
    let file = dir.path().join("movie.MOV");
    fs::write(&file, b"movie").unwrap();
    let local = data_encoding::HEXLOWER.encode(&Sha256::digest(b"movie"));
    db.upsert_seen(&record).await.unwrap();
    db.mark_downloaded(
        "PrimarySync",
        "L1",
        "live_original",
        &file,
        &local,
        Some(&local),
    )
    .await
    .unwrap();
    db.mark_failed(
        "PrimarySync",
        "L1",
        "live_original",
        "synthetic HTTP 410 for next sibling",
    )
    .await
    .unwrap();
    db.prepare_for_retry(Some("PrimarySync"), RetryErrorRetention::Clear)
        .await
        .unwrap();
    (dir, db, record)
}

fn status(db: &SqliteStateDb) -> String {
    db.acquire_lock("pending publication status")
        .unwrap()
        .query_row(
            "SELECT status FROM assets WHERE id='L1' AND version_size='live_original'",
            [],
            |row| row.get(0),
        )
        .unwrap()
}

#[tokio::test]
async fn pending_publication_recovery_preserves_timestamp_receipts_provenance_and_metadata_debt() {
    let (dir, db, record) = fixture().await;
    {
        let conn = db.acquire_lock("seed prior metadata debt").unwrap();
        conn.execute_batch("UPDATE assets SET metadata_write_failed_at=123,capture_repair_metadata_hash='repair-needed'; \
            UPDATE asset_metadata_paths SET metadata_write_failed_at=456,capture_repair_metadata_hash='repair-needed',source_checksum='saved-source';").unwrap();
    }
    db.set_asset_verification(
        "PrimarySync",
        "L1",
        "live_original",
        AssetVerificationState::TransientFailure,
        "old expired URL",
    )
    .await
    .unwrap();
    let mut unrelated = record.clone();
    unrelated.id = "other".into();
    unrelated.version_size = VersionSizeKey::Original;
    db.upsert_seen(&unrelated).await.unwrap();
    db.set_asset_verification(
        "PrimarySync",
        "other",
        "original",
        AssetVerificationState::Unknown,
        "keep unrelated identity",
    )
    .await
    .unwrap();
    let before = db
        .pending_publication("PrimarySync", "L1", VersionSizeKey::LiveOriginal)
        .await
        .unwrap()
        .unwrap();
    assert!(
        db.recover_pending_publication(&before, &record, "L1", "L1", &CancellationToken::new())
            .await
            .unwrap()
    );
    let after = db
        .pending_publication("PrimarySync", "L1", VersionSizeKey::LiveOriginal)
        .await
        .unwrap()
        .unwrap();
    let mut expected = before.clone();
    expected.status = "downloaded".into();
    expected.last_error = None;
    assert_eq!(after, expected);
    let fields:(i64,String,i64,String,String) = db.acquire_lock("preserved metadata debt").unwrap().query_row(
        "SELECT a.metadata_write_failed_at,a.capture_repair_metadata_hash,p.metadata_write_failed_at,p.capture_repair_metadata_hash,p.source_checksum \
         FROM assets a JOIN asset_metadata_paths p USING(library,id,version_size) WHERE a.id='L1'",[],
        |r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?))).unwrap();
    assert_eq!(
        fields,
        (
            123,
            "repair-needed".into(),
            456,
            "repair-needed".into(),
            "saved-source".into()
        )
    );
    assert_eq!(
        db.acquire_lock("verification recovery")
            .unwrap()
            .query_row(
                "SELECT count(*) FROM asset_verifications WHERE id='L1'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
        0
    );
    assert_eq!(
        db.acquire_lock("unrelated verification")
            .unwrap()
            .query_row(
                "SELECT state FROM asset_verifications WHERE id='other'",
                [],
                |r| r.get::<_, String>(0)
            )
            .unwrap(),
        "unknown"
    );
    // A second racing reader can recognize the exact completed snapshot.
    assert!(
        db.recover_pending_publication(&before, &record, "L1", "L1", &CancellationToken::new())
            .await
            .unwrap()
    );
    let path = dir.path().join("state.db");
    drop(db);
    let reopened = SqliteStateDb::open(&path).await.unwrap();
    assert_eq!(
        reopened
            .pending_publication("PrimarySync", "L1", VersionSizeKey::LiveOriginal)
            .await
            .unwrap()
            .unwrap(),
        expected
    );
    assert_eq!(fs::read(dir.path().join("movie.MOV")).unwrap(), b"movie");
}

#[tokio::test]
async fn pending_publication_recovery_refuses_stale_row_and_receipt_snapshots() {
    for mutation in [
        "UPDATE assets SET checksum='new-content' WHERE id='L1'",
        "UPDATE assets SET size_bytes=6 WHERE id='L1'",
        "UPDATE assets SET local_path='other.MOV' WHERE id='L1'",
        "UPDATE assets SET is_deleted=1 WHERE id='L1'",
        "UPDATE assets SET downloaded_at=NULL WHERE id='L1'",
        "UPDATE assets SET download_attempts=2 WHERE id='L1'",
        "UPDATE assets SET last_error='new error' WHERE id='L1'",
        "UPDATE assets SET metadata_hash='new metadata' WHERE id='L1'",
        "UPDATE assets SET created_at=created_at+1 WHERE id='L1'",
        "UPDATE assets SET added_at=added_at+1 WHERE id='L1'",
        "UPDATE asset_metadata_paths SET provider_checksum='new-content' WHERE id='L1'",
        "UPDATE asset_metadata_paths SET local_checksum='new-local' WHERE id='L1'",
        "UPDATE asset_metadata_paths SET download_checksum='different-download' WHERE id='L1'",
        "UPDATE asset_metadata_paths SET source_checksum='changed provenance' WHERE id='L1'",
        "DELETE FROM asset_metadata_paths WHERE id='L1'",
    ] {
        let (_dir, db, record) = fixture().await;
        let proof = db
            .pending_publication("PrimarySync", "L1", VersionSizeKey::LiveOriginal)
            .await
            .unwrap()
            .unwrap();
        db.acquire_lock("inject stale snapshot")
            .unwrap()
            .execute_batch(mutation)
            .unwrap();
        assert!(
            !db.recover_pending_publication(&proof, &record, "L1", "L1", &CancellationToken::new())
                .await
                .unwrap(),
            "{mutation}"
        );
        assert_eq!(status(&db), "pending", "{mutation}");
    }
}

#[tokio::test]
async fn pending_publication_recovery_rechecks_all_owners_and_preserves_unknown_identity() {
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Case {
        Library,
        Id,
        Rendition,
        Malformed,
        Reservation,
        UnknownContent,
        OtherDestination,
        UnknownIdentity,
        ProtectedId,
        ProtectedPath,
        Mapping,
    }

    for case in [
        Case::Library,
        Case::Id,
        Case::Rendition,
        Case::Malformed,
        Case::Reservation,
        Case::UnknownContent,
        Case::OtherDestination,
        Case::UnknownIdentity,
        Case::ProtectedId,
        Case::ProtectedPath,
        Case::Mapping,
    ] {
        let (_dir, db, record) = fixture().await;
        let proof = db
            .pending_publication("PrimarySync", "L1", VersionSizeKey::LiveOriginal)
            .await
            .unwrap()
            .unwrap();
        let key = crate::fs_util::confined_path_key(&proof.local_path).unwrap();
        {
            let conn = db.acquire_lock("inject ownership change").unwrap();
            match case {
                Case::Library | Case::Id | Case::Rendition | Case::Malformed => {
                    let library = if case == Case::Library {
                        "SharedSync-other"
                    } else {
                        "PrimarySync"
                    };
                    let id = if case == Case::Id || case == Case::Malformed {
                        "foreign"
                    } else {
                        "L1"
                    };
                    let version = if case == Case::Rendition {
                        "original"
                    } else {
                        "live_original"
                    };
                    let path = if case == Case::Malformed {
                        "old/../BROKEN.MOV".to_owned()
                    } else {
                        proof
                            .local_path
                            .parent()
                            .unwrap()
                            .join(".")
                            .join("movie.MOV")
                            .to_string_lossy()
                            .into_owned()
                    };
                    conn.execute("INSERT INTO asset_metadata_paths(library,id,version_size,local_path,provider_checksum) VALUES(?1,?2,?3,?4,'foreign')",
                        rusqlite::params![library,id,version,path]).unwrap();
                }
                Case::Reservation | Case::UnknownContent | Case::OtherDestination => {
                    let path = if case == Case::OtherDestination {
                        proof.local_path.with_file_name("other.MOV")
                    } else {
                        proof.local_path.clone()
                    };
                    let destination = crate::fs_util::confined_path_key(&path).unwrap();
                    conn.execute("INSERT INTO reconciliation_paths(library,id,version_size,requested_path_key,destination_path_key,destination_path,provider_checksum,provider_size) \
                        VALUES('PrimarySync',?1,'live_original',?2,?3,?4,?5,?6)",
                        rusqlite::params![if case==Case::Reservation {"foreign"} else {"L1"},key,destination,path.to_str().unwrap(),
                            if case==Case::UnknownContent {""} else {"provider-a"},if case==Case::UnknownContent {-1} else {5}]).unwrap();
                }
                Case::UnknownIdentity => {
                    conn.execute("INSERT INTO asset_verifications VALUES('PrimarySync','L1','live_original','unknown','unresolved',1)",[]).unwrap();
                }
                Case::ProtectedId => {
                    conn.execute("INSERT INTO unattributed_legacy(library,asset_id,evidence_version,original_evidence,files,prepared_at) \
                    VALUES('PrimarySync','L1',1,'{}','[]',1)",[]).unwrap();
                }
                Case::ProtectedPath => {
                    conn.execute("INSERT INTO unattributed_legacy_paths VALUES('PrimarySync','legacy',?1,?2)",rusqlite::params![key,proof.local_path.to_str().unwrap()]).unwrap();
                }
                Case::Mapping => {
                    conn.execute("INSERT INTO asset_master_mappings VALUES('PrimarySync','L1','different-master',1)",[]).unwrap();
                }
            }
        }
        let result = db
            .recover_pending_publication(&proof, &record, "L1", "L1", &CancellationToken::new())
            .await;
        assert!(!matches!(result, Ok(true)), "{case:?}");
        assert_eq!(status(&db), "pending", "{case:?}");
        if case == Case::UnknownIdentity {
            assert_eq!(
                db.acquire_lock("unknown debt kept")
                    .unwrap()
                    .query_row(
                        "SELECT state FROM asset_verifications WHERE id='L1'",
                        [],
                        |r| r.get::<_, String>(0)
                    )
                    .unwrap(),
                "unknown"
            );
        }
    }
}

#[tokio::test]
async fn pending_publication_recovery_cancel_and_failed_transaction_keep_retry_debt() {
    let (_dir, db, record) = fixture().await;
    let proof = db
        .pending_publication("PrimarySync", "L1", VersionSizeKey::LiveOriginal)
        .await
        .unwrap()
        .unwrap();
    let cancelled = CancellationToken::new();
    cancelled.cancel();
    assert!(
        !db.recover_pending_publication(&proof, &record, "L1", "L1", &cancelled)
            .await
            .unwrap()
    );
    assert_eq!(status(&db), "pending");
    db.set_asset_verification(
        "PrimarySync",
        "L1",
        "live_original",
        AssetVerificationState::TransientFailure,
        "old failure",
    )
    .await
    .unwrap();
    db.acquire_lock("fail verification cleanup")
        .unwrap()
        .execute_batch(
            "CREATE TRIGGER reject_recovery_cleanup BEFORE DELETE ON asset_verifications \
         BEGIN SELECT RAISE(ABORT,'synthetic write failure'); END;",
        )
        .unwrap();
    assert!(
        db.recover_pending_publication(&proof, &record, "L1", "L1", &CancellationToken::new())
            .await
            .is_err()
    );
    assert_eq!(status(&db), "pending");
    assert_eq!(
        db.acquire_lock("debt survived rollback")
            .unwrap()
            .query_row(
                "SELECT count(*) FROM asset_verifications WHERE id='L1'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
        1
    );
    db.acquire_lock("permit recovery")
        .unwrap()
        .execute_batch("DROP TRIGGER reject_recovery_cleanup")
        .unwrap();
    assert!(
        db.recover_pending_publication(&proof, &record, "L1", "L1", &CancellationToken::new())
            .await
            .unwrap()
    );
    assert_eq!(status(&db), "downloaded");
}

#[tokio::test]
async fn pending_publication_candidate_requires_prior_current_content_and_unmodified_hashes() {
    for mutation in [
        "UPDATE assets SET downloaded_at=NULL",
        "UPDATE assets SET download_checksum=NULL",
        "UPDATE assets SET download_checksum='metadata-changed-original'",
        "UPDATE assets SET local_checksum='untrusted'",
        "UPDATE assets SET is_deleted=1",
        "UPDATE assets SET status='policy_excluded'",
        "UPDATE asset_metadata_paths SET provider_checksum='prior-generation'",
        "DELETE FROM asset_metadata_paths",
    ] {
        let (_dir, db, _record) = fixture().await;
        db.acquire_lock("candidate qualification")
            .unwrap()
            .execute_batch(mutation)
            .unwrap();
        assert!(
            db.pending_publication("PrimarySync", "L1", VersionSizeKey::LiveOriginal)
                .await
                .unwrap()
                .is_none(),
            "{mutation}"
        );
    }
}

#[tokio::test]
async fn pending_publication_recovery_accepts_missing_path_download_hash_with_matching_current_row()
{
    let (_dir, db, record) = fixture().await;
    db.acquire_lock("legacy path receipt download hash")
        .unwrap()
        .execute_batch("UPDATE asset_metadata_paths SET download_checksum=NULL")
        .unwrap();
    let proof = db
        .pending_publication("PrimarySync", "L1", VersionSizeKey::LiveOriginal)
        .await
        .unwrap()
        .unwrap();
    assert!(proof.receipt_download_checksum.is_none());
    assert!(
        db.recover_pending_publication(&proof, &record, "L1", "L1", &CancellationToken::new())
            .await
            .unwrap()
    );
    assert_eq!(status(&db), "downloaded");
    assert!(
        db.pending_publication("PrimarySync", "L1", VersionSizeKey::LiveOriginal)
            .await
            .unwrap()
            .unwrap()
            .receipt_download_checksum
            .is_none()
    );
}

#[tokio::test]
async fn pending_publication_recovery_respects_managed_claims_after_snapshot() {
    for mutation in [
        "UPDATE primary_layout_claims SET operation='pending-handover'",
        "UPDATE primary_layout_claims SET library='SharedSync-foreign'",
        "UPDATE primary_layout_claims SET child='foreign'",
        "UPDATE primary_layout_claims SET version='live_adjusted'",
        "UPDATE primary_layout_claims SET provider_checksum='new-content'",
        "UPDATE primary_layout_claims SET local_checksum='different-local-bytes'",
    ] {
        let (dir, db, record) = fixture().await;
        db.set_asset_verification(
            "PrimarySync",
            "L1",
            "live_original",
            AssetVerificationState::TransientFailure,
            "retained retry debt",
        )
        .await
        .unwrap();
        let proof = db
            .pending_publication("PrimarySync", "L1", VersionSizeKey::LiveOriginal)
            .await
            .unwrap()
            .unwrap();
        {
            // A layout can claim the prior path after the recovery reader's snapshot.
            let conn = db
                .acquire_lock("claim current publication after snapshot")
                .unwrap();
            conn.execute(
                "INSERT INTO primary_layout_claims(path_key,family,operation,library,child,version,provider_checksum,local_checksum,native_path) \
                 VALUES(?1,'managed-family',NULL,'PrimarySync','L1','live_original',?2,?3,X'00')",
                rusqlite::params![super::super::primary_layout::path_key(&proof.local_path).unwrap(),proof.checksum,proof.local_checksum],
            ).unwrap();
            conn.execute_batch(mutation).unwrap();
        }
        assert!(
            db.recover_pending_publication(&proof, &record, "L1", "L1", &CancellationToken::new())
                .await
                .is_err(),
            "{mutation}"
        );
        let db_path = dir.path().join("state.db");
        drop(db);
        let reopened = SqliteStateDb::open(&db_path).await.unwrap();
        assert_eq!(
            reopened
                .pending_publication("PrimarySync", "L1", VersionSizeKey::LiveOriginal)
                .await
                .unwrap()
                .unwrap(),
            proof,
            "{mutation}"
        );
        assert_eq!(
            reopened
                .acquire_lock("claim refusal retains retry debt")
                .unwrap()
                .query_row(
                    "SELECT state FROM asset_verifications WHERE id='L1'",
                    [],
                    |r| r.get::<_, String>(0)
                )
                .unwrap(),
            "transient_failure",
            "{mutation}"
        );
        assert_eq!(fs::read(&proof.local_path).unwrap(), b"movie");
        reopened
            .acquire_lock("release managed claim")
            .unwrap()
            .execute_batch("DELETE FROM primary_layout_claims")
            .unwrap();
        assert!(
            reopened
                .recover_pending_publication(&proof, &record, "L1", "L1", &CancellationToken::new())
                .await
                .unwrap()
        );
        let mut completed = proof.clone();
        completed.status = "downloaded".into();
        completed.last_error = None;
        drop(reopened);
        let quiet = SqliteStateDb::open(&db_path).await.unwrap();
        assert_eq!(
            quiet
                .pending_publication("PrimarySync", "L1", VersionSizeKey::LiveOriginal)
                .await
                .unwrap()
                .unwrap(),
            completed
        );
        assert!(
            quiet
                .recover_pending_publication(
                    &completed,
                    &record,
                    "L1",
                    "L1",
                    &CancellationToken::new()
                )
                .await
                .unwrap()
        );
        assert_eq!(fs::read(&proof.local_path).unwrap(), b"movie");
        assert_eq!(
            fs::read_dir(proof.local_path.parent().unwrap())
                .unwrap()
                .filter_map(Result::ok)
                .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "MOV"))
                .count(),
            1
        );
    }
}

#[tokio::test]
async fn pending_publication_recovery_accepts_exact_committed_managed_receipt() {
    let (dir, db, record) = fixture().await;
    let proof = db
        .pending_publication("PrimarySync", "L1", VersionSizeKey::LiveOriginal)
        .await
        .unwrap()
        .unwrap();
    db.acquire_lock("compatible committed managed receipt").unwrap().execute(
        "INSERT INTO primary_layout_claims(path_key,family,operation,library,child,version,provider_checksum,local_checksum,native_path) \
         VALUES(?1,'managed-family',NULL,'PrimarySync','L1','live_original',?2,?3,X'00')",
        rusqlite::params![super::super::primary_layout::path_key(&proof.local_path).unwrap(),proof.checksum,proof.local_checksum],
    ).unwrap();
    assert!(
        db.recover_pending_publication(&proof, &record, "L1", "L1", &CancellationToken::new())
            .await
            .unwrap()
    );
    let mut completed = proof.clone();
    completed.status = "downloaded".into();
    completed.last_error = None;
    drop(db);
    let reopened = SqliteStateDb::open(&dir.path().join("state.db"))
        .await
        .unwrap();
    assert_eq!(
        reopened
            .pending_publication("PrimarySync", "L1", VersionSizeKey::LiveOriginal)
            .await
            .unwrap()
            .unwrap(),
        completed
    );
    assert!(
        reopened
            .recover_pending_publication(&completed, &record, "L1", "L1", &CancellationToken::new())
            .await
            .unwrap()
    );
    assert_eq!(fs::read(&proof.local_path).unwrap(), b"movie");
}
