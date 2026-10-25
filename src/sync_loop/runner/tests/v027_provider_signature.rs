//! Complete-file provider proof exercised through the bound normal cycle.
use super::{BaselineSession, MEDIA, OLD_TOKEN, ZONE, baseline_owner, historical_rows, pair, zone};
use crate::commands::{AlbumPass, PassKind};
use crate::download;
use crate::icloud::photos::PhotoLibrary;
use crate::state::{DownloadStateStore, SqliteStateDb};
use crate::sync_cycle::{run_cycle, sync_token_key};
use crate::sync_loop::test_support::{
    make_run_cycle_config, make_run_cycle_download_config_builder,
    make_run_cycle_library_state_with_passes, make_shared_session_for_run_cycle,
};
use base64::Engine as _;
use serde_json::json;
use sha1::{Digest, Sha1};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{method, path};
use wiremock::{Mock, ResponseTemplate};

const MOTION: &[u8] = b"\0\0\0ftypqt  \0\0\0\0qt  ";

fn signature(bytes: &[u8]) -> String {
    let mut hash = Sha1::new();
    hash.update(b"com.apple.XattrObjectSalt\0com.apple.DataObjectSalt\0");
    hash.update(bytes);
    let mut value = vec![1];
    value.extend_from_slice(&hash.finalize());
    base64::engine::general_purpose::STANDARD.encode(value)
}

#[tokio::test]
async fn v027_provider_signature_missing_receipt_reuses_renamed_bytes_across_normal_cycles() {
    for database in ["private", "shared"] {
        exercise(database, None).await;
    }
}

#[tokio::test]
async fn v027_provider_signature_wrong_tail_unknown_prefix_and_edited_bytes_stay_pending() {
    for failure in ["tail", "unsupported", "edited"] {
        exercise("private", Some(failure)).await;
    }
}

