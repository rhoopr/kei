//! Transaction and reopen proof for receipt-backed baseline adoption.

use std::path::{Path, PathBuf};

use rusqlite::{Connection, OptionalExtension, params, types::Value};
use sha2::{Digest, Sha256};

use super::{
    BaselineReceipt, DownloadContextStateStore, DownloadStateStore, MetadataRewriteStore,
    ReconciliationStateStore, SqliteStateDb,
};
use crate::state::db::account::AccountOwner;
use crate::state::db::download_receipts::DownloadReceipt;
use crate::state::db::provider_selection::SelectionPath;
use crate::state::{
    AssetRecord, ReconciliationContent, ReconciliationPathKey, ReconciliationReservation,
    VersionSizeKey, error::StateError,
};
use crate::test_helpers::TestAssetRecord;

fn admission_owner() -> AccountOwner {
    AccountOwner::authenticated(
        "noalias@example.invalid",
        "com",
        &serde_json::from_value(serde_json::json!({
            "dsInfo": {"dsid": "synthetic-noalias"}
        }))
        .unwrap(),
    )
    .unwrap()
}

struct Fixture {
    _directory: tempfile::TempDir,
    db_path: PathBuf,
    media: PathBuf,
    db: SqliteStateDb,
    receipt: BaselineReceipt,
}

impl Fixture {
    async fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let db_path = directory.path().join("state.db");
        let media = directory.path().join("old-suffixed.JPG");
        std::fs::write(&media, b"good").unwrap();
        let db = SqliteStateDb::open(&db_path).await.unwrap();
        let source = TestAssetRecord::new("old-master")
            .checksum("generation")
            .size(4)
            .build();
        let digest = data_encoding::HEXLOWER.encode(&Sha256::digest(b"good"));
        db.upsert_seen(&source).await.unwrap();
        db.mark_verified_download(
            "PrimarySync",
            "old-master",
            "original",
            &media,
            &digest,
            Some(&digest),
            false,
        )
        .await
        .unwrap();
        assert!(
            db.claim_legacy_master_state_owner("PrimarySync", "old-master", "current")
                .await
                .unwrap()
        );
        let receipt = db.get_baseline_receipts().await.unwrap().remove(0);
        Self {
            _directory: directory,
            db_path,
            media,
            db,
            receipt,
        }
    }

    fn selected(&self, id: &str) -> AssetRecord {
        TestAssetRecord::new(id)
            .library("PrimarySync")
            .checksum("generation")
            .size(4)
            .build()
    }

    fn source_snapshot(&self) -> Vec<Vec<Value>> {
        let conn = Connection::open(&self.db_path).unwrap();
        let mut rows = Vec::new();
        for table in ["assets", "asset_metadata_paths"] {
            let mut statement = conn
                .prepare(&format!(
                    "SELECT * FROM {table} WHERE library='PrimarySync' AND id='old-master'"
                ))
                .unwrap();
            let count = statement.column_count();
            rows.extend(
                statement
                    .query_map([], |row| {
                        (0..count)
                            .map(|column| row.get::<_, Value>(column))
                            .collect::<Result<Vec<_>, _>>()
                    })
                    .unwrap()
                    .collect::<Result<Vec<_>, _>>()
                    .unwrap(),
            );
        }
        rows
    }

    fn selected_count(&self, id: &str) -> i64 {
        Connection::open(&self.db_path)
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM assets WHERE library='PrimarySync' AND id=?1",
                [id],
                |row| row.get(0),
            )
            .unwrap()
    }
}

#[tokio::test]
async fn baseline_adoption_preserves_old_rows_and_refuses_foreign_current_identities() {
    let fixture = Fixture::new().await;
    let before = fixture.source_snapshot();
    let mtime = std::fs::metadata(&fixture.media)
        .unwrap()
        .modified()
        .unwrap();
    for id in ["current-a", "current-b"] {
        assert!(
            fixture
                .db
                .adopt_baseline_receipt(
                    &fixture.selected(id),
                    &fixture.receipt,
                    id,
                    Box::new(|| Ok(())),
                )
                .await
                .is_err(),
            "foreign state identity must not adopt the explicit owner path"
        );
        assert_eq!(fixture.selected_count(id), 0);
    }
    fixture
        .db
        .adopt_baseline_receipt(
            &fixture.selected("current"),
            &fixture.receipt,
            "current",
            Box::new(|| Ok(())),
        )
        .await
        .unwrap();
    assert_eq!(fixture.selected_count("current"), 1);
    assert_eq!(fixture.source_snapshot(), before);
    assert_eq!(std::fs::read(&fixture.media).unwrap(), b"good");
    assert_eq!(
        std::fs::metadata(&fixture.media)
            .unwrap()
            .modified()
            .unwrap(),
        mtime
    );
    let db_path = fixture.db_path.clone();
    drop(fixture.db);
    let db = SqliteStateDb::open(&db_path).await.unwrap();
    assert!(db.guard_primary_writer(&fixture.media).await.is_err());
    let conn = Connection::open(&db_path).unwrap();
    let records: Vec<(String,String,String)> = conn.prepare("SELECT a.id,a.local_path,p.preserved_path_key FROM assets a JOIN asset_metadata_paths p ON p.library=a.library AND p.id=a.id AND p.version_size=a.version_size AND p.local_path=a.local_path WHERE a.library='PrimarySync' AND a.id='current' ORDER BY a.id").unwrap().query_map([], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?))).unwrap().collect::<Result<_,_>>().unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].0, "current");
    assert_eq!(PathBuf::from(&records[0].1), fixture.media);
    assert_eq!(
        records[0].2,
        super::primary_layout::path_key(&fixture.media).unwrap()
    );
}

