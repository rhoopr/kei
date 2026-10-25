use crate::state::MetadataRewriteStore;
use crate::state::db::{CheckpointTransition, SqliteStateDb};
use crate::state::db::{LegacyFileEvidence, SparseEvidence, SparseIdentityStore, SparseSourceId};

fn transition(key: &str, expected: Option<&str>, next: &str) -> CheckpointTransition {
    CheckpointTransition {
        expected_metadata: vec![(key.into(), expected.map(str::to_owned))],
        legacy_preservation_proofs: Vec::new(),
        legacy_config_hash: None,
        sparse_identity_proofs: Vec::new(),
        metadata_updates: vec![(key.into(), next.into())],
        metadata_deletes: Vec::new(),
    }
}

fn protected_rows(path: &std::path::Path) -> Vec<Vec<rusqlite::types::Value>> {
    let conn = rusqlite::Connection::open(path).unwrap();
    let mut rows = Vec::new();
    for table in [
        "unattributed_legacy",
        "unattributed_legacy_paths",
        "unattributed_legacy_proofs",
        "metadata_capture_retries",
    ] {
        let mut stmt = conn.prepare(&format!("SELECT * FROM {table}")).unwrap();
        let count = stmt.column_count();
        rows.extend(
            stmt.query_map([], |row| {
                (0..count)
                    .map(|i| row.get(i))
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
async fn v027_checkpoint_preserves_historical_owner_without_requiring_attribution() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");
    let media = dir.path().join("original.jpg");
    std::fs::write(&media, b"legacy bytes").unwrap();
    let mtime = std::fs::metadata(&media).unwrap().modified().unwrap();
    let current = crate::sync_cycle::sync_token_key("PrimarySync");
    {
        let db = SqliteStateDb::open(&path).await.unwrap();
        // Same valid preparation sequence used by legacy_preservation owner tests:
        // an existing downloaded master has two possible child owners, then the
        // durable ambiguity and exact file evidence protect it without activation.
        let sha = crate::download::file::compute_sha256(&media).await.unwrap();
        let record = crate::test_helpers::TestAssetRecord::new("master")
            .checksum("provider")
            .size(12)
            .build();
        db.upsert_seen(&record).await.unwrap();
        db.mark_downloaded("PrimarySync", "master", "original", &media, &sha, None)
            .await
            .unwrap();
        db.set_metadata_capture_revision_for_test("PrimarySync", "master", 0);
        for child in ["child-a", "child-b"] {
            db.upsert_asset_master_mapping("PrimarySync", child, "master")
                .await
                .unwrap();
        }
        let candidate = db
            .get_metadata_capture_candidates("PrimarySync", 1, 1)
            .await
            .unwrap()
            .remove(0);
        assert!(
            db.defer_metadata_capture_ambiguity(&candidate, 1)
                .await
                .unwrap()
        );
        db.set_metadata("sync_token:PrimarySync", "rollback-before")
            .await
            .unwrap();
        let snapshot = db
            .legacy_preparation_snapshots("PrimarySync", 10)
            .await
            .unwrap()
            .remove(0);
        let files = vec![
            LegacyFileEvidence {
                root: dir.path().into(),
                path: media.clone(),
                sha256: Some(sha),
                size: Some(12),
            },
            LegacyFileEvidence {
                root: dir.path().into(),
                path: dir.path().join("original.jpg.xmp"),
                sha256: None,
                size: None,
            },
        ];
        assert!(
            db.prepare_legacy_preservation(&snapshot, &files)
                .await
                .unwrap()
        );
    }
    let before = protected_rows(&path);
    {
        let db = SqliteStateDb::open(&path).await.unwrap();
        assert!(
            db.commit_checkpoint_transition(transition(
                "sync_token:PrimarySync",
                Some("rollback-before"),
                "unsafe-old-advance"
            ))
            .await
            .is_err(),
            "old checkpoint semantics must retain their historical guard"
        );
        assert!(
            db.guard_primary_writer(&media).await.is_err(),
            "historical media stays protected"
        );
        db.commit_checkpoint_transition(transition(&current, None, "v27-observed-work-accounted"))
            .await
            .expect("v27 observed-work checkpoint must not certify historical attribution");
    }
    let db = SqliteStateDb::open(&path).await.unwrap();
    assert_eq!(protected_rows(&path), before);
    assert_eq!(
        db.get_metadata("sync_token:PrimarySync")
            .await
            .unwrap()
            .as_deref(),
        Some("rollback-before")
    );
    assert_eq!(
        db.get_metadata(&current).await.unwrap().as_deref(),
        Some("v27-observed-work-accounted")
    );
    assert!(
        db.commit_checkpoint_transition(transition(&current, None, "stale-plan"))
            .await
            .is_err(),
        "epoch separation must preserve expected-metadata CAS"
    );
    db.observe_sparse_identity(
        "PrimarySync",
        &SparseSourceId::new("current-unresolved"),
        &SparseEvidence::new(r#"[1,"target","SharedSync-private","private-owner"]"#.into()),
        chrono::Utc::now(),
    )
    .await
    .unwrap();
    let marker = crate::state::unresolved_identity_key("PrimarySync");
    db.set_metadata(&marker, "1").await.unwrap();
    let mut unsafe_clear = transition(
        &current,
        Some("v27-observed-work-accounted"),
        "must-not-advance",
    );
    unsafe_clear.metadata_deletes.push(marker.clone());
    assert!(
        db.commit_checkpoint_transition(unsafe_clear).await.is_err(),
        "current sparse identity cannot be discarded without its proof"
    );
    assert_eq!(
        db.get_metadata(&marker).await.unwrap().as_deref(),
        Some("1")
    );
    assert_eq!(
        db.get_metadata(&current).await.unwrap().as_deref(),
        Some("v27-observed-work-accounted")
    );
    assert_eq!(protected_rows(&path), before);
    assert_eq!(std::fs::read(&media).unwrap(), b"legacy bytes");
    assert_eq!(
        std::fs::metadata(&media).unwrap().modified().unwrap(),
        mtime
    );
}
