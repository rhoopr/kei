use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rusqlite::OptionalExtension as _;
use rustc_hash::{FxHashMap, FxHashSet};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

use super::{PendingRetryEvidence, PendingRetryPlanning, PendingRetryTarget};
use crate::download::filter::derive_expected_paths;
use crate::download::planner::{TaskPlanner, pending_record_for_task};
use crate::download::{DownloadConfig, DownloadRunMode};
use crate::icloud::photos::{PhotoAlbum, PhotoAlbumConfig, PhotosSession, inbox::ShadowCapture};
use crate::state::db::{
    PreparedPublication, TempFileOwnershipStore, account::AccountOwner,
    provider_selection::SelectionPath,
};
use crate::state::{
    AssetRecord, DownloadStateStore, MetadataRewriteStore, RetryErrorRetention, SqliteStateDb,
    VersionSizeKey,
};
use crate::test_helpers::TestPhotoAsset;

#[tokio::test]
async fn issue845_l1_targeted_pending_recovery_keeps_current_legacy_movie_across_reopens() {
    for library in ["PrimarySync", "SharedSync-synthetic"] {
        for (metadata, run_mode) in [
            (false, DownloadRunMode::Download),
            (true, DownloadRunMode::Download),
            (false, DownloadRunMode::DryRun),
        ] {
            let dir = TempDir::new().unwrap();
            let db_path = dir.path().join("state.db");
            let mut config = DownloadConfig::test_default();
            config.directory = Arc::from(dir.path().join("media"));
            config.folder_structure = "{album}".to_owned();
            config.album_name = Some(Arc::from("Album"));
            config.metadata.set_exif_datetime = metadata;
            let movie = b"\0\0\0\x14ftypqt  \0\0\0\0qt  ";
            let asset = TestPhotoAsset::new("L1")
                .filename("IMG_0001.HEIC")
                .item_type("public.heic")
                .orig_file_type("public.heic")
                .orig_url("http://127.0.0.1/unused-still")
                .orig_size(2000)
                .live_photo(
                    "http://127.0.0.1/unused-movie",
                    "provider",
                    movie.len() as u64,
                )
                .build()
                .with_source_zone(Arc::from(library));
            let derived = derive_expected_paths(&asset, &config)
                .into_iter()
                .find(|p| p.version_size == VersionSizeKey::LiveOriginal)
                .unwrap();
            let record = AssetRecord::new_pending(
                Arc::from(library),
                "L1".to_owned(),
                VersionSizeKey::LiveOriginal,
                derived.checksum.to_string(),
                derived.filename.clone(),
                asset.created(),
                Some(asset.added_date()),
                derived.size,
                crate::download::filter::determine_media_type(VersionSizeKey::LiveOriginal, &asset),
            )
            .with_metadata_arc(crate::download::filter::metadata_for_selected_version(
                &asset,
                &config,
                VersionSizeKey::LiveOriginal,
            ));
            let local = data_encoding::HEXLOWER.encode(&Sha256::digest(movie));
            let parent = derived.path.parent().unwrap();
            fs::create_dir_all(parent).unwrap();
            let file = parent.join("IMG_0001-L1-28_HEVC-L1-689.MOV");
            fs::write(&file, movie).unwrap();
            let db = SqliteStateDb::open(&db_path).await.unwrap();
            db.upsert_seen(&record).await.unwrap();
            db.mark_verified_download(
                library,
                "L1",
                "live_original",
                &file,
                &local,
                Some(&local),
                false,
            )
            .await
            .unwrap();
            let timestamp: i64 = db
                .acquire_lock("initial targeted publication")
                .unwrap()
                .query_row("SELECT downloaded_at FROM assets WHERE id='L1'", [], |r| {
                    r.get(0)
                })
                .unwrap();
            drop(db);
            for cycle in 0..3 {
                let db = Arc::new(SqliteStateDb::open(&db_path).await.unwrap());
                let mut checked_config = config.clone();
                checked_config.state_db = Some(db.clone());
                let configs = [Arc::new(checked_config.check_ownership().await.unwrap())];
                if cycle < 2 {
                    db.mark_failed(
                        library,
                        "L1",
                        "live_original",
                        "synthetic HTTP 410 for -690",
                    )
                    .await
                    .unwrap();
                }
                db.prepare_for_retry(Some(library), RetryErrorRetention::Clear)
                    .await
                    .unwrap();
                let records = db.get_pending().await.unwrap();
                let evidence: FxHashMap<_, _> = records
                    .iter()
                    .map(|r| {
                        (
                            PendingRetryTarget::from_record(r),
                            PendingRetryEvidence::from_record(r),
                        )
                    })
                    .collect();
                let mut targets: FxHashSet<_> = records
                    .iter()
                    .map(PendingRetryTarget::from_record)
                    .collect();
                let mut planner = TaskPlanner::for_download(Some(db.as_ref())).await.unwrap();
                let mut tasks = Vec::new();
                let mut sources = FxHashMap::default();
                PendingRetryPlanning {
                    db: db.as_ref(),
                    run_mode,
                    shutdown: &CancellationToken::new(),
                    pass_configs: &configs,
                    pending_evidence: &evidence,
                    pending_targets: &mut targets,
                    task_planner: &mut planner,
                    tasks: &mut tasks,
                    retry_sources: &mut sources,
                }
                .plan_resolved_asset(&asset, "L1")
                .await
                .unwrap();
                if run_mode.downloads_files() {
                    assert!(
                        tasks.is_empty(),
                        "{library} metadata={metadata} cycle={cycle}"
                    );
                    assert!(targets.is_empty());
                    assert!(sources.is_empty());
                } else {
                    // Preview recognizes the same verified existing file without
                    // scheduling a duplicate transfer or settling durable state.
                    assert!(targets.is_empty());
                    assert!(tasks.is_empty());
                    assert!(sources.is_empty());
                }
                let state:(String,String,i64,String,String)=db.acquire_lock("targeted durable outcome").unwrap().query_row(
                    "SELECT status,local_path,downloaded_at,local_checksum,download_checksum FROM assets WHERE id='L1'",[],
                    |r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?))).unwrap();
                assert_eq!(
                    state,
                    (
                        if run_mode.downloads_files() {
                            "downloaded".into()
                        } else {
                            "pending".into()
                        },
                        file.to_str().unwrap().into(),
                        timestamp,
                        local.clone(),
                        local.clone()
                    )
                );
                assert_eq!(fs::read(&file).unwrap(), movie);
                assert_eq!(fs::read_dir(parent).unwrap().count(), 1);
                drop(planner);
                drop(db);
            }
        }
    }
}

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
        panic!("compact deferred retry must not query the provider");
    }
    fn clone_box(&self) -> Box<dyn PhotosSession> {
        Box::new(self.clone())
    }
}

