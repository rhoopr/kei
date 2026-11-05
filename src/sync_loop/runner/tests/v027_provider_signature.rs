//! Complete-file provider proof exercised through the bound normal cycle.
use super::{
    BaselineSession, MEDIA, OLD_TOKEN, ZONE, baseline_owner, generation, historical_rows, pair,
    zone,
};
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
async fn v027_source_backed_foreign_receipt_materializes_own_path_across_normal_cycles() {
    for database in ["private", "shared"] {
        exercise(database, None, true).await;
    }
}

#[tokio::test]
async fn v027_provider_signature_foreign_bytes_materialize_own_path_across_normal_cycles() {
    for database in ["private", "shared"] {
        exercise(database, None, false).await;
    }
}

#[tokio::test]
async fn v027_provider_signature_wrong_tail_unknown_prefix_and_edited_bytes_stay_pending() {
    for failure in ["tail", "unsupported", "edited"] {
        exercise("private", Some(failure), false).await;
    }
}

async fn exercise(database: &'static str, failure: Option<&str>, source_backed: bool) {
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
    let existing_body = if failure.is_some() {
        MEDIA.to_vec()
    } else {
        provider_bytes.clone()
    };
    let server = crate::start_wiremock_or_skip!();
    for (endpoint, expected, body) in [
        ("/existing", usize::from(failure.is_none()), existing_body),
        ("/missing", 1, MEDIA.to_vec()),
        ("/concurrent", 1, MEDIA.to_vec()),
    ] {
        Mock::given(method("GET"))
            .and(path(endpoint))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_bytes(body)
                    .insert_header("content-type", "image/jpeg"),
            )
            .expect(expected as u64)
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
    let old_path = if failure.is_some() {
        // The negative proof control is an unowned candidate at the requested
        // name, not a foreign owner that the new identity policy would skip.
        root.join("existing.JPG")
    } else {
        root.join("renamed-old_2.JPG")
    };
    std::fs::write(&old_path, &old_bytes).unwrap();
    let old_mtime = std::fs::metadata(&old_path).unwrap().modified().unwrap();
    let db_path = dir.path().join("state.db");
    {
        let db = SqliteStateDb::open_owned(&db_path, &baseline_owner())
            .await
            .unwrap();
        if failure.is_none() {
            let record = crate::test_helpers::TestAssetRecord::new("old-primary-master")
                .library("PrimarySync")
                .filename("existing.JPG")
                .checksum(&checksum)
                .size(provider_bytes.len() as u64)
                .build();
            db.upsert_seen(&record).await.unwrap();
            let local = download::file::compute_sha256(&old_path).await.unwrap();
            let source = source_backed.then_some(local.as_str());
            db.mark_verified_download(
                "PrimarySync",
                "old-primary-master",
                "original",
                &old_path,
                &local,
                source,
                false,
            )
            .await
            .unwrap();
        }
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
        let new_path = root.join("existing.JPG");
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
        let old = completed.iter().find(|r| {
            r.library.as_ref() == "PrimarySync"
                && r.id.as_ref() == "old-primary-master"
                && r.version_size.as_str() == "original"
        });
        if failure.is_some() {
            assert!(
                current.is_none(),
                "negative provider proof must not complete"
            );
            assert_eq!(
                std::fs::read(&new_path).unwrap(),
                old_bytes,
                "negative provider proof must leave the occupied candidate unchanged"
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
            let current = current.unwrap();
            assert_eq!(current.local_path.as_ref(), Some(&new_path));
            assert_eq!(std::fs::read(&new_path).unwrap(), provider_bytes);
            let new_local = download::file::compute_sha256(&new_path).await.unwrap();
            assert_eq!(current.local_checksum.as_deref(), Some(new_local.as_str()));
            assert_eq!(
                old.unwrap().local_path.as_ref(),
                Some(&old_path),
                "foreign legacy owner path must remain unchanged"
            );
            assert_eq!(result.failed_count, 0, "{result:?}");
            assert!(result.db_sync_token_advance_safe, "{result:?}");
            assert_eq!(result.stats.downloaded, if cycle == 0 { 4 } else { 0 });
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

#[tokio::test]
async fn issue853_plain_paths_and_sibling_history_survive_shared_selection() {
    super::PRESERVE_SHARED_CLAIMS
        .scope(true, exercise_issue853(None))
        .await;
}

#[tokio::test]
async fn issue853_other_shared_owner_is_not_reassigned() {
    super::PRESERVE_SHARED_CLAIMS
        .scope(true, exercise_issue853(Some("foreign")))
        .await;
}

#[tokio::test]
async fn issue853_ambiguous_plain_path_stays_pending_across_reopens() {
    super::PRESERVE_SHARED_CLAIMS
        .scope(true, exercise_issue853(Some("tail")))
        .await;
}

async fn exercise_issue853(failure: Option<&str>) {
    let database = "shared";
    // The tail control is deliberately beyond the old 64KiB prefix limit.
    let mut provider_bytes = MEDIA.to_vec();
    provider_bytes.resize(provider_bytes.len().max(65_537), 42);
    let mut checksum = signature(&provider_bytes);
    if failure == Some("unsupported") {
        let mut decoded = base64::engine::general_purpose::STANDARD
            .decode(&checksum)
            .unwrap();
        decoded[0] = 4;
        checksum = base64::engine::general_purpose::STANDARD.encode(decoded);
    }
    let mut selected_candidate_bytes = provider_bytes.clone();
    if failure == Some("tail") {
        *selected_candidate_bytes.last_mut().unwrap() ^= 1;
    }
    if failure == Some("edited") {
        selected_candidate_bytes[32] ^= 1;
    }
    let existing_gets = usize::from(failure != Some("tail"));
    let server = crate::test_helpers::start_wiremock_or_skip("issue853 acceptance")
        .await
        .expect("acceptance server must bind");
    for (endpoint, expected, body) in [
        ("/existing", existing_gets, provider_bytes.clone()),
        ("/missing", 1, MEDIA.to_vec()),
        ("/concurrent", 1, MEDIA.to_vec()),
    ] {
        Mock::given(method("GET"))
            .and(path(endpoint))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_bytes(body)
                    .insert_header("content-type", "image/jpeg"),
            )
            .expect(expected as u64)
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
    let old_path = root.join("existing.JPG");
    let motion_path = root.join("existing.MOV");
    let sidecar_path = root.join("existing.JPG.xmp");
    let selected_own_still = root.join("existing_YXNzZXQ.JPG");
    let selected_own_motion = root.join("existing_YXNzZXQ.MOV");
    std::fs::write(&motion_path, MOTION).unwrap();
    std::fs::write(&sidecar_path, b"historical sidecar evidence").unwrap();
    let motion_mtime = std::fs::metadata(&motion_path).unwrap().modified().unwrap();
    let sidecar_mtime = std::fs::metadata(&sidecar_path)
        .unwrap()
        .modified()
        .unwrap();
    let old_bytes = provider_bytes.clone();
    std::fs::write(&old_path, &old_bytes).unwrap();
    if failure == Some("tail") {
        std::fs::write(&selected_own_still, &selected_candidate_bytes).unwrap();
    }
    let foreign_path = root.join("foreign-shared.JPG");
    if failure == Some("foreign") {
        std::fs::write(&foreign_path, &provider_bytes).unwrap();
    }
    let old_mtime = std::fs::metadata(&old_path).unwrap().modified().unwrap();
    let selected_candidate_mtime = if failure == Some("tail") {
        Some(
            std::fs::metadata(&selected_own_still)
                .unwrap()
                .modified()
                .unwrap(),
        )
    } else {
        None
    };
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
        if failure == Some("foreign") {
            let other = crate::test_helpers::TestAssetRecord::new("asset-other-shared")
                .library(ZONE)
                .filename("foreign-shared.JPG")
                .checksum(&checksum)
                .size(provider_bytes.len() as u64)
                .build();
            db.upsert_seen(&other).await.unwrap();
            let other_hash = download::file::compute_sha256(&foreign_path).await.unwrap();
            db.mark_verified_download(
                ZONE,
                "asset-other-shared",
                "original",
                &foreign_path,
                &other_hash,
                Some(&other_hash),
                false,
            )
            .await
            .unwrap();
        }
        let motion_record = crate::test_helpers::TestAssetRecord::new("old-primary-master")
            .library("PrimarySync")
            .version_size(crate::state::VersionSizeKey::LiveOriginal)
            .filename("existing.MOV")
            .checksum(&signature(MOTION))
            .size(MOTION.len() as u64)
            .build();
        db.upsert_seen(&motion_record).await.unwrap();
        let motion_local = download::file::compute_sha256(&motion_path).await.unwrap();
        db.mark_verified_download(
            "PrimarySync",
            "old-primary-master",
            "live_original",
            &motion_path,
            &motion_local,
            None,
            false,
        )
        .await
        .unwrap();
        for child in ["historical-child-a", "historical-child-b"] {
            db.upsert_asset_master_mapping("PrimarySync", child, "old-primary-master")
                .await
                .unwrap();
        }
        db.set_metadata_capture_revision_for_test("PrimarySync", "old-primary-master", 0);
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
        db.set_metadata(&format!("sync_token:{ZONE}"), OLD_TOKEN)
            .await
            .unwrap();
    }
    let original_rows = historical_rows(&db_path);
    let foreign_snapshot = || {
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        let mut result = Vec::new();
        for table in ["assets", "asset_metadata_paths"] {
            let mut stmt = conn
                .prepare(&format!(
                    "SELECT * FROM {table} WHERE library=?1 AND id=?2 ORDER BY version_size"
                ))
                .unwrap();
            let count = stmt.column_count();
            result.extend(
                stmt.query_map([ZONE, "asset-other-shared"], |row| {
                    (0..count)
                        .map(|i| row.get::<_, rusqlite::types::Value>(i))
                        .collect::<rusqlite::Result<Vec<_>>>()
                })
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap(),
            );
        }
        result
    };
    let foreign_rows = foreign_snapshot();
    let foreign_mtime = if failure == Some("foreign") {
        assert!(!foreign_rows.is_empty());
        Some(
            std::fs::metadata(&foreign_path)
                .unwrap()
                .modified()
                .unwrap(),
        )
    } else {
        None
    };
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
    let mut durable_snapshot = None;
    let pass_scope = json!({"zone":zone(),"name":"",
        "list_type":"CPLAssetAndMasterByAssetDateWithoutHiddenOrDeleted",
        "obj_type":"CPLAssetByAssetDateWithoutHiddenOrDeleted",
        "query_filter":null,"container_id":null,"cross_zone_sources":[]});
    let expected_pass = format!(
        "{:x}",
        sha2::Sha256::digest(
            json!({"kind":"Unfiled","scope":pass_scope,"excludes":[]})
                .to_string()
                .as_bytes()
        )
    );
    let expected_scope = json!({"format":1,"realm":"com","container":"com.apple.photos.cloud",
        "environment":"production","database":"shared","zone":zone()})
    .to_string();
    let account = baseline_owner();
    let (account_key, provider_key) = account.authenticated_keys().unwrap();
    for cycle in 0..3 {
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
        assert_eq!(passes[0].album.selection_scope(), pass_scope);
        let mut selected = make_run_cycle_library_state_with_passes(ZONE, &token_key, passes);
        selected.library = library;
        let base = make_run_cycle_download_config_builder(&root, db.clone());
        let builder = |mode, exclude, groups, zone| {
            let mut config = (*base(mode, exclude, groups, zone)).clone();
            config.folder_structure = "none".into();
            config.file_match_policy = crate::types::FileMatchPolicy::NameId7;
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
        assert_eq!(std::fs::read(&motion_path).unwrap(), MOTION);
        assert_eq!(
            std::fs::metadata(&motion_path).unwrap().modified().unwrap(),
            motion_mtime
        );
        assert_eq!(
            std::fs::read(&sidecar_path).unwrap(),
            b"historical sidecar evidence"
        );
        assert_eq!(
            std::fs::metadata(&sidecar_path)
                .unwrap()
                .modified()
                .unwrap(),
            sidecar_mtime
        );
        // Every possible extra entry, including a directory or symlink, fails.
        let mut actual_tree = std::collections::BTreeMap::new();
        for entry in std::fs::read_dir(&root).unwrap() {
            let entry = entry.unwrap();
            assert!(
                entry.file_type().unwrap().is_file(),
                "unexpected non-file tree entry"
            );
            let name = entry.file_name().into_string().unwrap();
            assert!(
                actual_tree
                    .insert(name, std::fs::read(entry.path()).unwrap())
                    .is_none()
            );
        }
        let mut expected_tree = std::collections::BTreeMap::from([
            ("existing.JPG".to_owned(), old_bytes.clone()),
            ("existing.MOV".to_owned(), MOTION.to_vec()),
            (
                "existing.JPG.xmp".to_owned(),
                b"historical sidecar evidence".to_vec(),
            ),
            (
                "existing_YXNzZXQ.JPG".to_owned(),
                if failure == Some("tail") {
                    selected_candidate_bytes.clone()
                } else {
                    provider_bytes.clone()
                },
            ),
            ("existing_YXNzZXQ.MOV".to_owned(), MOTION.to_vec()),
            ("missing_YXNzZXQ.JPG".to_owned(), MEDIA.to_vec()),
            ("concurrent_YXNzZXQ.JPG".to_owned(), MEDIA.to_vec()),
        ]);
        if failure == Some("foreign") {
            expected_tree.insert("foreign-shared.JPG".to_owned(), provider_bytes.clone());
        }
        assert_eq!(
            actual_tree, expected_tree,
            "complete media tree changed or gained an entry"
        );
        assert_eq!(
            db.get_metadata(&format!("sync_token:{ZONE}"))
                .await
                .unwrap()
                .as_deref(),
            Some(OLD_TOKEN)
        );
        let completed = db.get_downloaded_page(0, 100).await.unwrap();
        let independent = completed
            .iter()
            .find(|r| {
                r.library.as_ref() == ZONE
                    && r.id.as_ref() == "asset-current-missing"
                    && r.version_size.as_str() == "original"
            })
            .expect("independent eligible file must progress");
        let independent_path = independent.local_path.as_ref().unwrap();
        assert_eq!(std::fs::read(independent_path).unwrap(), MEDIA);
        let connection = rusqlite::Connection::open(&db_path).unwrap();
        let guessed: i64 = connection.query_row(
            "SELECT COUNT(*) FROM legacy_master_state_owners WHERE library=?1 AND master_record_name=?2",
            ["PrimarySync", "old-primary-master"], |r| r.get(0)).unwrap();
        assert_eq!(
            guessed, 0,
            "current coverage cannot invent historical sibling ownership"
        );
        let current = completed.iter().find(|r| {
            r.library.as_ref() == ZONE
                && r.id.as_ref() == "asset-current-existing"
                && r.version_size.as_str() == "original"
        });
        // Snapshot every compact receipt and admitted-destination column.
        fn rows(conn: &rusqlite::Connection, sql: &str) -> Vec<Vec<rusqlite::types::Value>> {
            let mut stmt = conn.prepare(sql).unwrap();
            let count = stmt.column_count();
            stmt.query_map([], |row| {
                (0..count)
                    .map(|i| row.get(i))
                    .collect::<rusqlite::Result<Vec<rusqlite::types::Value>>>()
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
        }
        let receipts = rows(&connection, "SELECT * FROM download_receipts ORDER BY id");
        let destinations = rows(
            &connection,
            "SELECT * FROM provider_active_destinations ORDER BY generation,pass_key,child,version_size,path",
        );
        assert_eq!(receipts.len(), if failure == Some("tail") { 3 } else { 4 });
        assert_eq!(
            destinations.len(),
            if failure == Some("tail") { 6 } else { 4 }
        );
        if let Some((old_receipts, old_destinations)) = &durable_snapshot {
            assert_eq!(
                &receipts, old_receipts,
                "compact receipts changed on reopen"
            );
            assert_eq!(
                &destinations, old_destinations,
                "admitted destinations changed on reopen"
            );
        } else {
            durable_snapshot = Some((receipts.clone(), destinations.clone()));
        }
        use rusqlite::types::Value as SqlValue;
        let mut stmt = connection.prepare(
            "SELECT account_key,provider_key,scope,library,child,master,state_id,origin_pass,version_size,provider_checksum,provider_size,native_path,source_checksum,local_checksum,proof_hash FROM download_receipts WHERE library=?1 AND child=?2 ORDER BY version_size"
        ).unwrap();
        let selected_receipts = stmt
            .query_map([ZONE, "asset-current-existing"], |row| {
                (0..15)
                    .map(|i| row.get::<_, SqlValue>(i))
                    .collect::<rusqlite::Result<Vec<_>>>()
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        let expected_selected_receipts: Vec<(&str, &std::path::Path, &[u8], String)> =
            if failure == Some("tail") {
                vec![(
                    "live_original",
                    &selected_own_motion,
                    MOTION,
                    signature(MOTION),
                )]
            } else {
                vec![
                    (
                        "live_original",
                        &selected_own_motion,
                        MOTION,
                        signature(MOTION),
                    ),
                    (
                        "original",
                        &selected_own_still,
                        provider_bytes.as_slice(),
                        checksum.clone(),
                    ),
                ]
            };
        assert_eq!(selected_receipts.len(), expected_selected_receipts.len());
        for (row, (version, path, content, provider)) in
            selected_receipts.iter().zip(expected_selected_receipts)
        {
            let digest = data_encoding::HEXLOWER.encode(&sha2::Sha256::digest(content));
            let native = crate::state::db::provider_selection::SelectionPath::from_path(path)
                .key()
                .unwrap();
            let expected = vec![
                SqlValue::Text(account_key.into()),
                SqlValue::Text(provider_key.into()),
                SqlValue::Text(expected_scope.clone()),
                SqlValue::Text(ZONE.into()),
                SqlValue::Text("asset-current-existing".into()),
                SqlValue::Text("current-existing".into()),
                SqlValue::Text("asset-current-existing".into()),
                SqlValue::Text(expected_pass.clone()),
                SqlValue::Text(version.into()),
                SqlValue::Text(provider),
                SqlValue::Integer(i64::try_from(content.len()).unwrap()),
                SqlValue::Text(native),
                SqlValue::Text(digest.clone()),
                SqlValue::Text(digest),
            ];
            assert_eq!(
                &row[..14],
                expected.as_slice(),
                "incorrect scoped current receipt"
            );
            assert!(matches!(&row[14], SqlValue::Text(proof) if proof.len() == 64));
        }
        let mut stmt = connection.prepare(
            "SELECT DISTINCT version_size,path,checksum,admitted,verified_media,source_checksum,local_checksum FROM provider_active_destinations WHERE library=?1 AND child=?2 ORDER BY version_size"
        ).unwrap();
        let selected_destinations = stmt
            .query_map([ZONE, "asset-current-existing"], |row| {
                (0..7)
                    .map(|i| row.get::<_, SqlValue>(i))
                    .collect::<rusqlite::Result<Vec<_>>>()
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        let native = |path: &std::path::Path| {
            crate::state::db::provider_selection::SelectionPath::from_path(path)
                .key()
                .unwrap()
        };
        let motion_hash = data_encoding::HEXLOWER.encode(&sha2::Sha256::digest(MOTION));
        let still_hash = data_encoding::HEXLOWER.encode(&sha2::Sha256::digest(&provider_bytes));
        let pending = failure == Some("tail");
        let expected_destinations = vec![
            vec![
                SqlValue::Text("live_original".into()),
                SqlValue::Text(native(&selected_own_motion)),
                SqlValue::Text(signature(MOTION)),
                SqlValue::Integer(1),
                SqlValue::Integer(1),
                SqlValue::Text(motion_hash.clone()),
                SqlValue::Text(motion_hash),
            ],
            vec![
                SqlValue::Text("original".into()),
                SqlValue::Text(native(&selected_own_still)),
                SqlValue::Text(checksum.clone()),
                SqlValue::Integer(1),
                SqlValue::Integer(i64::from(!pending)),
                if pending {
                    SqlValue::Null
                } else {
                    SqlValue::Text(still_hash.clone())
                },
                if pending {
                    SqlValue::Null
                } else {
                    SqlValue::Text(still_hash)
                },
            ],
        ];
        assert_eq!(
            selected_destinations, expected_destinations,
            "unexpected selected destination or verification"
        );
        if failure == Some("foreign") {
            assert_eq!(std::fs::read(&foreign_path).unwrap(), provider_bytes);
            assert_eq!(
                std::fs::metadata(&foreign_path)
                    .unwrap()
                    .modified()
                    .unwrap(),
                foreign_mtime.unwrap()
            );
            assert_eq!(
                foreign_snapshot(),
                foreign_rows,
                "separate Shared owner evidence must remain intact"
            );
            let other = completed
                .iter()
                .find(|r| {
                    r.library.as_ref() == ZONE
                        && r.id.as_ref() == "asset-other-shared"
                        && r.version_size.as_str() == "original"
                })
                .expect("other Shared owner must remain recorded");
            assert_eq!(other.local_path.as_ref(), Some(&foreign_path));
            assert_eq!(
                current
                    .expect("selected Shared child must complete")
                    .local_path
                    .as_ref(),
                Some(&selected_own_still),
                "the selected child must materialize its own Shared identity path"
            );
        } else if failure.is_some() {
            assert!(
                current.is_none(),
                "negative provider proof must not complete"
            );
            assert_eq!(
                std::fs::read(&selected_own_still).unwrap(),
                selected_candidate_bytes,
                "negative proof must not overwrite the unowned candidate"
            );
            assert_eq!(
                std::fs::metadata(&selected_own_still)
                    .unwrap()
                    .modified()
                    .unwrap(),
                selected_candidate_mtime.unwrap(),
                "negative proof must not rewrite the unowned candidate"
            );
            let connection = rusqlite::Connection::open(&db_path).unwrap();
            let pending: i64 = connection.query_row(
                "SELECT count(*) FROM assets WHERE library=?1 AND id='asset-current-existing' AND version_size='original' AND status IN ('pending','failed')",
                [ZONE], |r| r.get(0)).unwrap();
            assert_eq!(
                pending, 1,
                "uncertainty must retain independently visible work"
            );
            assert_eq!(result.stats.downloaded, if cycle == 0 { 3 } else { 0 });
        } else {
            assert_eq!(
                current.unwrap().local_path.as_ref(),
                Some(&selected_own_still)
            );
            let motion = completed
                .iter()
                .find(|r| {
                    r.library.as_ref() == ZONE
                        && r.id.as_ref() == "asset-current-existing"
                        && r.version_size.as_str() == "live_original"
                })
                .unwrap();
            assert_eq!(motion.local_path.as_ref(), Some(&selected_own_motion));
            assert_eq!(result.failed_count, 0, "{result:?}");
            assert!(result.db_sync_token_advance_safe, "{result:?}");
            assert_eq!(result.stats.downloaded, if cycle == 0 { 4 } else { 0 });
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
        let requests = server.received_requests().await.unwrap();
        let count = |path: &str| {
            requests
                .iter()
                .filter(|r| r.method.as_str() == "GET" && r.url.path() == path)
                .count()
        };
        assert_eq!(
            (
                count("/existing"),
                count("/companion"),
                count("/missing"),
                count("/concurrent")
            ),
            (existing_gets, 1, 1, 1)
        );
        eprintln!(
            "ISSUE853_REACHED scenario={failure:?} cycle={cycle} selected_receipts={} active_destinations={} media_files={} existing_gets={existing_gets} companion_gets=1 independent_gets=2",
            selected_receipts.len(),
            destinations.len(),
            actual_tree.len()
        );
    }
    server.verify().await;
}

#[tokio::test]
async fn issue853_wrong_account_cannot_use_owned_historical_state() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("state.db");
    let media = dir.path().join("existing.JPG");
    std::fs::write(&media, MEDIA).unwrap();
    let mtime = std::fs::metadata(&media).unwrap().modified().unwrap();
    let owner = baseline_owner();
    {
        let db = SqliteStateDb::open_owned(&db_path, &owner).await.unwrap();
        let record = crate::test_helpers::TestAssetRecord::new("old-primary-master")
            .library("PrimarySync")
            .filename("existing.JPG")
            .checksum(&signature(MEDIA))
            .size(MEDIA.len() as u64)
            .build();
        db.upsert_seen(&record).await.unwrap();
        let checksum = download::file::compute_sha256(&media).await.unwrap();
        db.mark_verified_download(
            "PrimarySync",
            "old-primary-master",
            "original",
            &media,
            &checksum,
            None,
            false,
        )
        .await
        .unwrap();
    }
    let rows = historical_rows(&db_path);
    let wrong = crate::state::db::account::AccountOwner::authenticated(
        "different@example.invalid",
        "com",
        &serde_json::from_value(json!({"dsInfo":{"dsid":"synthetic-other-account"}})).unwrap(),
    )
    .unwrap();
    assert!(SqliteStateDb::open_owned(&db_path, &wrong).await.is_err());
    let correct = SqliteStateDb::open_owned(&db_path, &owner).await.unwrap();
    assert_eq!(correct.get_downloaded_page(0, 100).await.unwrap().len(), 1);
    assert_eq!(historical_rows(&db_path), rows);
    assert_eq!(std::fs::read(&media).unwrap(), MEDIA);
    assert_eq!(
        std::fs::metadata(&media).unwrap().modified().unwrap(),
        mtime
    );

    // An unbound historical database must enter the explicit adoption owner;
    // opening it as a new account is not proof of identity.
    let unbound_path = dir.path().join("unbound.db");
    {
        let db = SqliteStateDb::open(&unbound_path).await.unwrap();
        let record = crate::test_helpers::TestAssetRecord::new("unbound-master")
            .library("PrimarySync")
            .filename("existing.JPG")
            .checksum(&signature(MEDIA))
            .size(MEDIA.len() as u64)
            .build();
        db.upsert_seen(&record).await.unwrap();
        let checksum = download::file::compute_sha256(&media).await.unwrap();
        db.mark_verified_download(
            "PrimarySync",
            "unbound-master",
            "original",
            &media,
            &checksum,
            None,
            false,
        )
        .await
        .unwrap();
    }
    let before = historical_rows(&unbound_path);
    assert!(matches!(
        SqliteStateDb::open_owned(&unbound_path, &owner).await,
        Err(crate::state::error::StateError::AccountOwnerMissing)
    ));
    assert_eq!(historical_rows(&unbound_path), before);
    assert_eq!(std::fs::read(&media).unwrap(), MEDIA);
    assert_eq!(
        std::fs::metadata(&media).unwrap().modified().unwrap(),
        mtime
    );
    eprintln!("ISSUE853_REACHED account_wrong_and_unbound_refused");
}

fn owner_rows(path: &std::path::Path, id: &str) -> Vec<Vec<rusqlite::types::Value>> {
    let conn = rusqlite::Connection::open(path).unwrap();
    let mut rows = Vec::new();
    for table in ["assets", "asset_metadata_paths"] {
        let mut stmt = conn
            .prepare(&format!(
                "SELECT * FROM {table} WHERE library=?1 AND id=?2 ORDER BY version_size"
            ))
            .unwrap();
        let count = stmt.column_count();
        rows.extend(
            stmt.query_map([ZONE, id], |row| {
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

fn legacy_claim_count(path: &std::path::Path) -> i64 {
    rusqlite::Connection::open(path)
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM legacy_master_state_owners \
             WHERE library=?1 AND master_record_name='old-master' \
               AND asset_record_name='asset-current-child'",
            [ZONE],
            |row| row.get(0),
        )
        .unwrap()
}

async fn mount_issue853_jpeg(server: &wiremock::MockServer, route: &str, expected: u64) {
    Mock::given(method("GET"))
        .and(path(route))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(MEDIA)
                .insert_header("content-type", "image/jpeg"),
        )
        .expect(expected)
        .mount(server)
        .await;
}

async fn run_issue853_cycle(
    database: &'static str,
    root: &std::path::Path,
    db_path: &std::path::Path,
    inventory: Vec<serde_json::Value>,
    concurrent: Vec<serde_json::Value>,
    cycle: usize,
) -> (crate::sync_cycle::CycleResult, BaselineSession) {
    let db = Arc::new(
        SqliteStateDb::open_owned(db_path, &baseline_owner())
            .await
            .unwrap(),
    );
    let session = BaselineSession {
        database,
        cycle,
        inventory: Arc::new(inventory),
        concurrent: Arc::new(concurrent),
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
    let token_key = sync_token_key(ZONE);
    let mut selected = make_run_cycle_library_state_with_passes(ZONE, &token_key, passes);
    selected.library = library;
    let base = make_run_cycle_download_config_builder(root, db.clone());
    let builder = |mode, exclude, groups, zone| {
        let mut config = (*base(mode, exclude, groups, zone)).clone();
        config.folder_structure = "none".into();
        Arc::new(config)
    };
    let (_session_dir, shared) = make_shared_session_for_run_cycle().await;
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
    (result, session)
}

#[tokio::test]
async fn issue853_existing_file_m_to_a_reuses_without_download() {
    super::PRESERVE_SHARED_CLAIMS
        .scope(true, issue853_existing_file_m_to_a_case())
        .await;
}

async fn issue853_existing_file_m_to_a_case() {
    let server = crate::test_helpers::start_wiremock_or_skip("issue853 m-to-a reuse")
        .await
        .expect("acceptance server must bind");
    mount_issue853_jpeg(&server, "/reuse", 0).await;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("photos");
    std::fs::create_dir_all(&root).unwrap();
    let path = root.join("reuse.JPG");
    std::fs::write(&path, MEDIA).unwrap();
    let mtime = std::fs::metadata(&path).unwrap().modified().unwrap();
    let checksum = generation(1);
    let digest = download::file::compute_sha256(&path).await.unwrap();
    let db_path = dir.path().join("state.db");
    {
        let db = SqliteStateDb::open_owned(&db_path, &baseline_owner())
            .await
            .unwrap();
        db.upsert_seen(
            &crate::test_helpers::TestAssetRecord::new("old-master")
                .library(ZONE)
                .filename("reuse.JPG")
                .checksum(&checksum)
                .size(MEDIA.len() as u64)
                .build(),
        )
        .await
        .unwrap();
        db.mark_verified_download(
            ZONE,
            "old-master",
            "original",
            &path,
            &digest,
            Some(&digest),
            false,
        )
        .await
        .unwrap();
        assert!(
            db.claim_legacy_master_state_owner(ZONE, "old-master", "asset-current-child")
                .await
                .unwrap()
        );
        db.set_metadata(&format!("sync_token:{ZONE}"), OLD_TOKEN)
            .await
            .unwrap();
    }
    let old_rows = owner_rows(&db_path, "old-master");
    let inventory = pair(
        "current-child",
        "reuse.JPG",
        &format!("{}/reuse", server.uri()),
        1,
    );
    let (result, _) =
        run_issue853_cycle("shared", &root, &db_path, inventory.clone(), Vec::new(), 0).await;
    assert_eq!(result.failed_count, 0, "{result:?}");
    assert_eq!(result.stats.downloaded, 0);
    {
        let db = SqliteStateDb::open_owned(&db_path, &baseline_owner())
            .await
            .unwrap();
        let completed = db.get_downloaded_page(0, 100).await.unwrap();
        let current = completed
            .iter()
            .find(|row| {
                row.library.as_ref() == ZONE
                    && row.id.as_ref() == "asset-current-child"
                    && row.version_size.as_str() == "original"
            })
            .unwrap();
        assert_eq!(current.local_path.as_ref(), Some(&path));
    }
    assert_eq!(std::fs::read(&path).unwrap(), MEDIA);
    assert_eq!(std::fs::metadata(&path).unwrap().modified().unwrap(), mtime);
    assert_eq!(owner_rows(&db_path, "old-master"), old_rows);
    assert_eq!(legacy_claim_count(&db_path), 1);
    assert_eq!(
        server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|request| request.method.as_str() == "GET" && request.url.path() == "/reuse")
            .count(),
        0
    );
    {
        let db = SqliteStateDb::open_owned(&db_path, &baseline_owner())
            .await
            .unwrap();
        db.set_metadata(&sync_token_key(ZONE), "v27-after-cycle-1")
            .await
            .unwrap();
    }
    let (quiet, _) = run_issue853_cycle("shared", &root, &db_path, inventory, Vec::new(), 1).await;
    assert_eq!(quiet.failed_count, 0, "{quiet:?}");
    assert_eq!(quiet.stats.downloaded, 0);
    assert_eq!(std::fs::read(&path).unwrap(), MEDIA);
    assert_eq!(std::fs::metadata(&path).unwrap().modified().unwrap(), mtime);
    server.verify().await;
}

#[tokio::test]
async fn issue853_lazy_claim_existing_file_m_to_a_reuses_on_first_cycle() {
    super::PRESERVE_SHARED_CLAIMS
        .scope(true, issue853_lazy_claim_existing_file_m_to_a_case())
        .await;
}

async fn issue853_lazy_claim_existing_file_m_to_a_case() {
    let server = crate::test_helpers::start_wiremock_or_skip("issue853 lazy m-to-a reuse")
        .await
        .expect("acceptance server must bind");
    mount_issue853_jpeg(&server, "/reuse", 0).await;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("photos");
    std::fs::create_dir_all(&root).unwrap();
    let path = root.join("reuse.JPG");
    std::fs::write(&path, MEDIA).unwrap();
    let mtime = std::fs::metadata(&path).unwrap().modified().unwrap();
    let checksum = generation(1);
    let digest = download::file::compute_sha256(&path).await.unwrap();
    let db_path = dir.path().join("state.db");
    {
        let db = SqliteStateDb::open_owned(&db_path, &baseline_owner())
            .await
            .unwrap();
        db.upsert_seen(
            &crate::test_helpers::TestAssetRecord::new("old-master")
                .library(ZONE)
                .filename("reuse.JPG")
                .checksum(&checksum)
                .size(MEDIA.len() as u64)
                .added_at(chrono::DateTime::from_timestamp_millis(1_700_000_000_000).unwrap())
                .build(),
        )
        .await
        .unwrap();
        db.mark_verified_download(
            ZONE,
            "old-master",
            "original",
            &path,
            &digest,
            Some(&digest),
            false,
        )
        .await
        .unwrap();
        db.set_metadata(&format!("sync_token:{ZONE}"), OLD_TOKEN)
            .await
            .unwrap();
    }
    assert_eq!(legacy_claim_count(&db_path), 0);
    let mut inventory = pair(
        "old-master",
        "reuse.JPG",
        &format!("{}/reuse", server.uri()),
        1,
    );
    inventory[1]["recordName"] = json!("asset-current-child");
    let (result, _) =
        run_issue853_cycle("shared", &root, &db_path, inventory.clone(), Vec::new(), 0).await;
    assert_eq!(legacy_claim_count(&db_path), 1);
    assert_eq!(result.failed_count, 0, "{result:?}");
    assert_eq!(result.stats.downloaded, 0);
    {
        let db = SqliteStateDb::open_owned(&db_path, &baseline_owner())
            .await
            .unwrap();
        let completed = db.get_downloaded_page(0, 100).await.unwrap();
        if let Some(current) = completed.iter().find(|row| {
            row.library.as_ref() == ZONE
                && row.id.as_ref() == "asset-current-child"
                && row.version_size.as_str() == "original"
        }) {
            assert_eq!(
                current.local_path.as_ref(),
                Some(&path),
                "selected child must reuse the logical M path"
            );
            assert_eq!(current.local_checksum.as_deref(), Some(digest.as_str()));
        }
        let old = completed
            .iter()
            .find(|row| {
                row.library.as_ref() == ZONE
                    && row.id.as_ref() == "old-master"
                    && row.version_size.as_str() == "original"
            })
            .expect("completed legacy owner must remain downloaded");
        assert_eq!(old.local_path.as_ref(), Some(&path));
        assert_eq!(old.local_checksum.as_deref(), Some(digest.as_str()));
        assert_eq!(&*old.checksum, checksum.as_str());
    }
    assert_eq!(std::fs::read(&path).unwrap(), MEDIA);
    assert_eq!(std::fs::metadata(&path).unwrap().modified().unwrap(), mtime);
    let after_first = owner_rows(&db_path, "old-master");
    assert_eq!(
        server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|request| request.method.as_str() == "GET" && request.url.path() == "/reuse")
            .count(),
        0
    );
    {
        let db = SqliteStateDb::open_owned(&db_path, &baseline_owner())
            .await
            .unwrap();
        db.set_metadata(&sync_token_key(ZONE), "v27-after-cycle-1")
            .await
            .unwrap();
    }
    let (quiet, _) = run_issue853_cycle("shared", &root, &db_path, inventory, Vec::new(), 1).await;
    assert_eq!(quiet.failed_count, 0, "{quiet:?}");
    assert_eq!(quiet.stats.downloaded, 0);
    assert_eq!(legacy_claim_count(&db_path), 1);
    assert_eq!(owner_rows(&db_path, "old-master"), after_first);
    assert_eq!(std::fs::read(&path).unwrap(), MEDIA);
    assert_eq!(std::fs::metadata(&path).unwrap().modified().unwrap(), mtime);
    server.verify().await;
}

#[tokio::test]
async fn issue853_mixed_owner_completion_publishes_safe_sibling_and_reopens() {
    super::PRESERVE_SHARED_CLAIMS
        .scope(true, issue853_mixed_owner_completion_case())
        .await;
}

async fn issue853_mixed_owner_completion_case() {
    use crate::state::{
        ReconciliationContent, ReconciliationPathKey, ReconciliationReservation,
        ReconciliationStateStore, VersionSizeKey,
    };

    let server = crate::test_helpers::start_wiremock_or_skip("issue853 mixed completion")
        .await
        .expect("acceptance server must bind");
    mount_issue853_jpeg(&server, "/mixed", 1).await;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("photos");
    std::fs::create_dir_all(&root).unwrap();
    let path = root.join("mixed.JPG");
    std::fs::write(&path, MEDIA).unwrap();
    let mtime = std::fs::metadata(&path).unwrap().modified().unwrap();
    let checksum = generation(1);
    let digest = download::file::compute_sha256(&path).await.unwrap();
    let db_path = dir.path().join("state.db");
    {
        let db = SqliteStateDb::open_owned(&db_path, &baseline_owner())
            .await
            .unwrap();
        db.upsert_seen(
            &crate::test_helpers::TestAssetRecord::new("old-master")
                .library(ZONE)
                .filename("mixed.JPG")
                .checksum(&checksum)
                .size(MEDIA.len() as u64)
                .build(),
        )
        .await
        .unwrap();
        db.mark_verified_download(
            ZONE,
            "old-master",
            "original",
            &path,
            &digest,
            Some(&digest),
            false,
        )
        .await
        .unwrap();
        assert!(
            db.claim_legacy_master_state_owner(ZONE, "old-master", "asset-current-child")
                .await
                .unwrap()
        );
        let key = crate::fs_util::confined_path_key(&path).unwrap();
        db.reserve_reconciliation_paths(
            &[ReconciliationReservation {
                content: Some(ReconciliationContent {
                    checksum: checksum.clone().into_boxed_str(),
                    size: MEDIA.len() as u64,
                }),
                library: ZONE.into(),
                asset_id: "foreign".into(),
                version_size: VersionSizeKey::Original,
                requested_path_key: ReconciliationPathKey(key.clone()),
                destination_path_key: ReconciliationPathKey(key),
                destination_path: path.clone(),
            }],
            None,
        )
        .await
        .unwrap();
        db.set_metadata(&format!("sync_token:{ZONE}"), OLD_TOKEN)
            .await
            .unwrap();
    }
    let old_rows = owner_rows(&db_path, "old-master");
    let inventory = pair(
        "current-child",
        "mixed.JPG",
        &format!("{}/mixed", server.uri()),
        1,
    );
    let (result, _) =
        run_issue853_cycle("shared", &root, &db_path, inventory.clone(), Vec::new(), 0).await;
    assert_eq!(result.failed_count, 0, "{result:?}");
    assert_eq!(result.stats.downloaded, 1);
    let safe = {
        let db = SqliteStateDb::open_owned(&db_path, &baseline_owner())
            .await
            .unwrap();
        let completed = db.get_downloaded_page(0, 100).await.unwrap();
        let current = completed
            .iter()
            .find(|row| {
                row.library.as_ref() == ZONE
                    && row.id.as_ref() == "asset-current-child"
                    && row.version_size.as_str() == "original"
            })
            .expect("selected child must complete");
        let safe = current.local_path.as_ref().unwrap().clone();
        assert_ne!(safe, path);
        let reservations = db.get_reconciliation_reservations().await.unwrap();
        assert!(reservations.iter().any(|reservation| {
            reservation.asset_id.as_ref() == "foreign" && reservation.destination_path == path
        }));
        safe
    };
    assert_eq!(std::fs::read(&safe).unwrap(), MEDIA);
    assert_eq!(std::fs::read(&path).unwrap(), MEDIA);
    assert_eq!(std::fs::metadata(&path).unwrap().modified().unwrap(), mtime);
    assert_eq!(owner_rows(&db_path, "old-master"), old_rows);
    assert_eq!(legacy_claim_count(&db_path), 1);
    assert_eq!(
        server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|request| request.method.as_str() == "GET" && request.url.path() == "/mixed")
            .count(),
        1
    );
    {
        let db = SqliteStateDb::open_owned(&db_path, &baseline_owner())
            .await
            .unwrap();
        db.set_metadata(&sync_token_key(ZONE), "v27-after-cycle-1")
            .await
            .unwrap();
    }
    let (quiet, _) = run_issue853_cycle("shared", &root, &db_path, inventory, Vec::new(), 1).await;
    assert_eq!(quiet.failed_count, 0, "{quiet:?}");
    assert_eq!(quiet.stats.downloaded, 0);
    assert_eq!(std::fs::read(&safe).unwrap(), MEDIA);
    assert_eq!(std::fs::read(&path).unwrap(), MEDIA);
    assert_eq!(std::fs::metadata(&path).unwrap().modified().unwrap(), mtime);
    server.verify().await;
}

#[tokio::test]
async fn issue853_proven_reservation_missing_file_completes_at_retained_path() {
    super::PRESERVE_SHARED_CLAIMS
        .scope(true, issue853_proven_reservation_missing_case())
        .await;
}

async fn issue853_proven_reservation_missing_case() {
    use crate::state::{
        ReconciliationContent, ReconciliationPathKey, ReconciliationReservation,
        ReconciliationStateStore, VersionSizeKey,
    };

    let server = crate::test_helpers::start_wiremock_or_skip("issue853 reservation completion")
        .await
        .expect("acceptance server must bind");
    mount_issue853_jpeg(&server, "/reserved", 1).await;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("photos");
    std::fs::create_dir_all(&root).unwrap();
    let path = root.join("reserved.JPG");
    let checksum = generation(1);
    let db_path = dir.path().join("state.db");
    {
        let db = SqliteStateDb::open_owned(&db_path, &baseline_owner())
            .await
            .unwrap();
        assert!(
            db.claim_legacy_master_state_owner(ZONE, "old-master", "asset-current-child")
                .await
                .unwrap()
        );
        let key = crate::fs_util::confined_path_key(&path).unwrap();
        db.reserve_reconciliation_paths(
            &[ReconciliationReservation {
                content: Some(ReconciliationContent {
                    checksum: checksum.clone().into_boxed_str(),
                    size: MEDIA.len() as u64,
                }),
                library: ZONE.into(),
                asset_id: "old-master".into(),
                version_size: VersionSizeKey::Original,
                requested_path_key: ReconciliationPathKey(key.clone()),
                destination_path_key: ReconciliationPathKey(key),
                destination_path: path.clone(),
            }],
            Some("asset-current-child"),
        )
        .await
        .unwrap();
        db.set_metadata(&format!("sync_token:{ZONE}"), OLD_TOKEN)
            .await
            .unwrap();
    }
    let old_rows = owner_rows(&db_path, "old-master");
    let inventory = pair(
        "current-child",
        "reserved.JPG",
        &format!("{}/reserved", server.uri()),
        1,
    );
    let (result, _) =
        run_issue853_cycle("shared", &root, &db_path, inventory.clone(), Vec::new(), 0).await;
    assert_eq!(result.failed_count, 0, "{result:?}");
    assert_eq!(result.stats.downloaded, 1);
    assert_eq!(std::fs::read(&path).unwrap(), MEDIA);
    {
        let db = SqliteStateDb::open_owned(&db_path, &baseline_owner())
            .await
            .unwrap();
        let completed = db.get_downloaded_page(0, 100).await.unwrap();
        let current = completed
            .iter()
            .find(|row| {
                row.library.as_ref() == ZONE
                    && row.id.as_ref() == "asset-current-child"
                    && row.version_size.as_str() == "original"
            })
            .expect("selected child must complete");
        assert_eq!(current.local_path.as_ref(), Some(&path));
        assert!(
            !root.join("reserved_YXNzZXQ.JPG").exists(),
            "canonical reservation must not spill to a sibling"
        );
        assert_eq!(owner_rows(&db_path, "old-master"), old_rows);
        let reservations = db.get_reconciliation_reservations().await.unwrap();
        assert!(
            reservations
                .iter()
                .all(|reservation| reservation.asset_id.as_ref() != "asset-current-child"),
            "no unnecessary A reservation row"
        );
        if let Some(reservation) = reservations
            .iter()
            .find(|reservation| reservation.asset_id.as_ref() == "old-master")
        {
            assert_eq!(reservation.destination_path, path);
        }
    }
    {
        let db = SqliteStateDb::open_owned(&db_path, &baseline_owner())
            .await
            .unwrap();
        db.set_metadata(&sync_token_key(ZONE), "v27-after-cycle-1")
            .await
            .unwrap();
    }
    let (quiet, _) = run_issue853_cycle("shared", &root, &db_path, inventory, Vec::new(), 1).await;
    assert_eq!(quiet.failed_count, 0, "{quiet:?}");
    assert_eq!(quiet.stats.downloaded, 0);
    assert_eq!(std::fs::read(&path).unwrap(), MEDIA);
    server.verify().await;
}