async fn exercise(database: &'static str, failure: Option<&str>) {
    let server = crate::start_wiremock_or_skip!();
    for (endpoint, expected) in [("/existing", 0), ("/missing", 1), ("/concurrent", 1)] {
        Mock::given(method("GET"))
            .and(path(endpoint))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_bytes(MEDIA)
                    .insert_header("content-type", "image/jpeg"),
            )
            .expect(expected)
            .mount(&server)
            .await;
    }
    Mock::given(method("GET"))
        .and(path("/companion"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(MOTION)
                .insert_header("content-type", "video/quicktime"),
        )
        .expect(1)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("photos");
    std::fs::create_dir_all(&root).unwrap();
    let old_path = root.join("renamed-old_2.JPG");
    // The tail control is deliberately beyond the old 64KiB prefix limit.
    let mut provider_bytes = MEDIA.to_vec();
    provider_bytes.resize(provider_bytes.len().max(65_537), 42);
    let mut old_bytes = provider_bytes.clone();
    if failure == Some("tail") {
        *old_bytes.last_mut().unwrap() ^= 1;
    }
    if failure == Some("edited") {
        old_bytes[32] ^= 1;
    }
    let mut checksum = signature(&provider_bytes);
    if failure == Some("unsupported") {
        let mut decoded = base64::engine::general_purpose::STANDARD
            .decode(&checksum)
            .unwrap();
        decoded[0] = 4;
        checksum = base64::engine::general_purpose::STANDARD.encode(decoded);
    }
    std::fs::write(&old_path, &old_bytes).unwrap();
    let old_mtime = std::fs::metadata(&old_path).unwrap().modified().unwrap();
    let db_path = dir.path().join("state.db");
    {
        let db = SqliteStateDb::open_owned(&db_path, &baseline_owner())
            .await
            .unwrap();
        let record = crate::test_helpers::TestAssetRecord::new("old-primary-master")
            .library("PrimarySync")
            .filename("existing.JPG")
            .checksum(&checksum)
            .size(provider_bytes.len() as u64)
            .build();
        db.upsert_seen(&record).await.unwrap();
        let local = download::file::compute_sha256(&old_path).await.unwrap();
        db.mark_verified_download(
            "PrimarySync",
            "old-primary-master",
            "original",
            &old_path,
            &local,
            None,
            false,
        )
        .await
        .unwrap();
        db.set_metadata(&format!("sync_token:{ZONE}"), OLD_TOKEN)
            .await
            .unwrap();
    }
    let original_rows = historical_rows(&db_path);
    let mut existing = pair(
        "current-existing",
        "existing.JPG",
        &format!("{}/existing", server.uri()),
        1,
    );
    existing[0]["fields"]["resOriginalRes"]["value"]["fileChecksum"] = json!(checksum);
    existing[0]["fields"]["resOriginalRes"]["value"]["size"] = json!(provider_bytes.len());
    existing[0]["fields"]["resOriginalVidComplRes"] = json!({"value":{
        "downloadURL":format!("{}/companion",server.uri()),"size":MOTION.len(),
        "fileChecksum":signature(MOTION)}});
    existing[0]["fields"]["resOriginalVidComplFileType"] =
        json!({"value":"com.apple.quicktime-movie"});
    let mut inventory = existing;
    inventory.extend(pair(
        "current-missing",
        "missing.JPG",
        &format!("{}/missing", server.uri()),
        2,
    ));
    let concurrent = pair(
        "during-baseline",
        "concurrent.JPG",
        &format!("{}/concurrent", server.uri()),
        3,
    );
    let (_session_dir, shared) = make_shared_session_for_run_cycle().await;
    let token_key = sync_token_key(ZONE);
    for cycle in 0..if failure.is_some() { 1 } else { 3 } {
        let db = Arc::new(
            SqliteStateDb::open_owned(&db_path, &baseline_owner())
                .await
                .unwrap(),
        );
        let session = BaselineSession {
            database,
            cycle,
            inventory: Arc::new(inventory.clone()),
            concurrent: Arc::new(concurrent.clone()),
            incremental_addition: Arc::new(Vec::new()),
            requests: Arc::new(Mutex::new(Vec::new())),
            inventory_queries: Arc::new(AtomicUsize::new(0)),
            inventory_starts: Arc::new(AtomicUsize::new(0)),
            boundaries: Arc::new(AtomicUsize::new(0)),
            changes: Arc::new(AtomicUsize::new(0)),
        };
        let mut library = PhotoLibrary::new(
            format!("https://synthetic.example/database/1/photos/production/{database}"),
            Arc::new(std::collections::HashMap::new()),
            Box::new(session.clone()),
            Arc::new(zone()),
            database.into(),
            crate::retry::RetryConfig::default(),
        )
        .await
        .unwrap();
        library.shadow_capture = Some(crate::icloud::photos::inbox::ShadowCapture::new(
            db.clone(),
            baseline_owner(),
            "com",
        ));
        let passes = vec![AlbumPass {
            kind: PassKind::Unfiled,
            album: library.all(),
            exclude_ids: Arc::new(rustc_hash::FxHashSet::default()),
        }];
        let mut selected = make_run_cycle_library_state_with_passes(ZONE, &token_key, passes);
        selected.library = library;
        let base = make_run_cycle_download_config_builder(&root, db.clone());
        let builder = |mode, exclude, groups, zone| {
            let mut config = (*base(mode, exclude, groups, zone)).clone();
            config.folder_structure = "none".into();
            Arc::new(config)
        };
        let result = run_cycle(
            &[&selected],
            &make_run_cycle_config(),
            Some(db.as_ref()),
            false,
            &builder,
            download::DownloadControls::download_hidden(),
            &shared,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(std::fs::read(&old_path).unwrap(), old_bytes);
        assert_eq!(
            std::fs::metadata(&old_path).unwrap().modified().unwrap(),
            old_mtime
        );
        assert_eq!(historical_rows(&db_path), original_rows);
        assert!(
            !root.join("existing.JPG").exists(),
            "never download a duplicate"
        );
        assert_eq!(
            db.get_metadata(&format!("sync_token:{ZONE}"))
                .await
                .unwrap()
                .as_deref(),
            Some(OLD_TOKEN)
        );
        let completed = db.get_downloaded_page(0, 100).await.unwrap();
        let current = completed.iter().find(|r| {
            r.library.as_ref() == ZONE
                && r.id.as_ref() == "asset-current-existing"
                && r.version_size.as_str() == "original"
        });
        if failure.is_some() {
            assert!(
                current.is_none(),
                "negative provider proof must not complete"
            );
            let connection = rusqlite::Connection::open(&db_path).unwrap();
            let pending: i64 = connection.query_row(
                "SELECT count(*) FROM assets WHERE library=?1 AND id='asset-current-existing' AND version_size='original' AND status IN ('pending','failed')",
                [ZONE], |r| r.get(0)).unwrap();
            assert_eq!(
                pending, 1,
                "uncertainty must retain independently visible work"
            );
        } else {
            assert_eq!(current.unwrap().local_path.as_ref(), Some(&old_path));
            assert_eq!(result.failed_count, 0, "{result:?}");
            assert!(result.db_sync_token_advance_safe, "{result:?}");
            assert_eq!(result.stats.downloaded, if cycle == 0 { 3 } else { 0 });
            assert_eq!(
                db.get_metadata(&token_key).await.unwrap().as_deref(),
                Some(format!("v27-after-cycle-{}", cycle + 1).as_str())
            );
            if cycle > 0 {
                assert_eq!(session.inventory_queries.load(Ordering::SeqCst), 0);
            } else {
                assert_eq!(session.inventory_starts.load(Ordering::SeqCst), 2);
            }
        }
    }
    server.verify().await;
}
