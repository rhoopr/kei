//! Transaction and reopen proof for receipt-backed baseline adoption.

use std::path::PathBuf;

use rusqlite::{Connection, params, types::Value};
use sha2::{Digest, Sha256};

use super::{
    BaselineReceipt, DownloadContextStateStore, DownloadStateStore, MetadataRewriteStore,
    SqliteStateDb,
};
use crate::state::{AssetRecord, error::StateError};
use crate::test_helpers::TestAssetRecord;

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
            .library("SharedSync")
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
                "SELECT COUNT(*) FROM assets WHERE library='SharedSync' AND id=?1",
                [id],
                |row| row.get(0),
            )
            .unwrap()
    }
}

#[tokio::test]
async fn baseline_adoption_preserves_old_rows_and_distinct_current_identities_across_reopen() {
    let fixture = Fixture::new().await;
    let before = fixture.source_snapshot();
    let mtime = std::fs::metadata(&fixture.media)
        .unwrap()
        .modified()
        .unwrap();
    for id in ["current-a", "current-b"] {
        fixture
            .db
            .adopt_baseline_receipt(&fixture.selected(id), &fixture.receipt, Box::new(|| Ok(())))
            .await
            .unwrap();
        assert_eq!(fixture.selected_count(id), 1);
    }
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
    let records: Vec<(String,String,String)> = conn.prepare("SELECT a.id,a.local_path,p.preserved_path_key FROM assets a JOIN asset_metadata_paths p ON p.library=a.library AND p.id=a.id AND p.version_size=a.version_size AND p.local_path=a.local_path WHERE a.library='SharedSync' ORDER BY a.id").unwrap().query_map([], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?))).unwrap().collect::<Result<_,_>>().unwrap();
    assert_eq!(records.len(), 2);
    assert_eq!(records[0].0, "current-a");
    assert_eq!(records[1].0, "current-b");
    for (_, path, preserved) in records {
        assert_eq!(PathBuf::from(path), fixture.media);
        assert_eq!(
            preserved,
            super::primary_layout::path_key(&fixture.media).unwrap()
        );
    }
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
async fn baseline_adoption_rejects_stale_source_receipt_and_source_deletion() {
    for deleted in [false, true] {
        let fixture = Fixture::new().await;
        let conn = Connection::open(&fixture.db_path).unwrap();
        if deleted {
            conn.execute(
                "UPDATE assets SET is_deleted=1 WHERE library='PrimarySync' AND id='old-master'",
                [],
            )
            .unwrap();
        } else {
            conn.execute("UPDATE asset_metadata_paths SET local_checksum=?1 WHERE library='PrimarySync' AND id='old-master'", ["0".repeat(64)]).unwrap();
        }
        let before = fixture.source_snapshot();
        assert!(
            fixture
                .db
                .adopt_baseline_receipt(
                    &fixture.selected("current"),
                    &fixture.receipt,
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
        .library("SharedSync")
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
                Box::new(|| Ok(()))
            )
            .await
            .is_err()
    );
    let conn = Connection::open(&fixture.db_path).unwrap();
    let values: (String, i64) = conn
        .query_row(
            "SELECT checksum,size_bytes FROM assets WHERE library='SharedSync' AND id='current'",
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
            .adopt_baseline_receipt(&fixture.selected("current"), &invalid, Box::new(|| Ok(())))
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
        conn.execute("INSERT INTO reconciliation_paths(library,id,version_size,requested_path_key,destination_path_key,destination_path,provider_checksum,provider_size) VALUES('SharedSync','current','original','requested',?1,?2,'generation',4)", params![key,destination.to_str().unwrap()]).unwrap();
        let result = fixture
            .db
            .adopt_baseline_receipt(
                &fixture.selected("current"),
                &fixture.receipt,
                Box::new(|| Ok(())),
            )
            .await;
        assert_eq!(result.is_ok(), same_path);
        let after: String = conn.query_row("SELECT destination_path FROM reconciliation_paths WHERE library='SharedSync' AND id='current'", [], |row| row.get(0)).unwrap();
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
    let retained: (String,String,String) = conn.query_row("SELECT a.status,v.state,v.reason FROM assets a JOIN asset_verifications v ON v.library=a.library AND v.id=a.id AND v.version_size=a.version_size WHERE a.library='SharedSync' AND a.id='pending'", [], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?))).unwrap();
    assert_eq!(
        retained,
        (
            "pending".into(),
            "unknown".into(),
            "baseline_match_uncertain".into()
        )
    );
    let newer = TestAssetRecord::new("newer")
        .library("SharedSync")
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
            "SELECT checksum,size_bytes FROM assets WHERE library='SharedSync' AND id='newer'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(values, ("newer-generation".into(), 7));
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM asset_verifications WHERE library='SharedSync' AND id='newer'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(count, 0);
}
