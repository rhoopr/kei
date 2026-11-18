//! Preserved L1 failure history through normal full dispatch and DB reopens.
//! The selected provider basename is synthetic; its compatible family is explicit.

use std::fs;
use std::path::Path;
use std::sync::Arc;

use base64::Engine as _;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::super::super::test_support::{
    SelectedMediaSession, open_fixture_db, selected_pass_with_session,
};
use super::{mock_asset_record_for, mock_master_record_with_filename};
use crate::download::filter::derive_expected_paths;
use crate::download::{DownloadConfig, DownloadControls, download_photos_with_sync};
use crate::icloud::photos::PhotoAsset;
use crate::state::DownloadStateStore;
use crate::state::{AssetRecord, RetryErrorRetention, SqliteStateDb, VersionSizeKey};

const ID: &str = "SYNTHETIC_L1";
const MOVIE: &[u8] = b"\0\0\0\x14ftypqt  \0\0\0\0qt  ";
const PRIOR_PUBLICATION: i64 = 1790390631;

fn receipts(db: &SqliteStateDb) -> Vec<Value> {
    let conn = db.acquire_lock("L1 receipt snapshot").unwrap();
    let mut query = conn.prepare("SELECT local_path,provider_checksum,local_checksum,download_checksum,source_checksum \
        FROM asset_metadata_paths WHERE library='PrimarySync' AND id=?1 AND version_size='live_original' ORDER BY local_path").unwrap();
    query
        .query_map([ID], |row| {
            Ok(json!([
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, Option<String>>(4)?
            ]))
        })
        .unwrap()
        .map(Result::unwrap)
        .collect()
}

fn rendition(db: &SqliteStateDb, version: VersionSizeKey) -> Value {
    db.acquire_lock("L1 rendition snapshot").unwrap().query_row(
        "SELECT status,is_deleted,checksum,size_bytes,local_path,local_checksum,download_checksum,downloaded_at,download_attempts,last_error \
         FROM assets WHERE library='PrimarySync' AND id=?1 AND version_size=?2",
        [ID,version.as_str()], |row| Ok(json!({
            "status":row.get::<_,String>(0)?,"deleted":row.get::<_,i64>(1)?,"provider":row.get::<_,String>(2)?,
            "size":row.get::<_,i64>(3)?,"path":row.get::<_,String>(4)?,"local":row.get::<_,String>(5)?,
            "download":row.get::<_,String>(6)?,"downloaded_at":row.get::<_,i64>(7)?,
            "attempts":row.get::<_,i64>(8)?,"error":row.get::<_,Option<String>>(9)?
        }))
    ).unwrap()
}

async fn publish(
    db: &SqliteStateDb,
    asset: &PhotoAsset,
    config: &DownloadConfig,
    version: VersionSizeKey,
    file: &Path,
    bytes: &[u8],
) {
    let derived = derive_expected_paths(asset, config)
        .into_iter()
        .find(|p| p.version_size == version)
        .unwrap();
    let record = AssetRecord::new_pending(
        Arc::from("PrimarySync"),
        ID.to_owned(),
        version,
        derived.checksum.to_string(),
        derived.filename,
        asset.created(),
        Some(asset.added_date()),
        derived.size,
        crate::download::filter::determine_media_type(version, asset),
    )
    .with_metadata_arc(crate::download::filter::metadata_for_selected_version(
        asset, config, version,
    ));
    db.upsert_seen(&record).await.unwrap();
    let local = data_encoding::HEXLOWER.encode(&Sha256::digest(bytes));
    // Model a prior verified transfer, including its independent path source
    // receipt. Later failed/pending observations must not erase that proof.
    db.mark_verified_download(
        "PrimarySync",
        ID,
        version.as_str(),
        file,
        &local,
        Some(&local),
        false,
        &Default::default(),
    )
    .await
    .unwrap();
}

fn reservations(db: &SqliteStateDb) -> Vec<Value> {
    let conn = db.acquire_lock("L1 reservation snapshot").unwrap();
    let mut query = conn.prepare("SELECT library,id,version_size,requested_path_key,destination_path_key,destination_path,provider_checksum,provider_size FROM reconciliation_paths ORDER BY library,id,version_size,requested_path_key").unwrap();
    query
        .query_map([], |row| {
            Ok(json!([
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, String>(6)?,
                row.get::<_, i64>(7)?
            ]))
        })
        .unwrap()
        .map(Result::unwrap)
        .collect()
}

