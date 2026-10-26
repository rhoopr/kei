use super::TaskPlanner;
use crate::download::{DownloadConfig, baseline_matching::BaselineMatch};
use crate::icloud::photos::{PhotoAlbum, PhotoAlbumConfig, PhotosSession, inbox::ShadowCapture};
use crate::state::db::DownloadStateStore;
use crate::state::db::{
    ReconciliationContent, ReconciliationPathKey, ReconciliationReservation,
    ReconciliationStateStore, TempFileOwnershipStore,
};
use crate::state::{
    SqliteStateDb,
    db::{PreparedPublication, account::AccountOwner, provider_selection::SelectionPath},
};
use crate::test_helpers::TestPhotoAsset;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::sync::Arc;

#[derive(Clone)]
struct NoRequests;
#[async_trait::async_trait]
impl PhotosSession for NoRequests {
    async fn post(
        &self,
        _url: &str,
        _body: String,
        _headers: &[(&str, &str)],
    ) -> anyhow::Result<Value> {
        panic!("compact matching must not query the provider");
    }
    fn clone_box(&self) -> Box<dyn PhotosSession> {
        Box::new(self.clone())
    }
}

#[tokio::test]
async fn v027_compact_only_changed_template_modified_file_is_uncertain() {
    let root = tempfile::tempdir().unwrap();
    let db_path = root.path().join("state.db");
    let owner = AccountOwner::authenticated(
        "compact@example.invalid",
        "com",
        &serde_json::from_value(json!({"dsInfo":{"dsid":"compact-matcher"}})).unwrap(),
    )
    .unwrap();
    let db = Arc::new(SqliteStateDb::open_owned(&db_path, &owner).await.unwrap());
    let mut album = PhotoAlbum::new(
        PhotoAlbumConfig {
            params: Arc::new(std::collections::HashMap::new()),
            service_endpoint: "https://invalid.example".into(),
            name: "selected".into(),
            list_type: "CPLAssetAndMasterByAssetDateWithoutHiddenOrDeleted".into(),
            obj_type: "CPLAssetByAssetDateWithoutHiddenOrDeleted".into(),
            query_filter: None,
            page_size: 1,
            zone_id: Arc::new(json!({"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"})),
            retry_config: crate::retry::RetryConfig::default(),
            container_id: None,
            cross_zone_sources: Vec::new(),
        },
        Box::new(NoRequests),
    );
    album.set_shadow_capture(
        ShadowCapture::new(db.clone(), owner.clone(), "com"),
        "private".into(),
    );
    let asset = TestPhotoAsset::new("compact-child")
        .filename("photo.JPG")
        .orig_file_type("public.jpeg")
        .orig_size(4)
        .orig_checksum(&data_encoding::BASE64.encode(&[0xa7; 32]))
        .build();
    let mut config = DownloadConfig::test_default();
    config.directory = root.path().to_path_buf().into();
    config.folder_structure = "new-template".into();
    config.state_db = Some(db.clone());
    config.primary_layout_source = Some(Arc::new(album));
    config.primary_layout_pass = Some("selected-pass".into());
    let mut planner = TaskPlanner::for_download(Some(db.as_ref())).await.unwrap();
    let plan = planner.plan_download_asset(&asset, &config).await.unwrap();
    let task = plan.tasks.first().unwrap();
    let record = super::pending_record_for_task(&config, &asset, task);
    db.upsert_seen(&record).await.unwrap();
    let old = root.path().join("old-renamed.JPG");
    std::fs::write(&old, b"metadata-output").unwrap();
    let local = format!("{:x}", Sha256::digest(b"metadata-output"));
    let temporary = root.path().join("owned.part");
    db.claim_temp_file(&temporary).await.unwrap();
    let proof = PreparedPublication {
        binding: task.publication_binding.as_deref().cloned(),
        library: task.library.to_string(),
        id: task.asset_id.to_string(),
        version: task.version_size.as_str().into(),
        provider_checksum: task.checksum.to_string(),
        provider_size: task.size,
        temporary: SelectionPath::from_path(&temporary),
        destination: SelectionPath::from_path(&old),
        local_checksum: local.clone(),
        source_checksum: format!("{:x}", Sha256::digest(b"raw!")),
    };
    db.prepare_temp_publication(&temporary, &proof)
        .await
        .unwrap();
    db.mark_verified_download(
        &proof.library,
        &proof.id,
        &proof.version,
        &old,
        &local,
        Some(&proof.source_checksum),
        false,
    )
    .await
    .unwrap();
    // Remove compatibility hints, retaining only the current provider row and
    // the actual scoped completed-download owner.
    rusqlite::Connection::open(&db_path).unwrap().execute_batch(
        "DELETE FROM asset_metadata_paths; UPDATE assets SET local_path=NULL,local_checksum=NULL;").unwrap();
    let mut planner = TaskPlanner::for_download(Some(db.as_ref())).await.unwrap();
    // This is authenticated receipt binding, not a provider-signature proof:
    // metadata-edited local bytes deliberately differ from the raw source hash.
    let untouched_bytes = std::fs::read(&old).unwrap();
    let untouched_mtime = std::fs::metadata(&old).unwrap().modified().unwrap();
    let decision = planner.match_existing(&asset, &config, task).await;
    let BaselineMatch::RecordedPresent { receipt, .. } = &decision else {
        panic!("scoped completed receipt must match");
    };
    assert_eq!(receipt.native_path.to_path(), old);
    // Positive control uses the same pending status as the reservation refusal.
    rusqlite::Connection::open(&db_path)
        .unwrap()
        .execute_batch("UPDATE assets SET status='pending';")
        .unwrap();
    crate::download::baseline_matching::commit_match(
        decision,
        &config,
        &asset,
        task,
        &tokio_util::sync::CancellationToken::new(),
    )
    .await
    .unwrap();
    assert!(db.get_pending().await.unwrap().is_empty());
    assert_eq!(std::fs::read(&old).unwrap(), untouched_bytes);
    assert_eq!(
        std::fs::metadata(&old).unwrap().modified().unwrap(),
        untouched_mtime
    );
    assert!(!task.download_path.exists());
    rusqlite::Connection::open(&db_path).unwrap().execute_batch(
        "DELETE FROM asset_metadata_paths; UPDATE assets SET local_path=NULL,local_checksum=NULL;").unwrap();
    // Introduce a valid foreign reservation after the positive match. The
    // transaction must recheck it instead of relying on the planner snapshot.
    let decision = planner.match_existing(&asset, &config, task).await;
    let key = crate::fs_util::confined_path_key(&old).unwrap();
    db.reserve_reconciliation_paths(&[ReconciliationReservation {
        library: "PrimarySync".into(),
        asset_id: "foreign-child".into(),
        version_size: task.version_size,
        requested_path_key: ReconciliationPathKey(key.clone()),
        destination_path_key: ReconciliationPathKey(key),
        destination_path: old.clone(),
        content: Some(ReconciliationContent {
            checksum: task.checksum.to_string().into_boxed_str(),
            size: task.size,
        }),
    }])
    .await
    .unwrap();
    rusqlite::Connection::open(&db_path)
        .unwrap()
        .execute_batch("UPDATE assets SET status='pending';")
        .unwrap();
    assert!(
        crate::download::baseline_matching::commit_match(
            decision,
            &config,
            &asset,
            task,
            &tokio_util::sync::CancellationToken::new()
        )
        .await
        .is_err()
    );
    assert_eq!(db.get_pending().await.unwrap().len(), 1);
    assert_eq!(std::fs::read(&old).unwrap(), b"metadata-output");
    assert_eq!(
        std::fs::metadata(&old).unwrap().modified().unwrap(),
        untouched_mtime
    );
    assert!(!task.download_path.exists());
    rusqlite::Connection::open(&db_path)
        .unwrap()
        .execute_batch("DELETE FROM reconciliation_paths; DELETE FROM asset_metadata_paths; UPDATE assets SET status='downloaded',local_path=NULL,local_checksum=NULL;")
        .unwrap();
    // The restored receipt still matches before changing only the local bytes.
    assert!(matches!(
        planner.match_existing(&asset, &config, task).await,
        BaselineMatch::RecordedPresent { .. }
    ));
    std::fs::write(&old, b"changed-output!").unwrap();
    let changed_time = std::fs::metadata(&old).unwrap().modified().unwrap();
    assert!(matches!(
        planner.match_existing(&asset, &config, task).await,
        BaselineMatch::Uncertain(_)
    ));
    assert_eq!(std::fs::read(&old).unwrap(), b"changed-output!");
    assert_eq!(
        std::fs::metadata(&old).unwrap().modified().unwrap(),
        changed_time
    );
    assert!(!task.download_path.exists());
}
