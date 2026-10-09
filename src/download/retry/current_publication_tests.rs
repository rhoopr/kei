use std::fs;
use std::sync::Arc;

use rustc_hash::{FxHashMap, FxHashSet};
use sha2::{Digest, Sha256};
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

use super::{PendingRetryEvidence, PendingRetryPlanning, PendingRetryTarget};
use crate::download::filter::derive_expected_paths;
use crate::download::planner::TaskPlanner;
use crate::download::{DownloadConfig, DownloadRunMode};
use crate::state::{AssetRecord, RetryErrorRetention, SqliteStateDb, VersionSizeKey};
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
            db.mark_downloaded(library, "L1", "live_original", &file, &local, Some(&local))
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
                let db = SqliteStateDb::open(&db_path).await.unwrap();
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
                let mut planner = TaskPlanner::for_download(Some(&db)).await.unwrap();
                let mut tasks = Vec::new();
                let mut sources = FxHashMap::default();
                let configs = [Arc::new(config.clone())];
                PendingRetryPlanning {
                    db: &db,
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
                    // Queued targets leave this planner set; the row stays
                    // pending until an actual download/recovery completes.
                    assert!(targets.is_empty());
                    assert_eq!(tasks.len(), 1);
                    assert_eq!(sources.len(), 1);
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