#[tokio::test]
async fn existing_mixed_alias_is_idempotent_but_foreign_pending_reservation_blocks() {
    for provider_signature in [false, true] {
        let fixture = Fixture::new().await;
        let digest = data_encoding::HEXLOWER.encode(&Sha256::digest(b"good"));
        for id in ["current", "foreign"] {
            let row = TestAssetRecord::new(id)
                .library("PrimarySync")
                .checksum("generation")
                .size(4)
                .build();
            fixture.db.upsert_seen(&row).await.unwrap();
            fixture
                .db
                .mark_verified_download(
                    "PrimarySync",
                    id,
                    "original",
                    &fixture.media,
                    &digest,
                    Some(&digest),
                    false,
                )
                .await
                .unwrap();
        }
        let mtime = std::fs::metadata(&fixture.media)
            .unwrap()
            .modified()
            .unwrap();
        let selected = fixture.selected("current");
        let receipt = fixture.receipt.clone();
        assert_eq!(preserved_marker(&fixture.db_path, "current"), None);
        let before = marker_provenance_snapshot(&fixture.db_path);
        if provider_signature {
            fixture
                .db
                .adopt_provider_signature(&selected, &receipt, "current", Box::new(|| Ok(())))
                .await
                .unwrap();
        } else {
            fixture
                .db
                .adopt_baseline_receipt(&selected, &receipt, "current", Box::new(|| Ok(())))
                .await
                .unwrap();
        }
        assert!(
            preserved_marker(&fixture.db_path, "current").is_some(),
            "first adoption must establish the missing protective marker"
        );
        assert_eq!(marker_provenance_snapshot(&fixture.db_path), before);
        assert_eq!(fixture.selected_count("current"), 1);
        assert_eq!(std::fs::read(&fixture.media).unwrap(), b"good");
        assert_eq!(
            std::fs::metadata(&fixture.media)
                .unwrap()
                .modified()
                .unwrap(),
            mtime
        );

        let db_path = fixture.db_path.clone();
        drop(fixture.db);
        let db = SqliteStateDb::open(&db_path).await.unwrap();
        let key = crate::fs_util::confined_path_key(&fixture.media).unwrap();
        let reservation_db = SqliteStateDb::open(&db_path).await.unwrap();
        reservation_db
            .reserve_reconciliation_paths(
                &[ReconciliationReservation {
                    content: Some(ReconciliationContent {
                        checksum: "generation".into(),
                        size: 4,
                    }),
                    library: std::sync::Arc::from("PrimarySync"),
                    asset_id: "foreign".into(),
                    version_size: VersionSizeKey::Original,
                    requested_path_key: ReconciliationPathKey(key.clone()),
                    destination_path_key: ReconciliationPathKey(key),
                    destination_path: fixture.media.clone(),
                }],
                None,
            )
            .await
            .unwrap();
        let before_refusal = adoption_snapshot(&db_path);
        if provider_signature {
            assert!(
                db.adopt_provider_signature(&selected, &receipt, "current", Box::new(|| Ok(())),)
                    .await
                    .is_err(),
                "foreign pending reservation blocks the provider path"
            );
        } else {
            assert!(
                db.adopt_baseline_receipt(&selected, &receipt, "current", Box::new(|| Ok(())))
                    .await
                    .is_err(),
                "foreign pending reservation blocks the historical path"
            );
        }
        assert_eq!(adoption_snapshot(&db_path), before_refusal);
        let reservations: i64 = Connection::open(&db_path)
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM reconciliation_paths WHERE library='PrimarySync' AND id='foreign'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(reservations, 1, "foreign reservation must be retained");
        assert_eq!(std::fs::read(&fixture.media).unwrap(), b"good");
    }
}

