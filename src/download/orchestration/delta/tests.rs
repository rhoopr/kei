use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use reqwest::Client;
use rustc_hash::FxHashSet;
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

use crate::commands::{AlbumPass, PassKind};
use crate::icloud::photos::{PhotoAsset, PhotosSession};
use crate::state::{SqliteStateDb, VersionSizeKey};
use crate::test_helpers::{MockPhotosFlow, TestAssetRecord};

use super::super::dispatch::download_photos_with_sync;
use super::super::incremental::download_photos_incremental;
use super::super::models::{
    ASSET_DELTA_HYDRATION_INCOMPLETE_REASON, DownloadControls, DownloadOutcome, DownloadReporting,
    DownloadRunMode, DownloadStore, INCREMENTAL_DELETE_STATE_WRITE_FAILED_REASON,
    INCREMENTAL_HIDDEN_STATE_WRITE_FAILED_REASON, SyncMode, SyncResult,
    UNKNOWN_ALBUM_RELATION_ASSET_REASON, UNPARSABLE_RELATION_DELTA_REASON,
};
use super::super::test_support::{
    album_with_session_and_container, changes_album, changes_album_with_container,
    changes_zone_session, changes_zone_session_with_query_page, hard_deleted_change_record,
    incremental_photo_records, incremental_photo_records_with_favorite, mock_album,
    relation_delete_record, relation_delta_record, seed_complete_album_snapshot,
    seed_downloaded_metadata_asset, test_config, unused_unfiled_changes_pass,
};