fn compact_owner() -> AccountOwner {
    AccountOwner::authenticated(
        "deferred@example.invalid",
        "com",
        &serde_json::from_value(json!({"dsInfo":{"dsid":"compact-deferred"}})).unwrap(),
    )
    .unwrap()
}

fn compact_album(db: Arc<SqliteStateDb>, owner: AccountOwner) -> PhotoAlbum {
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
    album.set_shadow_capture(ShadowCapture::new(db, owner, "com"), "private".into());
    album
}

fn compact_asset(id: &str, filename: &str, fill: u8) -> crate::icloud::photos::PhotoAsset {
    TestPhotoAsset::new(id)
        .filename(filename)
        .orig_file_type("public.jpeg")
        .orig_size(4)
        .orig_checksum(&data_encoding::BASE64.encode(&[fill; 32]))
        .build()
}

async fn seed_compact_publication(
    db: &SqliteStateDb,
    config: &DownloadConfig,
    root: &Path,
    asset: &crate::icloud::photos::PhotoAsset,
    contents: &[u8],
    source: &[u8],
    pending: bool,
) -> (PathBuf, String, String, AssetRecord) {
    let mut planner = TaskPlanner::for_download(Some(db)).await.unwrap();
    let plan = planner.plan_download_asset(asset, config).await.unwrap();
    let task = plan.tasks.first().unwrap();
    let record = pending_record_for_task(config, asset, task);
    db.upsert_seen(&record).await.unwrap();
    let old = root.join(format!("old-{}.JPG", asset.state_id()));
    fs::write(&old, contents).unwrap();
    let local = data_encoding::HEXLOWER.encode(&Sha256::digest(contents));
    let source = data_encoding::HEXLOWER.encode(&Sha256::digest(source));
    let temporary = root.join(format!("{}.part", asset.state_id()));
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
        source_checksum: source.clone(),
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
    if pending {
        db.acquire_lock("make compact retry pending")
            .unwrap()
            .execute(
                "UPDATE assets SET status='pending',last_error='synthetic compact retry' WHERE library=?1 AND id=?2",
                rusqlite::params![proof.library, proof.id],
            )
            .unwrap();
    }
    (old, local, source, record)
}