#[tokio::test]
async fn legacy_owned_null_marker_is_protected_without_rewriting_provenance() {
    for provider_signature in [false, true] {
        let fixture = Fixture::new().await;
        let digest = data_encoding::HEXLOWER.encode(&Sha256::digest(b"good"));
        for id in ["current", "foreign"] {
            let row = TestAssetRecord::new(id)
                .library("PrimarySync")
                .checksum("generation")
                .size(4)
                .build();
            fixture.db.upsert_seen(&row).await.unwrap();
            fixture
                .db
                .mark_verified_download(
                    "PrimarySync",
                    id,
                    "original",
                    &fixture.media,
                    &digest,
                    Some(&digest),
                    false,
                )
                .await
                .unwrap();
        }
        fixture
            .db
            .record_metadata_write_failure("PrimarySync", "current", "original")
            .await
            .unwrap();
        assert_eq!(preserved_marker(&fixture.db_path, "current"), None);
        let provenance = marker_provenance_snapshot(&fixture.db_path);
        let before_mtime = std::fs::metadata(&fixture.media)
            .unwrap()
            .modified()
            .unwrap();
        let selected = fixture.selected("current");
        let receipt = fixture.receipt.clone();
        if provider_signature {
            fixture
                .db
                .adopt_provider_signature(&selected, &receipt, "current", Box::new(|| Ok(())))
                .await
                .unwrap();
        } else {
            fixture
                .db
                .adopt_baseline_receipt(&selected, &receipt, "current", Box::new(|| Ok(())))
                .await
                .unwrap();
        }
        assert!(
            preserved_marker(&fixture.db_path, "current").is_some(),
            "missing preserved_path_key must be established"
        );
        assert_eq!(marker_provenance_snapshot(&fixture.db_path), provenance);
        assert_eq!(std::fs::read(&fixture.media).unwrap(), b"good");
        assert_eq!(
            std::fs::metadata(&fixture.media)
                .unwrap()
                .modified()
                .unwrap(),
            before_mtime
        );
        assert!(
            fixture
                .db
                .guard_primary_writer(&fixture.media)
                .await
                .is_err(),
            "established marker must protect the media path"
        );
        let mut sidecar = fixture.media.as_os_str().to_owned();
        sidecar.push(".xmp");
        assert!(
            fixture
                .db
                .guard_primary_writer(&std::path::PathBuf::from(sidecar))
                .await
                .is_err(),
            "established marker must protect the sidecar path"
        );

        let marker = preserved_marker(&fixture.db_path, "current");
        let provenance = marker_provenance_snapshot(&fixture.db_path);
        if provider_signature {
            fixture
                .db
                .adopt_provider_signature(&selected, &receipt, "current", Box::new(|| Ok(())))
                .await
                .unwrap();
        } else {
            fixture
                .db
                .adopt_baseline_receipt(&selected, &receipt, "current", Box::new(|| Ok(())))
                .await
                .unwrap();
        }
        assert_eq!(preserved_marker(&fixture.db_path, "current"), marker);
        assert_eq!(marker_provenance_snapshot(&fixture.db_path), provenance);

        let reservation_db = SqliteStateDb::open(&fixture.db_path).await.unwrap();
        let key = crate::fs_util::confined_path_key(&fixture.media).unwrap();
        reservation_db
            .reserve_reconciliation_paths(
                &[ReconciliationReservation {
                    content: Some(ReconciliationContent {
                        checksum: "generation".into(),
                        size: 4,
                    }),
                    library: std::sync::Arc::from("PrimarySync"),
                    asset_id: "foreign".into(),
                    version_size: VersionSizeKey::Original,
                    requested_path_key: ReconciliationPathKey(key.clone()),
                    destination_path_key: ReconciliationPathKey(key),
                    destination_path: fixture.media.clone(),
                }],
                None,
            )
            .await
            .unwrap();
        let marker = preserved_marker(&fixture.db_path, "current");
        let provenance = marker_provenance_snapshot(&fixture.db_path);
        if provider_signature {
            assert!(
                fixture
                    .db
                    .adopt_provider_signature(&selected, &receipt, "current", Box::new(|| Ok(())),)
                    .await
                    .is_err()
            );
        } else {
            assert!(
                fixture
                    .db
                    .adopt_baseline_receipt(&selected, &receipt, "current", Box::new(|| Ok(())))
                    .await
                    .is_err()
            );
        }
        assert_eq!(preserved_marker(&fixture.db_path, "current"), marker);
        assert_eq!(marker_provenance_snapshot(&fixture.db_path), provenance);
        assert_eq!(std::fs::read(&fixture.media).unwrap(), b"good");
    }
}

fn preserved_marker(path: &std::path::Path, id: &str) -> Option<String> {
    Connection::open(path)
        .unwrap()
        .query_row(
            "SELECT preserved_path_key FROM asset_metadata_paths
             WHERE library='PrimarySync' AND id=?1 AND version_size='original'
               AND local_path=(SELECT local_path FROM assets WHERE library='PrimarySync' AND id=?1 AND version_size='original')",
            [id],
            |row| row.get(0),
        )
        .unwrap()
}

fn marker_provenance_snapshot(path: &std::path::Path) -> Vec<Vec<Value>> {
    let conn = Connection::open(path).unwrap();
    let mut rows = Vec::new();
    for query in [
        "SELECT library,id,version_size,status,local_path,local_checksum,download_checksum,metadata_hash FROM assets WHERE library='PrimarySync' AND id IN ('old-master','current','foreign') ORDER BY id,version_size",
        "SELECT library,id,version_size,local_path,provider_checksum,local_checksum,source_checksum,metadata_write_failed_at,capture_repair_metadata_hash,capture_repair_output_checksum,capture_repair_output_size FROM asset_metadata_paths WHERE library='PrimarySync' AND id IN ('old-master','current','foreign') ORDER BY id,version_size,local_path",
        "SELECT * FROM download_receipts WHERE library='PrimarySync' AND state_id IN ('old-master','current','foreign') ORDER BY state_id,version_size,native_path",
        "SELECT * FROM metadata_capture_retries WHERE library='PrimarySync' AND asset_id IN ('old-master','current','foreign') ORDER BY asset_id,target_revision",
        "SELECT * FROM asset_verifications WHERE library='PrimarySync' AND id IN ('old-master','current','foreign') ORDER BY id,version_size",
    ] {
        let mut statement = conn.prepare(query).unwrap();
        let count = statement.column_count();
        rows.extend(
            statement
                .query_map([], |row| {
                    (0..count)
                        .map(|i| row.get::<_, Value>(i))
                        .collect::<rusqlite::Result<Vec<_>>>()
                })
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap(),
        );
    }
    rows
}

#[tokio::test]
async fn completed_target_fast_path_ignores_unrelated_malformed_compact_owner() {
    let (completed, completed_count, completed_marker, malformed_count) =
        malformed_compact_case(true).await;
    assert!(
        completed,
        "exact completed target must settle despite malformed unrelated compact owner"
    );
    assert_eq!(completed_count, 1);
    assert!(
        completed_marker.is_some(),
        "missing target marker may be established"
    );
    assert_eq!(
        malformed_count, 1,
        "malformed unrelated receipt must remain untouched"
    );

    let (created, created_count, created_marker, malformed_count) =
        malformed_compact_case(false).await;
    assert!(
        !created,
        "new admission must keep the full canonical census"
    );
    assert_eq!(created_count, 0);
    assert_eq!(created_marker, None);
    assert_eq!(malformed_count, 1);
}

