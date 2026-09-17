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