#[derive(Clone)]
struct RelationHydrationSession {
    incremental_calls: Arc<AtomicUsize>,
    hydrate_calls: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl PhotosSession for RelationHydrationSession {
    async fn post(
        &self,
        url: &str,
        body: String,
        _headers: &[(&str, &str)],
    ) -> anyhow::Result<Value> {
        if !url.contains("/changes/zone?") {
            return Ok(json!({"records": []}));
        }

        let request: Value = serde_json::from_str(&body)?;
        let sync_token = request["zones"]
            .as_array()
            .and_then(|zones| zones.first())
            .and_then(|zone| zone.get("syncToken"))
            .and_then(Value::as_str);
        let records = if sync_token == Some("zone-token-prev") {
            self.incremental_calls.fetch_add(1, Ordering::SeqCst);
            vec![relation_delta_record(
                "container-vacation",
                "asset-MASTER_HYDRATED",
            )]
        } else {
            self.hydrate_calls.fetch_add(1, Ordering::SeqCst);
            incremental_photo_records("MASTER_HYDRATED")
        };

        Ok(json!({
            "zones": [{
                "zoneID": {"zoneName": "PrimarySync", "ownerRecordName": "_defaultOwner"},
                "syncToken": "zone-token-next",
                "moreComing": false,
                "records": records,
            }]
        }))
    }

    fn clone_box(&self) -> Box<dyn PhotosSession> {
        Box::new(self.clone())
    }
}

fn album_delta_record(container_id: &str, album_name: &str) -> Value {
    json!({
        "recordName": container_id,
        "recordType": "CPLAlbum",
        "fields": {
            "albumName": {"value": album_name}
        }
    })
}

fn deleted_album_delta_record(container_id: &str) -> Value {
    json!({
        "recordName": container_id,
        "recordType": "CPLAlbum",
        "fields": {},
        "deleted": true,
    })
}

fn unparsable_relation_delete_record() -> Value {
    json!({
        "recordName": "not-a-relation-delta",
        "recordType": "CPLContainerRelation",
        "deleted": true
    })
}

fn flagged_incremental_records(record_name: &str, flag: (&str, i64)) -> Vec<Value> {
    let mut records = incremental_photo_records(record_name);
    records[0]["fields"][flag.0] = json!({"value": flag.1, "type": "INT64"});
    records
}

fn flagged_cpl_asset_record(record_name: &str, master_ref: &str, flag: (&str, i64)) -> Value {
    let mut records = incremental_photo_records(master_ref);
    let mut asset = records.remove(1);
    asset["recordName"] = json!(record_name);
    asset["fields"][flag.0] = json!({"value": flag.1, "type": "INT64"});
    asset
}

fn soft_deleted_cpl_asset_record(record_name: &str, master_ref: &str) -> Value {
    flagged_cpl_asset_record(record_name, master_ref, ("isDeleted", 1))
}

fn hidden_cpl_asset_record(record_name: &str, master_ref: &str) -> Value {
    flagged_cpl_asset_record(record_name, master_ref, ("isHidden", 1))
}

fn soft_deleted_cpl_asset_record_without_master_ref(record_name: &str) -> Value {
    let mut asset = soft_deleted_cpl_asset_record(record_name, "MISSING_MASTER_REF");
    asset["fields"].as_object_mut().unwrap().remove("masterRef");
    asset
}

fn assert_source_flags(
    records: &[crate::state::AssetRecord],
    asset_id: &str,
    expected_deleted: bool,
    expected_hidden: bool,
) {
    let record = records
        .iter()
        .find(|record| record.id.as_ref() == asset_id)
        .unwrap_or_else(|| panic!("missing state row for {asset_id}"));
    assert_eq!(
        record.metadata.is_deleted, expected_deleted,
        "is_deleted mismatch for {asset_id}"
    );
    assert_eq!(
        record.metadata.is_hidden, expected_hidden,
        "is_hidden mismatch for {asset_id}"
    );
}

async fn run_incremental_change_records(
    db: Arc<crate::state::SqliteStateDb>,
    records: Vec<Value>,
) -> SyncResult {
    let session = MockPhotosFlow::new()
        .changes_zone_page(records, "zone-token-after", false)
        .build();
    let passes = vec![AlbumPass {
        kind: PassKind::Unfiled,
        album: mock_album("Library", session),
        exclude_ids: Arc::new(FxHashSet::default()),
    }];

    let mut config = test_config();
    config.state_db = Some(db);
    download_photos_incremental(
        &Client::new(),
        &passes,
        &Arc::new(config),
        "zone-token-before",
        DownloadControls::download_hidden(),
        CancellationToken::new(),
    )
    .await
    .unwrap()
}

#[tokio::test]
async fn download_incremental_delete_and_hidden_events_mark_state_without_downloads() {
    let db = Arc::new(crate::state::SqliteStateDb::open_in_memory().unwrap());
    for id in ["SOFT_DELETE", "HARD_DELETE", "HIDDEN_ASSET"] {
        db.upsert_seen(&TestAssetRecord::new(id).build())
            .await
            .unwrap();
    }

    let mut records = Vec::new();
    records.extend(flagged_incremental_records("SOFT_DELETE", ("isDeleted", 1)));
    records.push(hard_deleted_change_record("HARD_DELETE"));
    records.extend(flagged_incremental_records("HIDDEN_ASSET", ("isHidden", 1)));
    records.extend(incremental_photo_records("CREATED_ASSET"));
    let session = MockPhotosFlow::new()
        .changes_zone_page(records, "zone-token-after", false)
        .build();
    let passes = vec![AlbumPass {
        kind: PassKind::Album,
        album: mock_album("Library", session),
        exclude_ids: Arc::new(FxHashSet::default()),
    }];

    let mut config = test_config();
    let dir = TempDir::new().unwrap();
    config.directory = Arc::from(dir.path());
    config.state_db = Some(db.clone());

    let result = download_photos_incremental(
        &Client::new(),
        &passes,
        &Arc::new(config),
        "zone-token-before",
        DownloadControls::new(DownloadRunMode::PrintFilenames, DownloadReporting::hidden()),
        CancellationToken::new(),
    )
    .await
    .unwrap();

    assert!(matches!(result.outcome, DownloadOutcome::Success));
    assert_eq!(
        result.sync_token, None,
        "print-only incremental runs must not advance the sync token"
    );
    let pending = db.get_pending().await.unwrap();
    assert!(
        pending.iter().all(
            |record| record.id.as_ref() != "SOFT_DELETE" && record.id.as_ref() != "HARD_DELETE"
        ),
        "source-deleted pending rows must be pruned: {pending:?}"
    );
    assert_source_flags(&pending, "HIDDEN_ASSET", false, true);
}

#[tokio::test]
async fn incremental_soft_delete_write_error_blocks_sync_token() {
    let db = Arc::new(crate::state::SqliteStateDb::open_in_memory().unwrap());
    db.upsert_seen(&TestAssetRecord::new("SOFT_DELETE_ERR").build())
        .await
        .unwrap();
    {
        let conn = db.acquire_lock("test").unwrap();
        conn.execute("DROP TABLE assets", []).unwrap();
    }
    let result = run_incremental_change_records(
        db,
        flagged_incremental_records("SOFT_DELETE_ERR", ("isDeleted", 1)),
    )
    .await;

    assert!(matches!(
        result.outcome,
        DownloadOutcome::PartialFailure { failed_count: 1 }
    ));
    assert_eq!(result.sync_token, None);
    assert_eq!(result.stats.state_write_failures, 1);
    assert!(result.stats.sync_token_blocked);
    assert_eq!(
        result.stats.sync_token_blocked_reason,
        Some(INCREMENTAL_DELETE_STATE_WRITE_FAILED_REASON)
    );
}

#[tokio::test]
async fn incremental_hidden_write_error_blocks_sync_token() {
    let db = Arc::new(crate::state::SqliteStateDb::open_in_memory().unwrap());
    db.upsert_seen(&TestAssetRecord::new("HIDDEN_ERR").build())
        .await
        .unwrap();
    {
        let conn = db.acquire_lock("test").unwrap();
        conn.execute("DROP TABLE assets", []).unwrap();
    }
    let result = run_incremental_change_records(
        db,
        flagged_incremental_records("HIDDEN_ERR", ("isHidden", 1)),
    )
    .await;

    assert!(matches!(
        result.outcome,
        DownloadOutcome::PartialFailure { failed_count: 1 }
    ));
    assert_eq!(result.sync_token, None);
    assert_eq!(result.stats.state_write_failures, 1);
    assert!(result.stats.sync_token_blocked);
    assert_eq!(
        result.stats.sync_token_blocked_reason,
        Some(INCREMENTAL_HIDDEN_STATE_WRITE_FAILED_REASON)
    );
}

#[tokio::test]
async fn incremental_untracked_cplmaster_soft_delete_zero_rows_advances_sync_token() {
    let db = Arc::new(crate::state::SqliteStateDb::open_in_memory().unwrap());
    let result = run_incremental_change_records(
        db.clone(),
        flagged_incremental_records("UNTRACKED_SOFT_DELETE", ("isDeleted", 1)),
    )
    .await;

    assert!(matches!(result.outcome, DownloadOutcome::Success));
    assert_eq!(result.sync_token.as_deref(), Some("zone-token-after"));
    assert_eq!(result.stats.state_write_failures, 0);
    assert!(!result.stats.sync_token_blocked);
    assert!(db.get_pending().await.unwrap().is_empty());
}

#[tokio::test]
async fn incremental_untracked_cplasset_soft_delete_zero_rows_advances_sync_token() {
    let db = Arc::new(crate::state::SqliteStateDb::open_in_memory().unwrap());
    let result = run_incremental_change_records(
        db,
        vec![soft_deleted_cpl_asset_record(
            "asset-UNTRACKED_SOFT_DELETE",
            "UNTRACKED_SOFT_DELETE",
        )],
    )
    .await;

    assert!(matches!(result.outcome, DownloadOutcome::Success));
    assert_eq!(result.sync_token.as_deref(), Some("zone-token-after"));
    assert_eq!(result.stats.state_write_failures, 0);
    assert!(!result.stats.sync_token_blocked);
}

#[tokio::test]
async fn incremental_untracked_cplasset_soft_delete_without_master_ref_advances_sync_token() {
    let db = Arc::new(crate::state::SqliteStateDb::open_in_memory().unwrap());
    let result = run_incremental_change_records(
        db,
        vec![soft_deleted_cpl_asset_record_without_master_ref(
            "asset-UNTRACKED_SOFT_DELETE",
        )],
    )
    .await;

    assert!(matches!(result.outcome, DownloadOutcome::Success));
    assert_eq!(result.sync_token.as_deref(), Some("zone-token-after"));
    assert_eq!(result.stats.state_write_failures, 0);
    assert!(!result.stats.sync_token_blocked);
}

#[tokio::test]
async fn incremental_cplasset_soft_delete_prunes_master_ref_pending_row() {
    let db = Arc::new(crate::state::SqliteStateDb::open_in_memory().unwrap());
    db.upsert_seen(&TestAssetRecord::new("TRACKED_MASTER").build())
        .await
        .unwrap();
    let result = run_incremental_change_records(
        db.clone(),
        vec![soft_deleted_cpl_asset_record(
            "asset-TRACKED_MASTER",
            "TRACKED_MASTER",
        )],
    )
    .await;

    assert!(matches!(result.outcome, DownloadOutcome::Success));
    assert_eq!(result.sync_token.as_deref(), Some("zone-token-after"));
    assert_eq!(result.stats.state_write_failures, 0);
    assert!(!result.stats.sync_token_blocked);
    assert!(
        db.get_pending().await.unwrap().is_empty(),
        "source-deleted pending row should be removed"
    );
}

#[tokio::test]
async fn incremental_cplasset_hidden_marks_master_ref_row() {
    let db = Arc::new(crate::state::SqliteStateDb::open_in_memory().unwrap());
    db.upsert_seen(&TestAssetRecord::new("TRACKED_HIDDEN_MASTER").build())
        .await
        .unwrap();
    let result = run_incremental_change_records(
        db.clone(),
        vec![hidden_cpl_asset_record(
            "asset-TRACKED_HIDDEN_MASTER",
            "TRACKED_HIDDEN_MASTER",
        )],
    )
    .await;

    assert!(matches!(result.outcome, DownloadOutcome::Success));
    assert_eq!(result.sync_token.as_deref(), Some("zone-token-after"));
    assert_eq!(result.stats.state_write_failures, 0);
    assert!(!result.stats.sync_token_blocked);
    let pending = db.get_pending().await.unwrap();
    assert_source_flags(&pending, "TRACKED_HIDDEN_MASTER", false, true);
}

#[tokio::test]
async fn incremental_cplasset_soft_delete_prunes_sibling_pending_row_before_master_ref() {
    let db = Arc::new(crate::state::SqliteStateDb::open_in_memory().unwrap());
    db.upsert_seen(&TestAssetRecord::new("TRACKED_MASTER").build())
        .await
        .unwrap();
    db.upsert_seen(&TestAssetRecord::new("asset-TRACKED_SIBLING").build())
        .await
        .unwrap();
    db.upsert_asset_master_mapping("PrimarySync", "asset-TRACKED_SIBLING", "TRACKED_MASTER")
        .await
        .unwrap();

    let result = run_incremental_change_records(
        db.clone(),
        vec![soft_deleted_cpl_asset_record(
            "asset-TRACKED_SIBLING",
            "TRACKED_MASTER",
        )],
    )
    .await;

    assert!(matches!(result.outcome, DownloadOutcome::Success));
    assert_eq!(result.sync_token.as_deref(), Some("zone-token-after"));
    assert_eq!(result.stats.state_write_failures, 0);
    assert!(!result.stats.sync_token_blocked);
    let pending = db.get_pending().await.unwrap();
    assert_source_flags(&pending, "TRACKED_MASTER", false, false);
    assert!(
        pending
            .iter()
            .all(|record| record.id.as_ref() != "asset-TRACKED_SIBLING"),
        "source-deleted sibling pending row should be removed: {pending:?}"
    );
}

#[tokio::test]
async fn incremental_cplasset_hidden_marks_sibling_state_row_before_master_ref() {
    let db = Arc::new(crate::state::SqliteStateDb::open_in_memory().unwrap());
    db.upsert_seen(&TestAssetRecord::new("TRACKED_HIDDEN_MASTER").build())
        .await
        .unwrap();
    db.upsert_seen(&TestAssetRecord::new("asset-TRACKED_HIDDEN_SIBLING").build())
        .await
        .unwrap();
    db.upsert_asset_master_mapping(
        "PrimarySync",
        "asset-TRACKED_HIDDEN_SIBLING",
        "TRACKED_HIDDEN_MASTER",
    )
    .await
    .unwrap();

    let result = run_incremental_change_records(
        db.clone(),
        vec![hidden_cpl_asset_record(
            "asset-TRACKED_HIDDEN_SIBLING",
            "TRACKED_HIDDEN_MASTER",
        )],
    )
    .await;

    assert!(matches!(result.outcome, DownloadOutcome::Success));
    assert_eq!(result.sync_token.as_deref(), Some("zone-token-after"));
    assert_eq!(result.stats.state_write_failures, 0);
    assert!(!result.stats.sync_token_blocked);
    let pending = db.get_pending().await.unwrap();
    assert_source_flags(&pending, "TRACKED_HIDDEN_MASTER", false, false);
    assert_source_flags(&pending, "asset-TRACKED_HIDDEN_SIBLING", false, true);
}

#[tokio::test]
async fn incremental_change_persists_asset_master_mapping() {
    let db = Arc::new(crate::state::SqliteStateDb::open_in_memory().unwrap());
    let result = run_incremental_change_records(
        db.clone(),
        flagged_incremental_records("MAPPED_MASTER", ("isDeleted", 1)),
    )
    .await;

    assert!(matches!(result.outcome, DownloadOutcome::Success));
    assert_eq!(
        db.get_master_record_name_for_asset("PrimarySync", "asset-MAPPED_MASTER")
            .await
            .unwrap()
            .as_deref(),
        Some("MAPPED_MASTER")
    );
}

#[tokio::test]
async fn incremental_untracked_hidden_zero_rows_advances_sync_token() {
    let db = Arc::new(crate::state::SqliteStateDb::open_in_memory().unwrap());
    let result = run_incremental_change_records(
        db,
        flagged_incremental_records("UNTRACKED_HIDDEN", ("isHidden", 1)),
    )
    .await;

    assert!(matches!(result.outcome, DownloadOutcome::Success));
    assert_eq!(result.sync_token.as_deref(), Some("zone-token-after"));
    assert_eq!(result.stats.state_write_failures, 0);
    assert!(!result.stats.sync_token_blocked);
}

#[tokio::test]
async fn incremental_unresolved_hard_delete_zero_rows_advances_sync_token() {
    let db = Arc::new(crate::state::SqliteStateDb::open_in_memory().unwrap());
    db.upsert_seen(&TestAssetRecord::new("TRACKED_MASTER").build())
        .await
        .unwrap();
    let result = run_incremental_change_records(
        db.clone(),
        vec![hard_deleted_change_record("asset-TRACKED_MASTER")],
    )
    .await;

    assert!(matches!(result.outcome, DownloadOutcome::Success));
    assert_eq!(result.sync_token.as_deref(), Some("zone-token-after"));
    assert_eq!(result.stats.state_write_failures, 0);
    assert!(!result.stats.sync_token_blocked);
    let pending = db.get_pending().await.unwrap();
    assert_source_flags(&pending, "TRACKED_MASTER", false, false);
}

#[tokio::test]
async fn incremental_unresolved_master_hard_delete_prunes_sibling_pending_rows() {
    let db = Arc::new(crate::state::SqliteStateDb::open_in_memory().unwrap());
    db.upsert_seen(&TestAssetRecord::new("TRACKED_MASTER").build())
        .await
        .unwrap();
    db.upsert_seen(&TestAssetRecord::new("asset-SIBLING").build())
        .await
        .unwrap();
    db.upsert_asset_master_mapping("PrimarySync", "asset-SIBLING", "TRACKED_MASTER")
        .await
        .unwrap();

    let result = run_incremental_change_records(
        db.clone(),
        vec![hard_deleted_change_record("TRACKED_MASTER")],
    )
    .await;

    assert!(matches!(result.outcome, DownloadOutcome::Success));
    assert_eq!(result.sync_token.as_deref(), Some("zone-token-after"));
    assert_eq!(result.stats.state_write_failures, 0);
    assert!(!result.stats.sync_token_blocked);
    assert!(
        db.get_pending().await.unwrap().is_empty(),
        "master-family hard delete should remove pending retry rows"
    );
}

#[tokio::test]
async fn incremental_hard_delete_uses_library_scoped_asset_master_mapping() {
    let db = Arc::new(crate::state::SqliteStateDb::open_in_memory().unwrap());
    db.upsert_seen(
        &TestAssetRecord::new("PRIMARY_MASTER")
            .library("PrimarySync")
            .build(),
    )
    .await
    .unwrap();
    db.upsert_seen(
        &TestAssetRecord::new("SHARED_MASTER")
            .library("SharedSync-AAAA")
            .build(),
    )
    .await
    .unwrap();
    db.upsert_asset_master_mapping("PrimarySync", "asset-SAME", "PRIMARY_MASTER")
        .await
        .unwrap();
    db.upsert_asset_master_mapping("SharedSync-AAAA", "asset-SAME", "SHARED_MASTER")
        .await
        .unwrap();

    let result =
        run_incremental_change_records(db.clone(), vec![hard_deleted_change_record("asset-SAME")])
            .await;

    assert!(matches!(result.outcome, DownloadOutcome::Success));
    assert_eq!(result.sync_token.as_deref(), Some("zone-token-after"));
    assert_eq!(result.stats.state_write_failures, 0);
    assert!(!result.stats.sync_token_blocked);

    let pending = db.get_pending().await.unwrap();
    assert!(
        pending
            .iter()
            .all(|record| record.library.as_ref() != "PrimarySync"),
        "primary source-deleted pending row should be removed: {pending:?}"
    );
    let shared = pending
        .iter()
        .find(|record| record.library.as_ref() == "SharedSync-AAAA")
        .expect("shared row");
    assert_eq!(shared.id.as_ref(), "SHARED_MASTER");
    assert!(
        !shared.metadata.is_deleted,
        "same CPLAsset record name in another library must stay isolated"
    );
}

#[tokio::test]
async fn incremental_hard_delete_recovers_mapping_from_album_history() {
    let db = Arc::new(crate::state::SqliteStateDb::open_in_memory().unwrap());
    db.upsert_seen(
        &TestAssetRecord::new("TRACKED_MASTER")
            .library("PrimarySync")
            .build(),
    )
    .await
    .unwrap();
    db.mark_downloaded(
        "PrimarySync",
        "TRACKED_MASTER",
        VersionSizeKey::Original.as_str(),
        std::path::Path::new("/tmp/codex/kei/tests/tracked-master.jpg"),
        "seeded-local-sha256",
        None,
    )
    .await
    .unwrap();
    db.upsert_album_container("PrimarySync", "container-a", "Vacation", "album")
        .await
        .unwrap();
    db.upsert_album_membership_delta(
        "PrimarySync",
        "container-a",
        "asset-TRACKED_MASTER",
        Some("TRACKED_MASTER"),
        "icloud",
    )
    .await
    .unwrap();
    assert_eq!(
        db.get_master_record_name_for_asset("PrimarySync", "asset-TRACKED_MASTER")
            .await
            .unwrap(),
        None
    );

    let session = MockPhotosFlow::new()
        .changes_zone_page(
            vec![hard_deleted_change_record("asset-TRACKED_MASTER")],
            "zone-token-after",
            false,
        )
        .build();
    let passes = vec![AlbumPass {
        kind: PassKind::Unfiled,
        album: mock_album("Library", session),
        exclude_ids: Arc::new(FxHashSet::default()),
    }];
    let mut config = test_config();
    config.sync_mode = SyncMode::Incremental {
        zone_sync_token: "zone-token-before".to_string(),
    };
    config.state_db = Some(db.clone());

    let result = download_photos_with_sync(
        &Client::new(),
        &passes,
        Arc::new(config),
        DownloadControls::download_hidden(),
        CancellationToken::new(),
    )
    .await
    .unwrap();

    assert!(matches!(result.outcome, DownloadOutcome::Success));
    assert_eq!(result.sync_token.as_deref(), Some("zone-token-after"));
    assert_eq!(result.stats.state_write_failures, 0);
    assert!(!result.stats.sync_token_blocked);
    assert_eq!(
        db.get_master_record_name_for_asset("PrimarySync", "asset-TRACKED_MASTER")
            .await
            .unwrap()
            .as_deref(),
        Some("TRACKED_MASTER")
    );
    let is_deleted: i64 = db
        .acquire_lock("test")
        .unwrap()
        .query_row(
            "SELECT is_deleted FROM assets \
             WHERE library = 'PrimarySync' AND id = 'TRACKED_MASTER'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(is_deleted, 1);
}

#[tokio::test]
async fn changes_stream_mixed_malformed_asset_relation_preserves_valid_and_blocks_token() {
    let db = Arc::new(crate::state::SqliteStateDb::open_in_memory().unwrap());
    db.upsert_album_container("PrimarySync", "container-a", "Vacation", "album")
        .await
        .unwrap();
    let mut malformed = incremental_photo_records("MALFORMED_ASSET");
    malformed[0]["fields"]["resOriginalRes"]["value"]
        .as_object_mut()
        .expect("resource value object")
        .remove("downloadURL");
    let mut records = incremental_photo_records("VALID_ASSET");
    records.extend(malformed);
    records.push(relation_delta_record("container-a", "asset-VALID_ASSET"));
    let session = MockPhotosFlow::new()
        .changes_zone_page(records, "zone-token-after", false)
        .build();
    let passes = vec![
        AlbumPass {
            kind: PassKind::Album,
            album: changes_album_with_container("Vacation", Some("container-a"), session),
            exclude_ids: Arc::new(FxHashSet::default()),
        },
        unused_unfiled_changes_pass(),
    ];

    let mut config = test_config();
    let dir = TempDir::new().unwrap();
    config.directory = Arc::from(dir.path());
    config.state_db = Some(db.clone());
    let result = download_photos_incremental(
        &Client::new(),
        &passes,
        &Arc::new(config),
        "zone-token-before",
        DownloadControls::new(DownloadRunMode::PrintFilenames, DownloadReporting::hidden()),
        CancellationToken::new(),
    )
    .await
    .unwrap();

    assert!(matches!(
        result.outcome,
        DownloadOutcome::PartialFailure { failed_count: 1 }
    ));
    assert_eq!(result.sync_token, None);
    assert_eq!(result.stats.enumeration_errors, 1);
    let pending = db.get_pending().await.unwrap();
    assert!(
        pending
            .iter()
            .any(|record| record.id.as_ref() == "asset-VALID_ASSET"),
        "valid incremental asset should still be recorded for the planned work"
    );
    let memberships = db
        .get_live_selected_album_memberships_for_asset(
            "PrimarySync",
            "asset-VALID_ASSET",
            &["container-a"],
        )
        .await
        .unwrap();
    assert_eq!(memberships.len(), 1);
}

#[tokio::test]
async fn asset_only_delta_recovers_identity_refreshes_metadata_and_advances_token() {
    #[derive(Debug, Clone, Copy)]
    enum IdentitySource {
        Delta,
        State,
        ProviderLookup,
    }

    for identity_source in [
        IdentitySource::Delta,
        IdentitySource::State,
        IdentitySource::ProviderLookup,
    ] {
        let db = Arc::new(SqliteStateDb::open_in_memory().expect("state db"));
        let dir = TempDir::new().expect("temp dir");
        let stored_records = incremental_photo_records_with_favorite("ASSET_ONLY", false);
        let stored_asset = PhotoAsset::new(stored_records[0].clone(), stored_records[1].clone());
        let changed_records = incremental_photo_records_with_favorite("ASSET_ONLY", true);
        let mut delta_record = changed_records[1].clone();
        if !matches!(identity_source, IdentitySource::Delta) {
            delta_record["fields"]
                .as_object_mut()
                .expect("asset fields")
                .remove("masterRef");
        }
        if matches!(identity_source, IdentitySource::State) {
            db.upsert_asset_master_mapping("PrimarySync", "asset-ASSET_ONLY", "ASSET_ONLY")
                .await
                .expect("seed durable mapping");
        }
        let session = changes_zone_session_with_query_page(
            Arc::new(AtomicUsize::new(0)),
            vec![delta_record],
            json!({"records": changed_records}),
            0,
        );
        let session_probe = session.clone();
        let pass = AlbumPass {
            kind: PassKind::Unfiled,
            album: changes_album("", session),
            exclude_ids: Arc::new(FxHashSet::default()),
        };
        let mut config = test_config();
        config.directory = Arc::from(dir.path());
        config.state_db = Some(Arc::clone(&db) as Arc<dyn DownloadStore>);
        let media_path =
            seed_downloaded_metadata_asset(db.as_ref(), &config, &pass, &stored_asset).await;

        let result = download_photos_incremental(
            &Client::new(),
            std::slice::from_ref(&pass),
            &Arc::new(config),
            "zone-token-prev",
            DownloadControls::download_hidden(),
            CancellationToken::new(),
        )
        .await
        .expect("asset-only delta should hydrate");

        assert!(matches!(result.outcome, DownloadOutcome::Success));
        assert_eq!(result.stats.downloaded, 0, "{identity_source:?}");
        assert_eq!(result.sync_token.as_deref(), Some("zone-token-next"));
        assert!(!result.stats.sync_token_blocked);
        assert_eq!(
            session_probe.records_query_count(),
            match identity_source {
                IdentitySource::ProviderLookup => 2,
                IdentitySource::Delta | IdentitySource::State => 1,
            },
            "{identity_source:?}"
        );
        assert_eq!(
            db.get_master_record_name_for_asset("PrimarySync", "asset-ASSET_ONLY")
                .await
                .expect("read durable mapping")
                .as_deref(),
            Some("ASSET_ONLY"),
            "{identity_source:?}"
        );
        let refreshed = db
            .get_downloaded_page(0, 1)
            .await
            .expect("read refreshed row")
            .remove(0);
        assert!(refreshed.metadata.is_favorite);
        assert!(media_path.exists());
    }
}

#[tokio::test]
async fn unresolved_asset_only_delta_preserves_incremental_token() {
    let records = incremental_photo_records_with_favorite("ASSET_UNKNOWN", true);
    let session = changes_zone_session_with_query_page(
        Arc::new(AtomicUsize::new(0)),
        vec![records[1].clone()],
        json!({"records": []}),
        0,
    );
    let pass = AlbumPass {
        kind: PassKind::Unfiled,
        album: changes_album("", session),
        exclude_ids: Arc::new(FxHashSet::default()),
    };
    let mut config = test_config();
    config.state_db = Some(Arc::new(SqliteStateDb::open_in_memory().expect("state db")));

    let result = download_photos_incremental(
        &Client::new(),
        std::slice::from_ref(&pass),
        &Arc::new(config),
        "zone-token-prev",
        DownloadControls::download_hidden(),
        CancellationToken::new(),
    )
    .await
    .expect("inconclusive hydration should remain a safe partial sync");

    assert_eq!(result.sync_token, None);
    assert!(result.stats.sync_token_blocked);
    assert_eq!(
        result.stats.sync_token_blocked_reason,
        Some(ASSET_DELTA_HYDRATION_INCOMPLETE_REASON)
    );
}

#[tokio::test]
async fn asset_only_delta_master_deletion_tombstones_entire_family() {
    let db = Arc::new(SqliteStateDb::open_in_memory().expect("state db"));
    for record_name in [
        "ASSET_ONLY_DELETED",
        "asset-ASSET_ONLY_DELETED",
        "asset-ASSET_ONLY_DELETED-SIBLING",
    ] {
        db.upsert_seen(&TestAssetRecord::new(record_name).build())
            .await
            .expect("seed family state row");
    }
    for asset_record_name in [
        "asset-ASSET_ONLY_DELETED",
        "asset-ASSET_ONLY_DELETED-SIBLING",
    ] {
        db.upsert_asset_master_mapping("PrimarySync", asset_record_name, "ASSET_ONLY_DELETED")
            .await
            .expect("seed family mapping");
    }

    let records = incremental_photo_records("ASSET_ONLY_DELETED");
    let session = changes_zone_session_with_query_page(
        Arc::new(AtomicUsize::new(0)),
        vec![records[1].clone()],
        json!({
            "records": [{
                "recordName": "ASSET_ONLY_DELETED",
                "serverErrorCode": "UNKNOWN_ITEM",
                "reason": "record not found"
            }]
        }),
        0,
    );
    let pass = AlbumPass {
        kind: PassKind::Unfiled,
        album: changes_album("", session),
        exclude_ids: Arc::new(FxHashSet::default()),
    };
    let mut config = test_config();
    config.state_db = Some(Arc::clone(&db) as Arc<dyn DownloadStore>);

    let result = download_photos_incremental(
        &Client::new(),
        std::slice::from_ref(&pass),
        &Arc::new(config),
        "zone-token-prev",
        DownloadControls::download_hidden(),
        CancellationToken::new(),
    )
    .await
    .expect("master deletion should resolve the complete family");

    assert!(matches!(result.outcome, DownloadOutcome::Success));
    assert_eq!(result.sync_token.as_deref(), Some("zone-token-next"));
    assert!(!result.stats.sync_token_blocked);
    let summary = db.get_summary().await.expect("read state summary");
    assert_eq!(summary.source_deleted, 3);
    assert_eq!(summary.pending, 0);
}

#[tokio::test]
async fn incremental_relation_add_before_photo_routes_to_album_not_unfiled() {
    let db = Arc::new(SqliteStateDb::open_in_memory().expect("state db"));
    seed_complete_album_snapshot(&db, "container-vacation", "Vacation", &[]).await;
    let mut records = vec![relation_delta_record(
        "container-vacation",
        "asset-MASTER_CHANGED",
    )];
    records.extend(incremental_photo_records("MASTER_CHANGED"));
    let calls = Arc::new(AtomicUsize::new(0));
    let session = changes_zone_session(Arc::clone(&calls), records);
    let passes = vec![
        AlbumPass {
            kind: PassKind::Album,
            album: changes_album_with_container("Vacation", Some("container-vacation"), session),
            exclude_ids: Arc::new(FxHashSet::default()),
        },
        unused_unfiled_changes_pass(),
    ];

    let mut config = test_config();
    let dir = TempDir::new().expect("temp dir");
    config.directory = Arc::from(dir.path());
    config.folder_structure = "Unfiled".to_string();
    config.folder_structure_albums = Arc::from("{album}");
    config.state_db = Some(Arc::clone(&db) as Arc<dyn DownloadStore>);
    let result = download_photos_incremental(
        &Client::new(),
        &passes,
        &Arc::new(config),
        "zone-token-prev",
        DownloadControls::new(DownloadRunMode::PrintFilenames, DownloadReporting::hidden()),
        CancellationToken::new(),
    )
    .await
    .expect("relation-add routing should succeed");

    assert!(matches!(result.outcome, DownloadOutcome::Success));
    let album_rows = db.get_all_asset_albums("PrimarySync").await.unwrap();
    assert_eq!(
        album_rows,
        vec![("asset-MASTER_CHANGED".to_string(), "Vacation".to_string())],
        "relation add should route the photo event through the album pass"
    );
}

#[tokio::test]
async fn incremental_relation_delete_before_photo_routes_to_unfiled() {
    let db = Arc::new(SqliteStateDb::open_in_memory().expect("state db"));
    seed_complete_album_snapshot(
        &db,
        "container-vacation",
        "Vacation",
        &[("asset-MASTER_CHANGED", "MASTER_CHANGED")],
    )
    .await;
    let mut records = vec![relation_delete_record(
        "container-vacation",
        "asset-MASTER_CHANGED",
    )];
    records.extend(incremental_photo_records("MASTER_CHANGED"));
    let calls = Arc::new(AtomicUsize::new(0));
    let session = changes_zone_session(Arc::clone(&calls), records);
    let passes = vec![
        AlbumPass {
            kind: PassKind::Album,
            album: changes_album_with_container("Vacation", Some("container-vacation"), session),
            exclude_ids: Arc::new(FxHashSet::default()),
        },
        unused_unfiled_changes_pass(),
    ];

    let mut config = test_config();
    let dir = TempDir::new().expect("temp dir");
    config.directory = Arc::from(dir.path());
    config.folder_structure = "Unfiled".to_string();
    config.folder_structure_albums = Arc::from("{album}");
    config.state_db = Some(Arc::clone(&db) as Arc<dyn DownloadStore>);
    let result = download_photos_incremental(
        &Client::new(),
        &passes,
        &Arc::new(config),
        "zone-token-prev",
        DownloadControls::new(DownloadRunMode::PrintFilenames, DownloadReporting::hidden()),
        CancellationToken::new(),
    )
    .await
    .expect("relation-delete routing should succeed");

    assert!(matches!(result.outcome, DownloadOutcome::Success));
    let album_rows = db.get_all_asset_albums("PrimarySync").await.unwrap();
    assert!(
        album_rows.is_empty(),
        "relation delete should route the photo event only through the unfiled pass"
    );
}

#[tokio::test]
async fn selected_relation_add_without_photo_blocks_incremental_token() {
    let db = Arc::new(SqliteStateDb::open_in_memory().expect("state db"));
    seed_complete_album_snapshot(&db, "container-vacation", "Vacation", &[]).await;
    let calls = Arc::new(AtomicUsize::new(0));
    let session = changes_zone_session(
        Arc::clone(&calls),
        vec![relation_delta_record(
            "container-vacation",
            "asset-MASTER_UNKNOWN",
        )],
    );
    let passes = vec![AlbumPass {
        kind: PassKind::Album,
        album: changes_album_with_container("Vacation", Some("container-vacation"), session),
        exclude_ids: Arc::new(FxHashSet::default()),
    }];

    let mut config = test_config();
    config.state_db = Some(Arc::clone(&db) as Arc<dyn DownloadStore>);
    let result = download_photos_incremental(
        &Client::new(),
        &passes,
        &Arc::new(config),
        "zone-token-prev",
        DownloadControls::new(DownloadRunMode::Download, DownloadReporting::hidden()),
        CancellationToken::new(),
    )
    .await
    .expect("unknown selected relation add should not fall back to full here");

    assert!(matches!(result.outcome, DownloadOutcome::Success));
    assert_eq!(result.sync_token, None);
    assert!(result.stats.sync_token_blocked);
    assert_eq!(
        result.stats.sync_token_blocked_reason,
        Some(UNKNOWN_ALBUM_RELATION_ASSET_REASON)
    );
}

#[tokio::test]
async fn selected_relation_add_without_photo_uses_persisted_asset_master_mapping() {
    let db = Arc::new(SqliteStateDb::open_in_memory().expect("state db"));
    seed_complete_album_snapshot(&db, "container-vacation", "Vacation", &[]).await;
    db.upsert_asset_master_mapping("PrimarySync", "asset-MASTER_KNOWN", "MASTER_KNOWN")
        .await
        .unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let session = changes_zone_session(
        Arc::clone(&calls),
        vec![relation_delta_record(
            "container-vacation",
            "asset-MASTER_KNOWN",
        )],
    );
    let passes = vec![AlbumPass {
        kind: PassKind::Album,
        album: changes_album_with_container("Vacation", Some("container-vacation"), session),
        exclude_ids: Arc::new(FxHashSet::default()),
    }];

    let mut config = test_config();
    config.state_db = Some(Arc::clone(&db) as Arc<dyn DownloadStore>);
    let result = download_photos_incremental(
        &Client::new(),
        &passes,
        &Arc::new(config),
        "zone-token-prev",
        DownloadControls::new(DownloadRunMode::Download, DownloadReporting::hidden()),
        CancellationToken::new(),
    )
    .await
    .expect("known selected relation add should not fall back to full here");

    assert!(matches!(result.outcome, DownloadOutcome::Success));
    assert_eq!(result.sync_token.as_deref(), Some("zone-token-next"));
    assert!(!result.stats.sync_token_blocked);
    let memberships = db
        .get_live_selected_album_memberships_for_asset(
            "PrimarySync",
            "asset-MASTER_KNOWN",
            &["container-vacation"],
        )
        .await
        .unwrap();
    assert_eq!(memberships.len(), 1);
    assert_eq!(
        memberships[0].master_record_name.as_deref(),
        Some("MASTER_KNOWN")
    );
}

#[tokio::test]
async fn selected_relation_add_without_photo_hydrates_missing_asset() {
    let db = Arc::new(SqliteStateDb::open_in_memory().expect("state db"));
    seed_complete_album_snapshot(&db, "container-vacation", "Vacation", &[]).await;
    let session = RelationHydrationSession {
        incremental_calls: Arc::new(AtomicUsize::new(0)),
        hydrate_calls: Arc::new(AtomicUsize::new(0)),
    };
    let passes = vec![AlbumPass {
        kind: PassKind::Album,
        album: changes_album_with_container(
            "Vacation",
            Some("container-vacation"),
            session.clone(),
        ),
        exclude_ids: Arc::new(FxHashSet::default()),
    }];

    let mut config = test_config();
    config.state_db = Some(Arc::clone(&db) as Arc<dyn DownloadStore>);
    let result = download_photos_incremental(
        &Client::new(),
        &passes,
        &Arc::new(config),
        "zone-token-prev",
        DownloadControls::new(DownloadRunMode::PrintFilenames, DownloadReporting::hidden()),
        CancellationToken::new(),
    )
    .await
    .expect("missing selected relation asset should hydrate");

    assert!(matches!(result.outcome, DownloadOutcome::Success));
    assert!(!result.stats.sync_token_blocked);
    assert_eq!(
        session.incremental_calls.load(Ordering::SeqCst),
        1,
        "incremental changes should be read once"
    );
    assert_eq!(
        session.hydrate_calls.load(Ordering::SeqCst),
        1,
        "missing relation asset should trigger one bounded hydrate scan"
    );
    let memberships = db
        .get_live_selected_album_memberships_for_asset(
            "PrimarySync",
            "asset-MASTER_HYDRATED",
            &["container-vacation"],
        )
        .await
        .unwrap();
    assert_eq!(memberships.len(), 1);
    assert_eq!(
        memberships[0].master_record_name.as_deref(),
        Some("MASTER_HYDRATED")
    );
}

#[tokio::test]
async fn incremental_relation_add_unknown_unselected_container_advances_sync_token() {
    let db = Arc::new(SqliteStateDb::open_in_memory().expect("state db"));
    let calls = Arc::new(AtomicUsize::new(0));
    let session = changes_zone_session(
        Arc::clone(&calls),
        vec![relation_delta_record(
            "container-missing",
            "asset-MASTER_UNKNOWN",
        )],
    );
    let passes = vec![AlbumPass {
        kind: PassKind::Unfiled,
        album: changes_album("", session),
        exclude_ids: Arc::new(FxHashSet::default()),
    }];

    let mut config = test_config();
    config.state_db = Some(db);
    let result = download_photos_incremental(
        &Client::new(),
        &passes,
        &Arc::new(config),
        "zone-token-prev",
        DownloadControls::new(DownloadRunMode::Download, DownloadReporting::hidden()),
        CancellationToken::new(),
    )
    .await
    .expect("unknown relation container should not fall back to full here");

    assert!(matches!(result.outcome, DownloadOutcome::Success));
    assert_eq!(result.sync_token.as_deref(), Some("zone-token-next"));
    assert!(!result.stats.sync_token_blocked);
}

#[tokio::test]
async fn incremental_album_delta_delete_invalidates_snapshot_through_download_flow() {
    let db = Arc::new(SqliteStateDb::open_in_memory().expect("state db"));
    seed_complete_album_snapshot(
        &db,
        "container-vacation",
        "Vacation",
        &[("asset-MASTER_OLD", "MASTER_OLD")],
    )
    .await;
    let calls = Arc::new(AtomicUsize::new(0));
    let session = changes_zone_session(
        Arc::clone(&calls),
        vec![deleted_album_delta_record("container-vacation")],
    );
    let passes = vec![AlbumPass {
        kind: PassKind::Album,
        album: changes_album_with_container("Vacation", Some("container-vacation"), session),
        exclude_ids: Arc::new(FxHashSet::default()),
    }];

    let mut config = test_config();
    config.state_db = Some(Arc::clone(&db) as Arc<dyn DownloadStore>);
    let result = download_photos_incremental(
        &Client::new(),
        &passes,
        &Arc::new(config),
        "zone-token-prev",
        DownloadControls::new(DownloadRunMode::Download, DownloadReporting::hidden()),
        CancellationToken::new(),
    )
    .await
    .expect("album delete delta should be applied through incremental flow");

    assert!(matches!(result.outcome, DownloadOutcome::Success));
    assert_eq!(result.sync_token.as_deref(), Some("zone-token-next"));
    assert!(
        !db.selected_album_containers_have_complete_snapshots(
            "PrimarySync",
            &["container-vacation"]
        )
        .await
        .unwrap(),
        "deleted album delta must invalidate trusted membership snapshots"
    );
}

#[tokio::test]
async fn incremental_relation_add_writes_membership_after_album_delta() {
    let db = Arc::new(SqliteStateDb::open_in_memory().expect("state db"));
    let calls = Arc::new(AtomicUsize::new(0));
    let session = changes_zone_session(
        Arc::clone(&calls),
        vec![
            relation_delta_record("container-a", "asset-record-a"),
            album_delta_record("container-a", "Vacation"),
        ],
    );
    let passes = vec![AlbumPass {
        kind: PassKind::Unfiled,
        album: changes_album("", session),
        exclude_ids: Arc::new(FxHashSet::default()),
    }];

    let mut config = test_config();
    config.state_db = Some(db.clone());
    let result = download_photos_incremental(
        &Client::new(),
        &passes,
        &Arc::new(config),
        "zone-token-prev",
        DownloadControls::new(DownloadRunMode::Download, DownloadReporting::hidden()),
        CancellationToken::new(),
    )
    .await
    .expect("incremental relation delta should succeed");

    assert!(matches!(result.outcome, DownloadOutcome::Success));
    assert_eq!(result.sync_token.as_deref(), Some("zone-token-next"));
    let memberships = db
        .get_live_selected_album_memberships_for_asset(
            "PrimarySync",
            "asset-record-a",
            &["container-a"],
        )
        .await
        .unwrap();
    assert_eq!(memberships.len(), 1);
    assert_eq!(memberships[0].container_id, "container-a");
}

#[tokio::test]
async fn incremental_relation_delete_marks_membership_deleted() {
    let db = Arc::new(SqliteStateDb::open_in_memory().expect("state db"));
    db.upsert_album_container("PrimarySync", "container-a", "Vacation", "album")
        .await
        .unwrap();
    db.upsert_album_membership_delta(
        "PrimarySync",
        "container-a",
        "asset-record-a",
        Some("master-a"),
        "icloud",
    )
    .await
    .unwrap();

    let calls = Arc::new(AtomicUsize::new(0));
    let session = changes_zone_session(
        Arc::clone(&calls),
        vec![relation_delete_record("container-a", "asset-record-a")],
    );
    let passes = vec![AlbumPass {
        kind: PassKind::Album,
        album: album_with_session_and_container(
            "PrimarySync",
            "Vacation",
            Some("container-a"),
            Box::new(session),
        ),
        exclude_ids: Arc::new(FxHashSet::default()),
    }];

    let mut config = test_config();
    config.state_db = Some(db.clone());
    let result = download_photos_incremental(
        &Client::new(),
        &passes,
        &Arc::new(config),
        "zone-token-prev",
        DownloadControls::new(DownloadRunMode::Download, DownloadReporting::hidden()),
        CancellationToken::new(),
    )
    .await
    .expect("incremental relation delete should succeed");

    assert!(matches!(result.outcome, DownloadOutcome::Success));
    let memberships = db
        .get_live_selected_album_memberships_for_asset(
            "PrimarySync",
            "asset-record-a",
            &["container-a"],
        )
        .await
        .unwrap();
    assert!(memberships.is_empty());
}

#[tokio::test]
async fn unparsable_relation_delete_blocks_incremental_token() {
    let calls = Arc::new(AtomicUsize::new(0));
    let session = changes_zone_session(
        Arc::clone(&calls),
        vec![unparsable_relation_delete_record()],
    );
    let passes = vec![AlbumPass {
        kind: PassKind::Unfiled,
        album: changes_album("", session),
        exclude_ids: Arc::new(FxHashSet::default()),
    }];

    let config = Arc::new(test_config());
    let result = download_photos_incremental(
        &Client::new(),
        &passes,
        &config,
        "zone-token-prev",
        DownloadControls::new(DownloadRunMode::Download, DownloadReporting::hidden()),
        CancellationToken::new(),
    )
    .await
    .expect("unparsable relation delete should not fall back to full here");

    assert!(matches!(result.outcome, DownloadOutcome::Success));
    assert_eq!(result.sync_token, None);
    assert!(result.stats.sync_token_blocked);
    assert_eq!(
        result.stats.sync_token_blocked_reason,
        Some(UNPARSABLE_RELATION_DELTA_REASON)
    );
}