async fn malformed_compact_case(target_completed: bool) -> (bool, i64, Option<String>, i64) {
    let dir = tempfile::tempdir().unwrap();
    let media = dir.path().join("media.JPG");
    std::fs::write(&media, b"good").unwrap();
    let digest = data_encoding::HEXLOWER.encode(&Sha256::digest(b"good"));
    let db_path = dir.path().join("state.db");
    let owner = admission_owner();
    let db = SqliteStateDb::open_owned(&db_path, &owner).await.unwrap();
    db.upsert_seen(
        &TestAssetRecord::new("old-master")
            .library("PrimarySync")
            .checksum("generation")
            .size(4)
            .build(),
    )
    .await
    .unwrap();
    db.mark_verified_download(
        "PrimarySync",
        "old-master",
        "original",
        &media,
        &digest,
        Some(&digest),
        false,
    )
    .await
    .unwrap();
    assert!(
        db.claim_legacy_master_state_owner("PrimarySync", "old-master", "current")
            .await
            .unwrap()
    );
    if target_completed {
        db.upsert_seen(
            &TestAssetRecord::new("current")
                .library("PrimarySync")
                .checksum("generation")
                .size(4)
                .build(),
        )
        .await
        .unwrap();
        db.mark_verified_download(
            "PrimarySync",
            "current",
            "original",
            &media,
            &digest,
            Some(&digest),
            false,
        )
        .await
        .unwrap();
    }
    let (account_key, provider_key) = owner.authenticated_keys().unwrap();
    let malformed_path = media.parent().unwrap().join("..").join("malformed.JPG");
    DownloadReceipt {
        account_key: account_key.to_string(),
        provider_key: provider_key.to_string(),
        scope: serde_json::json!({
            "database": "private",
            "zone": {"zoneName": "PrimarySync", "ownerRecordName": "_defaultOwner"}
        })
        .to_string(),
        library: "PrimarySync".into(),
        child: "malformed-child".into(),
        master: "malformed-master".into(),
        state_id: "malformed-owner".into(),
        origin_pass: "pass".into(),
        version_size: "original".into(),
        provider_checksum: "generation".into(),
        provider_size: 4,
        native_path: SelectionPath::from_path(&malformed_path),
        source_checksum: digest.clone(),
        local_checksum: digest.clone(),
    }
    .save(&db.acquire_lock("save malformed compact owner").unwrap())
    .unwrap();
    let receipt = db
        .get_baseline_receipts()
        .await
        .unwrap()
        .into_iter()
        .find(|receipt| receipt.id == "old-master")
        .unwrap();
    let selected = TestAssetRecord::new("current")
        .library("PrimarySync")
        .checksum("generation")
        .size(4)
        .build();
    let created = db
        .adopt_baseline_receipt(&selected, &receipt, "current", Box::new(|| Ok(())))
        .await
        .is_ok();
    let count = fixture_current_count(&db_path, "current");
    let marker = if count == 0 {
        None
    } else {
        preserved_marker(&db_path, "current")
    };
    let malformed_count: i64 = Connection::open(&db_path)
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM download_receipts WHERE state_id='malformed-owner'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(std::fs::read(&media).unwrap(), b"good");
    (created, count, marker, malformed_count)
}

#[tokio::test]
async fn compact_receipt_native_path_is_canonical_owner_evidence() {
    let dir = tempfile::tempdir().unwrap();
    let media = dir.path().join("media.JPG");
    std::fs::write(&media, b"good").unwrap();
    let digest = data_encoding::HEXLOWER.encode(&Sha256::digest(b"good"));
    let db_path = dir.path().join("state.db");
    let owner = admission_owner();
    let db = SqliteStateDb::open_owned(&db_path, &owner).await.unwrap();
    db.upsert_seen(
        &TestAssetRecord::new("old-master")
            .library("PrimarySync")
            .checksum("generation")
            .size(4)
            .build(),
    )
    .await
    .unwrap();
    db.mark_verified_download(
        "PrimarySync",
        "old-master",
        "original",
        &media,
        &digest,
        Some(&digest),
        false,
    )
    .await
    .unwrap();
    assert!(
        db.claim_legacy_master_state_owner("PrimarySync", "old-master", "current")
            .await
            .unwrap()
    );
    let (account_key, provider_key) = owner.authenticated_keys().unwrap();
    let spelled = media.parent().unwrap().join(".").join("media.JPG");
    let receipt = DownloadReceipt {
        account_key: account_key.to_string(),
        provider_key: provider_key.to_string(),
        scope: serde_json::json!({
            "database": "private",
            "zone": {"zoneName": "PrimarySync", "ownerRecordName": "_defaultOwner"}
        })
        .to_string(),
        library: "PrimarySync".into(),
        child: "foreign-child".into(),
        master: "foreign-master".into(),
        state_id: "foreign".into(),
        origin_pass: "pass".into(),
        version_size: "original".into(),
        provider_checksum: "generation".into(),
        provider_size: 4,
        native_path: SelectionPath::from_path(&spelled),
        source_checksum: digest.clone(),
        local_checksum: digest.clone(),
    };
    receipt
        .save(&db.acquire_lock("save foreign compact receipt").unwrap())
        .unwrap();
    let receipt = db
        .get_baseline_receipts()
        .await
        .unwrap()
        .into_iter()
        .find(|receipt| receipt.id == "old-master")
        .unwrap();
    let selected = TestAssetRecord::new("current")
        .library("PrimarySync")
        .checksum("generation")
        .size(4)
        .build();
    assert!(
        db.adopt_baseline_receipt(&selected, &receipt, "current", Box::new(|| Ok(())))
            .await
            .is_err(),
        "compact native path owner is decoded and canonicalized, not missed as raw text"
    );
    assert_eq!(std::fs::read(&media).unwrap(), b"good");
    assert_eq!(fixture_current_count(&db_path, "current"), 0);
}

