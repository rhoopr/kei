//! Tests moved from `state::db::tests`, with their original names and assertions.
use crate::state::db::{CheckpointTransition, ScopedDbSyncToken, SqliteStateDb};
use crate::state::error::StateError;

// ── enum_in_progress markers ───────────────────────────────────────────

#[tokio::test]
async fn begin_enum_progress_inserts_marker() {
    let db = SqliteStateDb::open_in_memory().unwrap();
    db.begin_enum_progress("PrimarySync").await.unwrap();
    let zones = db.list_interrupted_enumerations().await.unwrap();
    assert_eq!(zones, vec!["PrimarySync".to_string()]);
}

#[tokio::test]
async fn end_enum_progress_clears_marker() {
    let db = SqliteStateDb::open_in_memory().unwrap();
    db.begin_enum_progress("PrimarySync").await.unwrap();
    db.end_enum_progress("PrimarySync").await.unwrap();
    let zones = db.list_interrupted_enumerations().await.unwrap();
    assert!(zones.is_empty());
}

#[tokio::test]
async fn end_enum_progress_is_idempotent() {
    let db = SqliteStateDb::open_in_memory().unwrap();
    // No marker set — end should be a no-op without error
    db.end_enum_progress("NotThere").await.unwrap();
    assert!(db.list_interrupted_enumerations().await.unwrap().is_empty());
}

#[tokio::test]
async fn list_interrupted_enumerations_tracks_multiple_zones() {
    let db = SqliteStateDb::open_in_memory().unwrap();
    db.begin_enum_progress("PrimarySync").await.unwrap();
    db.begin_enum_progress("SharedSync-ABC123").await.unwrap();
    let mut zones = db.list_interrupted_enumerations().await.unwrap();
    zones.sort();
    assert_eq!(zones, vec!["PrimarySync", "SharedSync-ABC123"]);
}

#[tokio::test]
async fn begin_enum_progress_is_idempotent_for_same_zone() {
    let db = SqliteStateDb::open_in_memory().unwrap();
    db.begin_enum_progress("PrimarySync").await.unwrap();
    db.begin_enum_progress("PrimarySync").await.unwrap();
    let zones = db.list_interrupted_enumerations().await.unwrap();
    assert_eq!(zones, vec!["PrimarySync".to_string()]);
}

#[tokio::test]
async fn begin_enum_progress_preserves_original_timestamp_on_reentry() {
    let db = SqliteStateDb::open_in_memory().unwrap();
    let original_ts = "1700000000";

    fn read_marker(db: &SqliteStateDb) -> Option<String> {
        let conn = db.acquire_lock("read_marker").unwrap();
        conn.query_row(
            "SELECT value FROM metadata WHERE key = 'enum_in_progress:PrimarySync'",
            [],
            |row| row.get::<_, String>(0),
        )
        .ok()
    }

    // Seed an older marker timestamp directly.
    {
        let conn = db.acquire_lock("seed_marker").unwrap();
        conn.execute(
            "INSERT INTO metadata (key, value) VALUES ('enum_in_progress:PrimarySync', ?1)",
            [original_ts],
        )
        .unwrap();
    }

    // Re-entering begin_enum_progress on a live marker must not overwrite it.
    db.begin_enum_progress("PrimarySync").await.unwrap();
    assert_eq!(
        read_marker(&db).as_deref(),
        Some(original_ts),
        "re-entering begin_enum_progress must not rewrite the original timestamp"
    );

    // After end_enum_progress, the marker clears; a subsequent begin
    // should install a fresh timestamp.
    db.end_enum_progress("PrimarySync").await.unwrap();
    db.begin_enum_progress("PrimarySync").await.unwrap();
    let fresh = read_marker(&db).expect("marker must exist after begin");
    assert_ne!(
        fresh, original_ts,
        "after end_enum_progress, a new begin should install a fresh timestamp"
    );
}