#[tokio::test]
async fn deferred_compact_retry_keeps_target_while_peer_completes_then_recovers_after_reopen() {
    let root = TempDir::new().unwrap();
    let db_path = root.path().join("state.db");
    let owner = compact_owner();
    let db = Arc::new(SqliteStateDb::open_owned(&db_path, &owner).await.unwrap());
    let mut config = DownloadConfig::test_default();
    config.directory = root.path().join("media").into();
    config.folder_structure = "none".into();
    config.state_db = Some(db.clone());
    config.primary_layout_source = Some(Arc::new(compact_album(db.clone(), owner.clone())));
    config.primary_layout_pass = Some("selected-pass".into());

    let media = config.directory.as_ref();
    fs::create_dir_all(media).unwrap();
    let target = compact_asset("target", "target.JPG", 0xa7);
    let peer = compact_asset("peer", "peer.JPG", 0xb8);
    let (target_path, target_local, target_source, target_record) = seed_compact_publication(
        &db,
        &config,
        media,
        &target,
        b"target-output",
        b"target-raw",
        false,
    )
    .await;
    let (peer_path, _peer_local, _peer_source, _peer_record) = seed_compact_publication(
        &db,
        &config,
        media,
        &peer,
        b"peer-output",
        b"peer-raw",
        true,
    )
    .await;

    let target_row: i64 = db
        .acquire_lock("target compatibility receipt exists")
        .unwrap()
        .query_row(
            "SELECT count(*) FROM asset_metadata_paths
             WHERE library='PrimarySync' AND id='target' AND version_size='original'
               AND local_path=?1 AND local_checksum=?2 AND source_checksum=?3",
            rusqlite::params![target_path.to_str().unwrap(), target_local, target_source],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(target_row, 1);
    db.acquire_lock("remove target compatibility receipt")
        .unwrap()
        .execute(
            "DELETE FROM asset_metadata_paths WHERE library='PrimarySync' AND id='target' AND version_size='original'",
            [],
        )
        .unwrap();

    // The target is already downloaded, but its compact receipt remains
    // unconsumed. This is the production state that reaches Deferred after the
    // compatibility row disappears; the peer is a normal pending retry.
    let mut pending = db.get_pending().await.unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].id.as_ref(), "peer");
    pending.push(target_record.clone());
    let evidence: FxHashMap<_, _> = pending
        .iter()
        .map(|record| {
            (
                PendingRetryTarget::from_record(record),
                PendingRetryEvidence::from_record(record),
            )
        })
        .collect();
    let mut targets: FxHashSet<_> = pending
        .iter()
        .map(PendingRetryTarget::from_record)
        .collect();
    let mut planner = TaskPlanner::for_download(Some(db.as_ref())).await.unwrap();
    let mut tasks = Vec::new();
    let mut sources = FxHashMap::default();
    let configs = [Arc::new(config.clone())];
    let shutdown = CancellationToken::new();
    {
        let mut plan = PendingRetryPlanning {
            db: db.as_ref(),
            run_mode: DownloadRunMode::Download,
            shutdown: &shutdown,
            pass_configs: &configs,
            pending_evidence: &evidence,
            pending_targets: &mut targets,
            task_planner: &mut planner,
            tasks: &mut tasks,
            retry_sources: &mut sources,
        };
        plan.plan_resolved_asset(&target, "target").await.unwrap();
        plan.plan_resolved_asset(&peer, "peer").await.unwrap();
    }
    assert!(
        !targets
            .iter()
            .any(|target| target.asset_id.as_ref() == "peer")
    );
    assert!(tasks.iter().any(|task| task.asset_id.as_ref() == "target"));
    assert_eq!(
        db.acquire_lock("target remains durably pending").unwrap().query_row::<String, _, _>(
            "SELECT status FROM assets WHERE library='PrimarySync' AND id='target' AND version_size='original'",
            [],
            |r| r.get(0),
        ).unwrap(),
        "pending"
    );
    assert_eq!(
        db.acquire_lock("peer completed").unwrap().query_row::<String, _, _>(
            "SELECT status FROM assets WHERE library='PrimarySync' AND id='peer' AND version_size='original'",
            [],
            |r| r.get(0),
        ).unwrap(),
        "downloaded"
    );
    assert_eq!(fs::read(&target_path).unwrap(), b"target-output");
    assert_eq!(fs::read(&peer_path).unwrap(), b"peer-output");
    let target_marker: Option<Option<String>> = db
        .acquire_lock("target has no marker yet")
        .unwrap()
        .query_row(
            "SELECT preserved_path_key FROM asset_metadata_paths WHERE library='PrimarySync' AND id='target' AND version_size='original'",
            [],
            |r| r.get(0),
        )
        .optional()
        .unwrap();
    assert!(target_marker.is_none());
    let target_receipts: i64 = db
        .acquire_lock("target receipt remains unconsumed")
        .unwrap()
        .query_row(
            "SELECT count(*) FROM download_receipts WHERE library='PrimarySync' AND state_id='target'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(target_receipts, 1);
    let peer_marker: Option<String> = db
        .acquire_lock("peer is protected")
        .unwrap()
        .query_row(
            "SELECT preserved_path_key FROM asset_metadata_paths WHERE library='PrimarySync' AND id='peer' AND version_size='original'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(peer_marker.is_some());

    drop(planner);
    drop(db);
    let reopened = Arc::new(SqliteStateDb::open_owned(&db_path, &owner).await.unwrap());
    let reopened_pending = SqliteStateDb::get_pending(reopened.as_ref()).await.unwrap();
    assert_eq!(reopened_pending.len(), 1);
    assert_eq!(reopened_pending[0].id.as_ref(), "target");
    let reopened_evidence: FxHashMap<_, _> = reopened_pending
        .iter()
        .map(|record| {
            (
                PendingRetryTarget::from_record(record),
                PendingRetryEvidence::from_record(record),
            )
        })
        .collect();
    let reopened_target = PendingRetryTarget::from_record(&reopened_pending[0]);
    let mut reopened_targets = FxHashSet::from_iter([reopened_target]);
    let mut reopened_planner = TaskPlanner::for_download(Some(reopened.as_ref()))
        .await
        .unwrap();
    let mut reopened_tasks = Vec::new();
    let mut reopened_sources = FxHashMap::default();
    let mut reopened_config = config.clone();
    reopened_config.state_db = Some(reopened.clone());
    reopened_config.primary_layout_source =
        Some(Arc::new(compact_album(reopened.clone(), owner.clone())));
    let reopened_configs = [Arc::new(reopened_config.clone())];
    PendingRetryPlanning {
        db: reopened.as_ref(),
        run_mode: DownloadRunMode::Download,
        shutdown: &shutdown,
        pass_configs: &reopened_configs,
        pending_evidence: &reopened_evidence,
        pending_targets: &mut reopened_targets,
        task_planner: &mut reopened_planner,
        tasks: &mut reopened_tasks,
        retry_sources: &mut reopened_sources,
    }
    .plan_resolved_asset(&target, "target")
    .await
    .unwrap();
    assert!(
        reopened_targets.is_empty(),
        "reopened pending work must recover through the compact receipt"
    );
    assert!(reopened_tasks.is_empty());
    assert_eq!(
        reopened.acquire_lock("target completed after reopen").unwrap().query_row::<String, _, _>(
            "SELECT status FROM assets WHERE library='PrimarySync' AND id='target' AND version_size='original'",
            [],
            |r| r.get(0),
        ).unwrap(),
        "downloaded"
    );
    assert_eq!(fs::read(&target_path).unwrap(), b"target-output");
    assert!(reopened.guard_primary_writer(&target_path).await.is_err());
    assert!(
        SqliteStateDb::get_pending(reopened.as_ref())
            .await
            .unwrap()
            .is_empty()
    );
    drop(reopened_planner);
    let quiet_mtime = fs::metadata(&target_path).unwrap().modified().unwrap();
    let receipts_before: i64 = reopened
        .acquire_lock("count compact receipts")
        .unwrap()
        .query_row("SELECT count(*) FROM download_receipts", [], |r| r.get(0))
        .unwrap();
    let quiet_pending = SqliteStateDb::get_pending(reopened.as_ref()).await.unwrap();
    assert!(quiet_pending.is_empty());
    // A supplied recovered owner must also stay quiet; this is not an empty
    // target list that exits planning before reading the exact marker.
    let quiet_evidence: FxHashMap<_, _> = std::iter::once((
        PendingRetryTarget::from_record(&target_record),
        PendingRetryEvidence::from_record(&target_record),
    ))
    .collect();
    let mut quiet_targets = FxHashSet::from_iter([PendingRetryTarget::from_record(&target_record)]);
    let mut quiet_planner = TaskPlanner::for_download(Some(reopened.as_ref()))
        .await
        .unwrap();
    let mut quiet_tasks = Vec::new();
    let mut quiet_sources = FxHashMap::default();
    let quiet_configs = [Arc::new(reopened_config)];
    PendingRetryPlanning {
        db: reopened.as_ref(),
        run_mode: DownloadRunMode::Download,
        shutdown: &shutdown,
        pass_configs: &quiet_configs,
        pending_evidence: &quiet_evidence,
        pending_targets: &mut quiet_targets,
        task_planner: &mut quiet_planner,
        tasks: &mut quiet_tasks,
        retry_sources: &mut quiet_sources,
    }
    .plan_resolved_asset(&target, "target")
    .await
    .unwrap();
    assert!(quiet_tasks.is_empty());
    assert!(quiet_sources.is_empty());
    assert_eq!(fs::read(&target_path).unwrap(), b"target-output");
    assert_eq!(
        fs::metadata(&target_path).unwrap().modified().unwrap(),
        quiet_mtime
    );
    assert_eq!(
        reopened
            .acquire_lock("quiet receipt count")
            .unwrap()
            .query_row::<i64, _, _>("SELECT count(*) FROM download_receipts", [], |r| r.get(0))
            .unwrap(),
        receipts_before
    );
}