fn fixture_current_count(path: &std::path::Path, id: &str) -> i64 {
    Connection::open(path)
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM assets WHERE library='PrimarySync' AND id=?1",
            [id],
            |row| row.get(0),
        )
        .unwrap()
}

fn adoption_snapshot(path: &std::path::Path) -> Vec<Vec<Value>> {
    let conn = Connection::open(path).unwrap();
    let mut rows = Vec::new();
    for query in [
        "SELECT * FROM assets WHERE library='PrimarySync' AND id IN ('old-master','current','foreign') ORDER BY id,version_size",
        "SELECT * FROM asset_metadata_paths WHERE library='PrimarySync' AND id IN ('old-master','current','foreign') ORDER BY id,version_size,local_path",
        "SELECT * FROM asset_verifications WHERE library='PrimarySync' AND id IN ('old-master','current','foreign') ORDER BY id,version_size",
        "SELECT * FROM download_receipts WHERE library='PrimarySync' AND state_id IN ('old-master','current','foreign') ORDER BY state_id,version_size,native_path",
        "SELECT * FROM metadata_capture_retries WHERE library='PrimarySync' AND asset_id IN ('old-master','current','foreign') ORDER BY asset_id,target_revision",
        "SELECT * FROM provider_active_destinations WHERE library='PrimarySync' AND asset_id IN ('old-master','current','foreign') ORDER BY asset_id,version_size,path",
        "SELECT * FROM reconciliation_paths WHERE library='PrimarySync' AND id='foreign' ORDER BY version_size,requested_path_key",
    ] {
        let mut statement = conn.prepare(query).unwrap();
        let count = statement.column_count();
        rows.extend(
            statement
                .query_map([], |row| {
                    (0..count)
                        .map(|i| row.get::<_, Value>(i))
                        .collect::<rusqlite::Result<Vec<_>>>()
                })
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap(),
        );
    }
    rows
}

#[tokio::test]
async fn baseline_adoption_callback_failure_rolls_back_every_new_row() {
    let fixture = Fixture::new().await;
    let before = fixture.source_snapshot();
    let result = fixture
        .db
        .adopt_baseline_receipt(
            &fixture.selected("current"),
            &fixture.receipt,
            "current",
            Box::new(|| {
                Err(StateError::Invariant {
                    operation: "test validation",
                    detail: "file changed".into(),
                })
            }),
        )
        .await;
    assert!(result.is_err());
    assert_eq!(fixture.selected_count("current"), 0);
    assert_eq!(fixture.source_snapshot(), before);
    assert!(
        fixture
            .db
            .guard_primary_writer(&fixture.media)
            .await
            .is_ok()
    );
}

#[tokio::test]
async fn baseline_adoption_rejects_stale_receipt_even_when_source_is_deleted() {
    for deleted in [false, true] {
        let fixture = Fixture::new().await;
        let conn = Connection::open(&fixture.db_path).unwrap();
        if deleted {
            conn.execute(
                "UPDATE assets SET is_deleted=1 WHERE library='PrimarySync' AND id='old-master'",
                [],
            )
            .unwrap();
        }
        {
            conn.execute("UPDATE asset_metadata_paths SET local_checksum=?1 WHERE library='PrimarySync' AND id='old-master'", ["0".repeat(64)]).unwrap();
        }
        let before = fixture.source_snapshot();
        assert!(
            fixture
                .db
                .adopt_baseline_receipt(
                    &fixture.selected("current"),
                    &fixture.receipt,
                    "current",
                    Box::new(|| Ok(()))
                )
                .await
                .is_err()
        );
        assert_eq!(fixture.selected_count("current"), 0);
        assert_eq!(fixture.source_snapshot(), before);
    }
}