#[tokio::test]
async fn test_metadata_get_set() {
    let db = SqliteStateDb::open_in_memory().unwrap();

    // Missing key returns None
    assert_eq!(db.get_metadata("config_hash").await.unwrap(), None);

    // Set and retrieve
    db.set_metadata("config_hash", "abc123").await.unwrap();
    assert_eq!(
        db.get_metadata("config_hash").await.unwrap(),
        Some("abc123".to_string())
    );

    // Overwrite
    db.set_metadata("config_hash", "def456").await.unwrap();
    assert_eq!(
        db.get_metadata("config_hash").await.unwrap(),
        Some("def456".to_string())
    );
}

#[tokio::test]
async fn checkpoint_transition_is_atomic() {
    let db = SqliteStateDb::open_in_memory().unwrap();
    db.set_metadata("sync_token:zone", "old-token")
        .await
        .unwrap();
    db.set_metadata("enum_config_hash", "old-hash")
        .await
        .unwrap();
    db.set_metadata("pending_enum_config_hash", "new-hash")
        .await
        .unwrap();

    db.with_conn("install_test_trigger", |conn| {
        conn.execute_batch(
            "CREATE TRIGGER fail_enum_hash_update \
                 BEFORE UPDATE ON metadata \
                 WHEN NEW.key = 'enum_config_hash' \
                 BEGIN SELECT RAISE(ABORT, 'simulated promotion failure'); END;",
        )
        .map_err(|e| StateError::query("install_test_trigger", e))?;
        Ok(())
    })
    .await
    .unwrap();

    let result = db
        .commit_checkpoint_transition(CheckpointTransition {
            expected_metadata: Vec::new(),
            legacy_preservation_proofs: Vec::new(),
            legacy_config_hash: None,
            sparse_identity_proofs: Vec::new(),
            metadata_updates: vec![
                ("sync_token:zone".into(), "new-token".into()),
                ("enum_config_hash".into(), "new-hash".into()),
            ],
            metadata_deletes: vec!["pending_enum_config_hash".into()],
        })
        .await;

    assert!(result.is_err());
    assert_eq!(
        db.get_metadata("sync_token:zone").await.unwrap().as_deref(),
        Some("old-token")
    );
    assert_eq!(
        db.get_metadata("enum_config_hash")
            .await
            .unwrap()
            .as_deref(),
        Some("old-hash")
    );
    assert_eq!(
        db.get_metadata("pending_enum_config_hash")
            .await
            .unwrap()
            .as_deref(),
        Some("new-hash")
    );
}

#[tokio::test]
async fn test_delete_metadata_by_prefix() {
    let db = SqliteStateDb::open_in_memory().unwrap();

    db.set_metadata("sync_token:zone1", "tok1").await.unwrap();
    db.set_metadata("sync_token:zone2", "tok2").await.unwrap();
    db.set_metadata("config_hash", "abc").await.unwrap();

    // Only deletes matching prefix
    let deleted = db.delete_metadata_by_prefix("sync_token:").await.unwrap();
    assert_eq!(deleted, 2);

    assert_eq!(db.get_metadata("sync_token:zone1").await.unwrap(), None);
    assert_eq!(db.get_metadata("sync_token:zone2").await.unwrap(), None);
    // Unrelated key is untouched
    assert_eq!(
        db.get_metadata("config_hash").await.unwrap(),
        Some("abc".to_string())
    );

    // No-op when nothing matches
    let deleted = db.delete_metadata_by_prefix("nonexistent:").await.unwrap();
    assert_eq!(deleted, 0);
}

#[tokio::test]
async fn scoped_db_sync_token_roundtrips_by_exact_scope_key() {
    let db = SqliteStateDb::open_in_memory().unwrap();
    let row = ScopedDbSyncToken {
        provider: "icloud".to_string(),
        account: "test@example.com".to_string(),
        shape_version: 1,
        scope_hash: "scope-a".to_string(),
        selected_zones_json: r#"["PrimarySync"]"#.to_string(),
        scope_json: r#"{"coverage":{"kind":"bounded-recent-count","count":1000}}"#.to_string(),
        token: "db-token-a".to_string(),
    };

    db.upsert_scoped_db_sync_token(row.clone()).await.unwrap();

    let loaded = db
        .get_scoped_db_sync_token("icloud", "test@example.com", 1, "scope-a")
        .await
        .unwrap()
        .expect("scoped token should exist");
    assert_eq!(loaded, row);
    assert!(
        db.get_scoped_db_sync_token("icloud", "test@example.com", 1, "scope-b")
            .await
            .unwrap()
            .is_none(),
        "different scope hash must not reuse the token"
    );
}

