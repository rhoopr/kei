//! Complete-file provider proof exercised through the bound normal cycle.
use super::{BaselineSession, MEDIA, OLD_TOKEN, ZONE, baseline_owner, historical_rows, pair, zone};
use crate::commands::{AlbumPass, PassKind};
use crate::download;
use crate::icloud::photos::PhotoLibrary;
use crate::state::SqliteStateDb;
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

pub(in crate::sync_loop::runner::tests) async fn exercise_released_schema(failure: Option<&str>) {
    let database = "shared";
    let server = crate::test_helpers::start_wiremock_or_skip("issue853 acceptance")
        .await
        .expect("acceptance server must bind");
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
        .expect(0)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("photos");
    std::fs::create_dir_all(&root).unwrap();
    let old_path = root.join("existing.JPG");
    let motion_path = root.join("existing.MOV");
    let sidecar_path = root.join("existing.JPG.xmp");
    std::fs::write(&motion_path, MOTION).unwrap();
    std::fs::write(&sidecar_path, b"historical sidecar evidence").unwrap();
    let motion_mtime = std::fs::metadata(&motion_path).unwrap().modified().unwrap();
    let sidecar_mtime = std::fs::metadata(&sidecar_path)
        .unwrap()
        .modified()
        .unwrap();
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
    let foreign_path = root.join("foreign-shared.JPG");
    if failure == Some("foreign") {
        std::fs::write(&foreign_path, &provider_bytes).unwrap();
    }
    let old_mtime = std::fs::metadata(&old_path).unwrap().modified().unwrap();
    // Schema was emitted by v0.24.0; the inserted catalogue and files are
    // synthetic reconstructions, not rows written by a released provider sync.
    let source = dir.path().join("released.db");
    let db_path = dir.path().join("state.db");
    {
        let conn = rusqlite::Connection::open(&source).unwrap();
        conn.execute_batch(include_str!(
            "../../../../../tests/data/released-v0240-schema.sql"
        ))
        .unwrap();
        assert_eq!(
            conn.query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            25
        );
        for (library, id, version, filename, path, content, provider) in [
            (
                "PrimarySync",
                "old-primary-master",
                "original",
                "existing.JPG",
                &old_path,
                old_bytes.as_slice(),
                checksum.clone(),
            ),
            (
                "PrimarySync",
                "old-primary-master",
                "live_original",
                "existing.MOV",
                &motion_path,
                MOTION,
                signature(MOTION),
            ),
        ] {
            let local = data_encoding::HEXLOWER.encode(&sha2::Sha256::digest(content));
            conn.execute("INSERT INTO assets(library,id,version_size,checksum,filename,created_at,added_at,size_bytes,media_type,status,downloaded_at,local_path,last_seen_at,local_checksum,download_checksum) VALUES (?1,?2,?3,?4,?5,1700000000,1700000002,?6,'photo','downloaded',1700000003,?7,1700000004,?8,?8)", rusqlite::params![library,id,version,provider,filename,content.len() as i64,path.to_str().unwrap(),local]).unwrap();
            conn.execute("INSERT INTO asset_metadata_paths(library,id,version_size,local_path,provider_checksum,local_checksum,download_checksum) VALUES (?1,?2,?3,?4,?5,?6,?6)", rusqlite::params![library,id,version,path.to_str().unwrap(),provider,local]).unwrap();
        }
        if failure == Some("foreign") {
            let local = data_encoding::HEXLOWER.encode(&sha2::Sha256::digest(&provider_bytes));
            conn.execute("INSERT INTO assets(library,id,version_size,checksum,filename,created_at,size_bytes,media_type,status,local_path,last_seen_at,local_checksum,download_checksum) VALUES (?1,'asset-other-shared','original',?2,'foreign-shared.JPG',1700000000,?3,'photo','downloaded',?4,1700000000,?5,?5)", rusqlite::params![ZONE,checksum,provider_bytes.len() as i64,foreign_path.to_str().unwrap(),local]).unwrap();
            conn.execute("INSERT INTO asset_metadata_paths(library,id,version_size,local_path,provider_checksum,local_checksum,download_checksum) VALUES (?1,'asset-other-shared','original',?2,?3,?4,?4)", rusqlite::params![ZONE,foreign_path.to_str().unwrap(),checksum,local]).unwrap();
        }
        conn.execute_batch("INSERT INTO asset_master_mappings VALUES ('PrimarySync','historical-child-a','old-primary-master',1700000000),('PrimarySync','historical-child-b','old-primary-master',1700000000); INSERT INTO asset_metadata_capture_revisions VALUES ('PrimarySync','old-primary-master',0,1700000000);").unwrap();
        conn.execute(
            "INSERT INTO metadata VALUES (?1,?2)",
            rusqlite::params![format!("sync_token:{ZONE}"), OLD_TOKEN],
        )
        .unwrap();
    }
    // Compare every released column, not only the post-migration snapshot.
    // Additional current-schema columns do not weaken this preservation check.
    fn released_rows(
        path: &std::path::Path,
        columns: &[(String, Vec<String>)],
    ) -> Vec<Vec<Vec<rusqlite::types::Value>>> {
        let conn = rusqlite::Connection::open(path).unwrap();
        columns
            .iter()
            .map(|(table, names)| {
                let sql = format!(
                    "SELECT {} FROM {} ORDER BY 1,2,3",
                    names
                        .iter()
                        .map(|n| format!("\"{n}\""))
                        .collect::<Vec<_>>()
                        .join(","),
                    table
                );
                let mut statement = conn.prepare(&sql).unwrap();
                statement
                    .query_map([], |row| {
                        (0..names.len())
                            .map(|i| row.get(i))
                            .collect::<rusqlite::Result<Vec<_>>>()
                    })
                    .unwrap()
                    .collect::<rusqlite::Result<Vec<_>>>()
                    .unwrap()
            })
            .collect()
    }
    let columns: Vec<_> = {
        let conn = rusqlite::Connection::open(&source).unwrap();
        [
            "assets",
            "asset_metadata_paths",
            "asset_master_mappings",
            "asset_metadata_capture_revisions",
        ]
        .into_iter()
        .map(|table| {
            let mut statement = conn
                .prepare(&format!(
                    "SELECT name FROM pragma_table_info('{table}') ORDER BY cid"
                ))
                .unwrap();
            let names = statement
                .query_map([], |row| row.get::<_, String>(0))
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap();
            (table.to_owned(), names)
        })
        .collect()
    };
    let legacy_rows = released_rows(&source, &columns);
    let legacy_bytes = std::fs::read(&source).unwrap();
    crate::state::db::account::adopt_legacy(
        &source,
        &db_path,
        &baseline_owner(),
        "baseline@example.invalid",
        "com",
    )
    .await
    .unwrap();
    assert_eq!(std::fs::read(&source).unwrap(), legacy_bytes);
    assert_eq!(
        released_rows(&db_path, &columns),
        legacy_rows,
        "released columns must survive authenticated adoption"
    );
    {
        let db = SqliteStateDb::open_owned(&db_path, &baseline_owner())
            .await
            .unwrap();
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
        assert_eq!(receipts.len(), if failure == Some("tail") { 2 } else { 4 });
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
        if failure == Some("tail") {
            assert!(
                selected_receipts.is_empty(),
                "ambiguous pair must not obtain compact completion"
            );
        } else {
            assert_eq!(selected_receipts.len(), 2);
            for (row, (version, path, content, provider)) in selected_receipts.iter().zip([
                ("live_original", &motion_path, MOTION, signature(MOTION)),
                (
                    "original",
                    &old_path,
                    provider_bytes.as_slice(),
                    checksum.clone(),
                ),
            ]) {
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
        let pending_path = root.join("existing_YXNzZXQ.JPG");
        let expected_destinations = vec![
            vec![
                SqlValue::Text("live_original".into()),
                SqlValue::Text(native(&motion_path)),
                SqlValue::Text(signature(MOTION)),
                SqlValue::Integer(1),
                SqlValue::Integer(1),
                SqlValue::Text(motion_hash.clone()),
                SqlValue::Text(motion_hash),
            ],
            vec![
                SqlValue::Text("original".into()),
                SqlValue::Text(native(if pending { &pending_path } else { &old_path })),
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
                Some(&old_path),
                "the selected child must use the independently proven Primary path"
            );
        } else if failure.is_some() {
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
            let motion = completed
                .iter()
                .find(|r| {
                    r.library.as_ref() == ZONE
                        && r.id.as_ref() == "asset-current-existing"
                        && r.version_size.as_str() == "live_original"
                })
                .unwrap();
            assert_eq!(motion.local_path.as_ref(), Some(&motion_path));
            assert_eq!(result.failed_count, 0, "{result:?}");
            assert!(result.db_sync_token_advance_safe, "{result:?}");
            assert_eq!(result.stats.downloaded, if cycle == 0 { 2 } else { 0 });
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
            (0, 0, 1, 1)
        );
        eprintln!(
            "RELEASED_SCHEMA_REACHED scenario={failure:?} cycle={cycle} selected_receipts={} active_destinations={} media_files={} existing_gets=0 companion_gets=0 independent_gets=2",
            selected_receipts.len(),
            destinations.len(),
            actual_tree.len()
        );
    }
    server.verify().await;
}