#[tokio::test]
async fn baseline_adoption_does_not_overwrite_newer_current_generation() {
    let fixture = Fixture::new().await;
    let newer = TestAssetRecord::new("current")
        .library("PrimarySync")
        .checksum("newer-generation")
        .size(7)
        .build();
    fixture.db.upsert_seen(&newer).await.unwrap();
    assert!(
        fixture
            .db
            .adopt_baseline_receipt(
                &fixture.selected("current"),
                &fixture.receipt,
                "current",
                Box::new(|| Ok(()))
            )
            .await
            .is_err()
    );
    let conn = Connection::open(&fixture.db_path).unwrap();
    let values: (String, i64) = conn
        .query_row(
            "SELECT checksum,size_bytes FROM assets WHERE library='PrimarySync' AND id='current'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(values, ("newer-generation".into(), 7));
}

#[tokio::test]
async fn baseline_adoption_writer_guard_covers_normalized_path_and_sidecar() {
    let fixture = Fixture::new().await;
    fixture
        .db
        .adopt_baseline_receipt(
            &fixture.selected("current"),
            &fixture.receipt,
            "current",
            Box::new(|| Ok(())),
        )
        .await
        .unwrap();
    let alias = fixture
        .media
        .parent()
        .unwrap()
        .join(".")
        .join(fixture.media.file_name().unwrap());
    assert!(fixture.db.guard_primary_writer(&alias).await.is_err());
    let mut sidecar = fixture.media.as_os_str().to_owned();
    sidecar.push(".xmp");
    let sidecar = PathBuf::from(sidecar);
    assert!(fixture.db.guard_primary_writer(&sidecar).await.is_err());
}

#[tokio::test]
async fn baseline_adoption_rejects_layout_provenance_added_after_matching() {
    let fixture = Fixture::new().await;
    let conn = Connection::open(&fixture.db_path).unwrap();
    conn.execute("INSERT INTO primary_layout_bindings(family,library,source_library,child,generation,decision,policy,binding) VALUES ('family','PrimarySync','PrimarySync','old-master',1,'decision','policy',X'00')", []).unwrap();
    assert!(
        fixture
            .db
            .adopt_baseline_receipt(
                &fixture.selected("current"),
                &fixture.receipt,
                "current",
                Box::new(|| Ok(()))
            )
            .await
            .is_err()
    );
    assert_eq!(fixture.selected_count("current"), 0);
}

#[tokio::test]
async fn baseline_adoption_rejects_invalid_digest_even_when_database_matches() {
    let fixture = Fixture::new().await;
    let mut invalid = fixture.receipt.clone();
    invalid.source_checksum = Some("z".repeat(64));
    Connection::open(&fixture.db_path)
        .unwrap()
        .execute(
            "UPDATE asset_metadata_paths SET source_checksum=?1 WHERE library=?2 AND id=?3",
            params![invalid.source_checksum, invalid.library, invalid.id],
        )
        .unwrap();
    assert!(
        fixture
            .db
            .adopt_baseline_receipt(
                &fixture.selected("current"),
                &invalid,
                "current",
                Box::new(|| Ok(())),
            )
            .await
            .is_err()
    );
}

#[tokio::test]
async fn baseline_adoption_preserves_reserved_destination_and_accepts_normalized_same_path() {
    for same_path in [false, true] {
        let fixture = Fixture::new().await;
        let destination = if same_path {
            fixture
                .media
                .parent()
                .unwrap()
                .join(".")
                .join(fixture.media.file_name().unwrap())
        } else {
            fixture.media.parent().unwrap().join("reserved-other.JPG")
        };
        let key = crate::fs_util::confined_path_key(&destination).unwrap();
        let conn = Connection::open(&fixture.db_path).unwrap();
        conn.execute("INSERT INTO reconciliation_paths(library,id,version_size,requested_path_key,destination_path_key,destination_path,provider_checksum,provider_size) VALUES('PrimarySync','current','original','requested',?1,?2,'generation',4)", params![key,destination.to_str().unwrap()]).unwrap();
        let result = fixture
            .db
            .adopt_baseline_receipt(
                &fixture.selected("current"),
                &fixture.receipt,
                "current",
                Box::new(|| Ok(())),
            )
            .await;
        assert_eq!(result.is_ok(), same_path);
        let after: String = conn.query_row("SELECT destination_path FROM reconciliation_paths WHERE library='PrimarySync' AND id='current'", [], |row| row.get(0)).unwrap();
        assert_eq!(PathBuf::from(after), destination);
        assert_eq!(fixture.selected_count("current"), i64::from(same_path));
    }
}

#[tokio::test]
async fn baseline_deferral_retains_unknown_work_and_refuses_stale_target() {
    let fixture = Fixture::new().await;
    fixture
        .db
        .defer_baseline_match(&fixture.selected("pending"))
        .await
        .unwrap();
    let conn = Connection::open(&fixture.db_path).unwrap();
    let retained: (String,String,String) = conn.query_row("SELECT a.status,v.state,v.reason FROM assets a JOIN asset_verifications v ON v.library=a.library AND v.id=a.id AND v.version_size=a.version_size WHERE a.library='PrimarySync' AND a.id='pending'", [], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?))).unwrap();
    assert_eq!(
        retained,
        (
            "pending".into(),
            "unknown".into(),
            "baseline_match_uncertain".into()
        )
    );
    let newer = TestAssetRecord::new("newer")
        .library("PrimarySync")
        .checksum("newer-generation")
        .size(7)
        .build();
    fixture.db.upsert_seen(&newer).await.unwrap();
    assert!(
        fixture
            .db
            .defer_baseline_match(&fixture.selected("newer"))
            .await
            .is_err()
    );
    let values: (String, i64) = conn
        .query_row(
            "SELECT checksum,size_bytes FROM assets WHERE library='PrimarySync' AND id='newer'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(values, ("newer-generation".into(), 7));
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM asset_verifications WHERE library='PrimarySync' AND id='newer'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(count, 0);
}