#[tokio::test]
async fn scoped_db_sync_token_upsert_preserves_created_at_and_updates_token() {
    let db = SqliteStateDb::open_in_memory().unwrap();
    let mut row = ScopedDbSyncToken {
        provider: "icloud".to_string(),
        account: "test@example.com".to_string(),
        shape_version: 1,
        scope_hash: "scope-a".to_string(),
        selected_zones_json: r#"["PrimarySync"]"#.to_string(),
        scope_json: r#"{"coverage":{"kind":"complete"}}"#.to_string(),
        token: "db-token-a".to_string(),
    };
    db.upsert_scoped_db_sync_token(row.clone()).await.unwrap();
    row.token = "db-token-b".to_string();
    db.upsert_scoped_db_sync_token(row.clone()).await.unwrap();

    let loaded = db
        .get_scoped_db_sync_token("icloud", "test@example.com", 1, "scope-a")
        .await
        .unwrap()
        .expect("scoped token should exist");
    assert_eq!(loaded.token, "db-token-b");

    let conn = db.conn.lock().unwrap();
    let (created_at, updated_at): (i64, i64) = conn
        .query_row(
            "SELECT created_at, updated_at FROM scoped_db_sync_tokens \
                 WHERE provider = 'icloud' AND account = 'test@example.com' \
                    AND shape_version = 1 AND scope_hash = 'scope-a'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert!(
        updated_at >= created_at,
        "updated_at must not move backwards"
    );
}

#[tokio::test]
async fn metadata_empty_string_key_and_value() {
    // Arrange
    let db = SqliteStateDb::open_in_memory().unwrap();

    // Act: set metadata with an empty key
    db.set_metadata("", "some_value").await.unwrap();

    // Assert: can retrieve by empty key
    let val = db.get_metadata("").await.unwrap();
    assert_eq!(val, Some("some_value".to_string()));

    // Act: set metadata with a normal key but empty value
    db.set_metadata("last_sync_token", "").await.unwrap();

    // Assert: empty value is stored and retrievable
    let val = db.get_metadata("last_sync_token").await.unwrap();
    assert_eq!(val, Some(String::new()));

    // Act: overwrite empty key with empty value
    db.set_metadata("", "").await.unwrap();
    let val = db.get_metadata("").await.unwrap();
    assert_eq!(val, Some(String::new()));
}