async fn run_l1_cases(cases: &[(&str, bool)]) {
    // Retain real failed/retry preparation and release all DB handles each cycle.
    for &(start, unrelated_reservation) in cases {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_bytes(MOVIE)
                    .insert_header("content-type", "video/quicktime"),
            )
            .expect(0)
            .mount(&server)
            .await;
        let temporary = TempDir::new().unwrap();
        let db_path = temporary.path().join("state.db");
        let media = temporary.path().join("media");
        let still_bytes = vec![1_u8; 2000];
        let provider = base64::engine::general_purpose::STANDARD.encode(Sha256::digest(MOVIE));
        let mut master = mock_master_record_with_filename(ID, "IMG_0001.HEIC");
        master["fields"]["itemType"]["value"] = json!("public.heic");
        master["fields"]["resOriginalFileType"]["value"] = json!("public.heic");
        master["fields"]["resOriginalRes"]["value"] = json!({"downloadURL":format!("{}/still",server.uri()),"size":2000,
            "fileChecksum":base64::engine::general_purpose::STANDARD.encode(Sha256::digest(&still_bytes))});
        master["fields"]["resOriginalVidComplRes"] = json!({"value":{"downloadURL":format!("{}/movie",server.uri()),
            "size":MOVIE.len(),"fileChecksum":provider}});
        master["fields"]["resOriginalVidComplFileType"] =
            json!({"value":"com.apple.quicktime-movie"});
        let mut child = mock_asset_record_for(&format!("asset-{ID}"), ID);
        child["fields"]["masterRef"]["value"]["zoneID"]["ownerRecordName"] = json!("_defaultOwner");
        let asset = PhotoAsset::new(master.clone(), child.clone());
        let mut config = DownloadConfig::test_default();
        config.directory = Arc::from(media.as_path());
        config.folder_structure = "{album}".to_owned();
        config.album_name = Some(Arc::from("SyntheticAlbum"));
        config.concurrent_downloads = 1;
        let parent = media.join("SyntheticAlbum");
        fs::create_dir_all(&parent).unwrap();
        let still = parent.join(format!("IMG_0001-{ID}-27.HEIC"));
        fs::write(&still, &still_bytes).unwrap();
        let old: Vec<_> = (635..=689)
            .map(|n| parent.join(format!("IMG_0001-{ID}-28_HEVC-{ID}-{n}.MOV")))
            .collect();
        for file in &old {
            fs::write(file, MOVIE).unwrap();
        }
        let db = open_fixture_db(&db_path).await;
        db.upsert_asset_master_mapping("PrimarySync", &format!("asset-{ID}"), ID)
            .await
            .unwrap();
        assert!(
            db.claim_legacy_master_state_owner("PrimarySync", ID, &format!("asset-{ID}"))
                .await
                .unwrap()
        );
        publish(
            &db,
            &asset,
            &config,
            VersionSizeKey::Original,
            &still,
            &still_bytes,
        )
        .await;
        for file in &old {
            publish(
                &db,
                &asset,
                &config,
                VersionSizeKey::LiveOriginal,
                file,
                MOVIE,
            )
            .await;
        }
        db.acquire_lock("L1 retained timestamp").unwrap().execute(
            "UPDATE assets SET downloaded_at=?1 WHERE library='PrimarySync' AND id=?2 AND version_size='live_original'",
            rusqlite::params![PRIOR_PUBLICATION,ID]).unwrap();
        db.begin_metadata_capture_revision("PrimarySync", crate::state::METADATA_CAPTURE_REVISION)
            .await
            .unwrap();
        db.complete_metadata_capture_revision(
            "PrimarySync",
            crate::state::METADATA_CAPTURE_REVISION,
        )
        .await
        .unwrap();
        let baseline = receipts(&db);
        assert_eq!(baseline.len(), 55);
        let initial = rendition(&db, VersionSizeKey::LiveOriginal);
        let initial_still = rendition(&db, VersionSizeKey::Original);
        let unrelated_file = media.join("UnrelatedAlbum/owned.MOV");
        if unrelated_reservation {
            fs::create_dir_all(unrelated_file.parent().unwrap()).unwrap();
            fs::write(&unrelated_file, b"unrelated preserved bytes").unwrap();
            let key = crate::fs_util::confined_path_key(&unrelated_file).unwrap();
            db.acquire_lock("unrelated saved reservation").unwrap().execute(
                "INSERT INTO reconciliation_paths(library,id,version_size,requested_path_key,destination_path_key,destination_path,provider_checksum,provider_size) VALUES('PrimarySync','UNRELATED','original',?1,?1,?2,'unrelated-provider',25)",
                rusqlite::params![key,unrelated_file.to_str().unwrap()]).unwrap();
        }
        if start == "own-destination" {
            let requested = derive_expected_paths(&asset, &config)
                .into_iter()
                .find(|path| path.version_size == VersionSizeKey::LiveOriginal)
                .unwrap()
                .path;
            let destination = parent.join("saved-different-choice.MOV");
            db.acquire_lock("preexisting different own reservation").unwrap().execute(
                "INSERT INTO reconciliation_paths(library,id,version_size,requested_path_key,destination_path_key,destination_path,provider_checksum,provider_size) VALUES('PrimarySync',?1,'live_original',?2,?3,?4,?5,?6)",
                rusqlite::params![ID,crate::fs_util::confined_path_key(&requested).unwrap(),
                    crate::fs_util::confined_path_key(&destination).unwrap(),destination.to_str().unwrap(),
                    provider,MOVIE.len() as i64]).unwrap();
        }
        let initial_reservations = reservations(&db);
        if start == "both-pending" || start == "mixed-state-write-failure" {
            db.mark_failed(
                "PrimarySync",
                ID,
                "original",
                "synthetic later still failure",
            )
            .await
            .unwrap();
        }
        if start != "downloaded" {
            db.mark_failed(
                "PrimarySync",
                ID,
                "live_original",
                "Apple returned HTTP 410 while downloading SYNTHETIC-28_HEVC-ID-690.MOV",
            )
            .await
            .unwrap();
            let failed = rendition(&db, VersionSizeKey::LiveOriginal);
            assert_eq!(failed["status"], "failed");
            assert_eq!(failed["attempts"], 1);
            assert_eq!(failed["downloaded_at"], PRIOR_PUBLICATION);
            assert_eq!(failed["path"], old.last().unwrap().to_str().unwrap());
            if start == "pending" || start == "both-pending" || start == "mixed-state-write-failure"
            {
                db.acquire_lock("L1 later preserved observation").unwrap().execute(
                    "UPDATE assets SET download_attempts=0,last_error='Not resolved during sync' \
                     WHERE library='PrimarySync' AND id=?1 AND version_size='live_original'",[ID]).unwrap();
                db.prepare_for_retry(Some("PrimarySync"), RetryErrorRetention::Clear)
                    .await
                    .unwrap();
                assert_eq!(
                    rendition(&db, VersionSizeKey::LiveOriginal)["status"],
                    "pending"
                );
            }
        }
        drop(db);
        for cycle in 0..4 {
            let db = open_fixture_db(&db_path).await;
            if cycle == 1 {
                db.mark_failed(
                    "PrimarySync",
                    ID,
                    "live_original",
                    "synthetic repeated HTTP 410 after prior publication",
                )
                .await
                .unwrap();
            }
            let state_write_failure = (start == "state-write-failure"
                || start == "mixed-state-write-failure")
                && cycle == 0;
            let own_destination = start == "own-destination";
            let movie_attempts = rendition(&db, VersionSizeKey::LiveOriginal)["attempts"].clone();
            let still_attempts = rendition(&db, VersionSizeKey::Original)["attempts"].clone();
            if state_write_failure {
                db.acquire_lock("L1 forced finalization failure")
                    .unwrap()
                    .execute_batch(
                        "CREATE TRIGGER reject_l1_recovery BEFORE UPDATE ON assets \
                     WHEN NEW.id='SYNTHETIC_L1' AND NEW.version_size='live_original' \
                     AND OLD.status IN ('pending','failed') AND NEW.status='downloaded' \
                     BEGIN SELECT RAISE(ABORT,'synthetic recovery write failure'); END;",
                    )
                    .unwrap();
            }
            let passes = vec![selected_pass_with_session(
                &db,
                "SyntheticAlbum",
                Box::new(SelectedMediaSession(Arc::new(vec![
                    master.clone(),
                    child.clone(),
                ]))),
            )];
            config.state_db = Some(db.clone());
            let result = download_photos_with_sync(
                &reqwest::Client::new(),
                &passes,
                Arc::new(config.clone()),
                DownloadControls::download_hidden(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
            let destinations: Vec<String> = db.acquire_lock("L1 destination evidence").unwrap()
                .prepare("SELECT json_array(asset_id,version_size,compat_path,verified_media,verified_metadata,checksum) FROM provider_active_destinations").unwrap()
                .query_map([], |r| r.get(0)).unwrap().map(Result::unwrap).collect();
            assert_eq!(
                result.stats.downloaded, 0,
                "start={start} cycle={cycle}: {destinations:?}"
            );
            if state_write_failure {
                assert!(result.stats.state_write_failures > 0);
                // source_checkpoint_decision consumes this execution evidence,
                // independently of the token-proof reporting flag.
                assert!(result.checkpoint.state_write_failures > 0);
            } else if own_destination {
                assert!(result.checkpoint.enumeration_errors > 0);
                assert_eq!(result.stats.state_write_failures, 0);
            } else {
                assert_eq!(result.stats.failed, 0, "{result:?}");
                assert_eq!(result.stats.state_write_failures, 0);
                assert_eq!(result.checkpoint.enumeration_errors, 0);
            }
            assert_eq!(result.stats.assets_seen, 1);
            assert!(result.full_enumeration_ran);
            if !state_write_failure && !own_destination {
                assert!(
                    !result.stats.sync_token_blocked,
                    "start={start} cycle={cycle}: {result:?} {destinations:?}"
                );
                assert_eq!(result.sync_token.as_deref(), Some("fixture-token"));
            }
            let after = rendition(&db, VersionSizeKey::LiveOriginal);
            if state_write_failure || own_destination {
                assert_ne!(after["status"], "downloaded");
                for field in [
                    "path",
                    "provider",
                    "size",
                    "local",
                    "download",
                    "downloaded_at",
                ] {
                    assert_eq!(after[field], initial[field]);
                }
                if state_write_failure {
                    db.acquire_lock("L1 remove synthetic trigger")
                        .unwrap()
                        .execute_batch("DROP TRIGGER reject_l1_recovery")
                        .unwrap();
                }
            } else {
                let mut expected = initial.clone();
                expected["attempts"] = movie_attempts;
                assert_eq!(after, expected, "start={start} cycle={cycle}");
            }
            assert_eq!(receipts(&db), baseline);
            let actual_reservations = reservations(&db);
            let mut expected_reservations = initial_reservations.clone();
            // Existing reserved planning can save a healthy still's requested
            // path before its fast state check skips transfer. Allow only that
            // exact fixture choice; movie and unrelated reservations stay fixed.
            let planned_still = parent.join("IMG_0001.HEIC");
            let planned_still = planned_still.to_str().unwrap();
            let permitted_still = json!([
                "PrimarySync",
                ID,
                "original",
                planned_still,
                planned_still,
                planned_still,
                base64::engine::general_purpose::STANDARD.encode(Sha256::digest(&still_bytes)),
                2000
            ]);
            if unrelated_reservation
                && !own_destination
                && !(cycle == 0
                    && (start == "both-pending" || start == "mixed-state-write-failure"))
                && actual_reservations.contains(&permitted_still)
            {
                expected_reservations.insert(0, permitted_still);
            }
            assert_eq!(
                actual_reservations, expected_reservations,
                "start={start} cycle={cycle}"
            );
            let mut expected_still = initial_still.clone();
            expected_still["attempts"] = still_attempts;
            assert_eq!(
                rendition(&db, VersionSizeKey::Original),
                expected_still,
                "start={start} cycle={cycle}"
            );
            if unrelated_reservation {
                assert_eq!(
                    fs::read(&unrelated_file).unwrap(),
                    b"unrelated preserved bytes"
                );
            }
            assert_eq!(
                rendition(&db, VersionSizeKey::Original)["path"],
                still.to_str().unwrap()
            );
            assert_eq!(fs::read(&still).unwrap(), still_bytes);
            for file in &old {
                assert_eq!(fs::read(file).unwrap(), MOVIE);
            }
            assert_eq!(fs::read_dir(&parent).unwrap().count(), 56);
            config.state_db = None;
            drop(passes);
            drop(db);
        }
        assert!(server.received_requests().await.unwrap().is_empty());
    }
}

#[tokio::test]
async fn issue845_l1_full_cycle_recovers_unchanged_failed_movie_without_copying() {
    run_l1_cases(&[
        ("downloaded", false),
        ("failed", false),
        ("pending", false),
        ("state-write-failure", false),
    ])
    .await;
}

#[tokio::test]
async fn issue845_l1_unrelated_reservation_cannot_preclude_current_file_recovery() {
    run_l1_cases(&[
        ("pending", true),
        ("both-pending", true),
        ("state-write-failure", true),
        ("mixed-state-write-failure", true),
    ])
    .await;
}

#[tokio::test]
async fn issue845_l1_preexisting_different_destination_keeps_current_publication_pending() {
    run_l1_cases(&[("own-destination", true)]).await;
}