#[tokio::test]
async fn baseline_diagnostics_merge_destinations_reopen_and_settle_only_published_path() {
    use super::match_diagnostics::{MatchAttempt, MatchDiagnostics, MatchExplanation};
    use super::provider_selection::SelectionPath;
    let fixture = Fixture::new().await;
    let before = fixture.source_snapshot();
    let other = fixture._directory.path().join("second-album.JPG");
    fixture
        .db
        .upsert_seen(&fixture.selected("current"))
        .await
        .unwrap();
    fixture
        .db
        .mark_failed(
            "PrimarySync",
            "current",
            "original",
            "earlier sibling transfer failure",
        )
        .await
        .unwrap();

    for (path, reason) in [
        (&fixture.media, "provider_content_signature_mismatch"),
        (&other, "candidate_unreadable_unsafe_or_changed"),
    ] {
        let mut record = fixture.selected("current");
        let mut explanation = MatchExplanation::default();
        explanation.add(Some(path), reason);
        record.last_error = MatchDiagnostics {
            last_transfer_error: None,
            attempts: vec![MatchAttempt {
                generation: None,
                pass: Some("album".into()),
                child: "child".into(),
                destination: SelectionPath::from_path(path),
                checksum: "generation".into(),
                observed_at: 123,
                explanation,
            }],
            omitted: 0,
        }
        .encode();
        fixture.db.defer_baseline_match(&record).await.unwrap();
    }
    let saved: String = fixture
        .db
        .acquire_lock("reverse-order diagnostic")
        .unwrap()
        .query_row(
            "SELECT last_error FROM assets WHERE library='PrimarySync' AND id='current'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        MatchDiagnostics::decode(&saved)
            .unwrap()
            .last_transfer_error
            .as_deref(),
        Some("earlier sibling transfer failure")
    );
    fixture
        .db
        .mark_failed(
            "PrimarySync",
            "current",
            "original",
            "synthetic sibling transfer failure",
        )
        .await
        .unwrap();
    fixture
        .db
        .prepare_for_retry(Some("PrimarySync"), super::RetryErrorRetention::Clear)
        .await
        .unwrap();
    let reopened = SqliteStateDb::open(&fixture.db_path).await.unwrap();
    let report = reopened.pending_match_status(10).await.unwrap();
    assert_eq!(report.len(), 2);
    assert!(
        report
            .iter()
            .all(|entry| entry.schedule.is_none() && entry.attempt.observed_at == 123)
    );
    reopened
        .adopt_baseline_receipt(
            &fixture.selected("current"),
            &fixture.receipt,
            "current",
            Box::new(|| Ok(())),
        )
        .await
        .unwrap();
    let remaining = reopened.pending_match_status(10).await.unwrap();
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].attempt.destination.to_path(), other);
    assert_eq!(
        remaining[0].attempt.explanation.candidates[0].reason,
        "candidate_unreadable_unsafe_or_changed"
    );
    assert_eq!(fixture.source_snapshot(), before);
    assert_eq!(std::fs::read(&fixture.media).unwrap(), b"good");
}

#[tokio::test]
async fn deleted_source_receipt_preserves_history_but_never_revives_deleted_target() {
    let fixture = Fixture::new().await;
    fixture
        .db
        .mark_soft_deleted("PrimarySync", "old-master", None)
        .await
        .unwrap();
    let before = fixture.source_snapshot();
    let receipts = fixture.db.get_baseline_receipts().await.unwrap();
    assert_eq!(
        receipts.len(),
        1,
        "source deletion does not invalidate recorded byte provenance"
    );
    fixture
        .db
        .adopt_baseline_receipt(
            &fixture.selected("current"),
            &receipts[0],
            "current",
            Box::new(|| Ok(())),
        )
        .await
        .unwrap();
    assert_eq!(fixture.source_snapshot(), before);
    assert_eq!(fixture.selected_count("current"), 1);
    fixture
        .db
        .mark_soft_deleted("PrimarySync", "current", None)
        .await
        .unwrap();
    let target = || {
        fixture.db.acquire_lock("deleted target snapshot").unwrap().query_row::<(bool,String,String,Option<String>,Option<String>,Option<i64>),_,_>(
        "SELECT is_deleted,status,checksum,local_checksum,local_path,downloaded_at FROM assets WHERE library='PrimarySync' AND id='current'",[],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?))).unwrap()
    };
    let deleted_target = target();
    assert!(deleted_target.0);
    assert!(
        fixture
            .db
            .adopt_baseline_receipt(
                &fixture.selected("current"),
                &receipts[0],
                "current",
                Box::new(|| Ok(()))
            )
            .await
            .is_err(),
        "source proof cannot replace current restoration authority"
    );
    assert_eq!(fixture.source_snapshot(), before);
    assert_eq!(
        target(),
        deleted_target,
        "refusal must be atomic for the target too"
    );
}

const R3_SIZES: [usize; 3] = [100, 1_000, 5_000];
const R3_SAMPLES: usize = 3;
const R3_SCOPE: &str =
    r#"{"database":"private","zone":{"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"}}"#;