#[tokio::test]
async fn retained_checkpoint_metadata_fence_rolls_back_stale_plans() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");
    {
        let db = SqliteStateDb::open(&path).await.unwrap();
        db.set_metadata("sync_token:zone", "concurrent-cursor")
            .await
            .unwrap();
        db.set_metadata("enum_config_hash", "old-config")
            .await
            .unwrap();
        db.set_metadata("pending_enum_config_hash", "new-config")
            .await
            .unwrap();
        db.set_metadata("retained_checkpoint_hold:v27:zone", "durable-hold")
            .await
            .unwrap();
        let result = db
            .commit_checkpoint_transition(CheckpointTransition {
                expected_metadata: vec![("sync_token:zone".into(), Some("planned-cursor".into()))],
                legacy_preservation_proofs: Vec::new(),
                legacy_config_hash: None,
                sparse_identity_proofs: Vec::new(),
                metadata_updates: vec![
                    ("sync_token:zone".into(), "new-cursor".into()),
                    ("enum_config_hash".into(), "new-config".into()),
                ],
                metadata_deletes: vec![
                    "pending_enum_config_hash".into(),
                    "retained_checkpoint_hold:v27:zone".into(),
                ],
            })
            .await;
        assert!(result.is_err());
    }
    let db = SqliteStateDb::open(&path).await.unwrap();
    for (key, expected) in [
        ("sync_token:zone", "concurrent-cursor"),
        ("enum_config_hash", "old-config"),
        ("pending_enum_config_hash", "new-config"),
        ("retained_checkpoint_hold:v27:zone", "durable-hold"),
    ] {
        assert_eq!(
            db.get_metadata(key).await.unwrap().as_deref(),
            Some(expected)
        );
    }
    assert_eq!(
        db.get_summary()
            .await
            .unwrap()
            .provider_checkpoint_status
            .as_deref(),
        Some("preserved")
    );
    db.commit_checkpoint_transition(CheckpointTransition {
        expected_metadata: vec![("sync_token:zone".into(), Some("concurrent-cursor".into()))],
        legacy_preservation_proofs: Vec::new(),
        legacy_config_hash: None,
        sparse_identity_proofs: Vec::new(),
        metadata_updates: vec![("sync_token:zone".into(), "new-cursor".into())],
        metadata_deletes: vec!["retained_checkpoint_hold:v27:zone".into()],
    })
    .await
    .unwrap();
    drop(db);
    let db = SqliteStateDb::open(&path).await.unwrap();
    assert_eq!(
        db.get_metadata("sync_token:zone").await.unwrap().as_deref(),
        Some("new-cursor")
    );
    assert!(
        db.get_metadata("retained_checkpoint_hold:v27:zone")
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        db.get_summary()
            .await
            .unwrap()
            .provider_checkpoint_status
            .as_deref(),
        Some("current")
    );
}

#[tokio::test]
async fn epoch_promotion_failure_is_all_or_none_across_zones() {
    let db = SqliteStateDb::open_in_memory().unwrap();
    let valid = "PrimarySync";
    let invalid = "SharedSync-epoch-peer";
    let zones = [valid, invalid];
    {
        let conn = db
            .acquire_lock("seed valid and invalid epoch proofs")
            .unwrap();
        // The in-memory test schema has no account owner; the epoch was
        // established with the generic test identity. Bind the same identity so
        // the staged proof helper can read it.
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS account_owner(\
             singleton INTEGER PRIMARY KEY CHECK(singleton=1), \
             version INTEGER NOT NULL, account_key TEXT NOT NULL, \
             provider_key TEXT NOT NULL);\
             INSERT OR REPLACE INTO account_owner VALUES(1,1,'test-account','test-provider');",
        )
        .unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO metadata(key,value) VALUES(?1,'enum-valid'),(?2,'download-valid')",
            rusqlite::params![
                crate::sync_cycle::ENUM_CONFIG_HASH_KEY,
                crate::download::DOWNLOAD_CONFIG_HASH_KEY
            ],
        )
        .unwrap();
        crate::state::db::provider_generations::observation::seed_valid_staged_epoch_for_test(
            &conn,
            valid,
            "valid-token",
        )
        .unwrap();
        // The peer starts with a structurally valid staged binding and
        // descriptor, then only its token hash is corrupted. This is a present
        // but invalid binding, not an absent one.
        crate::state::db::provider_generations::observation::seed_valid_staged_epoch_for_test(
            &conn,
            invalid,
            "peer-token",
        )
        .unwrap();
        let peer_attempt = format!("attempt-{invalid}");
        let record_key = crate::state::db::sync_epoch::library_key(invalid);
        let record: String = conn
            .query_row(
                "SELECT value FROM metadata WHERE key=?1",
                [&record_key],
                |row| row.get(0),
            )
            .unwrap();
        let mut record: serde_json::Value = serde_json::from_str(&record).unwrap();
        assert_eq!(record["attempt"], serde_json::json!(peer_attempt));
        record["binding"]["token_sha256"] = serde_json::json!("corrupted-token-hash");
        conn.execute(
            "UPDATE metadata SET value=?2 WHERE key=?1",
            rusqlite::params![record_key, record.to_string()],
        )
        .unwrap();
    }
    let result = db
        .commit_observation_transition(
            CheckpointTransition {
                expected_metadata: vec![
                    (
                        crate::sync_cycle::ENUM_CONFIG_HASH_KEY.to_owned(),
                        Some("enum-valid".to_owned()),
                    ),
                    (
                        crate::sync_cycle::PENDING_ENUM_CONFIG_HASH_KEY.to_owned(),
                        None,
                    ),
                    (
                        crate::download::DOWNLOAD_CONFIG_HASH_KEY.to_owned(),
                        Some("download-valid".to_owned()),
                    ),
                    (
                        crate::sync_cycle::PENDING_DOWNLOAD_CONFIG_HASH_KEY.to_owned(),
                        None,
                    ),
                ],
                legacy_preservation_proofs: Vec::new(),
                legacy_config_hash: None,
                sparse_identity_proofs: Vec::new(),
                metadata_updates: vec![
                    (
                        crate::sync_cycle::ENUM_CONFIG_HASH_KEY.to_owned(),
                        "enum-valid".to_owned(),
                    ),
                    (format!("sync_token:v27:{valid}"), "valid-token".to_owned()),
                    (format!("sync_token:v27:{invalid}"), "peer-token".to_owned()),
                ],
                metadata_deletes: Vec::new(),
            },
            None,
            true,
        )
        .await;
    assert!(
        result.is_err(),
        "one present but invalid epoch proof must abort both zones"
    );
    {
        let conn = db.acquire_lock("inspect all-or-none refusal").unwrap();
        for zone in zones {
            let active: bool = conn
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM metadata WHERE key=?1)",
                    [format!("sync_token:v27:{zone}")],
                    |row| row.get(0),
                )
                .unwrap();
            assert!(!active, "no active token may commit for {zone}");
            let ready = crate::state::db::sync_epoch::read_library(&conn, zone)
                .unwrap()
                .expect("epoch record must be retained");
            assert_eq!(ready.state, "ready", "the epoch must stay ready for {zone}");
            assert!(
                ready.binding.is_some(),
                "the epoch binding must be retained for {zone}"
            );
        }
    }

    // The valid zone alone must promote. This proves the first failure was the
    // peer's present-but-invalid proof, not an invalid fixture on both sides.
    db.commit_observation_transition(
        CheckpointTransition {
            expected_metadata: vec![
                (
                    crate::sync_cycle::ENUM_CONFIG_HASH_KEY.to_owned(),
                    Some("enum-valid".to_owned()),
                ),
                (
                    crate::sync_cycle::PENDING_ENUM_CONFIG_HASH_KEY.to_owned(),
                    None,
                ),
                (
                    crate::download::DOWNLOAD_CONFIG_HASH_KEY.to_owned(),
                    Some("download-valid".to_owned()),
                ),
                (
                    crate::sync_cycle::PENDING_DOWNLOAD_CONFIG_HASH_KEY.to_owned(),
                    None,
                ),
            ],
            legacy_preservation_proofs: Vec::new(),
            legacy_config_hash: None,
            sparse_identity_proofs: Vec::new(),
            metadata_updates: vec![
                (
                    crate::sync_cycle::ENUM_CONFIG_HASH_KEY.to_owned(),
                    "enum-valid".to_owned(),
                ),
                (format!("sync_token:v27:{valid}"), "valid-token".to_owned()),
            ],
            metadata_deletes: Vec::new(),
        },
        None,
        true,
    )
    .await
    .expect("the valid zone must promote on its own");
    let conn = db.acquire_lock("inspect valid-only promotion").unwrap();
    let valid_active: bool = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM metadata WHERE key=?1)",
            [format!("sync_token:v27:{valid}")],
            |row| row.get(0),
        )
        .unwrap();
    assert!(valid_active, "the valid zone must commit on its own");
    let invalid_active: bool = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM metadata WHERE key=?1)",
            [format!("sync_token:v27:{invalid}")],
            |row| row.get(0),
        )
        .unwrap();
    assert!(!invalid_active, "the invalid peer must remain uncommitted");
}