fn r3_digest() -> String {
    data_encoding::HEXLOWER.encode(&Sha256::digest(b"good"))
}
async fn r3_download(db: &SqliteStateDb, id: &str, path: &Path, digest: &str) {
    db.upsert_seen(&TestAssetRecord::new(id).library("PrimarySync").build())
        .await
        .unwrap();
    db.mark_verified_download(
        "PrimarySync",
        id,
        "original",
        path,
        digest,
        Some(digest),
        false,
    )
    .await
    .unwrap();
}
async fn r3_fixture() -> (
    tempfile::TempDir,
    PathBuf,
    SqliteStateDb,
    Vec<BaselineReceipt>,
) {
    let directory = tempfile::tempdir().unwrap();
    let db_path = directory.path().join("state.db");
    let roots = directory.path().join("photos");
    std::fs::create_dir_all(&roots).unwrap();
    let db = SqliteStateDb::open_owned(&db_path, &admission_owner())
        .await
        .unwrap();
    let digest = r3_digest();
    for index in 0..R3_SAMPLES {
        let master = format!("r3-source-{index}");
        let path = roots.join(format!("source-{index}.JPG"));
        std::fs::write(&path, b"good").unwrap();
        r3_download(&db, &master, &path, &digest).await;
        assert!(
            db.claim_legacy_master_state_owner(
                "PrimarySync",
                &master,
                &format!("r3-current-{index}")
            )
            .await
            .unwrap()
        );
    }
    let receipts = db.get_baseline_receipts().await.unwrap();
    (directory, db_path, db, receipts)
}
async fn r3_seed(db: &SqliteStateDb, roots: &Path, count: usize) {
    let digest = r3_digest();
    let owner = admission_owner();
    let (account_key, provider_key) = owner.authenticated_keys().unwrap();
    for index in 0..count {
        let id = format!("r3-owner-{index}");
        let path = roots.join(format!("owner-{index}.JPG"));
        r3_download(db, &id, &path, &digest).await;
        DownloadReceipt {
            account_key: account_key.to_owned(),
            provider_key: provider_key.to_owned(),
            scope: R3_SCOPE.to_owned(),
            library: "PrimarySync".into(),
            child: format!("r3-receipt-child-{index}"),
            master: id.clone(),
            state_id: id,
            origin_pass: "r3-profile".into(),
            version_size: "original".into(),
            provider_checksum: "checksum123".into(),
            provider_size: 12345,
            native_path: SelectionPath::from_path(&path),
            source_checksum: digest.clone(),
            local_checksum: digest.clone(),
        }
        .save(&db.acquire_lock("r3 profile receipt").unwrap())
        .unwrap();
    }
}
fn r3_counts(path: &Path) -> (i64, i64, i64) {
    let conn = Connection::open(path).unwrap();
    let count = |sql: &str| conn.query_row(sql, [], |row| row.get(0)).unwrap();
    (
        count("SELECT COUNT(*) FROM assets WHERE local_path IS NOT NULL"),
        count("SELECT COUNT(*) FROM asset_metadata_paths WHERE local_path IS NOT NULL"),
        count("SELECT COUNT(*) FROM download_receipts"),
    )
}

fn r3_plans(path: &Path) -> Vec<String> {
    let conn = Connection::open(path).unwrap();
    [
        "SELECT library,id,local_path FROM assets WHERE local_path IS NOT NULL",
        "SELECT library,id,local_path FROM asset_metadata_paths WHERE local_path IS NOT NULL",
        "SELECT library,state_id,native_path FROM download_receipts",
    ]
    .into_iter()
    .map(|sql| {
        let mut statement = conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}")).unwrap();
        statement
            .query_map([], |row| row.get::<_, String>(3))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
            .join(" | ")
    })
    .collect()
}

fn r3_state(path: &Path, id: &str) -> Option<(String, String, String)> {
    Connection::open(path)
        .unwrap()
        .query_row(
            "SELECT status,local_path,local_checksum FROM assets
             WHERE library='PrimarySync' AND id=?1 AND version_size='original'",
            [id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()
        .unwrap()
}

fn r3_target(receipts: &[BaselineReceipt], index: usize) -> (String, AssetRecord, BaselineReceipt) {
    let child = format!("r3-current-{index}");
    let record = TestAssetRecord::new(&child).library("PrimarySync").build();
    let receipt = receipts
        .iter()
        .find(|receipt| receipt.id == format!("r3-source-{index}"))
        .unwrap()
        .clone();
    (child, record, receipt)
}

async fn r3_measure(
    db: &SqliteStateDb,
    record: &AssetRecord,
    receipt: &BaselineReceipt,
    child: &str,
) -> std::time::Duration {
    let started = std::time::Instant::now();
    db.adopt_baseline_receipt(record, receipt, child, Box::new(|| Ok(())))
        .await
        .unwrap();
    let elapsed = started.elapsed();
    assert_eq!(
        super::provider_generations::take_reservation_read_counts(),
        (0, 1),
        "current census uses one path-keyed pending read"
    );
    elapsed
}

#[tokio::test]
#[ignore = "bounded census profile; run alone with --ignored --test-threads=1"]
async fn r3_census_profile_first_admission_and_idempotent_repeat() {
    let _ = super::provider_generations::take_reservation_read_counts();
    for owners in R3_SIZES {
        let (directory, db_path, db, receipts) = r3_fixture().await;
        let roots = directory.path().join("owners");
        std::fs::create_dir_all(&roots).unwrap();
        r3_seed(&db, &roots, owners).await;
        let counts = r3_counts(&db_path);
        eprintln!("R3_PROFILE size={owners} counts={counts:?}");
        for plan in r3_plans(&db_path) {
            eprintln!("R3_PROFILE size={owners} plan={plan}");
            assert!(plan.contains("SCAN"), "unexpected census plan: {plan}");
        }
        let minimum = owners as i64 + R3_SAMPLES as i64;
        assert!(counts.0 >= minimum && counts.1 >= minimum && counts.2 >= owners as i64);
        let (mut first, mut repeat) = (Vec::new(), Vec::new());
        for index in 0..R3_SAMPLES {
            let (child, record, receipt) = r3_target(&receipts, index);
            first.push(r3_measure(&db, &record, &receipt, &child).await);
            assert_eq!(
                r3_state(&db_path, &child),
                Some((
                    "downloaded".into(),
                    receipt.local_path.to_string_lossy().into_owned(),
                    r3_digest()
                ))
            );
            repeat.push(r3_measure(&db, &record, &receipt, &child).await);
        }
        for (label, samples) in [("first", &first), ("repeat", &repeat)] {
            let millis = samples
                .iter()
                .map(|sample| format!("{:.3}", sample.as_secs_f64() * 1000.0))
                .collect::<Vec<_>>();
            eprintln!("R3_PROFILE size={owners} {label}_ms={millis:?}");
        }
    }
}