#[tokio::test]
async fn pending_token_purge_preserves_ready_epoch_evidence_and_unrelated_metadata() {
    let db = SqliteStateDb::open_in_memory().unwrap();
    {
        let conn = db.acquire_lock("seed purge guard state").unwrap();
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS account_owner(\
             singleton INTEGER PRIMARY KEY CHECK(singleton=1), \
             version INTEGER NOT NULL, account_key TEXT NOT NULL, \
             provider_key TEXT NOT NULL);\
             INSERT OR REPLACE INTO account_owner VALUES(1,1,'test-account','test-provider');",
        )
        .unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO metadata(key,value) VALUES(?1,'enum-valid'),(?2,'download-valid')",
            rusqlite::params![
                crate::sync_cycle::ENUM_CONFIG_HASH_KEY,
                crate::download::DOWNLOAD_CONFIG_HASH_KEY
            ],
        )
        .unwrap();
        crate::state::db::provider_generations::observation::seed_valid_staged_epoch_for_test(
            &conn,
            "PrimarySync",
            "ready-token",
        )
        .unwrap();
        crate::state::db::provider_generations::observation::seed_valid_staged_epoch_for_test(
            &conn,
            "SharedSync:with:colon",
            "colon-ready-token",
        )
        .unwrap();
        for (key, value) in [
            (
                "pending_sync_token:v27:old-enum:PrimarySync",
                "superseded-same-zone",
            ),
            (
                "pending_sync_token:v27:enum-valid:StaleZone",
                "stale-other-zone",
            ),
            (
                "pending_sync_token:v27:old-enum:SharedSync:with:colon",
                "superseded-colon-zone",
            ),
        ] {
            conn.execute(
                "INSERT INTO metadata(key,value) VALUES(?1,?2)",
                rusqlite::params![key, value],
            )
            .unwrap();
        }
        conn.execute(
            "INSERT INTO metadata(key,value) VALUES('unrelated','kept')",
            [],
        )
        .unwrap();
    }
    let deleted = db.purge_superseded_pending_tokens().await.unwrap();
    assert_eq!(
        deleted, 3,
        "only the three superseded tokens may be removed"
    );
    let conn = db.acquire_lock("inspect purge guard state").unwrap();
    let present = |key: &str| -> bool {
        conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM metadata WHERE key=?1)",
            [key],
            |row| row.get(0),
        )
        .unwrap()
    };
    assert!(
        present("pending_sync_token:v27:enum-valid:PrimarySync"),
        "the exact ready staged token must be preserved"
    );
    assert!(
        present("pending_sync_token:v27:enum-valid:SharedSync:with:colon"),
        "a ready token for a zone containing ':' must be preserved"
    );
    let ready = crate::state::db::sync_epoch::read_library(&conn, "PrimarySync")
        .unwrap()
        .expect("ready record must be preserved");
    assert_eq!(
        ready
            .binding
            .as_ref()
            .map(|binding| binding.token_key.as_str()),
        Some("pending_sync_token:v27:enum-valid:PrimarySync"),
        "the ready binding must be preserved"
    );
    let colon = crate::state::db::sync_epoch::read_library(&conn, "SharedSync:with:colon")
        .unwrap()
        .expect("ready colon record must be preserved");
    assert_eq!(
        colon
            .binding
            .as_ref()
            .map(|binding| binding.token_key.as_str()),
        Some("pending_sync_token:v27:enum-valid:SharedSync:with:colon"),
        "the ready colon binding must be preserved"
    );
    assert!(
        !present("pending_sync_token:v27:old-enum:PrimarySync"),
        "a superseded token for the same zone must be removed"
    );
    assert!(
        !present("pending_sync_token:v27:enum-valid:StaleZone"),
        "a pending token without ready proof evidence must be removed"
    );
    assert!(
        !present("pending_sync_token:v27:old-enum:SharedSync:with:colon"),
        "a superseded token for a zone containing ':' must be removed"
    );
    assert!(present("unrelated"), "unrelated metadata must be preserved");
}

mod v027_historical;
