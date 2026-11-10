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
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::atomic::AtomicUsize;
use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{method, path};
use wiremock::{Mock, ResponseTemplate};

const MOTION: &[u8] = b"\0\0\0ftypqt  \0\0\0\0qt  ";

#[derive(Clone, Copy, Debug)]
pub(in crate::sync_loop::runner::tests) enum ReleasedSchemaCase {
    /// A foreign ambiguous Primary family plus the selected Shared child.
    /// The selected child must materialize its own native Shared destinations.
    ForeignOwnerNegative,
    /// The selected Shared child owns its separated still/motion history.
    /// Exact local bytes must verify reuse without a provider GET.
    CurrentIdentityReuse,
    /// The own still history is correct but the last byte is corrupt beyond
    /// the old 64 KiB prefix. Reuse must refuse without rewriting the candidate.
    TailCorruption,
}

fn signature(bytes: &[u8]) -> String {
    let mut hash = Sha1::new();
    hash.update(b"com.apple.XattrObjectSalt\0com.apple.DataObjectSalt\0");
    hash.update(bytes);
    let mut value = vec![1];
    value.extend_from_slice(&hash.finalize());
    base64::engine::general_purpose::STANDARD.encode(value)
}

fn sha256_hex(bytes: &[u8]) -> String {
    data_encoding::HEXLOWER.encode(&sha2::Sha256::digest(bytes))
}

fn rows(path: &Path, sql: &str) -> Vec<Vec<rusqlite::types::Value>> {
    let conn = rusqlite::Connection::open(path).unwrap();
    let mut statement = conn.prepare(sql).unwrap();
    let count = statement.column_count();
    statement
        .query_map([], |row| {
            (0..count)
                .map(|index| row.get(index))
                .collect::<rusqlite::Result<Vec<_>>>()
        })
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap()
}

fn released_rows(
    path: &Path,
    columns: &[(String, Vec<String>)],
) -> Vec<Vec<Vec<rusqlite::types::Value>>> {
    columns
        .iter()
        .map(|(table, names)| {
            let sql = format!(
                "SELECT {} FROM {} ORDER BY 1,2,3",
                names
                    .iter()
                    .map(|name| format!("\"{name}\""))
                    .collect::<Vec<_>>()
                    .join(","),
                table
            );
            rows(path, &sql)
        })
        .collect()
}

fn released_columns(path: &Path) -> Vec<(String, Vec<String>)> {
    let conn = rusqlite::Connection::open(path).unwrap();
    [
        "assets",
        "asset_metadata_paths",
        "asset_master_mappings",
        "asset_metadata_capture_revisions",
        "legacy_master_state_owners",
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
}

#[allow(
    clippy::too_many_arguments,
    reason = "fixture columns stay explicit for the schema-25 reconstruction"
)]
fn insert_released_asset(
    conn: &rusqlite::Connection,
    library: &str,
    id: &str,
    version: &str,
    filename: &str,
    local_path: &Path,
    record_bytes: &[u8],
    provider_checksum: &str,
    source_backed: bool,
) {
    let local = sha256_hex(record_bytes);
    let source = source_backed.then_some(local.as_str());
    conn.execute(
        "INSERT INTO assets(library,id,version_size,checksum,filename,created_at,added_at,size_bytes,media_type,status,downloaded_at,local_path,last_seen_at,local_checksum,download_checksum,source) \
         VALUES (?1,?2,?3,?4,?5,1700000000,1700000002,?6,'photo','downloaded',1700000003,?7,1700000004,?8,?8,'icloud')",
        rusqlite::params![
            library,
            id,
            version,
            provider_checksum,
            filename,
            record_bytes.len() as i64,
            local_path.to_str().unwrap(),
            local
        ],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO asset_metadata_paths(library,id,version_size,local_path,provider_checksum,local_checksum,download_checksum,source_checksum) \
         VALUES (?1,?2,?3,?4,?5,?6,?6,?7)",
        rusqlite::params![
            library,
            id,
            version,
            local_path.to_str().unwrap(),
            provider_checksum,
            local,
            source
        ],
    )
    .unwrap();
}

fn identity_snapshot(database: &Path) -> Vec<Vec<rusqlite::types::Value>> {
    [
        "SELECT * FROM assets WHERE library='SharedSync-v027-synthetic' AND id='shared-old-master' ORDER BY version_size",
        "SELECT * FROM asset_metadata_paths WHERE library='SharedSync-v027-synthetic' AND id='shared-old-master' ORDER BY version_size",
        "SELECT * FROM legacy_master_state_owners WHERE library='SharedSync-v027-synthetic' AND master_record_name='shared-old-master' ORDER BY master_record_name",
        "SELECT * FROM asset_metadata_capture_revisions WHERE library='SharedSync-v027-synthetic' AND asset_id='shared-old-master' ORDER BY asset_id",
    ]
    .into_iter()
    .flat_map(|sql| rows(database, sql))
    .collect()
}

fn receipt_rows(database: &Path, child: &str) -> Vec<Vec<rusqlite::types::Value>> {
    rows(
        database,
        &format!(
            "SELECT account_key,provider_key,scope,library,child,master,state_id,origin_pass,version_size,provider_checksum,provider_size,native_path,source_checksum,local_checksum,proof_hash \
             FROM download_receipts WHERE library='{ZONE}' AND child='{child}' ORDER BY version_size"
        ),
    )
}

#[allow(
    clippy::too_many_arguments,
    reason = "the complete scoped receipt is asserted field by field"
)]
fn assert_receipt(
    row: &[rusqlite::types::Value],
    account_key: &str,
    provider_key: &str,
    expected_scope: &str,
    child: &str,
    master: &str,
    expected_pass: &str,
    version: &str,
    provider_checksum: &str,
    bytes: &[u8],
    native: &Path,
) {
    use rusqlite::types::Value as SqlValue;
    let digest = sha256_hex(bytes);
    let expected = vec![
        SqlValue::Text(account_key.into()),
        SqlValue::Text(provider_key.into()),
        SqlValue::Text(expected_scope.into()),
        SqlValue::Text(ZONE.into()),
        SqlValue::Text(child.into()),
        SqlValue::Text(master.into()),
        SqlValue::Text(child.into()),
        SqlValue::Text(expected_pass.into()),
        SqlValue::Text(version.into()),
        SqlValue::Text(provider_checksum.into()),
        SqlValue::Integer(i64::try_from(bytes.len()).unwrap()),
        SqlValue::Text(
            crate::state::db::provider_selection::SelectionPath::from_path(native)
                .key()
                .unwrap(),
        ),
        SqlValue::Text(digest.clone()),
        SqlValue::Text(digest),
    ];
    assert_eq!(
        &row[..14],
        expected.as_slice(),
        "incorrect scoped current receipt"
    );
    assert!(
        matches!(&row[14], SqlValue::Text(proof)
            if proof.len() == 64
                && proof.bytes().all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))),
        "current receipt proof hash must be 64 lowercase hex characters"
    );
}

pub(in crate::sync_loop::runner::tests) async fn exercise_released_schema(
    case: ReleasedSchemaCase,
) {
    let database = "shared";
    let foreign = matches!(case, ReleasedSchemaCase::ForeignOwnerNegative);
    let tail = matches!(case, ReleasedSchemaCase::TailCorruption);

    let mut provider_bytes = MEDIA.to_vec();
    provider_bytes.resize(provider_bytes.len().max(65_537), 42);
    assert_eq!(provider_bytes.len(), 65_537, "tail proof is above 64 KiB");
    let checksum = signature(&provider_bytes);
    let mut corrupt_still = provider_bytes.clone();
    *corrupt_still.last_mut().unwrap() ^= 1;
    let provider_filename = if foreign {
        "existing.JPG"
    } else {
        "shared-only.JPG"
    };

    let server = crate::test_helpers::start_wiremock_or_skip("released schema acceptance")
        .await
        .expect("acceptance server must bind");
    for (endpoint, expected, body) in [
        ("/existing", usize::from(foreign), provider_bytes.clone()),
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
        .expect(u64::from(foreign))
        .mount(&server)
        .await;

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("photos");
    std::fs::create_dir_all(&root).unwrap();

    let old_path = root.join("existing.JPG");
    let old_motion = root.join("existing.MOV");
    let sidecar_path = root.join("existing.JPG.xmp");
    let own_still = if foreign {
        root.join("existing_YXNzZXQ.JPG")
    } else {
        root.join("shared-only.JPG")
    };
    let own_motion = if foreign {
        root.join("existing_YXNzZXQ.MOV")
    } else {
        root.join("shared-only.MOV")
    };
    let foreign_path = root.join("foreign-shared.JPG");

    let (
        own_still_bytes,
        own_still_mtime,
        own_motion_mtime,
        old_mtime,
        old_motion_mtime,
        sidecar_mtime,
    ) = if foreign {
        std::fs::write(&old_path, &provider_bytes).unwrap();
        std::fs::write(&old_motion, MOTION).unwrap();
        std::fs::write(&sidecar_path, b"historical sidecar evidence").unwrap();
        (
            provider_bytes.clone(),
            None,
            None,
            Some(std::fs::metadata(&old_path).unwrap().modified().unwrap()),
            Some(std::fs::metadata(&old_motion).unwrap().modified().unwrap()),
            Some(
                std::fs::metadata(&sidecar_path)
                    .unwrap()
                    .modified()
                    .unwrap(),
            ),
        )
    } else {
        let still = if tail {
            corrupt_still.clone()
        } else {
            provider_bytes.clone()
        };
        std::fs::write(&own_still, &still).unwrap();
        std::fs::write(&own_motion, MOTION).unwrap();
        (
            still,
            Some(std::fs::metadata(&own_still).unwrap().modified().unwrap()),
            Some(std::fs::metadata(&own_motion).unwrap().modified().unwrap()),
            None,
            None,
            None,
        )
    };
    if foreign {
        std::fs::write(&foreign_path, &provider_bytes).unwrap();
    }

    // Synthetic schema-25 reconstruction. No released producer run is claimed.
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
        if foreign {
            insert_released_asset(
                &conn,
                "PrimarySync",
                "old-primary-master",
                "original",
                "existing.JPG",
                &old_path,
                &provider_bytes,
                &checksum,
                false,
            );
            insert_released_asset(
                &conn,
                "PrimarySync",
                "old-primary-master",
                "live_original",
                "existing.MOV",
                &old_motion,
                MOTION,
                &signature(MOTION),
                false,
            );
            let local = sha256_hex(&provider_bytes);
            conn.execute(
                "INSERT INTO assets(library,id,version_size,checksum,filename,created_at,size_bytes,media_type,status,local_path,last_seen_at,local_checksum,download_checksum) \
                 VALUES (?1,'asset-other-shared','original',?2,'foreign-shared.JPG',1700000000,?3,'photo','downloaded',?4,1700000000,?5,?5)",
                rusqlite::params![
                    ZONE,
                    checksum,
                    provider_bytes.len() as i64,
                    foreign_path.to_str().unwrap(),
                    local
                ],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO asset_metadata_paths(library,id,version_size,local_path,provider_checksum,local_checksum,download_checksum) \
                 VALUES (?1,'asset-other-shared','original',?2,?3,?4,?4)",
                rusqlite::params![
                    ZONE,
                    foreign_path.to_str().unwrap(),
                    checksum,
                    local
                ],
            )
            .unwrap();
            conn.execute_batch(
                "INSERT INTO asset_master_mappings VALUES ('PrimarySync','historical-child-a','old-primary-master',1700000000),('PrimarySync','historical-child-b','old-primary-master',1700000000);
                 INSERT INTO asset_metadata_capture_revisions VALUES ('PrimarySync','old-primary-master',0,1700000000);",
            )
            .unwrap();
        } else {
            insert_released_asset(
                &conn,
                ZONE,
                "shared-old-master",
                "original",
                "shared-only.JPG",
                &own_still,
                &provider_bytes,
                &checksum,
                true,
            );
            insert_released_asset(
                &conn,
                ZONE,
                "shared-old-master",
                "live_original",
                "shared-only.MOV",
                &own_motion,
                MOTION,
                &signature(MOTION),
                true,
            );
            conn.execute_batch(&format!(
                "INSERT INTO legacy_master_state_owners VALUES ('{ZONE}','shared-old-master','asset-current-existing',1700000000);
                 INSERT INTO asset_master_mappings VALUES ('{ZONE}','asset-current-existing','current-existing',1700000000);
                 INSERT INTO asset_metadata_capture_revisions VALUES ('{ZONE}','shared-old-master',1,1700000000);"
            ))
            .unwrap();
        }
        conn.execute(
            "INSERT INTO metadata VALUES (?1,?2)",
            rusqlite::params![format!("sync_token:{ZONE}"), OLD_TOKEN],
        )
        .unwrap();
    }

    let columns = released_columns(&source);
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

    if foreign {
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

    if !foreign {
        assert!(
            receipt_rows(&db_path, "asset-current-existing").is_empty(),
            "current selected child starts without a current receipt"
        );
        assert!(
            rows(
                &db_path,
                &format!(
                    "SELECT id,version_size,local_path FROM asset_metadata_paths WHERE library='{ZONE}' AND id='asset-current-existing'"
                ),
            )
            .is_empty(),
            "current selected child starts without a current path proof"
        );
        let identity_history = rows(
            &db_path,
            &format!(
                "SELECT version_size,local_path,local_checksum,source_checksum FROM asset_metadata_paths WHERE library='{ZONE}' AND id='shared-old-master' ORDER BY version_size"
            ),
        );
        assert_eq!(identity_history.len(), 2, "same-identity source history");
        use rusqlite::types::Value as SqlValue;
        let expected_history = std::collections::BTreeSet::from([
            (
                "live_original".to_owned(),
                own_motion.to_str().unwrap().to_owned(),
                sha256_hex(MOTION),
                sha256_hex(MOTION),
            ),
            (
                "original".to_owned(),
                own_still.to_str().unwrap().to_owned(),
                sha256_hex(&provider_bytes),
                sha256_hex(&provider_bytes),
            ),
        ]);
        let mut actual_history = std::collections::BTreeSet::new();
        for row in &identity_history {
            let (
                SqlValue::Text(version),
                SqlValue::Text(path),
                SqlValue::Text(local),
                SqlValue::Text(source),
            ) = (&row[0], &row[1], &row[2], &row[3])
            else {
                panic!("historical identity row must carry text proof");
            };
            actual_history.insert((version.clone(), path.clone(), local.clone(), source.clone()));
        }
        assert_eq!(
            actual_history, expected_history,
            "exact still/motion same-identity source history"
        );
    }

    let original_rows = historical_rows(&db_path);
    let identity_rows = identity_snapshot(&db_path);
    let foreign_snapshot = || {
        rows(
            &db_path,
            &format!(
                "SELECT * FROM assets WHERE library='{ZONE}' AND id='asset-other-shared' ORDER BY version_size"
            ),
        )
        .into_iter()
        .chain(rows(
            &db_path,
            &format!(
                "SELECT * FROM asset_metadata_paths WHERE library='{ZONE}' AND id='asset-other-shared' ORDER BY version_size"
            ),
        ))
        .collect::<Vec<_>>()
    };
    let foreign_rows = foreign_snapshot();
    let foreign_mtime = foreign.then(|| {
        std::fs::metadata(&foreign_path)
            .unwrap()
            .modified()
            .unwrap()
    });

    let mut existing = pair(
        "current-existing",
        provider_filename,
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

    // Precompute the complete tree before any cycle runs.
    let mut expected_tree = BTreeMap::from([
        ("missing_YXNzZXQ.JPG".to_owned(), MEDIA.to_vec()),
        ("concurrent_YXNzZXQ.JPG".to_owned(), MEDIA.to_vec()),
        (
            own_still
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned(),
            own_still_bytes.clone(),
        ),
        (
            own_motion
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned(),
            MOTION.to_vec(),
        ),
    ]);
    if foreign {
        expected_tree.insert("existing.JPG".to_owned(), provider_bytes.clone());
        expected_tree.insert("existing.MOV".to_owned(), MOTION.to_vec());
        expected_tree.insert(
            "existing.JPG.xmp".to_owned(),
            b"historical sidecar evidence".to_vec(),
        );
        expected_tree.insert("foreign-shared.JPG".to_owned(), provider_bytes.clone());
    }

    let mut durable_snapshot = None;
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

        if foreign {
            assert_eq!(std::fs::read(&old_path).unwrap(), provider_bytes);
            assert_eq!(
                std::fs::metadata(&old_path).unwrap().modified().unwrap(),
                old_mtime.unwrap()
            );
            assert_eq!(std::fs::read(&old_motion).unwrap(), MOTION);
            assert_eq!(
                std::fs::metadata(&old_motion).unwrap().modified().unwrap(),
                old_motion_mtime.unwrap()
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
                sidecar_mtime.unwrap()
            );
            assert_eq!(passed_foreign_rows(&db_path), foreign_rows);
            assert_eq!(
                std::fs::read(&foreign_path).unwrap(),
                provider_bytes,
                "foreign Shared owner bytes must stay intact"
            );
            assert_eq!(
                std::fs::metadata(&foreign_path)
                    .unwrap()
                    .modified()
                    .unwrap(),
                foreign_mtime.unwrap()
            );
        }
        assert_eq!(std::fs::read(&own_still).unwrap(), own_still_bytes);
        if let Some(mtime) = own_still_mtime {
            assert_eq!(
                std::fs::metadata(&own_still).unwrap().modified().unwrap(),
                mtime
            );
        }
        assert_eq!(std::fs::read(&own_motion).unwrap(), MOTION);
        if let Some(mtime) = own_motion_mtime {
            assert_eq!(
                std::fs::metadata(&own_motion).unwrap().modified().unwrap(),
                mtime
            );
        }
        assert_eq!(historical_rows(&db_path), original_rows);
        if !foreign {
            assert_eq!(identity_snapshot(&db_path), identity_rows);
        }

        let mut actual_tree = BTreeMap::new();
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
        assert_eq!(
            actual_tree, expected_tree,
            "complete media tree changed or gained an entry"
        );

        let completed = db.get_downloaded_page(0, 100).await.unwrap();
        let current = completed.iter().find(|row| {
            row.library.as_ref() == ZONE
                && row.id.as_ref() == "asset-current-existing"
                && row.version_size.as_str() == "original"
        });
        let motion = completed.iter().find(|row| {
            row.library.as_ref() == ZONE
                && row.id.as_ref() == "asset-current-existing"
                && row.version_size.as_str() == "live_original"
        });
        if tail {
            assert!(current.is_none(), "corrupt own still must not complete");
            let motion = motion.expect("valid own motion must reuse");
            assert_eq!(motion.local_path.as_ref(), Some(&own_motion));
            assert_eq!(std::fs::read(&own_motion).unwrap(), MOTION);
        } else {
            let current = current.expect("selected child must complete");
            assert_eq!(current.local_path.as_ref(), Some(&own_still));
            assert_eq!(std::fs::read(&own_still).unwrap(), own_still_bytes);
            let motion = motion.expect("selected motion child must complete");
            assert_eq!(motion.local_path.as_ref(), Some(&own_motion));
        }

        let connection = rusqlite::Connection::open(&db_path).unwrap();
        let guessed: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM legacy_master_state_owners WHERE library='PrimarySync' AND master_record_name='old-primary-master'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            guessed, 0,
            "current coverage cannot invent historical sibling ownership"
        );
        drop(connection);

        if foreign {
            let receipts = receipt_rows(&db_path, "asset-current-existing");
            assert_eq!(receipts.len(), 2, "selected still and motion receipts");
            assert_receipt(
                &receipts[0],
                account_key,
                provider_key,
                &expected_scope,
                "asset-current-existing",
                "current-existing",
                &expected_pass,
                "live_original",
                &signature(MOTION),
                MOTION,
                &own_motion,
            );
            assert_receipt(
                &receipts[1],
                account_key,
                provider_key,
                &expected_scope,
                "asset-current-existing",
                "current-existing",
                &expected_pass,
                "original",
                &checksum,
                &provider_bytes,
                &own_still,
            );
            assert_eq!(result.failed_count, 0, "{result:?}");
            assert!(result.db_sync_token_advance_safe, "{result:?}");
            assert_eq!(result.stats.downloaded, if cycle == 0 { 4 } else { 0 });
            assert_eq!(
                db.get_metadata(&token_key).await.unwrap().as_deref(),
                Some(format!("v27-after-cycle-{}", cycle + 1).as_str())
            );
        } else {
            let receipts = receipt_rows(&db_path, "asset-current-existing");
            if tail {
                assert_eq!(
                    receipts.len(),
                    0,
                    "no current receipt may bless the corrupt still or the reused motion"
                );
                let still_obligation = rows(
                    &db_path,
                    &format!(
                        "SELECT status,local_path,local_checksum,checksum,size_bytes,download_checksum FROM assets WHERE library='{ZONE}' AND id='asset-current-existing' AND version_size='original'"
                    ),
                );
                assert_eq!(still_obligation.len(), 1, "current still debt row");
                use rusqlite::types::Value as SqlValue;
                assert!(
                    matches!(
                        &still_obligation[0][0],
                        SqlValue::Text(status) if status == "pending" || status == "failed"
                    ),
                    "current still must stay pending or failed"
                );
                assert_eq!(still_obligation[0][1], SqlValue::Null);
                assert_eq!(still_obligation[0][2], SqlValue::Null);
                assert_eq!(
                    still_obligation[0][3],
                    SqlValue::Text(checksum.clone()),
                    "current still row keeps the fixture provider checksum"
                );
                assert_eq!(
                    still_obligation[0][4],
                    SqlValue::Integer(65_537),
                    "current still row keeps the fixture provider size"
                );
                assert_eq!(
                    still_obligation[0][5],
                    SqlValue::Null,
                    "current still row has no download checksum"
                );
                assert!(
                    rows(
                        &db_path,
                        &format!(
                            "SELECT id FROM asset_metadata_paths WHERE library='{ZONE}' AND id='asset-current-existing' AND version_size='original'"
                        ),
                    )
                    .is_empty(),
                    "no current still path proof may exist"
                );
                let historical_still = rows(
                    &db_path,
                    &format!(
                        "SELECT status,local_path,local_checksum FROM assets WHERE library='{ZONE}' AND id='shared-old-master' AND version_size='original'"
                    ),
                );
                assert_eq!(historical_still.len(), 1, "before-corruption proof row");
                assert_eq!(historical_still[0][0], SqlValue::Text("downloaded".into()));
                assert_eq!(
                    historical_still[0][1],
                    SqlValue::Text(own_still.to_str().unwrap().into())
                );
                assert_eq!(
                    historical_still[0][2],
                    SqlValue::Text(sha256_hex(&provider_bytes))
                );
                let historical_still_path = rows(
                    &db_path,
                    &format!(
                        "SELECT local_checksum,source_checksum FROM asset_metadata_paths WHERE library='{ZONE}' AND id='shared-old-master' AND version_size='original'"
                    ),
                );
                assert_eq!(historical_still_path.len(), 1);
                assert_eq!(
                    historical_still_path[0][0],
                    SqlValue::Text(sha256_hex(&provider_bytes))
                );
                assert_eq!(
                    historical_still_path[0][1],
                    SqlValue::Text(sha256_hex(&provider_bytes))
                );
                let active_destinations = rows(
                    &db_path,
                    &format!(
                        "SELECT d.generation,d.pass_key,g.scope,g.sealed,g.checkpoint_ready,d.version_size,d.path,d.checksum,d.size_bytes,d.admitted,d.verified_media,d.source_checksum,d.local_checksum \
                         FROM provider_active_destinations d JOIN provider_active_generations g ON g.id=d.generation \
                         WHERE d.library='{ZONE}' AND d.child='asset-current-existing' ORDER BY d.generation,d.version_size"
                    ),
                );
                assert_eq!(
                    active_destinations.len(),
                    4,
                    "two retained roots times still and motion"
                );
                let mut generations = std::collections::BTreeSet::new();
                let literal_candidate = root.join("shared-only_YXNzZXQ.JPG");
                for row in &active_destinations {
                    let SqlValue::Text(generation) = &row[0] else {
                        panic!("generation must be text");
                    };
                    generations.insert(generation.clone());
                    assert_eq!(row[1], SqlValue::Text(expected_pass.clone()));
                    assert_eq!(row[2], SqlValue::Text(expected_scope.clone()));
                    assert_eq!(row[3], SqlValue::Integer(1));
                    assert_eq!(row[4], SqlValue::Integer(1));
                    let SqlValue::Text(version) = &row[5] else {
                        panic!("version must be text");
                    };
                    if version == "original" {
                        assert_eq!(
                            row[6],
                            SqlValue::Text(
                                crate::state::db::provider_selection::SelectionPath::from_path(
                                    &literal_candidate
                                )
                                .key()
                                .unwrap()
                            ),
                            "literal pending candidate path"
                        );
                        assert_eq!(row[7], SqlValue::Text(checksum.clone()));
                        assert_eq!(row[8], SqlValue::Integer(65_537));
                        assert_eq!(row[9], SqlValue::Integer(1));
                        assert_eq!(row[10], SqlValue::Integer(0));
                        assert_eq!(row[11], SqlValue::Null);
                        assert_eq!(row[12], SqlValue::Null);
                    } else {
                        assert_eq!(version, "live_original");
                        assert_eq!(
                            row[6],
                            SqlValue::Text(
                                crate::state::db::provider_selection::SelectionPath::from_path(
                                    &own_motion
                                )
                                .key()
                                .unwrap()
                            )
                        );
                        assert_eq!(row[7], SqlValue::Text(signature(MOTION)));
                        assert_eq!(row[8], SqlValue::Integer(20));
                        assert_eq!(row[9], SqlValue::Integer(1));
                        assert_eq!(row[10], SqlValue::Integer(1));
                        assert_eq!(row[11], SqlValue::Text(sha256_hex(MOTION)));
                        assert_eq!(row[12], SqlValue::Text(sha256_hex(MOTION)));
                    }
                }
                assert_eq!(generations.len(), 2, "two explicit retained roots");
                let motion = motion.expect("valid own motion must reuse");
                assert_eq!(motion.local_path.as_ref(), Some(&own_motion));
                assert_eq!(std::fs::read(&own_motion).unwrap(), MOTION);
                let motion_path = rows(
                    &db_path,
                    &format!(
                        "SELECT local_checksum,source_checksum FROM asset_metadata_paths WHERE library='{ZONE}' AND id='asset-current-existing' AND version_size='live_original'"
                    ),
                );
                assert_eq!(motion_path.len(), 1, "current motion path proof");
                assert_eq!(motion_path[0][0], SqlValue::Text(sha256_hex(MOTION)));
                assert_eq!(motion_path[0][1], SqlValue::Text(sha256_hex(MOTION)));
                assert_eq!(result.stats.downloaded, if cycle == 0 { 2 } else { 0 });
            } else {
                assert_eq!(
                    receipts.len(),
                    2,
                    "exactly the current still and motion receipts"
                );
                let mut actual = std::collections::BTreeSet::new();
                for receipt in &receipts {
                    use rusqlite::types::Value as SqlValue;
                    let SqlValue::Text(version) = &receipt[8] else {
                        panic!("receipt version must be text");
                    };
                    let SqlValue::Text(path) = &receipt[11] else {
                        panic!("receipt native path must be text");
                    };
                    actual.insert((version.clone(), path.clone()));
                }
                let native = |path: &Path| {
                    crate::state::db::provider_selection::SelectionPath::from_path(path)
                        .key()
                        .unwrap()
                };
                assert_eq!(
                    actual,
                    std::collections::BTreeSet::from([
                        ("live_original".to_owned(), native(&own_motion)),
                        ("original".to_owned(), native(&own_still)),
                    ]),
                    "current receipt identities are still and motion"
                );
                for receipt in &receipts {
                    let path: String = match &receipt[11] {
                        rusqlite::types::Value::Text(path) => path.clone(),
                        other => panic!("receipt native path must be text: {other:?}"),
                    };
                    let expected = if path == native(&own_motion) {
                        ("live_original", &signature(MOTION), MOTION, &own_motion)
                    } else {
                        assert_eq!(path, native(&own_still));
                        (
                            "original",
                            &checksum,
                            own_still_bytes.as_slice(),
                            &own_still,
                        )
                    };
                    assert_receipt(
                        receipt,
                        account_key,
                        provider_key,
                        &expected_scope,
                        "asset-current-existing",
                        "current-existing",
                        &expected_pass,
                        expected.0,
                        expected.1,
                        expected.2,
                        expected.3,
                    );
                }
                let active_destinations = rows(
                    &db_path,
                    &format!(
                        "SELECT d.generation,d.pass_key,g.scope,g.sealed,g.checkpoint_ready,d.version_size,d.path,d.checksum,d.size_bytes,d.admitted,d.verified_media,d.source_checksum,d.local_checksum \
                         FROM provider_active_destinations d JOIN provider_active_generations g ON g.id=d.generation \
                         WHERE d.library='{ZONE}' AND d.child='asset-current-existing' ORDER BY d.generation,d.version_size"
                    ),
                );
                assert_eq!(
                    active_destinations.len(),
                    2,
                    "one active root times still and motion"
                );
                let mut generations = std::collections::BTreeSet::new();
                use rusqlite::types::Value as SqlValue;
                for row in &active_destinations {
                    let SqlValue::Text(generation) = &row[0] else {
                        panic!("generation must be text");
                    };
                    generations.insert(generation.clone());
                    assert_eq!(row[1], SqlValue::Text(expected_pass.clone()));
                    assert_eq!(row[2], SqlValue::Text(expected_scope.clone()));
                    assert_eq!(row[3], SqlValue::Integer(1));
                    assert_eq!(row[4], SqlValue::Integer(1));
                    let SqlValue::Text(version) = &row[5] else {
                        panic!("version must be text");
                    };
                    let (path, checksum, size, source) = if version == "original" {
                        (
                            own_still.as_path(),
                            checksum.clone(),
                            65_537,
                            sha256_hex(&provider_bytes),
                        )
                    } else {
                        assert_eq!(version, "live_original");
                        (
                            own_motion.as_path(),
                            signature(MOTION),
                            i64::try_from(MOTION.len()).unwrap(),
                            sha256_hex(MOTION),
                        )
                    };
                    assert_eq!(
                        row[6],
                        SqlValue::Text(
                            crate::state::db::provider_selection::SelectionPath::from_path(path)
                                .key()
                                .unwrap()
                        )
                    );
                    assert_eq!(row[7], SqlValue::Text(checksum));
                    assert_eq!(row[8], SqlValue::Integer(size));
                    assert_eq!(row[9], SqlValue::Integer(1));
                    assert_eq!(row[10], SqlValue::Integer(1));
                    assert_eq!(row[11], SqlValue::Text(source.clone()));
                    assert_eq!(row[12], SqlValue::Text(source));
                }
                assert_eq!(generations.len(), 1, "one explicit active root");
                assert_eq!(
                    result.stats.downloaded,
                    if cycle == 0 { 2 } else { 0 },
                    "verified reuse completes the two owned renditions once"
                );
            }
            assert_eq!(
                result.failed_count,
                if tail {
                    if cycle == 0 { 6 } else { 2 }
                } else {
                    0
                },
                "{result:?}"
            );
            assert!(result.db_sync_token_advance_safe, "{result:?}");
            assert_eq!(
                db.get_metadata(&token_key).await.unwrap().as_deref(),
                Some(format!("v27-after-cycle-{}", cycle + 1).as_str())
            );
        }

        if let Some((old_receipts, old_destinations)) = &durable_snapshot {
            let connection = rusqlite::Connection::open(&db_path).unwrap();
            let receipts = rows(&db_path, "SELECT * FROM download_receipts ORDER BY id");
            let destinations = rows(
                &db_path,
                "SELECT * FROM provider_active_destinations ORDER BY generation,pass_key,child,version_size,path",
            );
            drop(connection);
            assert_eq!(
                &receipts, old_receipts,
                "compact receipts changed on reopen"
            );
            assert_eq!(
                &destinations, old_destinations,
                "admitted destinations changed on reopen"
            );
        } else {
            let receipts = rows(&db_path, "SELECT * FROM download_receipts ORDER BY id");
            let destinations = rows(
                &db_path,
                "SELECT * FROM provider_active_destinations ORDER BY generation,pass_key,child,version_size,path",
            );
            durable_snapshot = Some((receipts, destinations));
        }

        let requests = server.received_requests().await.unwrap();
        let count = |path: &str| {
            requests
                .iter()
                .filter(|request| request.method.as_str() == "GET" && request.url.path() == path)
                .count()
        };
        assert_eq!(
            (
                count("/existing"),
                count("/companion"),
                count("/missing"),
                count("/concurrent")
            ),
            (usize::from(foreign), usize::from(foreign), 1, 1)
        );
        eprintln!(
            "RELEASED_SCHEMA_REACHED case={case:?} cycle={cycle} selected_receipts={} media_files={} existing_gets={} companion_gets={}",
            receipt_rows(&db_path, "asset-current-existing").len(),
            actual_tree.len(),
            usize::from(foreign),
            usize::from(foreign)
        );
    }
    server.verify().await;
}

fn passed_foreign_rows(database: &Path) -> Vec<Vec<rusqlite::types::Value>> {
    rows(
        database,
        &format!(
            "SELECT * FROM assets WHERE library='{ZONE}' AND id='asset-other-shared' ORDER BY version_size"
        ),
    )
    .into_iter()
    .chain(rows(
        database,
        &format!(
            "SELECT * FROM asset_metadata_paths WHERE library='{ZONE}' AND id='asset-other-shared' ORDER BY version_size"
        ),
    ))
    .collect()
}

/// Populated released-state scenarios built from the genuine released schema
/// constructors. Rows are synthetic; no released producer run is claimed.
#[derive(Clone, Copy, Debug)]
pub(in crate::sync_loop::runner::tests) enum ReleasedPopulatedCase {
    /// Schema 28 Shared history whose exact child owns the separated still and
    /// motion. Both verify from local bytes with no provider GET.
    SameIdentityReuse28,
    /// The same positive on the genuine schema-36 shape.
    SameIdentityReuse36,
    /// Schema 36 Shared history with a recorded failed live companion. The
    /// still verifies locally, the companion expires once during the ordinary
    /// cycle and must settle from the exact fresh URL at the original stem.
    MotionRefresh36,
}

/// Wraps the ordinary baseline provider so only the cleanup lookup for the
/// expired companion receives a fresh URL. The initial inventory keeps the
/// stale URL, proving the refresh happened instead of a lucky first transfer.
#[derive(Clone)]
struct MotionRefreshSession {
    inner: BaselineSession,
    stale_motion_url: String,
    fresh_motion_url: String,
    cycle: usize,
    lookups: Arc<Mutex<Vec<(usize, serde_json::Value)>>>,
    stale_lookups: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl crate::icloud::photos::PhotosSession for MotionRefreshSession {
    async fn post(
        &self,
        url: &str,
        body: String,
        headers: &[(&str, &str)],
    ) -> anyhow::Result<serde_json::Value> {
        if url.contains("/records/lookup?") {
            let request: serde_json::Value = serde_json::from_str(&body)?;
            self.lookups
                .lock()
                .unwrap()
                .push((self.cycle, request.clone()));
            let requested = request["records"].as_array().cloned().unwrap_or_default();
            let mut records = Vec::new();
            for record in self
                .inner
                .inventory
                .iter()
                .chain(self.inner.concurrent.iter())
                .chain(self.inner.incremental_addition.iter())
            {
                if !requested
                    .iter()
                    .any(|query| query["recordName"] == record["recordName"])
                {
                    continue;
                }
                let mut record = record.clone();
                if let Some(companion) = record["fields"].get_mut("resOriginalVidComplRes")
                    && companion["value"]["downloadURL"] == json!(self.stale_motion_url)
                {
                    // The first exact revalidation must use the recorded stale
                    // URL so the cycle proves the 410 -> fresh cleanup path.
                    // Later lookups return the current URL.
                    let first_lookup = self
                        .stale_lookups
                        .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                        == 0;
                    if !first_lookup {
                        companion["value"]["downloadURL"] = json!(self.fresh_motion_url);
                    }
                }
                records.push(record);
            }
            return Ok(json!({ "records": records }));
        }
        self.inner.post(url, body, headers).await
    }

    fn clone_box(&self) -> Box<dyn crate::icloud::photos::PhotosSession> {
        Box::new(self.clone())
    }
}

/// Real application config for an unfiled-only sync: albums none, unfiled
/// true, every other option at its normal default.
fn make_populated_app_config() -> crate::config::Config {
    let data_dir = tempfile::tempdir().unwrap();
    let globals = crate::config::GlobalArgs {
        username: Some("baseline@example.invalid".to_string()),
        domain: None,
        data_dir: Some(data_dir.path().to_string_lossy().into_owned()),
    };
    let mut sync = crate::cli::SyncArgs::default();
    sync.config_overrides.albums = vec!["none".to_string()];
    sync.config_overrides.unfiled = Some(true);
    crate::config::Config::build(&globals, &crate::cli::PasswordArgs::default(), sync, None)
        .unwrap()
}

pub(in crate::sync_loop::runner::tests) async fn exercise_released_populated(
    case: ReleasedPopulatedCase,
) {
    let (trace, _trace_guard) = crate::test_helpers::TracingCapture::install();
    let motion_refresh = matches!(case, ReleasedPopulatedCase::MotionRefresh36);
    // The unchanged-master variant keeps the released master record name and
    // the saved exclusive owner/mapping consistent, so pending revalidation
    // asks for a record the provider actually serves.
    let current_child = if motion_refresh {
        "asset-old-master"
    } else {
        "asset-current-master"
    };
    let current_master = if motion_refresh {
        "old-master"
    } else {
        "current-master"
    };
    let provider_still = MEDIA.to_vec();
    let checksum = signature(&provider_still);
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("photos");
    std::fs::create_dir_all(&root).unwrap();

    let still_path = root.join("shared-only.JPG");
    let motion_path = root.join("shared-only.MOV");
    std::fs::write(&still_path, &provider_still).unwrap();
    if !motion_refresh {
        std::fs::write(&motion_path, MOTION).unwrap();
    }
    let still_mtime = std::fs::metadata(&still_path).unwrap().modified().unwrap();
    let motion_mtime =
        (!motion_refresh).then(|| std::fs::metadata(&motion_path).unwrap().modified().unwrap());

    let source = dir.path().join("released.db");
    let db_path = dir.path().join("state.db");
    {
        let conn = rusqlite::Connection::open(&source).unwrap();
        match case {
            ReleasedPopulatedCase::SameIdentityReuse28 => {
                crate::state::schema::migrate_genuine_v28(&conn).unwrap();
                assert_eq!(
                    conn.query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))
                        .unwrap(),
                    28
                );
            }
            ReleasedPopulatedCase::SameIdentityReuse36 | ReleasedPopulatedCase::MotionRefresh36 => {
                crate::state::schema::migrate_genuine_v36(&conn).unwrap();
                assert_eq!(
                    conn.query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))
                        .unwrap(),
                    36
                );
            }
        }
        // Synthetic rows in the genuine released schema shape.
        insert_released_asset(
            &conn,
            ZONE,
            "old-master",
            "original",
            "shared-only.JPG",
            &still_path,
            &provider_still,
            &checksum,
            true,
        );
        if motion_refresh {
            conn.execute(
                "INSERT INTO assets(library,id,version_size,checksum,filename,created_at,added_at,size_bytes,media_type,status,last_seen_at,download_attempts,last_error,source) \
                 VALUES (?1,'old-master','live_original',?2,'shared-only.MOV',1700000000,1700000002,?3,'video','failed',1700000004,2,'previous live companion transfer failed','icloud')",
                rusqlite::params![ZONE, signature(MOTION), MOTION.len() as i64],
            )
            .unwrap();
        } else {
            insert_released_asset(
                &conn,
                ZONE,
                "old-master",
                "live_original",
                "shared-only.MOV",
                &motion_path,
                MOTION,
                &signature(MOTION),
                true,
            );
        }
        conn.execute_batch(&format!(
            "INSERT INTO legacy_master_state_owners VALUES ('{ZONE}','old-master','{current_child}',1700000000);
             INSERT INTO asset_master_mappings VALUES ('{ZONE}','{current_child}','{current_master}',1700000000);
             INSERT INTO asset_metadata_capture_revisions VALUES ('{ZONE}','old-master',1,1700000000);"
        ))
        .unwrap();
        conn.execute(
            "INSERT INTO metadata VALUES (?1,?2)",
            rusqlite::params![format!("sync_token:{ZONE}"), OLD_TOKEN],
        )
        .unwrap();
    }

    let columns = released_columns(&source);
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
        "released rows must survive the documented existing-upgrade adoption"
    );

    // No test-only epoch marker, completed epoch, v27 token or current receipt.
    {
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        assert!(
            crate::state::db::sync_epoch::read_library(&conn, ZONE)
                .unwrap()
                .is_none(),
            "enrollment must happen through the production cycle, not setup"
        );
        let version = conn
            .query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))
            .unwrap();
        assert_eq!(version, i64::from(crate::state::schema::SCHEMA_VERSION));
    }
    assert!(
        receipt_rows(&db_path, current_child).is_empty(),
        "current selected child starts without a current receipt"
    );
    assert!(
        rows(
            &db_path,
            &format!(
                "SELECT id FROM asset_metadata_paths WHERE library='{ZONE}' AND id='asset-current'"
            ),
        )
        .is_empty(),
        "current selected child starts without a current path proof"
    );
    let identity_snapshot = || {
        rows(
            &db_path,
            &format!(
                "SELECT filename,status,local_path,local_checksum,checksum,size_bytes FROM assets WHERE library='{ZONE}' AND id='old-master' AND version_size='original'"
            ),
        )
        .into_iter()
        .chain(rows(
            &db_path,
            &format!(
                "SELECT version_size,local_path,local_checksum,source_checksum FROM asset_metadata_paths WHERE library='{ZONE}' AND id='old-master' AND version_size='original'"
            ),
        ))
        .chain(rows(
            &db_path,
            &format!(
                "SELECT * FROM legacy_master_state_owners WHERE library='{ZONE}' AND master_record_name='old-master'"
            ),
        ))
        .collect::<Vec<_>>()
    };
    let identity_before = identity_snapshot();

    let server = crate::test_helpers::start_wiremock_or_skip("released populated acceptance")
        .await
        .expect("acceptance server must bind");
    Mock::given(method("GET"))
        .and(path("/existing"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(provider_still.clone())
                .insert_header("content-type", "image/jpeg"),
        )
        .expect(0)
        .mount(&server)
        .await;
    for endpoint in ["/new", "/during"] {
        Mock::given(method("GET"))
            .and(path(endpoint))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_bytes(MEDIA)
                    .insert_header("content-type", "image/jpeg"),
            )
            .expect(1)
            .mount(&server)
            .await;
    }
    if motion_refresh {
        Mock::given(method("GET"))
            .and(path("/motion-stale"))
            .respond_with(ResponseTemplate::new(410))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/motion-fresh"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_bytes(MOTION)
                    .insert_header("content-type", "video/quicktime"),
            )
            .expect(1)
            .mount(&server)
            .await;
    }

    let mut existing = pair(
        current_master,
        "shared-only.JPG",
        &format!("{}/existing", server.uri()),
        1,
    );
    existing[0]["fields"]["resOriginalRes"]["value"]["fileChecksum"] = json!(checksum);
    existing[0]["fields"]["resOriginalRes"]["value"]["size"] = json!(provider_still.len());
    existing[0]["fields"]["resOriginalVidComplRes"] = json!({"value":{
        "downloadURL":if motion_refresh {
            format!("{}/motion-stale",server.uri())
        } else {
            format!("{}/existing-motion-never-requested",server.uri())
        },
        "size":MOTION.len(),
        "fileChecksum":signature(MOTION)}});
    existing[0]["fields"]["resOriginalVidComplFileType"] =
        json!({"value":"com.apple.quicktime-movie"});
    let mut new_item = pair("new-master", "new.JPG", &format!("{}/new", server.uri()), 2);
    new_item[0]["fields"]["resOriginalRes"]["value"]["fileChecksum"] = json!(checksum.clone());
    new_item[0]["fields"]["resOriginalRes"]["value"]["size"] = json!(MEDIA.len());
    let mut inventory = existing;
    inventory.extend(new_item);
    // A genuinely new item arrives on the concurrent bridge rather than the
    // first inventory page, so the baseline proves current progress as well as
    // byte-level reuse. The ordinary baseline provider's count side-channel
    // expects two initial assets and one concurrent addition.
    let mut concurrent = pair(
        "during-master",
        "during.JPG",
        &format!("{}/during", server.uri()),
        3,
    );
    concurrent[0]["fields"]["resOriginalRes"]["value"]["fileChecksum"] = json!(checksum.clone());
    concurrent[0]["fields"]["resOriginalRes"]["value"]["size"] = json!(MEDIA.len());

    let token_key = sync_token_key(ZONE);
    let (_session_dir, shared) = make_shared_session_for_run_cycle().await;
    let app_config = make_populated_app_config();
    let lookups = Arc::new(Mutex::new(Vec::new()));
    let stale_lookups = Arc::new(AtomicUsize::new(0));
    let new_name = if motion_refresh {
        "new.JPG"
    } else {
        "new_YXNzZXQ.JPG"
    };
    let during_name = if motion_refresh {
        "during.JPG"
    } else {
        "during_YXNzZXQ.JPG"
    };
    let expected_tree = BTreeMap::from([
        ("shared-only.JPG".to_owned(), provider_still.clone()),
        ("shared-only.MOV".to_owned(), MOTION.to_vec()),
        (new_name.to_owned(), MEDIA.to_vec()),
        (during_name.to_owned(), MEDIA.to_vec()),
    ]);
    let mut durable_snapshot = None;

    for cycle in 0..3 {
        let db = Arc::new(
            SqliteStateDb::open_owned(&db_path, &baseline_owner())
                .await
                .unwrap(),
        );
        let inner = BaselineSession {
            database: "shared",
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
            "https://synthetic.example/database/1/photos/production/shared".to_string(),
            Arc::new(std::collections::HashMap::new()),
            if motion_refresh {
                Box::new(MotionRefreshSession {
                    inner: inner.clone(),
                    stale_motion_url: format!("{}/motion-stale", server.uri()),
                    fresh_motion_url: format!("{}/motion-fresh", server.uri()),
                    cycle,
                    lookups: Arc::clone(&lookups),
                    stale_lookups: Arc::clone(&stale_lookups),
                })
            } else {
                Box::new(inner.clone())
            },
            Arc::new(zone()),
            "shared".into(),
            crate::retry::RetryConfig::default(),
        )
        .await
        .unwrap();
        library.shadow_capture = Some(crate::icloud::photos::inbox::ShadowCapture::new(
            db.clone(),
            baseline_owner(),
            "com",
        ));
        // Resolve the real application selection (current SyncArgs defaults:
        // albums none, unfiled true) through the production planner. This is
        // not a manual AlbumPass override.
        let selection = &app_config.filters.selection;
        let selected_zones = crate::commands::zone_name_set(std::slice::from_ref(&library));
        let collection_zones = selected_zones.clone();
        let resolved_scope = crate::commands::pass_scope_for_zone(
            selection,
            ZONE,
            &selected_zones,
            &collection_zones,
        );
        assert!(resolved_scope.include_unfiled);
        assert!(!resolved_scope.include_albums);
        assert!(!resolved_scope.include_smart_folders);
        let collection_context =
            crate::commands::build_collection_context(selection, std::slice::from_ref(&library))
                .await
                .unwrap();
        let plan = crate::commands::resolve_passes_for_scope(
            &library,
            selection,
            resolved_scope,
            &collection_context,
            &[],
        )
        .await
        .unwrap();
        assert_eq!(plan.passes.len(), 1, "unfiled-only selection");
        assert_eq!(plan.passes[0].kind, PassKind::Unfiled);
        let mut selected = make_run_cycle_library_state_with_passes(ZONE, &token_key, plan.passes);
        assert_eq!(selected.pass_scope, resolved_scope);
        selected.library = library;
        let base = make_run_cycle_download_config_builder(&root, db.clone());
        let builder = |mode, exclude, groups, zone| {
            let mut config = (*base(mode, exclude, groups, zone)).clone();
            config.folder_structure = "none".into();
            config.file_match_policy = if motion_refresh {
                // The reported motion case keeps the original companion stem,
                // which is the default name-size-dedup policy.
                crate::types::FileMatchPolicy::NameSizeDedupWithSuffix
            } else {
                crate::types::FileMatchPolicy::NameId7
            };
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

        let expected_downloaded = match (motion_refresh, cycle) {
            (true, 0) => 3,
            (false, 0) => 2,
            _ => 0,
        };
        if result.failed_count != 0 {
            for event in trace.events() {
                if matches!(event.level, tracing::Level::WARN | tracing::Level::ERROR) {
                    eprintln!("TRACE {:?} {:?}", event.level, event.fields);
                }
            }
        }
        assert_eq!(result.failed_count, 0, "cycle {cycle}: {result:?}");
        assert!(
            result.db_sync_token_advance_safe,
            "cycle {cycle}: {result:?}"
        );
        assert_eq!(
            result.stats.downloaded, expected_downloaded,
            "cycle {cycle}: {result:?}"
        );
        assert_eq!(
            db.get_metadata(&token_key).await.unwrap().as_deref(),
            Some(format!("v27-after-cycle-{}", cycle + 1).as_str()),
            "cycle {cycle}: current checkpoint must advance"
        );
        assert_eq!(
            db.get_metadata(&format!("sync_token:{ZONE}"))
                .await
                .unwrap(),
            None,
            "cycle {cycle}: released cursor must never cross the boundary"
        );

        if cycle == 0 {
            let conn = rusqlite::Connection::open(&db_path).unwrap();
            let record = crate::state::db::sync_epoch::read_library(&conn, ZONE)
                .unwrap()
                .expect("production cycle enrolls the library");
            assert_eq!(record.state, "complete", "enrollment must complete");
            assert_eq!(
                record.reason, "existing_upgrade",
                "faithful existing-upgrade enrollment, not a test shortcut"
            );
            assert_eq!(
                record.baseline_scope,
                selected.library.baseline_scope(),
                "enrollment scope must match the resolved selection"
            );
        }

        assert_eq!(std::fs::read(&still_path).unwrap(), provider_still);
        assert_eq!(
            std::fs::metadata(&still_path).unwrap().modified().unwrap(),
            still_mtime,
            "cycle {cycle}: reused still mtime changed"
        );
        if let Some(mtime) = motion_mtime {
            assert_eq!(
                std::fs::metadata(&motion_path).unwrap().modified().unwrap(),
                mtime,
                "cycle {cycle}: reused motion mtime changed"
            );
        }
        assert_eq!(
            identity_snapshot(),
            identity_before,
            "cycle {cycle}: exact historical identity mutated"
        );

        let completed_owner = if motion_refresh {
            "old-master"
        } else {
            current_child
        };
        let completed = db.get_downloaded_page(0, 100).await.unwrap();
        for (id, version, path) in [
            (completed_owner, "original", still_path.as_path()),
            (completed_owner, "live_original", motion_path.as_path()),
            (
                "asset-new-master",
                "original",
                root.join(new_name).as_path(),
            ),
            (
                "asset-during-master",
                "original",
                root.join(during_name).as_path(),
            ),
        ] {
            let row = completed
                .iter()
                .find(|row| {
                    row.library.as_ref() == ZONE
                        && row.id.as_ref() == id
                        && row.version_size.as_str() == version
                })
                .unwrap_or_else(|| panic!("missing completed {id}/{version}"));
            assert_eq!(
                row.local_path.as_ref(),
                Some(&path.to_path_buf()),
                "cycle {cycle}: {id}/{version} must keep the original destination"
            );
        }
        let receipts = receipt_rows(&db_path, current_child);
        assert_eq!(
            receipts.len(),
            2,
            "cycle {cycle}: still and motion receipts"
        );
        let mut versions = std::collections::BTreeSet::new();
        for receipt in &receipts {
            let rusqlite::types::Value::Text(version) = &receipt[8] else {
                panic!("receipt version must be text");
            };
            versions.insert(version.clone());
        }
        assert_eq!(
            versions,
            std::collections::BTreeSet::from(["live_original".to_owned(), "original".to_owned()]),
            "cycle {cycle}: receipt identities"
        );

        let mut actual_tree = BTreeMap::new();
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
        assert_eq!(
            actual_tree, expected_tree,
            "cycle {cycle}: media tree gained a suffixed duplicate or lost a name"
        );

        if let Some(old_receipts) = &durable_snapshot {
            let conn = rusqlite::Connection::open(&db_path).unwrap();
            let token = crate::state::db::sync_epoch::read_library(&conn, ZONE).unwrap();
            let receipts = rows(&db_path, "SELECT * FROM download_receipts ORDER BY id");
            drop(conn);
            assert_eq!(
                token.as_ref().map(|record| record.state.as_str()),
                Some("complete"),
                "cycle {cycle}: reopen must not disturb the checkpoint"
            );
            assert_eq!(&receipts, old_receipts, "cycle {cycle}: receipts churned");
        } else {
            durable_snapshot = Some(rows(
                &db_path,
                "SELECT * FROM download_receipts ORDER BY id",
            ));
        }

        if cycle > 0 {
            let conn = rusqlite::Connection::open(&db_path).unwrap();
            let record = crate::state::db::sync_epoch::read_library(&conn, ZONE)
                .unwrap()
                .unwrap();
            assert_eq!(record.state, "complete");
        }
    }

    if motion_refresh {
        let lookups = lookups.lock().unwrap();
        let allowed = [
            "asset-old-master",
            "old-master",
            "asset-new-master",
            "new-master",
            "asset-during-master",
            "during-master",
        ];
        let mut per_cycle = [0usize; 3];
        for (cycle, body) in lookups.iter() {
            per_cycle[*cycle] += 1;
            for record in body["records"].as_array().into_iter().flatten() {
                let name = record["recordName"].as_str().unwrap();
                assert!(
                    allowed.contains(&name),
                    "lookup crossed the fixture identity set: {name}"
                );
            }
        }
        assert!(
            lookups
                .iter()
                .any(|(cycle, body)| *cycle == 0 && body.to_string().contains("old-master")),
            "the expired-companion cycle must revalidate the exact released identity"
        );
        assert_eq!(per_cycle[1], 0, "first reopen must be quiet");
        assert_eq!(per_cycle[2], 0, "second reopen must be quiet");
        assert!(
            (1..=14).contains(&per_cycle[0]),
            "bounded first-cycle revalidation probes: {per_cycle:?}"
        );
    }
    let requests = server.received_requests().await.unwrap();
    let count = |path: &str| {
        requests
            .iter()
            .filter(|request| request.method.as_str() == "GET" && request.url.path() == path)
            .count()
    };
    assert_eq!(
        count("/existing"),
        0,
        "proven same-identity still must not GET"
    );
    assert_eq!(count("/new"), 1, "the genuinely new item transfers once");
    assert_eq!(
        count("/during"),
        1,
        "the concurrent new item transfers once"
    );
    if motion_refresh {
        assert_eq!(count("/motion-stale"), 1, "one expired attempt");
        assert_eq!(count("/motion-fresh"), 1, "one successful refresh");
    } else {
        assert_eq!(
            count("/existing-motion-never-requested"),
            0,
            "proven same-identity motion must not GET"
        );
    }
    server.verify().await;
    eprintln!(
        "RELEASED_POPULATED_REACHED case={case:?} still_gets={} new_gets={} lookups={}",
        count("/existing"),
        count("/new"),
        if motion_refresh {
            lookups.lock().unwrap().len()
        } else {
            0
        }
    );
}

/// Refusal controls built from the same released fixture family.
#[derive(Clone, Copy, Debug)]
pub(in crate::sync_loop::runner::tests) enum ReleasedControlCase {
    /// Equal bytes in a released Primary row do not grant identity over the
    /// selected Shared child. The Shared child must take a distinct safe copy
    /// and the rejected no-copy goal is recorded as such.
    ForeignEqualBytes28,
    /// A recorded failed old master whose saved exclusive child now maps to a
    /// different provider master has no original-destination authority. The
    /// old debt and original path must survive unchanged.
    ChangedMaster36,
}

pub(in crate::sync_loop::runner::tests) async fn exercise_released_controls(
    case: ReleasedControlCase,
) {
    let (trace, _trace_guard) = crate::test_helpers::TracingCapture::install();
    let changed_master = matches!(case, ReleasedControlCase::ChangedMaster36);
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("photos");
    std::fs::create_dir_all(&root).unwrap();
    let source = dir.path().join("released.db");
    let db_path = dir.path().join("state.db");

    let provider_still = MEDIA.to_vec();
    let checksum = signature(&provider_still);
    let old_still = root.join("primary-equal.JPG");
    let old_motion = root.join("primary-equal.MOV");
    let old_sidecar = root.join("primary-equal.JPG.xmp");
    let current_still = root.join("shared-only.JPG");
    let current_motion = root.join("shared-only.MOV");

    {
        let conn = rusqlite::Connection::open(&source).unwrap();
        match case {
            ReleasedControlCase::ForeignEqualBytes28 => {
                crate::state::schema::migrate_genuine_v28(&conn).unwrap();
            }
            ReleasedControlCase::ChangedMaster36 => {
                crate::state::schema::migrate_genuine_v36(&conn).unwrap();
            }
        }
        if changed_master {
            std::fs::write(&current_still, &provider_still).unwrap();
            insert_released_asset(
                &conn,
                ZONE,
                "old-master",
                "original",
                "shared-only.JPG",
                &current_still,
                &provider_still,
                &checksum,
                true,
            );
            conn.execute(
                "INSERT INTO assets(library,id,version_size,checksum,filename,created_at,added_at,size_bytes,media_type,status,last_seen_at,download_attempts,last_error,source) \
                 VALUES (?1,'old-master','live_original',?2,'shared-only.MOV',1700000000,1700000002,?3,'video','failed',1700000004,2,'previous live companion transfer failed','icloud')",
                rusqlite::params![ZONE, signature(MOTION), MOTION.len() as i64],
            )
            .unwrap();
            conn.execute_batch(&format!(
                "INSERT INTO legacy_master_state_owners VALUES ('{ZONE}','old-master','asset-current-master',1700000000);
                 INSERT INTO asset_master_mappings VALUES ('{ZONE}','asset-current-master','current-master',1700000000);
                 INSERT INTO asset_metadata_capture_revisions VALUES ('{ZONE}','old-master',1,1700000000);"
            ))
            .unwrap();
        } else {
            std::fs::write(&old_still, &provider_still).unwrap();
            std::fs::write(&old_motion, MOTION).unwrap();
            std::fs::write(&old_sidecar, b"released primary sidecar").unwrap();
            insert_released_asset(
                &conn,
                "PrimarySync",
                "old-primary-master",
                "original",
                "primary-equal.JPG",
                &old_still,
                &provider_still,
                &checksum,
                true,
            );
            insert_released_asset(
                &conn,
                "PrimarySync",
                "old-primary-master",
                "live_original",
                "primary-equal.MOV",
                &old_motion,
                MOTION,
                &signature(MOTION),
                true,
            );
            conn.execute_batch(
                "INSERT INTO asset_master_mappings VALUES ('PrimarySync','old-primary-child','old-primary-master',1700000000);
                 INSERT INTO asset_metadata_capture_revisions VALUES ('PrimarySync','old-primary-master',1,1700000000);",
            )
            .unwrap();
            conn.execute(
                "INSERT INTO metadata VALUES (?1,?2)",
                rusqlite::params![format!("sync_token:PrimarySync"), OLD_TOKEN],
            )
            .unwrap();
        }
        conn.execute(
            "INSERT INTO metadata VALUES (?1,?2)",
            rusqlite::params![format!("sync_token:{ZONE}"), OLD_TOKEN],
        )
        .unwrap();
    }

    let columns = released_columns(&source);
    let legacy_rows = released_rows(&source, &columns);
    crate::state::db::account::adopt_legacy(
        &source,
        &db_path,
        &baseline_owner(),
        "baseline@example.invalid",
        "com",
    )
    .await
    .unwrap();
    assert_eq!(released_rows(&db_path, &columns), legacy_rows);

    let old_snapshot = || {
        rows(
            &db_path,
            "SELECT * FROM assets WHERE library='PrimarySync' ORDER BY id,version_size",
        )
        .into_iter()
        .chain(rows(
            &db_path,
            "SELECT * FROM asset_metadata_paths WHERE library='PrimarySync' ORDER BY id,version_size",
        ))
        .collect::<Vec<_>>()
    };
    let old_before = old_snapshot();
    let old_still_mtime = changed_master.then(|| {
        std::fs::metadata(&current_still)
            .unwrap()
            .modified()
            .unwrap()
    });
    let foreign_mtime = (!changed_master).then(|| {
        (
            std::fs::metadata(&old_still).unwrap().modified().unwrap(),
            std::fs::metadata(&old_motion).unwrap().modified().unwrap(),
            std::fs::metadata(&old_sidecar).unwrap().modified().unwrap(),
        )
    });

    let server = crate::test_helpers::start_wiremock_or_skip("released control acceptance")
        .await
        .expect("acceptance server must bind");
    let stale_motion = format!("{}/motion-stale", server.uri());
    let fresh_motion = format!("{}/motion-fresh", server.uri());
    for (endpoint, bytes, content_type, expected) in [
        (
            "/shared-equal",
            MEDIA,
            "image/jpeg",
            usize::from(!changed_master),
        ),
        ("/new", MEDIA, "image/jpeg", 1),
        ("/during", MEDIA, "image/jpeg", 1),
    ] {
        Mock::given(method("GET"))
            .and(path(endpoint))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_bytes(bytes.to_vec())
                    .insert_header("content-type", content_type),
            )
            .expect(expected as u64)
            .mount(&server)
            .await;
    }
    if changed_master {
        Mock::given(method("GET"))
            .and(path("/motion-stale"))
            .respond_with(ResponseTemplate::new(410))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/motion-fresh"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_bytes(MOTION)
                    .insert_header("content-type", "video/quicktime"),
            )
            .expect(1)
            .mount(&server)
            .await;
    }

    let (current_master, current_child, still_url, filename) = if changed_master {
        (
            "current-master",
            "asset-current-master",
            format!("{}/existing", server.uri()),
            "shared-only.JPG",
        )
    } else {
        (
            "shared-master",
            "asset-shared-master",
            format!("{}/shared-equal", server.uri()),
            "shared-equal.JPG",
        )
    };
    let mut existing = pair(current_master, filename, &still_url, 1);
    existing[0]["fields"]["resOriginalRes"]["value"]["fileChecksum"] = json!(checksum.clone());
    existing[0]["fields"]["resOriginalRes"]["value"]["size"] = json!(provider_still.len());
    if changed_master {
        existing[0]["fields"]["resOriginalVidComplRes"] = json!({"value":{
            "downloadURL":stale_motion.clone(),
            "size":MOTION.len(),
            "fileChecksum":signature(MOTION)}});
        existing[0]["fields"]["resOriginalVidComplFileType"] =
            json!({"value":"com.apple.quicktime-movie"});
    }
    let mut new_item = pair("new-master", "new.JPG", &format!("{}/new", server.uri()), 2);
    new_item[0]["fields"]["resOriginalRes"]["value"]["fileChecksum"] = json!(checksum.clone());
    new_item[0]["fields"]["resOriginalRes"]["value"]["size"] = json!(MEDIA.len());
    let mut inventory = existing;
    inventory.extend(new_item);
    let mut concurrent = pair(
        "during-master",
        "during.JPG",
        &format!("{}/during", server.uri()),
        3,
    );
    concurrent[0]["fields"]["resOriginalRes"]["value"]["fileChecksum"] = json!(checksum.clone());
    concurrent[0]["fields"]["resOriginalRes"]["value"]["size"] = json!(MEDIA.len());

    let token_key = sync_token_key(ZONE);
    let app_config = make_populated_app_config();
    let (_session_dir, shared) = make_shared_session_for_run_cycle().await;
    let lookups = Arc::new(Mutex::new(Vec::new()));
    let stale_lookups = Arc::new(AtomicUsize::new(0));
    for cycle in 0..3 {
        let db = Arc::new(
            SqliteStateDb::open_owned(&db_path, &baseline_owner())
                .await
                .unwrap(),
        );
        let inner = BaselineSession {
            database: "shared",
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
            "https://synthetic.example/database/1/photos/production/shared".to_string(),
            Arc::new(std::collections::HashMap::new()),
            if changed_master {
                Box::new(MotionRefreshSession {
                    inner: inner.clone(),
                    stale_motion_url: stale_motion.clone(),
                    fresh_motion_url: fresh_motion.clone(),
                    cycle,
                    lookups: Arc::clone(&lookups),
                    stale_lookups: Arc::clone(&stale_lookups),
                })
            } else {
                Box::new(inner.clone())
            },
            Arc::new(zone()),
            "shared".into(),
            crate::retry::RetryConfig::default(),
        )
        .await
        .unwrap();
        library.shadow_capture = Some(crate::icloud::photos::inbox::ShadowCapture::new(
            db.clone(),
            baseline_owner(),
            "com",
        ));
        let selection = &app_config.filters.selection;
        let selected_zones = crate::commands::zone_name_set(std::slice::from_ref(&library));
        let collection_zones = selected_zones.clone();
        let resolved_scope = crate::commands::pass_scope_for_zone(
            selection,
            ZONE,
            &selected_zones,
            &collection_zones,
        );
        let collection_context =
            crate::commands::build_collection_context(selection, std::slice::from_ref(&library))
                .await
                .unwrap();
        let plan = crate::commands::resolve_passes_for_scope(
            &library,
            selection,
            resolved_scope,
            &collection_context,
            &[],
        )
        .await
        .unwrap();
        let mut selected = make_run_cycle_library_state_with_passes(ZONE, &token_key, plan.passes);
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
            &app_config,
            Some(db.as_ref()),
            false,
            &builder,
            download::DownloadControls::download_hidden(),
            &shared,
            &CancellationToken::new(),
        )
        .await
        .unwrap();

        if !changed_master {
            assert_eq!(result.failed_count, 0, "cycle {cycle}: {result:?}");
            assert!(result.db_sync_token_advance_safe, "cycle {cycle}");
            assert_eq!(result.stats.downloaded, if cycle == 0 { 3 } else { 0 });
            assert_eq!(
                db.get_metadata(&token_key).await.unwrap().as_deref(),
                Some(format!("v27-after-cycle-{}", cycle + 1).as_str())
            );
            // The released Primary bytes stay exactly where they were; the
            // selected Shared child took its own distinct destination.
            assert_eq!(std::fs::read(&old_still).unwrap(), provider_still);
            assert_eq!(std::fs::read(&old_motion).unwrap(), MOTION);
            assert_eq!(
                std::fs::read(&old_sidecar).unwrap(),
                b"released primary sidecar"
            );
            let (still_mtime, motion_mtime, sidecar_mtime) = foreign_mtime.unwrap();
            assert_eq!(
                std::fs::metadata(&old_still).unwrap().modified().unwrap(),
                still_mtime
            );
            assert_eq!(
                std::fs::metadata(&old_motion).unwrap().modified().unwrap(),
                motion_mtime
            );
            assert_eq!(
                std::fs::metadata(&old_sidecar).unwrap().modified().unwrap(),
                sidecar_mtime
            );
            assert_eq!(old_snapshot(), old_before, "released Primary rows changed");
            let completed = db.get_downloaded_page(0, 100).await.unwrap();
            let shared = completed
                .iter()
                .find(|row| {
                    row.library.as_ref() == ZONE
                        && row.id.as_ref() == current_child
                        && row.version_size.as_str() == "original"
                })
                .expect("the selected Shared child must complete");
            let shared_path = shared.local_path.as_ref().unwrap();
            assert_ne!(
                shared_path, &old_still,
                "equal Primary bytes must not be adopted as the Shared destination"
            );
            assert_eq!(std::fs::read(shared_path).unwrap(), provider_still);
        } else {
            // No original-destination authority for the changed master.
            assert!(
                !current_motion.exists(),
                "the old companion path must not be published with changed-master authority"
            );
            assert_eq!(
                std::fs::read(&current_still).unwrap(),
                provider_still,
                "the released still/path must survive"
            );
            assert_eq!(
                std::fs::metadata(&current_still)
                    .unwrap()
                    .modified()
                    .unwrap(),
                old_still_mtime.unwrap()
            );
            let retained = rows(
                &db_path,
                &format!(
                    "SELECT status,local_path,last_error FROM assets WHERE library='{ZONE}' AND id='old-master' AND version_size='live_original'"
                ),
            );
            assert_eq!(retained.len(), 1, "the released obligation must survive");
            assert_ne!(
                retained[0][0],
                rusqlite::types::Value::Text("downloaded".into()),
                "changed-master authority must not falsely complete the old debt"
            );
            assert_eq!(
                retained[0][1],
                rusqlite::types::Value::Null,
                "no destination may be attached to changed-master debt"
            );
            assert!(
                matches!(
                    &retained[0][0],
                    rusqlite::types::Value::Text(status)
                        if status == "failed" || status == "pending"
                ),
                "old debt stays failed or retryable, never completed: {retained:?}"
            );
            // The owner's durable reason is the pending released obligation
            // itself: provider identity retained, no destination, not completed.
            let retained_identity = rows(
                &db_path,
                &format!(
                    "SELECT checksum,size_bytes,filename FROM assets WHERE library='{ZONE}' AND id='old-master' AND version_size='live_original'"
                ),
            );
            assert_eq!(retained_identity.len(), 1);
            assert_eq!(
                retained_identity[0][0],
                rusqlite::types::Value::Text(signature(MOTION))
            );
            assert_eq!(
                retained_identity[0][1],
                rusqlite::types::Value::Integer(MOTION.len() as i64)
            );
            assert_eq!(
                retained_identity[0][2],
                rusqlite::types::Value::Text("shared-only.MOV".into())
            );
            assert_eq!(old_snapshot(), old_before);
            let receipts = rows(
                &db_path,
                &format!(
                    "SELECT native_path FROM download_receipts WHERE library='{ZONE}' ORDER BY id"
                ),
            );
            let old_key =
                crate::state::db::provider_selection::SelectionPath::from_path(&current_motion)
                    .key()
                    .unwrap();
            assert!(
                receipts
                    .iter()
                    .all(|row| row[0] != rusqlite::types::Value::Text(old_key.clone())),
                "no receipt may claim the old companion destination"
            );
            if result.failed_count != 0 {
                for event in trace.events() {
                    if matches!(event.level, tracing::Level::WARN | tracing::Level::ERROR) {
                        eprintln!("TRACE {:?} {:?}", event.level, event.fields);
                    }
                }
            }
        }
    }
    server.verify().await;
    eprintln!(
        "RELEASED_CONTROL_REACHED case={case:?} old_primary_rows={} changed_old_motion_exists={}",
        old_snapshot().len(),
        current_motion.exists()
    );
}

#[tokio::test]
async fn released_foreign_equal_bytes_take_a_distinct_shared_copy_28() {
    super::PRESERVE_SHARED_CLAIMS
        .scope(
            true,
            exercise_released_controls(ReleasedControlCase::ForeignEqualBytes28),
        )
        .await;
}

#[tokio::test]
async fn released_changed_master_has_no_original_destination_authority_36() {
    super::PRESERVE_SHARED_CLAIMS
        .scope(
            true,
            exercise_released_controls(ReleasedControlCase::ChangedMaster36),
        )
        .await;
}

/// Token-agnostic provider for the protected-history fault/reopen family.
#[derive(Clone)]
struct ProtectedSession {
    records: Arc<Vec<serde_json::Value>>,
    incremental: Arc<Vec<serde_json::Value>>,
    zone_name: String,
    cycle: usize,
    incremental_cycle: usize,
    inventories: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl crate::icloud::photos::PhotosSession for ProtectedSession {
    async fn post(
        &self,
        url: &str,
        body: String,
        _headers: &[(&str, &str)],
    ) -> anyhow::Result<serde_json::Value> {
        let request: serde_json::Value = serde_json::from_str(&body)?;
        if url.contains("/zones/list") {
            return Ok(
                json!({"zones":[{"zoneID":{"zoneName":self.zone_name,"ownerRecordName":"_defaultOwner"},
                "syncToken":"protected-boundary","deleted":false}]}),
            );
        }
        if request["query"]["recordType"] == "CheckIndexingState" {
            return Ok(json!({"records":[{"recordName":"index",
                "recordType":"CheckIndexingState",
                "fields":{"state":{"value":"FINISHED"}}}]}));
        }
        if url.contains("/internal/records/query/batch") {
            return Ok(crate::sync_loop::test_support::album_count_response(
                self.records.len() as u64 / 2,
            ));
        }
        if url.contains("/records/query?") {
            self.inventories
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let offset = request["query"]["filterBy"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|filter| filter["fieldName"] == "startRank")
                .and_then(|filter| filter["fieldValue"]["value"].as_u64())
                .unwrap_or(0);
            let records = if offset == 0 {
                self.records.as_ref().clone()
            } else {
                Vec::new()
            };
            return Ok(json!({"records":records,"syncToken":"query-not-checkpoint"}));
        }
        if url.contains("/records/lookup?") {
            let requested = request["records"].as_array().cloned().unwrap_or_default();
            let records: Vec<_> = self
                .records
                .iter()
                .chain(self.incremental.iter())
                .filter(|record| {
                    requested
                        .iter()
                        .any(|query| query["recordName"] == record["recordName"])
                })
                .cloned()
                .collect();
            return Ok(json!({"records":records}));
        }
        if url.contains("/changes/zone?") {
            let records = if self.cycle == self.incremental_cycle
                && (self.incremental_cycle != 0
                    || request["zones"][0]["syncToken"] == "protected-boundary")
            {
                self.incremental.as_ref().clone()
            } else {
                Vec::new()
            };
            return Ok(
                json!({"zones":[{"zoneID":{"zoneName":self.zone_name,"ownerRecordName":"_defaultOwner"},
                "records":records,
                "syncToken":format!("protected-after-{}", self.cycle),
                "moreComing":false}]}),
            );
        }
        anyhow::bail!("unexpected protected-history endpoint: {url}")
    }

    fn clone_box(&self) -> Box<dyn crate::icloud::photos::PhotosSession> {
        Box::new(self.clone())
    }
}

/// Protected released history plus an injected checkpoint fault, then real
/// recovery and two quiet reopens. Same fixture family as the reuse cases.
pub(in crate::sync_loop::runner::tests) async fn exercise_released_protected_history() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("photos");
    let protected_dir = root.join("protected");
    std::fs::create_dir_all(&protected_dir).unwrap();

    let old_still = protected_dir.join("old.JPG");
    let old_motion = protected_dir.join("old.MOV");
    let old_sidecar = protected_dir.join("old.JPG.xmp");
    let old_bytes = MEDIA.to_vec();
    std::fs::write(&old_still, &old_bytes).unwrap();
    std::fs::write(&old_motion, MOTION).unwrap();
    std::fs::write(&old_sidecar, b"protected released sidecar").unwrap();
    let old_mtimes = (
        std::fs::metadata(&old_still).unwrap().modified().unwrap(),
        std::fs::metadata(&old_motion).unwrap().modified().unwrap(),
        std::fs::metadata(&old_sidecar).unwrap().modified().unwrap(),
    );

    let source = dir.path().join("released.db");
    let db_path = dir.path().join("state.db");
    {
        let conn = rusqlite::Connection::open(&source).unwrap();
        crate::state::schema::migrate_genuine_v28(&conn).unwrap();
        insert_released_asset(
            &conn,
            "PrimarySync",
            "old-primary-master",
            "original",
            "old.JPG",
            &old_still,
            &old_bytes,
            &signature(&old_bytes),
            true,
        );
        insert_released_asset(
            &conn,
            "PrimarySync",
            "old-primary-master",
            "live_original",
            "old.MOV",
            &old_motion,
            MOTION,
            &signature(MOTION),
            true,
        );
        let shared_history = root.join("shared-history.JPG");
        std::fs::write(&shared_history, MEDIA).unwrap();
        insert_released_asset(
            &conn,
            ZONE,
            "shared-history-master",
            "original",
            "shared-history.JPG",
            &shared_history,
            MEDIA,
            &signature(MEDIA),
            true,
        );
        conn.execute_batch(&format!(
            "INSERT INTO legacy_master_state_owners VALUES ('{ZONE}','shared-history-master','asset-shared-history-master',1700000000);
             INSERT INTO asset_master_mappings VALUES ('{ZONE}','asset-shared-history-master','shared-history-master',1700000000);
             INSERT INTO asset_metadata_capture_revisions VALUES ('{ZONE}','shared-history-master',1,1700000000);
             INSERT INTO asset_master_mappings VALUES ('PrimarySync','old-primary-child','old-primary-master',1700000000);
             INSERT INTO asset_metadata_capture_revisions VALUES ('PrimarySync','old-primary-master',1,1700000000);
             INSERT INTO unresolved_sparse_identities(library,source_record_name,original_evidence,observed_evidence,generation,first_seen_at,last_outcome)
               VALUES('PrimarySync','historical-source','[1,\"historical-child\",\"SharedSync-absent\",\"historical-owner\"]','[1,\"historical-child\",\"SharedSync-absent\",\"historical-owner\"]',7,1000,'[invalid outcome');
             INSERT INTO metadata VALUES ('unresolved_asset_identity:PrimarySync','opaque original marker');"
        ))
        .unwrap();
        conn.execute(
            "INSERT INTO metadata VALUES (?1,?2)",
            rusqlite::params![format!("sync_token:PrimarySync"), OLD_TOKEN],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO metadata VALUES (?1,?2)",
            rusqlite::params![format!("sync_token:{ZONE}"), OLD_TOKEN],
        )
        .unwrap();
    }
    let columns = released_columns(&source);
    let legacy_rows = released_rows(&source, &columns);
    crate::state::db::account::adopt_legacy(
        &source,
        &db_path,
        &baseline_owner(),
        "baseline@example.invalid",
        "com",
    )
    .await
    .unwrap();
    assert_eq!(released_rows(&db_path, &columns), legacy_rows);

    let protected_snapshot = || {
        rows(
            &db_path,
            "SELECT * FROM assets WHERE library='PrimarySync' ORDER BY id,version_size",
        )
        .into_iter()
        .chain(rows(
            &db_path,
            "SELECT * FROM asset_metadata_paths WHERE library='PrimarySync' ORDER BY id,version_size",
        ))
        .chain(rows(
            &db_path,
            "SELECT * FROM unresolved_sparse_identities ORDER BY library,source_record_name",
        ))
        .chain(rows(
            &db_path,
            "SELECT key,value FROM metadata WHERE key LIKE 'unresolved_asset_identity:%' ORDER BY key",
        ))
        .collect::<Vec<_>>()
    };
    let protected_before = protected_snapshot();

    let server = crate::test_helpers::start_wiremock_or_skip("released protected history")
        .await
        .expect("acceptance server must bind");
    for endpoint in ["/current", "/new", "/later"] {
        Mock::given(method("GET"))
            .and(path(endpoint))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_bytes(MEDIA)
                    .insert_header("content-type", "image/jpeg"),
            )
            .expect(1)
            .mount(&server)
            .await;
    }
    let checksum = signature(MEDIA);
    let mut current = pair(
        "current-master",
        "current.JPG",
        &format!("{}/current", server.uri()),
        1,
    );
    current[0]["fields"]["resOriginalRes"]["value"]["fileChecksum"] = json!(checksum.clone());
    current[0]["fields"]["resOriginalRes"]["value"]["size"] = json!(MEDIA.len());
    let mut new_item = pair("new-master", "new.JPG", &format!("{}/new", server.uri()), 2);
    new_item[0]["fields"]["resOriginalRes"]["value"]["fileChecksum"] = json!(checksum.clone());
    new_item[0]["fields"]["resOriginalRes"]["value"]["size"] = json!(MEDIA.len());
    let mut history = pair(
        "shared-history-master",
        "shared-history.JPG",
        &format!("{}/shared-history", server.uri()),
        4,
    );
    history[0]["fields"]["resOriginalRes"]["value"]["fileChecksum"] = json!(checksum.clone());
    history[0]["fields"]["resOriginalRes"]["value"]["size"] = json!(MEDIA.len());
    let mut later = pair(
        "later-master",
        "later.JPG",
        &format!("{}/later", server.uri()),
        5,
    );
    later[0]["fields"]["resOriginalRes"]["value"]["fileChecksum"] = json!(checksum.clone());
    later[0]["fields"]["resOriginalRes"]["value"]["size"] = json!(MEDIA.len());
    let mut inventory = history;
    inventory.extend(current);
    inventory.extend(new_item);

    let token_key = sync_token_key(ZONE);
    let app_config = make_populated_app_config();
    let (_session_dir, shared) = make_shared_session_for_run_cycle().await;
    let inventories = Arc::new(AtomicUsize::new(0));
    let incremental = Arc::new(later);
    for cycle in 0..5 {
        let inner = Arc::new(
            SqliteStateDb::open_owned(&db_path, &baseline_owner())
                .await
                .unwrap(),
        );
        if cycle == 1 {
            // Fail the incremental checkpoint write once; the durable media
            // and previous state must survive and the next cycle must recover.
            let sql = format!(
                "CREATE TRIGGER fail_checkpoint BEFORE UPDATE ON metadata WHEN NEW.key='{}' BEGIN SELECT RAISE(ABORT,'injected checkpoint failure'); END;",
                token_key
            );
            inner
                .acquire_lock("install released checkpoint fault")
                .unwrap()
                .execute_batch(&sql)
                .unwrap();
        }
        let session = ProtectedSession {
            records: Arc::new(inventory.clone()),
            incremental: Arc::clone(&incremental),
            zone_name: ZONE.to_string(),
            cycle,
            incremental_cycle: 1,
            inventories: Arc::clone(&inventories),
        };
        let mut library = PhotoLibrary::new(
            "https://synthetic.example/database/1/photos/production/shared".to_string(),
            Arc::new(std::collections::HashMap::new()),
            Box::new(session.clone()),
            Arc::new(zone()),
            "shared".into(),
            crate::retry::RetryConfig::default(),
        )
        .await
        .unwrap();
        library.shadow_capture = Some(crate::icloud::photos::inbox::ShadowCapture::new(
            inner.clone(),
            baseline_owner(),
            "com",
        ));
        let selection = &app_config.filters.selection;
        let selected_zones = crate::commands::zone_name_set(std::slice::from_ref(&library));
        let collection_zones = selected_zones.clone();
        let resolved_scope = crate::commands::pass_scope_for_zone(
            selection,
            ZONE,
            &selected_zones,
            &collection_zones,
        );
        let collection_context =
            crate::commands::build_collection_context(selection, std::slice::from_ref(&library))
                .await
                .unwrap();
        let plan = crate::commands::resolve_passes_for_scope(
            &library,
            selection,
            resolved_scope,
            &collection_context,
            &[],
        )
        .await
        .unwrap();
        let mut selected = make_run_cycle_library_state_with_passes(ZONE, &token_key, plan.passes);
        selected.library = library;
        let base = make_run_cycle_download_config_builder(&root, inner.clone());
        let builder = |mode, exclude, groups, zone| {
            let mut config = (*base(mode, exclude, groups, zone)).clone();
            config.folder_structure = "none".into();
            config.file_match_policy = crate::types::FileMatchPolicy::NameId7;
            Arc::new(config)
        };
        let result = run_cycle(
            &[&selected],
            &app_config,
            Some(inner.as_ref()),
            false,
            &builder,
            download::DownloadControls::download_hidden(),
            &shared,
            &CancellationToken::new(),
        )
        .await
        .unwrap();

        assert_eq!(std::fs::read(&old_still).unwrap(), old_bytes);
        assert_eq!(std::fs::read(&old_motion).unwrap(), MOTION);
        assert_eq!(
            std::fs::read(&old_sidecar).unwrap(),
            b"protected released sidecar"
        );
        assert_eq!(
            (
                std::fs::metadata(&old_still).unwrap().modified().unwrap(),
                std::fs::metadata(&old_motion).unwrap().modified().unwrap(),
                std::fs::metadata(&old_sidecar).unwrap().modified().unwrap(),
            ),
            old_mtimes,
            "cycle {cycle}: protected mtimes"
        );
        assert_eq!(
            protected_snapshot(),
            protected_before,
            "cycle {cycle}: protected history rows changed"
        );
        assert_eq!(
            inner
                .get_metadata("sync_token:PrimarySync")
                .await
                .unwrap()
                .as_deref(),
            Some(OLD_TOKEN),
            "cycle {cycle}: unselected released cursor changed"
        );

        if cycle == 0 {
            let conn = rusqlite::Connection::open(&db_path).unwrap();
            let epoch = crate::state::db::sync_epoch::read_library(&conn, ZONE)
                .unwrap()
                .expect("the first cycle enrolls the populated library");
            assert_eq!(epoch.reason, "existing_upgrade");
            assert_eq!(epoch.state, "complete");
            assert_eq!(result.failed_count, 0, "{result:?}");
            assert!(result.db_sync_token_advance_safe, "{result:?}");
            assert_eq!(result.stats.downloaded, 2, "{result:?}");
            assert_eq!(
                inner.get_metadata(&token_key).await.unwrap().as_deref(),
                Some("protected-after-0")
            );
        } else if cycle == 1 {
            assert!(
                result.failed_count > 0 || result.stats.state_write_failures > 0,
                "the injected fault must be observable: {result:?}"
            );
            assert!(!result.db_sync_token_advance_safe, "{result:?}");
            assert_eq!(
                inner.get_metadata(&token_key).await.unwrap().as_deref(),
                Some("protected-after-0"),
                "the failed checkpoint must not advance the token"
            );
            inner
                .acquire_lock("remove released checkpoint fault")
                .unwrap()
                .execute_batch("DROP TRIGGER IF EXISTS fail_checkpoint")
                .unwrap();
        } else if cycle == 2 {
            assert_eq!(result.failed_count, 0, "recovery must settle: {result:?}");
            assert!(result.db_sync_token_advance_safe, "{result:?}");
            assert_eq!(
                inner.get_metadata(&token_key).await.unwrap().as_deref(),
                Some("protected-after-2"),
                "recovery cycle advances the current checkpoint"
            );
        } else {
            assert_eq!(result.failed_count, 0, "cycle {cycle}: {result:?}");
            assert!(result.db_sync_token_advance_safe, "cycle {cycle}");
            assert_eq!(result.stats.downloaded, 0, "cycle {cycle}: quiet reopen");
        }
    }
    let requests = server.received_requests().await.unwrap();
    let count = |path: &str| {
        requests
            .iter()
            .filter(|request| request.method.as_str() == "GET" && request.url.path() == path)
            .count()
    };
    assert_eq!(count("/current"), 1, "the current item transfers once");
    assert_eq!(count("/new"), 1, "the second current item transfers once");
    assert_eq!(count("/later"), 1, "the incremental item transfers once");
    assert_eq!(
        count("/shared-history"),
        0,
        "same-identity Shared history must reuse without a GET"
    );
    assert!(inventories.load(std::sync::atomic::Ordering::SeqCst) >= 1);
    server.verify().await;
    eprintln!(
        "RELEASED_PROTECTED_REACHED protected_rows={} current_gets={} inventories={}",
        protected_snapshot().len(),
        count("/current"),
        inventories.load(std::sync::atomic::Ordering::SeqCst)
    );
}

#[tokio::test]
async fn released_protected_history_survives_fault_and_quiet_reopens_28() {
    super::PRESERVE_SHARED_CLAIMS
        .scope(true, exercise_released_protected_history())
        .await;
}

/// C07/C15 category pair. Attribution-only ambiguity is not a preservation
/// row; a real unfinished preservation record keeps opaque evidence and paths
/// while same-library current media and the v27 token progress.
pub(in crate::sync_loop::runner::tests) async fn exercise_released_preservation_category(
    unfinished_preservation: bool,
) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("photos");
    std::fs::create_dir_all(&root).unwrap();
    let source = dir.path().join("released.db");
    let db_path = dir.path().join("state.db");

    let guarded = root.join("guarded.JPG");
    let guarded_sidecar = root.join("guarded.JPG.xmp");
    let preserved = root.join("preserved.JPG");
    let preserved_sidecar = root.join("preserved.JPG.xmp");
    let custom = root.join("custom-notes.xmp");
    std::fs::write(&guarded, b"guarded ambiguity bytes").unwrap();
    std::fs::write(&guarded_sidecar, b"guarded sidecar bytes").unwrap();
    std::fs::write(&preserved, b"opaque preserved media bytes").unwrap();
    std::fs::write(&preserved_sidecar, b"opaque preserved sidecar bytes").unwrap();
    std::fs::write(&custom, b"opaque custom xmp-named bytes").unwrap();
    let file_mtimes = [
        guarded.as_path(),
        guarded_sidecar.as_path(),
        preserved.as_path(),
        preserved_sidecar.as_path(),
        custom.as_path(),
    ]
    .map(|path| std::fs::metadata(path).unwrap().modified().unwrap());

    {
        let conn = rusqlite::Connection::open(&source).unwrap();
        if unfinished_preservation {
            crate::state::schema::migrate_genuine_v36(&conn).unwrap();
            let files = json!([
                {"root": root, "path": preserved, "sha256": null, "size": null},
                {"root": root, "path": preserved_sidecar, "sha256": null, "size": null},
                {"root": root, "path": custom, "sha256": null, "size": null},
            ]);
            conn.execute(
                "INSERT INTO unattributed_legacy(library,asset_id,evidence_version,original_evidence,files,prepared_at) \
                 VALUES(?1,'legacy-preserved',1,'opaque released preservation evidence',?2,1700000000)",
                rusqlite::params![ZONE, files.to_string()],
            )
            .unwrap();
            for path in [&preserved, &preserved_sidecar, &custom] {
                conn.execute(
                    "INSERT INTO unattributed_legacy_paths VALUES(?1,'legacy-preserved',?2,?3)",
                    rusqlite::params![
                        ZONE,
                        crate::fs_util::confined_path_key(path).unwrap(),
                        path.to_str().unwrap()
                    ],
                )
                .unwrap();
            }
        } else {
            crate::state::schema::migrate_genuine_v28(&conn).unwrap();
            conn.execute(
                "INSERT INTO unresolved_sparse_identities(library,source_record_name,original_evidence,observed_evidence,generation,first_seen_at,last_outcome) \
                 VALUES(?1,'ambiguous-source','[1,\"ambiguous-child\",\"SharedSync-absent\",\"historical-owner\"]','[1,\"ambiguous-child\",\"SharedSync-absent\",\"historical-owner\"]',7,1000,'[ambiguous]')",
                rusqlite::params![ZONE],
            )
            .unwrap();
        }
        conn.execute(
            "INSERT INTO metadata VALUES (?1,?2)",
            rusqlite::params![format!("sync_token:{ZONE}"), OLD_TOKEN],
        )
        .unwrap();
    }
    let columns = released_columns(&source);
    let legacy_rows = released_rows(&source, &columns);
    crate::state::db::account::adopt_legacy(
        &source,
        &db_path,
        &baseline_owner(),
        "baseline@example.invalid",
        "com",
    )
    .await
    .unwrap();
    assert_eq!(released_rows(&db_path, &columns), legacy_rows);

    let history_snapshot = || {
        rows(
            &db_path,
            "SELECT library,source_record_name,original_evidence,observed_evidence,last_outcome FROM unresolved_sparse_identities ORDER BY library,source_record_name",
        )
        .into_iter()
        .chain(rows(
            &db_path,
            "SELECT library,asset_id,evidence_version,original_evidence,files,prepared_at,active_generation FROM unattributed_legacy ORDER BY library,asset_id",
        ))
        .chain(rows(
            &db_path,
            "SELECT library,asset_id,path_key,local_path FROM unattributed_legacy_paths ORDER BY library,asset_id,path_key",
        ))
        .collect::<Vec<_>>()
    };
    let history_before = history_snapshot();
    let files_before = [
        &guarded,
        &guarded_sidecar,
        &preserved,
        &preserved_sidecar,
        &custom,
    ]
    .map(|path| std::fs::read(path).unwrap());

    let server = crate::test_helpers::start_wiremock_or_skip("released preservation category")
        .await
        .expect("acceptance server must bind");
    for endpoint in ["/current", "/new"] {
        Mock::given(method("GET"))
            .and(path(endpoint))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_bytes(MEDIA)
                    .insert_header("content-type", "image/jpeg"),
            )
            .expect(1)
            .mount(&server)
            .await;
    }
    let checksum = signature(MEDIA);
    let mut current = pair(
        "current-master",
        "current.JPG",
        &format!("{}/current", server.uri()),
        1,
    );
    current[0]["fields"]["resOriginalRes"]["value"]["fileChecksum"] = json!(checksum.clone());
    current[0]["fields"]["resOriginalRes"]["value"]["size"] = json!(MEDIA.len());
    let mut new_item = pair("new-master", "new.JPG", &format!("{}/new", server.uri()), 2);
    new_item[0]["fields"]["resOriginalRes"]["value"]["fileChecksum"] = json!(checksum.clone());
    new_item[0]["fields"]["resOriginalRes"]["value"]["size"] = json!(MEDIA.len());
    let mut inventory = current;
    inventory.extend(new_item);

    let token_key = sync_token_key(ZONE);
    let app_config = make_populated_app_config();
    let (_session_dir, shared) = make_shared_session_for_run_cycle().await;
    let inventories = Arc::new(AtomicUsize::new(0));
    for cycle in 0..3 {
        let inner = Arc::new(
            SqliteStateDb::open_owned(&db_path, &baseline_owner())
                .await
                .unwrap(),
        );
        let session = ProtectedSession {
            records: Arc::new(inventory.clone()),
            incremental: Arc::new(Vec::new()),
            zone_name: ZONE.to_string(),
            cycle,
            incremental_cycle: 1,
            inventories: Arc::clone(&inventories),
        };
        let mut library = PhotoLibrary::new(
            "https://synthetic.example/database/1/photos/production/shared".to_string(),
            Arc::new(std::collections::HashMap::new()),
            Box::new(session.clone()),
            Arc::new(zone()),
            "shared".into(),
            crate::retry::RetryConfig::default(),
        )
        .await
        .unwrap();
        library.shadow_capture = Some(crate::icloud::photos::inbox::ShadowCapture::new(
            inner.clone(),
            baseline_owner(),
            "com",
        ));
        let selection = &app_config.filters.selection;
        let selected_zones = crate::commands::zone_name_set(std::slice::from_ref(&library));
        let collection_zones = selected_zones.clone();
        let resolved_scope = crate::commands::pass_scope_for_zone(
            selection,
            ZONE,
            &selected_zones,
            &collection_zones,
        );
        let collection_context =
            crate::commands::build_collection_context(selection, std::slice::from_ref(&library))
                .await
                .unwrap();
        let plan = crate::commands::resolve_passes_for_scope(
            &library,
            selection,
            resolved_scope,
            &collection_context,
            &[],
        )
        .await
        .unwrap();
        let mut selected = make_run_cycle_library_state_with_passes(ZONE, &token_key, plan.passes);
        selected.library = library;
        let base = make_run_cycle_download_config_builder(&root, inner.clone());
        let builder = |mode, exclude, groups, zone| {
            let mut config = (*base(mode, exclude, groups, zone)).clone();
            config.folder_structure = "none".into();
            config.file_match_policy = crate::types::FileMatchPolicy::NameId7;
            Arc::new(config)
        };
        let result = run_cycle(
            &[&selected],
            &app_config,
            Some(inner.as_ref()),
            false,
            &builder,
            download::DownloadControls::download_hidden(),
            &shared,
            &CancellationToken::new(),
        )
        .await
        .unwrap();

        assert_eq!(result.failed_count, 0, "cycle {cycle}: {result:?}");
        assert!(result.db_sync_token_advance_safe, "cycle {cycle}");
        assert_eq!(
            inner.get_metadata(&token_key).await.unwrap().as_deref(),
            Some(format!("protected-after-{cycle}").as_str()),
            "cycle {cycle}: current v27 checkpoint"
        );
        assert_eq!(
            history_snapshot(),
            history_before,
            "cycle {cycle}: history rows"
        );
        let files_after = [
            &guarded,
            &guarded_sidecar,
            &preserved,
            &preserved_sidecar,
            &custom,
        ]
        .map(|path| std::fs::read(path).unwrap());
        assert_eq!(files_after, files_before, "cycle {cycle}: protected bytes");
        assert_eq!(
            [
                &guarded,
                &guarded_sidecar,
                &preserved,
                &preserved_sidecar,
                &custom
            ]
            .map(|path| std::fs::metadata(path).unwrap().modified().unwrap()),
            file_mtimes,
            "cycle {cycle}: protected mtimes"
        );

        let status = crate::cycle_reporter::classify_cycle(
            &result.stats,
            result.failed_count,
            result.session_expired,
        );
        if unfinished_preservation {
            assert!(
                result.stats.unattributed_legacy_assets >= 1,
                "cycle {cycle}: preservation asset count"
            );
            assert!(
                result.stats.unattributed_legacy_pending >= 1,
                "cycle {cycle}: unfinished preservation remains pending"
            );
            assert_eq!(
                status,
                crate::cycle_reporter::CycleStatus::Failed,
                "cycle {cycle}: report stays partial with failed_assets 0"
            );
        } else {
            assert_eq!(
                result.stats.unattributed_legacy_assets, 0,
                "cycle {cycle}: attribution-only is not a preservation row"
            );
            assert_eq!(
                result.stats.unattributed_legacy_pending, 0,
                "cycle {cycle}: attribution-only is not current preservation debt"
            );
            assert_eq!(
                status,
                crate::cycle_reporter::CycleStatus::Success,
                "cycle {cycle}: ordinary current progress"
            );
        }
        assert_eq!(
            result.stats.downloaded,
            if cycle == 0 { 2 } else { 0 },
            "cycle {cycle}: current media then quiet reopen"
        );
    }
    let requests = server.received_requests().await.unwrap();
    let count = |path: &str| {
        requests
            .iter()
            .filter(|request| request.method.as_str() == "GET" && request.url.path() == path)
            .count()
    };
    assert_eq!(count("/current"), 1);
    assert_eq!(count("/new"), 1);
    server.verify().await;
    eprintln!(
        "RELEASED_CATEGORY_REACHED preservation={unfinished_preservation} history_rows={}",
        history_snapshot().len()
    );
}

#[tokio::test]
async fn released_attribution_only_ambiguity_keeps_current_progress_28() {
    super::PRESERVE_SHARED_CLAIMS
        .scope(true, exercise_released_preservation_category(false))
        .await;
}

#[tokio::test]
async fn released_unfinished_preservation_reports_partial_while_progressing_36() {
    super::PRESERVE_SHARED_CLAIMS
        .scope(true, exercise_released_preservation_category(true))
        .await;
}

fn make_shared_hidden_app_config() -> crate::config::Config {
    let data_dir = tempfile::tempdir().unwrap();
    let globals = crate::config::GlobalArgs {
        username: Some("baseline@example.invalid".to_string()),
        domain: None,
        data_dir: Some(data_dir.path().to_string_lossy().into_owned()),
    };
    let mut sync = crate::cli::SyncArgs::default();
    sync.config_overrides.albums = vec!["none".to_string()];
    sync.config_overrides.libraries = vec!["shared".to_string()];
    sync.config_overrides.smart_folders = vec!["Hidden".to_string()];
    sync.config_overrides.unfiled = Some(true);
    sync.config_overrides.edited = Some(true);
    crate::config::Config::build(&globals, &crate::cli::PasswordArgs::default(), sync, None)
        .unwrap()
}

/// Shared library provider with separate Hidden and unfiled scopes.
#[derive(Clone)]
struct SharedHiddenSession {
    hidden: Arc<Vec<serde_json::Value>>,
    unfiled: Arc<Vec<serde_json::Value>>,
    cycle: usize,
    queries: Arc<Mutex<Vec<serde_json::Value>>>,
}

#[async_trait::async_trait]
impl crate::icloud::photos::PhotosSession for SharedHiddenSession {
    async fn post(
        &self,
        url: &str,
        body: String,
        _headers: &[(&str, &str)],
    ) -> anyhow::Result<serde_json::Value> {
        let request: serde_json::Value = serde_json::from_str(&body)?;
        if url.contains("/zones/list") {
            return Ok(json!({"zones":[{"zoneID":zone(),
                "syncToken":"shared-hidden-boundary","deleted":false}]}));
        }
        if request["query"]["recordType"] == "CheckIndexingState" {
            return Ok(json!({"records":[{"recordName":"index",
                "recordType":"CheckIndexingState",
                "fields":{"state":{"value":"FINISHED"}}}]}));
        }
        if url.contains("/internal/records/query/batch") {
            let batch = request["batch"].as_array().cloned().unwrap_or_default();
            let counts: Vec<_> = batch
                .iter()
                .map(|entry| {
                    let obj_type = entry["query"]["filterBy"]["fieldValue"]["value"][0]
                        .as_str()
                        .unwrap_or("");
                    let count = if obj_type == "CPLAssetHiddenByAssetDate" {
                        self.hidden.len() as u64 / 2
                    } else if obj_type == "CPLAssetByAssetDateWithoutHiddenOrDeleted" {
                        self.unfiled.len() as u64 / 2
                    } else {
                        0
                    };
                    json!({"records":[{"fields":{"itemCount":{"value":count}}}]})
                })
                .collect();
            return Ok(json!({"batch": counts}));
        }
        if url.contains("/records/query?") {
            self.queries.lock().unwrap().push(request.clone());
            let record_type = request["query"]["recordType"].as_str().unwrap_or("");
            let records = if record_type == "CPLAssetAndMasterHiddenByAssetDate" {
                self.hidden.as_ref().clone()
            } else if record_type == "CPLAssetAndMasterByAssetDateWithoutHiddenOrDeleted" {
                self.unfiled.as_ref().clone()
            } else {
                Vec::new()
            };
            return Ok(json!({"records":records,"syncToken":"query-not-checkpoint"}));
        }
        if url.contains("/records/lookup?") {
            let requested = request["records"].as_array().cloned().unwrap_or_default();
            let records: Vec<_> = self
                .hidden
                .iter()
                .chain(self.unfiled.iter())
                .filter(|record| {
                    requested
                        .iter()
                        .any(|query| query["recordName"] == record["recordName"])
                })
                .cloned()
                .collect();
            return Ok(json!({"records":records}));
        }
        if url.contains("/changes/zone?") {
            return Ok(json!({"zones":[{"zoneID":zone(),
                "records":[],
                "syncToken":format!("shared-hidden-after-{}", self.cycle),
                "moreComing":false}]}));
        }
        anyhow::bail!("unexpected Shared+Hidden endpoint: {url}")
    }

    fn clone_box(&self) -> Box<dyn crate::icloud::photos::PhotosSession> {
        Box::new(self.clone())
    }
}

/// C03 current Shared+Hidden edited/original progress with protected old
/// multiple-child history, plus the C08 ambiguous custom-sidecar no-repair
/// control.
pub(in crate::sync_loop::runner::tests) async fn exercise_released_shared_hidden_history() {
    let (trace, _trace_guard) = crate::test_helpers::TracingCapture::install();
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("photos");
    let protected = root.join("protected");
    std::fs::create_dir_all(&protected).unwrap();

    let old_still = protected.join("old.JPG");
    let old_sidecar = protected.join("old.JPG.xmp");
    let custom_sidecar = protected.join("custom-notes.xmp");
    let old_bytes = b"opaque old multiple-child bytes".to_vec();
    std::fs::write(&old_still, &old_bytes).unwrap();
    std::fs::write(&old_sidecar, b"opaque old sidecar bytes").unwrap();
    std::fs::write(&custom_sidecar, b"opaque custom ambiguous sidecar").unwrap();
    let old_mtimes = [
        std::fs::metadata(&old_still).unwrap().modified().unwrap(),
        std::fs::metadata(&old_sidecar).unwrap().modified().unwrap(),
        std::fs::metadata(&custom_sidecar)
            .unwrap()
            .modified()
            .unwrap(),
    ];

    let source = dir.path().join("released.db");
    let db_path = dir.path().join("state.db");
    {
        let conn = rusqlite::Connection::open(&source).unwrap();
        crate::state::schema::migrate_genuine_v28(&conn).unwrap();
        insert_released_asset(
            &conn,
            ZONE,
            "old-master",
            "original",
            "old.JPG",
            &old_still,
            &old_bytes,
            &signature(&old_bytes),
            false,
        );
        conn.execute_batch(&format!(
            "INSERT INTO asset_master_mappings VALUES ('{ZONE}','old-child-a','old-master',1700000000);
             INSERT INTO asset_master_mappings VALUES ('{ZONE}','old-child-b','old-master',1700000000);
             INSERT INTO asset_metadata_capture_revisions VALUES ('{ZONE}','old-master',0,1700000000);
             INSERT INTO unresolved_sparse_identities(library,source_record_name,original_evidence,observed_evidence,generation,first_seen_at,last_outcome)
               VALUES('{ZONE}','ambiguous-source','[1,\"ambiguous-child\",\"SharedSync-absent\",\"owner-a\"]','[1,\"ambiguous-child\",\"SharedSync-absent\",\"owner-b\"]',7,1000,'[malformed outcome');
             INSERT INTO asset_verifications VALUES('{ZONE}','old-master','original','unknown','multiple historical children',1700000000);"
        ))
        .unwrap();
        conn.execute(
            "INSERT INTO metadata VALUES (?1,?2)",
            rusqlite::params![format!("sync_token:{ZONE}"), OLD_TOKEN],
        )
        .unwrap();
    }
    let columns = released_columns(&source);
    let legacy_rows = released_rows(&source, &columns);
    crate::state::db::account::adopt_legacy(
        &source,
        &db_path,
        &baseline_owner(),
        "baseline@example.invalid",
        "com",
    )
    .await
    .unwrap();
    assert_eq!(released_rows(&db_path, &columns), legacy_rows);

    let protected_snapshot = || {
        rows(
            &db_path,
            &format!("SELECT * FROM assets WHERE library='{ZONE}' AND id='old-master' ORDER BY version_size"),
        )
        .into_iter()
        .chain(rows(
            &db_path,
            &format!("SELECT * FROM asset_metadata_paths WHERE library='{ZONE}' AND id='old-master' ORDER BY version_size"),
        ))
        .chain(rows(
            &db_path,
            &format!("SELECT * FROM asset_master_mappings WHERE library='{ZONE}' AND asset_record_name IN ('old-child-a','old-child-b') ORDER BY asset_record_name"),
        ))
        .chain(rows(
            &db_path,
            &format!("SELECT * FROM asset_metadata_capture_revisions WHERE library='{ZONE}' AND asset_id='old-master' ORDER BY asset_id"),
        ))
        .chain(rows(
            &db_path,
            &format!("SELECT * FROM unresolved_sparse_identities WHERE library='{ZONE}' ORDER BY source_record_name"),
        ))
        .chain(rows(
            &db_path,
            &format!("SELECT * FROM asset_verifications WHERE library='{ZONE}' ORDER BY id,version_size"),
        ))
        .collect::<Vec<_>>()
    };
    let protected_before = protected_snapshot();
    let mut config_hash_after = None;

    let server = crate::test_helpers::start_wiremock_or_skip("released Shared Hidden history")
        .await
        .expect("acceptance server must bind");
    for endpoint in ["/hidden-original", "/hidden-edited", "/peer"] {
        Mock::given(method("GET"))
            .and(path(endpoint))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(MEDIA))
            .expect(1)
            .mount(&server)
            .await;
    }
    let hidden_bytes = MEDIA.to_vec();
    let edited_bytes = MEDIA.to_vec();
    let mut hidden = pair(
        "hidden-master",
        "hidden.JPG",
        &format!("{}/hidden-original", server.uri()),
        11,
    );
    hidden[0]["fields"]["resOriginalRes"]["value"]["fileChecksum"] =
        json!(signature(&hidden_bytes));
    hidden[0]["fields"]["resOriginalRes"]["value"]["size"] = json!(hidden_bytes.len());
    hidden[0]["fields"]["resJPEGFullRes"] = json!({"value":{
        "downloadURL":format!("{}/hidden-edited",server.uri()),
        "size":edited_bytes.len(),
        "fileChecksum":signature(&edited_bytes)}});
    hidden[0]["fields"]["resJPEGFullFileType"] = json!({"value":"public.jpeg"});
    hidden[0]["fields"]["isHidden"] = json!({"value":1,"type":"INT64"});
    hidden[1]["fields"]["isHidden"] = json!({"value":1,"type":"INT64"});
    let mut peer = pair(
        "peer-master",
        "peer.JPG",
        &format!("{}/peer", server.uri()),
        12,
    );
    peer[0]["fields"]["resOriginalRes"]["value"]["fileChecksum"] = json!(signature(MEDIA));
    peer[0]["fields"]["resOriginalRes"]["value"]["size"] = json!(MEDIA.len());

    let token_key = sync_token_key(ZONE);
    let app_config = make_shared_hidden_app_config();
    let selection = &app_config.filters.selection;
    let selected = [PhotoLibrary::new(
        "https://synthetic.example/database/1/photos/production/shared".to_string(),
        Arc::new(std::collections::HashMap::new()),
        Box::new(SharedHiddenSession {
            hidden: Arc::new(hidden.clone()),
            unfiled: Arc::new(peer.clone()),
            cycle: 0,
            queries: Arc::new(Mutex::new(Vec::new())),
        }),
        Arc::new(zone()),
        "shared".into(),
        crate::retry::RetryConfig::default(),
    )
    .await
    .unwrap()];
    let selected_zones = crate::commands::zone_name_set(&selected);
    let collection_zones = crate::commands::zone_name_set(crate::commands::collection_libraries(
        selection, &selected, &selected,
    ));
    let shared_scope =
        crate::commands::pass_scope_for_zone(selection, ZONE, &selected_zones, &collection_zones);
    assert!(shared_scope.include_unfiled);
    assert!(!shared_scope.include_albums);
    assert!(shared_scope.include_smart_folders);
    let collection_context = crate::commands::build_collection_context(selection, &selected)
        .await
        .unwrap();
    let plan = crate::commands::resolve_passes_for_scope(
        &selected[0],
        selection,
        shared_scope,
        &collection_context,
        &[],
    )
    .await
    .unwrap();
    assert_eq!(plan.passes.len(), 2, "Hidden plus unfiled");
    assert!(
        plan.passes
            .iter()
            .any(|pass| pass.kind == PassKind::SmartFolder && pass.album.name.as_ref() == "Hidden")
    );
    assert!(
        plan.passes
            .iter()
            .any(|pass| pass.kind == PassKind::Unfiled)
    );

    let (_session_dir, shared) = make_shared_session_for_run_cycle().await;
    let queries = Arc::new(Mutex::new(Vec::new()));
    let mut current_paths = None;
    for cycle in 0..3 {
        let db = Arc::new(
            SqliteStateDb::open_owned(&db_path, &baseline_owner())
                .await
                .unwrap(),
        );
        let session = SharedHiddenSession {
            hidden: Arc::new(hidden.clone()),
            unfiled: Arc::new(peer.clone()),
            cycle,
            queries: Arc::clone(&queries),
        };
        let mut library = PhotoLibrary::new(
            "https://synthetic.example/database/1/photos/production/shared".to_string(),
            Arc::new(std::collections::HashMap::new()),
            Box::new(session.clone()),
            Arc::new(zone()),
            "shared".into(),
            crate::retry::RetryConfig::default(),
        )
        .await
        .unwrap();
        library.shadow_capture = Some(crate::icloud::photos::inbox::ShadowCapture::new(
            db.clone(),
            baseline_owner(),
            "com",
        ));
        let resolved = crate::commands::resolve_passes_for_scope(
            &library,
            selection,
            shared_scope,
            &collection_context,
            &[],
        )
        .await
        .unwrap();
        let mut selected =
            make_run_cycle_library_state_with_passes(ZONE, &token_key, resolved.passes);
        selected.library = library;
        let base = make_run_cycle_download_config_builder(&root, db.clone());
        let builder = |mode, exclude, groups, zone| {
            let mut config = (*base(mode, exclude, groups, zone)).clone();
            config.folder_structure = "none".into();
            config.file_match_policy = crate::types::FileMatchPolicy::NameId7;
            config.edited = true;
            config.retry.max_retries = 0;
            Arc::new(config)
        };
        let result = run_cycle(
            &[&selected],
            &app_config,
            Some(db.as_ref()),
            false,
            &builder,
            download::DownloadControls::download_hidden(),
            &shared,
            &CancellationToken::new(),
        )
        .await
        .unwrap();

        if result.failed_count != 0 {
            for event in trace.events() {
                if matches!(event.level, tracing::Level::WARN | tracing::Level::ERROR) {
                    eprintln!("TRACE {:?}", event.fields);
                }
            }
            for request in server.received_requests().await.unwrap() {
                if request.method.as_str() == "GET" {
                    eprintln!("GET {}", request.url.path());
                }
            }
        }
        assert_eq!(result.failed_count, 0, "cycle {cycle}: {result:?}");
        assert!(result.db_sync_token_advance_safe, "cycle {cycle}");
        assert_eq!(
            db.get_metadata(&token_key).await.unwrap().as_deref(),
            Some(format!("shared-hidden-after-{cycle}").as_str())
        );
        assert_eq!(
            result.stats.unattributed_legacy_pending, 0,
            "cycle {cycle}: no preservation row"
        );
        assert_eq!(
            crate::cycle_reporter::classify_cycle(
                &result.stats,
                result.failed_count,
                result.session_expired
            ),
            crate::cycle_reporter::CycleStatus::Success,
            "cycle {cycle}: no fake failed-asset count"
        );
        assert_eq!(
            protected_snapshot(),
            protected_before,
            "cycle {cycle}: protected multiple-child history mutated"
        );
        assert_eq!(
            [
                std::fs::metadata(&old_still).unwrap().modified().unwrap(),
                std::fs::metadata(&old_sidecar).unwrap().modified().unwrap(),
                std::fs::metadata(&custom_sidecar)
                    .unwrap()
                    .modified()
                    .unwrap(),
            ],
            old_mtimes,
            "cycle {cycle}: protected sidecar mtimes"
        );
        assert_eq!(std::fs::read(&old_still).unwrap(), old_bytes);
        assert_eq!(
            std::fs::read(&old_sidecar).unwrap(),
            b"opaque old sidecar bytes"
        );
        assert_eq!(
            std::fs::read(&custom_sidecar).unwrap(),
            b"opaque custom ambiguous sidecar"
        );
        let config_hash = rows(
            &db_path,
            "SELECT value FROM metadata WHERE key='config_hash'",
        );
        if let Some(expected) = &config_hash_after {
            assert_eq!(
                &config_hash, expected,
                "cycle {cycle}: ordinary metadata options unchanged"
            );
        } else {
            config_hash_after = Some(config_hash);
        }
        assert_eq!(result.stats.downloaded, if cycle == 0 { 3 } else { 0 });
        let completed = db.get_downloaded_page(0, 100).await.unwrap();
        let hidden_row = completed
            .iter()
            .find(|row| {
                row.id.as_ref() == "asset-hidden-master" && row.version_size.as_str() == "original"
            })
            .expect("current hidden original");
        let edited_row = completed
            .iter()
            .find(|row| {
                row.id.as_ref() == "asset-hidden-master" && row.version_size.as_str() == "adjusted"
            })
            .expect("current hidden edited");
        if let Some((original, edited)) = &current_paths {
            assert_eq!(hidden_row.local_path.as_ref(), Some(original));
            assert_eq!(edited_row.local_path.as_ref(), Some(edited));
        } else {
            current_paths = Some((
                hidden_row.local_path.clone().unwrap(),
                edited_row.local_path.clone().unwrap(),
            ));
        }
        assert_eq!(
            std::fs::read(hidden_row.local_path.as_ref().unwrap()).unwrap(),
            hidden_bytes
        );
        assert_eq!(
            std::fs::read(edited_row.local_path.as_ref().unwrap()).unwrap(),
            edited_bytes
        );
    }
    let records = rows(
        &db_path,
        "SELECT child FROM provider_active_decisions ORDER BY child",
    );
    assert!(
        records
            .iter()
            .all(|row| row[0] != rusqlite::types::Value::Text("old-master".into())),
        "old historical master must not become current work"
    );
    let lookups = queries.lock().unwrap().clone();
    assert!(
        lookups.iter().all(|body| {
            let text = body.to_string();
            !text.contains("old-master") && !text.contains("old-child")
        }),
        "old historical payload must not be parsed as a current request"
    );
    assert!(
        lookups.iter().any(|body| body
            .to_string()
            .contains("CPLAssetAndMasterHiddenByAssetDate")),
        "Hidden scope request"
    );
    assert!(
        lookups
            .iter()
            .any(|body| body.to_string().contains("WithoutHiddenOrDeleted")),
        "unfiled scope request"
    );
    let requests = server.received_requests().await.unwrap();
    let count = |path: &str| {
        requests
            .iter()
            .filter(|request| request.method.as_str() == "GET" && request.url.path() == path)
            .count()
    };
    assert_eq!(count("/hidden-original"), 1);
    assert_eq!(count("/hidden-edited"), 1);
    assert_eq!(count("/peer"), 1);
    server.verify().await;
    eprintln!(
        "RELEASED_SHARED_HIDDEN_REACHED protected_rows={} queries={}",
        protected_snapshot().len(),
        lookups.len()
    );
}

#[tokio::test]
async fn released_shared_hidden_edited_progress_keeps_old_ambiguity_28() {
    super::PRESERVE_SHARED_CLAIMS
        .scope(true, exercise_released_shared_hidden_history())
        .await;
}

/// Token-agnostic provider that observes one concurrent bridge item.
#[derive(Clone)]
struct ConfigReconcileSession {
    inventory: Arc<Vec<serde_json::Value>>,
    bridge: Arc<Vec<serde_json::Value>>,
    cycle: usize,
    changes: Arc<AtomicUsize>,
    requests: Arc<Mutex<Vec<(usize, serde_json::Value)>>>,
}

#[async_trait::async_trait]
impl crate::icloud::photos::PhotosSession for ConfigReconcileSession {
    async fn post(
        &self,
        url: &str,
        body: String,
        _headers: &[(&str, &str)],
    ) -> anyhow::Result<serde_json::Value> {
        let request: serde_json::Value = serde_json::from_str(&body)?;
        if url.contains("/zones/list") {
            return Ok(json!({"zones":[{"zoneID":zone(),
                "syncToken":"config-reconcile-boundary","deleted":false}]}));
        }
        if request["query"]["recordType"] == "CheckIndexingState" {
            return Ok(json!({"records":[{"recordName":"index",
                "recordType":"CheckIndexingState",
                "fields":{"state":{"value":"FINISHED"}}}]}));
        }
        if url.contains("/internal/records/query/batch") {
            let total = self.inventory.len() as u64 / 2;
            let batch = request["batch"].as_array().cloned().unwrap_or_default();
            let counts: Vec<_> = batch
                .iter()
                .map(|_| json!({"records":[{"fields":{"itemCount":{"value":total}}}]}))
                .collect();
            return Ok(json!({"batch": counts}));
        }
        if url.contains("/records/query?") {
            self.requests
                .lock()
                .unwrap()
                .push((self.cycle, request.clone()));
            let offset = request["query"]["filterBy"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|filter| filter["fieldName"] == "startRank")
                .and_then(|filter| filter["fieldValue"]["value"].as_u64())
                .unwrap_or(0);
            let records = if offset == 0 {
                self.inventory.as_ref().clone()
            } else {
                Vec::new()
            };
            return Ok(json!({"records":records,"syncToken":"query-not-checkpoint"}));
        }
        if url.contains("/records/lookup?") {
            let requested = request["records"].as_array().cloned().unwrap_or_default();
            let records: Vec<_> = self
                .inventory
                .iter()
                .chain(self.bridge.iter())
                .filter(|record| {
                    requested
                        .iter()
                        .any(|query| query["recordName"] == record["recordName"])
                })
                .cloned()
                .collect();
            return Ok(json!({"records":records}));
        }
        if url.contains("/changes/zone?") {
            self.requests
                .lock()
                .unwrap()
                .push((self.cycle, request.clone()));
            let change = self
                .changes
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let records = if self.cycle == 0 && change == 0 {
                self.bridge.as_ref().clone()
            } else {
                Vec::new()
            };
            return Ok(json!({"zones":[{"zoneID":zone(),
                "records":records,
                "syncToken":format!("config-reconcile-after-{}-{change}", self.cycle),
                "moreComing":false}]}));
        }
        anyhow::bail!("unexpected config reconciliation endpoint: {url}")
    }

    fn clone_box(&self) -> Box<dyn crate::icloud::photos::PhotosSession> {
        Box::new(self.clone())
    }
}

/// C12 initial baseline/config atomicity through the real staged promotion.
/// The released snapshot has an old active hash/cursor and a pending hash that
/// already matches the current options, so the normal reconciliation stage is
/// reachable without a manual deadline or epoch override.
pub(in crate::sync_loop::runner::tests) async fn exercise_released_config_reconciliation() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("photos");
    let protected = root.join("protected");
    std::fs::create_dir_all(&protected).unwrap();
    let old_path = protected.join("recovered.JPG");
    let old_sidecar = protected.join("recovered.JPG.xmp");
    let old_bytes = MEDIA.to_vec();
    std::fs::write(&old_path, &old_bytes).unwrap();
    std::fs::write(&old_sidecar, b"recovered historical sidecar").unwrap();
    let old_mtime = (
        std::fs::metadata(&old_path).unwrap().modified().unwrap(),
        std::fs::metadata(&old_sidecar).unwrap().modified().unwrap(),
    );

    let app_config = make_populated_app_config();
    let current_enum_hash = crate::download::compute_config_hash(&app_config);
    let source = dir.path().join("released.db");
    let db_path = dir.path().join("state.db");
    {
        let conn = rusqlite::Connection::open(&source).unwrap();
        crate::state::schema::migrate_genuine_v36(&conn).unwrap();
        assert_eq!(
            conn.query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            36,
            "initial genuine released schema"
        );
        insert_released_asset(
            &conn,
            ZONE,
            "recovered-old-master",
            "original",
            "recovered.JPG",
            &old_path,
            &old_bytes,
            &signature(&old_bytes),
            true,
        );
        conn.execute_batch(&format!(
            "INSERT INTO asset_master_mappings VALUES ('{ZONE}','recovered-old-child','recovered-old-master',1700000000);
             INSERT INTO asset_metadata_capture_revisions VALUES ('{ZONE}','recovered-old-master',1,1700000000);
             INSERT INTO metadata VALUES ('enum_config_hash','old-active-enum-hash');
             INSERT INTO metadata VALUES ('pending_enum_config_hash','{current_enum_hash}');
             INSERT INTO metadata VALUES ('config_hash','old-active-path-hash');
             INSERT INTO metadata VALUES ('sync_token:{ZONE}','old-active-v27-cursor');"
        ))
        .unwrap();
        let legacy_key = format!("sync_token:{ZONE}");
        assert_eq!(
            conn.query_row(
                "SELECT value FROM metadata WHERE key=?1",
                [&legacy_key],
                |row| row.get::<_, String>(0),
            )
            .unwrap(),
            "old-active-v27-cursor",
            "source carries the actual legacy released cursor"
        );
        assert_eq!(
            conn.query_row(
                "SELECT COUNT(*) FROM metadata WHERE key LIKE 'sync_token:v27:%'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
            0,
            "source has no hand-installed current v27 token"
        );
        assert_eq!(
            conn.query_row(
                "SELECT COUNT(*) FROM metadata WHERE key='sync_epoch:v1' OR key LIKE 'sync_epoch_library:v1:%'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
            0,
            "source has no completed epoch"
        );
        let receipts_table: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name='download_receipts')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        if receipts_table {
            assert_eq!(
                conn.query_row("SELECT COUNT(*) FROM download_receipts", [], |row| {
                    row.get::<_, i64>(0)
                })
                .unwrap(),
                0,
                "source has no current receipts"
            );
        }
    }
    let columns = released_columns(&source);
    let legacy_rows = released_rows(&source, &columns);
    crate::state::db::account::adopt_legacy(
        &source,
        &db_path,
        &baseline_owner(),
        "baseline@example.invalid",
        "com",
    )
    .await
    .unwrap();
    assert_eq!(released_rows(&db_path, &columns), legacy_rows);

    let protected_snapshot = || {
        rows(
            &db_path,
            &format!("SELECT * FROM assets WHERE library='{ZONE}' AND id='recovered-old-master' ORDER BY version_size"),
        )
        .into_iter()
        .chain(rows(
            &db_path,
            &format!("SELECT * FROM asset_metadata_paths WHERE library='{ZONE}' AND id='recovered-old-master' ORDER BY version_size"),
        ))
        .chain(rows(
            &db_path,
            &format!("SELECT * FROM asset_master_mappings WHERE library='{ZONE}' AND asset_record_name='recovered-old-child'"),
        ))
        .chain(rows(
            &db_path,
            &format!("SELECT * FROM asset_metadata_capture_revisions WHERE library='{ZONE}' AND asset_id='recovered-old-master'"),
        ))
        .collect::<Vec<_>>()
    };
    let protected_before = protected_snapshot();
    {
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        let metadata = |key: &str| -> Option<String> {
            conn.query_row("SELECT value FROM metadata WHERE key=?1", [key], |row| {
                row.get(0)
            })
            .ok()
        };
        assert_eq!(
            metadata("enum_config_hash").as_deref(),
            Some("old-active-enum-hash")
        );
        assert_eq!(
            metadata("pending_enum_config_hash").as_deref(),
            Some(current_enum_hash.as_str())
        );
        assert_eq!(
            metadata("config_hash").as_deref(),
            Some("old-active-path-hash")
        );
        assert_eq!(metadata("pending_download_config_hash"), None);
        assert_eq!(
            metadata(&format!("sync_token:{ZONE}")).as_deref(),
            Some("old-active-v27-cursor"),
            "actual legacy released cursor before the cycle"
        );
        assert_eq!(
            metadata(&format!("sync_token:v27:{ZONE}")),
            None,
            "no hand-installed current v27 token"
        );
        assert_eq!(
            metadata(&format!("sync_epoch_library:v1:{ZONE}")),
            None,
            "no completed epoch before the cycle"
        );
    }

    let server = crate::test_helpers::start_wiremock_or_skip("released config reconciliation")
        .await
        .expect("acceptance server must bind");
    for endpoint in ["/current-new", "/bridge-new"] {
        Mock::given(method("GET"))
            .and(path(endpoint))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_bytes(MEDIA)
                    .insert_header("content-type", "image/jpeg"),
            )
            .expect(1)
            .mount(&server)
            .await;
    }
    let checksum = signature(MEDIA);
    let mut current = pair(
        "current-new-master",
        "current-new.JPG",
        &format!("{}/current-new", server.uri()),
        21,
    );
    current[0]["fields"]["resOriginalRes"]["value"]["fileChecksum"] = json!(checksum.clone());
    current[0]["fields"]["resOriginalRes"]["value"]["size"] = json!(MEDIA.len());
    let mut bridge = pair(
        "bridge-new-master",
        "bridge-new.JPG",
        &format!("{}/bridge-new", server.uri()),
        22,
    );
    bridge[0]["fields"]["resOriginalRes"]["value"]["fileChecksum"] = json!(checksum.clone());
    bridge[0]["fields"]["resOriginalRes"]["value"]["size"] = json!(MEDIA.len());

    let token_key = sync_token_key(ZONE);
    let selection = &app_config.filters.selection;
    let (_session_dir, shared) = make_shared_session_for_run_cycle().await;
    let changes = Arc::new(AtomicUsize::new(0));
    let requests = Arc::new(Mutex::new(Vec::new()));
    let mut staged_token: Option<String> = None;
    let mut active_after_recovery: Option<String> = None;
    let mut receipts_after_promotion: Option<Vec<Vec<rusqlite::types::Value>>> = None;

    for cycle in 0..4 {
        let db = Arc::new(
            SqliteStateDb::open_owned(&db_path, &baseline_owner())
                .await
                .unwrap(),
        );
        if cycle == 0 {
            db.acquire_lock("install final promotion fault")
                .unwrap()
                .execute_batch(
                    "CREATE TRIGGER fail_promotion_insert BEFORE INSERT ON metadata WHEN NEW.key='enum_config_hash' BEGIN SELECT RAISE(ABORT,'injected promotion failure'); END;
                     CREATE TRIGGER fail_promotion_update BEFORE UPDATE ON metadata WHEN NEW.key='enum_config_hash' BEGIN SELECT RAISE(ABORT,'injected promotion failure'); END;",
                )
                .unwrap();
        }
        let session = ConfigReconcileSession {
            inventory: Arc::new(current.clone()),
            bridge: Arc::new(bridge.clone()),
            cycle,
            changes: Arc::clone(&changes),
            requests: Arc::clone(&requests),
        };
        let mut library = PhotoLibrary::new(
            "https://synthetic.example/database/1/photos/production/shared".to_string(),
            Arc::new(std::collections::HashMap::new()),
            Box::new(session.clone()),
            Arc::new(zone()),
            "shared".into(),
            crate::retry::RetryConfig::default(),
        )
        .await
        .unwrap();
        library.shadow_capture = Some(crate::icloud::photos::inbox::ShadowCapture::new(
            db.clone(),
            baseline_owner(),
            "com",
        ));
        let selected_zones = crate::commands::zone_name_set(std::slice::from_ref(&library));
        let collection_zones = selected_zones.clone();
        let resolved_scope = crate::commands::pass_scope_for_zone(
            selection,
            ZONE,
            &selected_zones,
            &collection_zones,
        );
        let collection_context =
            crate::commands::build_collection_context(selection, std::slice::from_ref(&library))
                .await
                .unwrap();
        let plan = crate::commands::resolve_passes_for_scope(
            &library,
            selection,
            resolved_scope,
            &collection_context,
            &[],
        )
        .await
        .unwrap();
        let mut selected = make_run_cycle_library_state_with_passes(ZONE, &token_key, plan.passes);
        selected.library = library;
        let base = make_run_cycle_download_config_builder(&root, db.clone());
        let builder = |mode, exclude, groups, zone| {
            let mut config = (*base(mode, exclude, groups, zone)).clone();
            config.folder_structure = "none".into();
            config.file_match_policy = crate::types::FileMatchPolicy::NameId7;
            config.retry.max_retries = 0;
            Arc::new(config)
        };
        let result = run_cycle(
            &[&selected],
            &app_config,
            Some(db.as_ref()),
            false,
            &builder,
            download::DownloadControls::download_hidden(),
            &shared,
            &CancellationToken::new(),
        )
        .await
        .unwrap();

        assert_eq!(
            protected_snapshot(),
            protected_before,
            "cycle {cycle}: recovered history changed"
        );
        assert_eq!(std::fs::read(&old_path).unwrap(), old_bytes);
        assert_eq!(
            std::fs::read(&old_sidecar).unwrap(),
            b"recovered historical sidecar"
        );
        assert_eq!(
            (
                std::fs::metadata(&old_path).unwrap().modified().unwrap(),
                std::fs::metadata(&old_sidecar).unwrap().modified().unwrap(),
            ),
            old_mtime,
            "cycle {cycle}: recovered mtimes"
        );

        if cycle == 0 {
            // The real staged/ready publication must exist before the fault.
            let conn = rusqlite::Connection::open(&db_path).unwrap();
            let epoch = crate::state::db::sync_epoch::read_library(&conn, ZONE)
                .unwrap()
                .expect("the cycle enrolls the library");
            let active_hash: String = conn
                .query_row(
                    "SELECT value FROM metadata WHERE key='enum_config_hash'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            let pending_hash: String = conn
                .query_row(
                    "SELECT value FROM metadata WHERE key='pending_enum_config_hash'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            let staged_key = format!("pending_sync_token:v27:{pending_hash}:{ZONE}");
            let staged: String = conn
                .query_row(
                    "SELECT value FROM metadata WHERE key=?1",
                    [&staged_key],
                    |row| row.get(0),
                )
                .unwrap();
            let staged_observation: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM metadata WHERE key=?1",
                    [format!(
                        "observed_selection_staged:v27:{pending_hash}:{ZONE}"
                    )],
                    |row| row.get(0),
                )
                .unwrap();
            let active_token: Option<String> = conn
                .query_row(
                    "SELECT value FROM metadata WHERE key=?1",
                    [&token_key],
                    |row| row.get(0),
                )
                .ok();
            assert_eq!(active_hash, "old-active-enum-hash");
            assert_eq!(pending_hash, current_enum_hash);
            assert_eq!(
                conn.query_row(
                    "SELECT value FROM metadata WHERE key=?1",
                    [format!("sync_token:{ZONE}")],
                    |row| row.get::<_, String>(0),
                )
                .ok(),
                None,
                "normal enrollment clears the actual old cursor"
            );
            assert_eq!(epoch.state, "ready", "valid staged epoch before promotion");
            assert!(epoch.binding.is_some(), "ready binding retained");
            assert_eq!(staged_observation, 1);
            let active_observation: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM metadata WHERE key=?1",
                    [format!("observed_selection:v27:{ZONE}")],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(
                active_observation, 0,
                "no partial ACTIVE observation before promotion"
            );
            assert!(
                active_token.as_deref() != Some(staged.as_str()),
                "the staged token must not become active before promotion"
            );
            assert_eq!(
                epoch
                    .binding
                    .as_ref()
                    .map(|binding| binding.token_sha256.as_str()),
                Some(
                    data_encoding::HEXLOWER
                        .encode(&sha2::Sha256::digest(staged.as_bytes()))
                        .as_str()
                )
            );
            assert!(!result.db_sync_token_advance_safe, "{result:?}");
            let completed = db.get_downloaded_page(0, 100).await.unwrap();
            for (id, path) in [
                (
                    "asset-current-new-master",
                    root.join("current-new_YXNzZXQ.JPG"),
                ),
                (
                    "asset-bridge-new-master",
                    root.join("bridge-new_YXNzZXQ.JPG"),
                ),
            ] {
                let row = completed
                    .iter()
                    .find(|row| row.id.as_ref() == id)
                    .unwrap_or_else(|| panic!("missing durable media {id}"));
                assert_eq!(
                    std::fs::read(row.local_path.as_ref().unwrap()).unwrap(),
                    MEDIA
                );
                assert_eq!(row.local_path.as_ref(), Some(&path));
            }
            staged_token = Some(staged);
            conn.execute_batch(
                "DROP TRIGGER IF EXISTS fail_promotion_insert; DROP TRIGGER IF EXISTS fail_promotion_update;",
            )
            .unwrap();
        } else if cycle == 1 {
            // Normal gate promotes the valid staged binding without a deadline.
            assert_eq!(result.failed_count, 0, "promotion cycle: {result:?}");
            assert!(result.db_sync_token_advance_safe, "promotion cycle");
            let conn = rusqlite::Connection::open(&db_path).unwrap();
            let active_hash: String = conn
                .query_row(
                    "SELECT value FROM metadata WHERE key='enum_config_hash'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            let pending: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM metadata WHERE key='pending_enum_config_hash'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            let active_token: String = conn
                .query_row(
                    "SELECT value FROM metadata WHERE key=?1",
                    [&token_key],
                    |row| row.get(0),
                )
                .unwrap();
            let active_observation: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM metadata WHERE key=?1",
                    [format!("observed_selection:v27:{ZONE}")],
                    |row| row.get(0),
                )
                .unwrap();
            let epoch = crate::state::db::sync_epoch::read_library(&conn, ZONE)
                .unwrap()
                .unwrap();
            assert_eq!(active_hash, current_enum_hash);
            assert_eq!(pending, 0);
            assert_eq!(active_token, staged_token.clone().unwrap());
            assert_eq!(active_observation, 1);
            assert_eq!(epoch.state, "complete");
            assert_eq!(result.stats.downloaded, 0, "promotion cycle");
            active_after_recovery = Some(active_token);
            receipts_after_promotion = Some(rows(
                &db_path,
                "SELECT * FROM download_receipts ORDER BY id",
            ));
        } else {
            assert_eq!(result.failed_count, 0, "cycle {cycle}: {result:?}");
            assert!(result.db_sync_token_advance_safe, "cycle {cycle}");
            assert_eq!(result.stats.downloaded, 0, "cycle {cycle}: quiet reopen");
            assert!(
                db.get_metadata(&token_key).await.unwrap().is_some(),
                "cycle {cycle}: quiet delta may advance the token but must keep one"
            );
            assert_eq!(
                rows(&db_path, "SELECT * FROM download_receipts ORDER BY id"),
                receipts_after_promotion.clone().unwrap(),
                "cycle {cycle}: receipt churn"
            );
        }
    }

    let requests = requests.lock().unwrap().clone();
    assert!(
        requests
            .iter()
            .all(|(_, request)| !request.to_string().contains("old-active-v27-cursor")),
        "the prior active cursor must not be requested"
    );
    let inventories_after_baseline = requests
        .iter()
        .filter(|(cycle, request)| *cycle > 0 && request.get("query").is_some())
        .count();
    assert_eq!(
        inventories_after_baseline, 0,
        "no DATA inventory after the initial baseline cycle"
    );
    let received = server.received_requests().await.unwrap();
    let count = |path: &str| {
        received
            .iter()
            .filter(|request| request.method.as_str() == "GET" && request.url.path() == path)
            .count()
    };
    assert_eq!(count("/current-new"), 1);
    assert_eq!(count("/bridge-new"), 1);
    server.verify().await;
    eprintln!(
        "RELEASED_CONFIG_RECONCILE_REACHED protected_rows={} staged_token={} promote_token={}",
        protected_snapshot().len(),
        staged_token.as_deref().unwrap_or(""),
        active_after_recovery.as_deref().unwrap_or("")
    );
}

#[tokio::test]
async fn released_config_reconciliation_stages_before_atomic_promotion_36() {
    super::PRESERVE_SHARED_CLAIMS
        .scope(true, exercise_released_config_reconciliation())
        .await;
}

/// C09 recovered genuine-36 analogue. Small representative of the reported
/// 22-class preserved history; reporter provenance remains E and this is not
/// an official producer output.
pub(in crate::sync_loop::runner::tests) async fn exercise_released_recovered_36() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("photos");
    let recovered = root.join("recovered");
    std::fs::create_dir_all(&recovered).unwrap();
    let old_still = recovered.join("recovered.JPG");
    let old_motion = recovered.join("recovered.MOV");
    let old_sidecar = recovered.join("recovered.JPG.xmp");
    let custom_sidecar = recovered.join("custom-recovered.xmp");
    let old_bytes = MEDIA.to_vec();
    std::fs::write(&old_still, &old_bytes).unwrap();
    std::fs::write(&old_motion, MOTION).unwrap();
    std::fs::write(&old_sidecar, b"recovered still sidecar").unwrap();
    std::fs::write(&custom_sidecar, b"recovered custom opaque xmp").unwrap();
    let old_mtimes = [
        std::fs::metadata(&old_still).unwrap().modified().unwrap(),
        std::fs::metadata(&old_motion).unwrap().modified().unwrap(),
        std::fs::metadata(&old_sidecar).unwrap().modified().unwrap(),
        std::fs::metadata(&custom_sidecar)
            .unwrap()
            .modified()
            .unwrap(),
    ];

    let source = dir.path().join("released.db");
    let db_path = dir.path().join("state.db");
    {
        let conn = rusqlite::Connection::open(&source).unwrap();
        crate::state::schema::migrate_genuine_v36(&conn).unwrap();
        insert_released_asset(
            &conn,
            ZONE,
            "recovered-master",
            "original",
            "recovered.JPG",
            &old_still,
            &old_bytes,
            &signature(&old_bytes),
            true,
        );
        insert_released_asset(
            &conn,
            ZONE,
            "recovered-master",
            "live_original",
            "recovered.MOV",
            &old_motion,
            MOTION,
            &signature(MOTION),
            true,
        );
        conn.execute_batch(&format!(
            "INSERT INTO legacy_master_state_owners VALUES ('{ZONE}','recovered-master','asset-recovered-master',1700000000);
             INSERT INTO asset_master_mappings VALUES ('{ZONE}','asset-recovered-master','recovered-master',1700000000);
             INSERT INTO asset_metadata_capture_revisions VALUES ('{ZONE}','recovered-master',1,1700000000);
             INSERT INTO metadata VALUES ('sync_token:{ZONE}','old-recovered-cursor');"
        ))
        .unwrap();
    }
    let columns = released_columns(&source);
    let legacy_rows = released_rows(&source, &columns);
    crate::state::db::account::adopt_legacy(
        &source,
        &db_path,
        &baseline_owner(),
        "baseline@example.invalid",
        "com",
    )
    .await
    .unwrap();
    assert_eq!(released_rows(&db_path, &columns), legacy_rows);
    assert!(
        rows(
            &db_path,
            &format!("SELECT * FROM download_receipts WHERE library='{ZONE}'"),
        )
        .is_empty(),
        "the recovered initial state has no current receipts"
    );

    let recovered_snapshot = || {
        rows(
            &db_path,
            &format!("SELECT id,version_size,checksum,filename,size_bytes,status,local_path,local_checksum,download_checksum,source FROM assets WHERE library='{ZONE}' AND id='recovered-master' ORDER BY version_size"),
        )
        .into_iter()
        .chain(rows(
            &db_path,
            &format!("SELECT id,version_size,local_path,local_checksum,download_checksum,source_checksum FROM asset_metadata_paths WHERE library='{ZONE}' AND id='recovered-master' ORDER BY version_size"),
        ))
        .chain(rows(
            &db_path,
            &format!("SELECT library,asset_record_name,master_record_name FROM asset_master_mappings WHERE library='{ZONE}' AND asset_record_name='asset-recovered-master'"),
        ))
        .chain(rows(
            &db_path,
            &format!("SELECT * FROM asset_metadata_capture_revisions WHERE library='{ZONE}' AND asset_id='recovered-master'"),
        ))
        .collect::<Vec<_>>()
    };
    let recovered_before = recovered_snapshot();

    let server = crate::test_helpers::start_wiremock_or_skip("released recovered 36")
        .await
        .expect("acceptance server must bind");
    Mock::given(method("GET"))
        .and(path("/current-new"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(MEDIA)
                .insert_header("content-type", "image/jpeg"),
        )
        .expect(1)
        .mount(&server)
        .await;
    let checksum = signature(MEDIA);
    let mut reused = pair(
        "recovered-master",
        "recovered.JPG",
        &format!("{}/unreused", server.uri()),
        31,
    );
    reused[0]["fields"]["resOriginalRes"]["value"]["fileChecksum"] = json!(checksum.clone());
    reused[0]["fields"]["resOriginalRes"]["value"]["size"] = json!(MEDIA.len());
    reused[0]["fields"]["resOriginalVidComplRes"] = json!({"value":{
        "downloadURL":format!("{}/unreused-motion",server.uri()),
        "size":MOTION.len(),
        "fileChecksum":signature(MOTION)}});
    reused[0]["fields"]["resOriginalVidComplFileType"] =
        json!({"value":"com.apple.quicktime-movie"});
    let mut current = pair(
        "current-new-master",
        "current-new.JPG",
        &format!("{}/current-new", server.uri()),
        32,
    );
    current[0]["fields"]["resOriginalRes"]["value"]["fileChecksum"] = json!(checksum.clone());
    current[0]["fields"]["resOriginalRes"]["value"]["size"] = json!(MEDIA.len());
    let mut inventory = reused;
    inventory.extend(current);

    let token_key = sync_token_key(ZONE);
    let app_config = make_populated_app_config();
    let selection = &app_config.filters.selection;
    let (_session_dir, shared) = make_shared_session_for_run_cycle().await;
    let inventories = Arc::new(AtomicUsize::new(0));
    for cycle in 0..3 {
        let db = Arc::new(
            SqliteStateDb::open_owned(&db_path, &baseline_owner())
                .await
                .unwrap(),
        );
        let session = ProtectedSession {
            records: Arc::new(inventory.clone()),
            incremental: Arc::new(Vec::new()),
            zone_name: ZONE.to_string(),
            cycle,
            incremental_cycle: 1,
            inventories: Arc::clone(&inventories),
        };
        let mut library = PhotoLibrary::new(
            "https://synthetic.example/database/1/photos/production/shared".to_string(),
            Arc::new(std::collections::HashMap::new()),
            Box::new(session.clone()),
            Arc::new(zone()),
            "shared".into(),
            crate::retry::RetryConfig::default(),
        )
        .await
        .unwrap();
        library.shadow_capture = Some(crate::icloud::photos::inbox::ShadowCapture::new(
            db.clone(),
            baseline_owner(),
            "com",
        ));
        let selected_zones = crate::commands::zone_name_set(std::slice::from_ref(&library));
        let collection_zones = selected_zones.clone();
        let resolved_scope = crate::commands::pass_scope_for_zone(
            selection,
            ZONE,
            &selected_zones,
            &collection_zones,
        );
        let collection_context =
            crate::commands::build_collection_context(selection, std::slice::from_ref(&library))
                .await
                .unwrap();
        let plan = crate::commands::resolve_passes_for_scope(
            &library,
            selection,
            resolved_scope,
            &collection_context,
            &[],
        )
        .await
        .unwrap();
        let mut selected = make_run_cycle_library_state_with_passes(ZONE, &token_key, plan.passes);
        selected.library = library;
        let base = make_run_cycle_download_config_builder(&root, db.clone());
        let builder = |mode, exclude, groups, zone| {
            let mut config = (*base(mode, exclude, groups, zone)).clone();
            config.folder_structure = "none".into();
            config.file_match_policy = crate::types::FileMatchPolicy::NameId7;
            config.retry.max_retries = 0;
            Arc::new(config)
        };
        let result = run_cycle(
            &[&selected],
            &app_config,
            Some(db.as_ref()),
            false,
            &builder,
            download::DownloadControls::download_hidden(),
            &shared,
            &CancellationToken::new(),
        )
        .await
        .unwrap();

        assert_eq!(result.failed_count, 0, "cycle {cycle}: {result:?}");
        assert!(result.db_sync_token_advance_safe, "cycle {cycle}");
        assert_eq!(result.stats.downloaded, if cycle == 0 { 1 } else { 0 });
        assert_eq!(
            recovered_snapshot(),
            recovered_before,
            "cycle {cycle}: recovered rows changed"
        );
        assert_eq!(std::fs::read(&old_still).unwrap(), old_bytes);
        assert_eq!(std::fs::read(&old_motion).unwrap(), MOTION);
        assert_eq!(
            std::fs::read(&old_sidecar).unwrap(),
            b"recovered still sidecar"
        );
        assert_eq!(
            std::fs::read(&custom_sidecar).unwrap(),
            b"recovered custom opaque xmp"
        );
        assert_eq!(
            [
                std::fs::metadata(&old_still).unwrap().modified().unwrap(),
                std::fs::metadata(&old_motion).unwrap().modified().unwrap(),
                std::fs::metadata(&old_sidecar).unwrap().modified().unwrap(),
                std::fs::metadata(&custom_sidecar)
                    .unwrap()
                    .modified()
                    .unwrap(),
            ],
            old_mtimes,
            "cycle {cycle}: recovered mtimes"
        );
        if cycle == 0 {
            let conn = rusqlite::Connection::open(&db_path).unwrap();
            let epoch = crate::state::db::sync_epoch::read_library(&conn, ZONE)
                .unwrap()
                .expect("the cycle enrolls the library");
            assert_eq!(epoch.state, "complete");
            assert_eq!(epoch.reason, "existing_upgrade");
            let receipts = rows(
                &db_path,
                &format!(
                    "SELECT child,version_size FROM download_receipts WHERE library='{ZONE}' ORDER BY child,version_size"
                ),
            );
            assert!(
                receipts
                    .iter()
                    .any(|row| row[0]
                        == rusqlite::types::Value::Text("asset-recovered-master".into())),
                "the current reuse receipt is produced by the cycle"
            );
            assert!(
                receipts
                    .iter()
                    .any(|row| row[0]
                        == rusqlite::types::Value::Text("asset-current-new-master".into())),
                "the new current receipt is produced by the cycle"
            );
        }
    }
    let requests = server.received_requests().await.unwrap();
    let count = |path: &str| {
        requests
            .iter()
            .filter(|request| request.method.as_str() == "GET" && request.url.path() == path)
            .count()
    };
    assert_eq!(count("/current-new"), 1);
    assert_eq!(count("/unreused"), 0);
    assert_eq!(count("/unreused-motion"), 0);
    server.verify().await;
    eprintln!(
        "RELEASED_RECOVERED36_REACHED rows={} inventories={} provenance=E representative=2_of_22",
        recovered_snapshot().len(),
        inventories.load(std::sync::atomic::Ordering::SeqCst)
    );
}

#[tokio::test]
async fn released_recovered_36_preserves_initial_history_and_progresses() {
    super::PRESERVE_SHARED_CLAIMS
        .scope(true, exercise_released_recovered_36())
        .await;
}
fn make_c13_album_app_config() -> crate::config::Config {
    let data_dir = tempfile::tempdir().unwrap();
    let globals = crate::config::GlobalArgs {
        username: Some("baseline@example.invalid".to_string()),
        domain: None,
        data_dir: Some(data_dir.path().to_string_lossy().into_owned()),
    };
    let mut sync = crate::cli::SyncArgs::default();
    sync.config_overrides.albums = vec!["Synthetic Album".to_string()];
    sync.config_overrides.libraries = vec!["primary".to_string()];
    sync.config_overrides.unfiled = Some(false);
    crate::config::Config::build(&globals, &crate::cli::PasswordArgs::default(), sync, None)
        .unwrap()
}

#[derive(Clone)]
struct C13AlbumSession {
    zone: &'static str,
    album_record: &'static str,
    album_name: String,
    inventory: Arc<Vec<serde_json::Value>>,
    delta: Arc<Vec<serde_json::Value>>,
    cycle: usize,
    pages: usize,
    pages_served: Arc<AtomicUsize>,
    records_served: Arc<AtomicUsize>,
    requested: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
}

async fn c13_relation_page(session: &C13AlbumSession, page: usize) -> serde_json::Value {
    let records: Vec<_> = (0..3)
        .map(|index| {
            json!({
                "recordName": format!("c13-relation-{page}-{index}"),
                "recordType": "CPLContainerRelation",
                "fields": {
                    "containerId": {"value": session.album_record},
                    "itemId": {"value": if index == 0 { "asset-delta-master" } else { "delta-master" }}
                }
            })
        })
        .collect();
    session
        .records_served
        .fetch_add(records.len(), std::sync::atomic::Ordering::SeqCst);
    json!({"zones":[{"zoneID":{"zoneName":session.zone,"ownerRecordName":"_defaultOwner"},
        "records":records,
        "syncToken":format!("c13-hydration-{}", page + 1),
        "moreComing": page + 1 < session.pages}]})
}

#[async_trait::async_trait]
impl crate::icloud::photos::PhotosSession for C13AlbumSession {
    async fn post(
        &self,
        url: &str,
        body: String,
        _headers: &[(&str, &str)],
    ) -> anyhow::Result<serde_json::Value> {
        let request: serde_json::Value = serde_json::from_str(&body)?;
        if url.contains("/zones/list") {
            return Ok(
                json!({"zones":[{"zoneID":{"zoneName":self.zone,"ownerRecordName":"_defaultOwner"},
                "syncToken":"c13-boundary","deleted":false}]}),
            );
        }
        if request["query"]["recordType"] == "CheckIndexingState" {
            return Ok(json!({"records":[{"recordName":"index",
                "recordType":"CheckIndexingState",
                "fields":{"state":{"value":"FINISHED"}}}]}));
        }
        if request["query"]["recordType"] == "CPLAlbumByPositionLive" {
            return Ok(json!({"records":[{"recordName":self.album_record,
                "recordType":"CPLAlbum",
                "fields":{"albumNameEnc":{"value":base64::engine::general_purpose::STANDARD.encode(self.album_name.as_bytes())}}}]}));
        }
        if url.contains("/internal/records/query/batch") {
            let total = self.inventory.len() as u64 / 2;
            let batch = request["batch"].as_array().cloned().unwrap_or_default();
            let counts: Vec<_> = batch
                .iter()
                .map(|_| json!({"records":[{"fields":{"itemCount":{"value":total}}}]}))
                .collect();
            return Ok(json!({"batch": counts}));
        }
        if url.contains("/records/query?") {
            let offset = request["query"]["filterBy"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|filter| filter["fieldName"] == "startRank")
                .and_then(|filter| filter["fieldValue"]["value"].as_u64())
                .unwrap_or(0);
            let records = if offset == 0 {
                self.inventory.as_ref().clone()
            } else {
                Vec::new()
            };
            return Ok(json!({"records":records,"syncToken":"c13-query-token"}));
        }
        if url.contains("/records/lookup?") {
            let requested = request["records"].as_array().cloned().unwrap_or_default();
            let names: Vec<_> = requested
                .iter()
                .map(|query| query["recordName"].clone())
                .collect();
            let records: Vec<_> = self
                .inventory
                .iter()
                .chain(self.delta.iter())
                .filter(|record| {
                    requested
                        .iter()
                        .any(|query| query["recordName"] == record["recordName"])
                })
                .cloned()
                .collect();
            if self.cycle == 1
                && names.iter().any(|name| {
                    name.as_str() == Some("asset-delta-master")
                        || name.as_str() == Some("delta-master")
                })
            {
                self.requested.notify_one();
                self.release.notified().await;
            }
            return Ok(json!({"records":records}));
        }
        if url.contains("/changes/zone?") {
            if self.cycle == 0 {
                return Ok(
                    json!({"zones":[{"zoneID":{"zoneName":self.zone,"ownerRecordName":"_defaultOwner"},
                    "records":[],
                    "syncToken":"c13-after-0",
                    "moreComing":false}]}),
                );
            }
            if self.cycle == 1 {
                let page = self.pages_served.load(std::sync::atomic::Ordering::SeqCst);
                if page < self.pages {
                    self.pages_served
                        .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    return Ok(c13_relation_page(self, page).await);
                }
                return Ok(
                    json!({"zones":[{"zoneID":{"zoneName":self.zone,"ownerRecordName":"_defaultOwner"},
                    "records":[],
                    "syncToken":"c13-after-1",
                    "moreComing":false}]}),
                );
            }
            let records = if self.cycle == 2 {
                self.delta.as_ref().clone()
            } else {
                Vec::new()
            };
            return Ok(
                json!({"zones":[{"zoneID":{"zoneName":self.zone,"ownerRecordName":"_defaultOwner"},
                "records":records,
                "syncToken":format!("c13-after-{}", self.cycle),
                "moreComing":false}]}),
            );
        }
        anyhow::bail!("unexpected C13 endpoint: {url}")
    }

    fn clone_box(&self) -> Box<dyn crate::icloud::photos::PhotosSession> {
        Box::new(self.clone())
    }
}

/// C13 genuine-entry relation wait/cancel and normal replay.
pub(in crate::sync_loop::runner::tests) async fn exercise_released_relation_cancel() {
    use std::time::Duration;
    use tracing_subscriber::layer::SubscriberExt;

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("photos");
    let protected = root.join("protected");
    std::fs::create_dir_all(&protected).unwrap();
    let old_path = protected.join("old.JPG");
    let old_sidecar = protected.join("old.JPG.xmp");
    let old_bytes = MEDIA.to_vec();
    std::fs::write(&old_path, &old_bytes).unwrap();
    std::fs::write(&old_sidecar, b"protected C13 sidecar").unwrap();
    let old_mtime = (
        std::fs::metadata(&old_path).unwrap().modified().unwrap(),
        std::fs::metadata(&old_sidecar).unwrap().modified().unwrap(),
    );

    let source = dir.path().join("released.db");
    let db_path = dir.path().join("state.db");
    {
        let conn = rusqlite::Connection::open(&source).unwrap();
        crate::state::schema::migrate_genuine_v28(&conn).unwrap();
        assert_eq!(
            conn.query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            28,
            "initial genuine released schema"
        );
        assert_eq!(
            conn.query_row(
                "SELECT COUNT(*) FROM metadata WHERE key LIKE 'sync_token%'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
            0,
            "no seeded current or legacy cursor"
        );
        assert_eq!(
            conn.query_row(
                "SELECT COUNT(*) FROM metadata WHERE key='sync_epoch:v1' OR key LIKE 'sync_epoch_library:v1:%'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
            0,
            "no seeded completed epoch"
        );
        let receipts_table: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name='download_receipts')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        if receipts_table {
            assert_eq!(
                conn.query_row("SELECT COUNT(*) FROM download_receipts", [], |row| {
                    row.get::<_, i64>(0)
                })
                .unwrap(),
                0,
                "no seeded current receipts"
            );
        }
        insert_released_asset(
            &conn,
            "PrimarySync",
            "old-primary-master",
            "original",
            "old.JPG",
            &old_path,
            &old_bytes,
            &signature(&old_bytes),
            true,
        );
        conn.execute_batch(
            "INSERT INTO asset_master_mappings VALUES ('PrimarySync','old-primary-child','old-primary-master',1700000000);
             INSERT INTO asset_metadata_capture_revisions VALUES ('PrimarySync','old-primary-master',1,1700000000);
             INSERT INTO unresolved_sparse_identities(library,source_record_name,original_evidence,observed_evidence,generation,first_seen_at,last_outcome)
               VALUES('PrimarySync','old-ambiguous','[1,\"old-child\",\"SharedSync-absent\",\"owner\"]','[1,\"old-child\",\"SharedSync-absent\",\"owner\"]',7,1000,'[malformed');",
        )
        .unwrap();
    }
    let columns = released_columns(&source);
    let legacy_rows = released_rows(&source, &columns);
    crate::state::db::account::adopt_legacy(
        &source,
        &db_path,
        &baseline_owner(),
        "baseline@example.invalid",
        "com",
    )
    .await
    .unwrap();
    assert_eq!(released_rows(&db_path, &columns), legacy_rows);

    let protected_snapshot = || {
        rows(
            &db_path,
            "SELECT * FROM assets WHERE library='PrimarySync' AND id='old-primary-master' ORDER BY version_size",
        )
        .into_iter()
        .chain(rows(
            &db_path,
            "SELECT * FROM asset_metadata_paths WHERE library='PrimarySync' AND id='old-primary-master' ORDER BY version_size",
        ))
        .chain(rows(
            &db_path,
            "SELECT * FROM asset_master_mappings WHERE library='PrimarySync' AND asset_record_name='old-primary-child'",
        ))
        .chain(rows(
            &db_path,
            "SELECT * FROM asset_metadata_capture_revisions WHERE library='PrimarySync' AND asset_id='old-primary-master'",
        ))
        .chain(rows(
            &db_path,
            "SELECT * FROM unresolved_sparse_identities WHERE library='PrimarySync' ORDER BY source_record_name",
        ))
        .collect::<Vec<_>>()
    };
    let protected_before = protected_snapshot();

    let server = crate::test_helpers::start_wiremock_or_skip("released C13 relation")
        .await
        .expect("acceptance server must bind");
    for (endpoint, expected) in [("/album-current", 1), ("/delta-current", 1)] {
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
    let checksum = signature(MEDIA);
    let mut current = pair(
        "album-master",
        "album.JPG",
        &format!("{}/album-current", server.uri()),
        41,
    );
    current[0]["fields"]["resOriginalRes"]["value"]["fileChecksum"] = json!(checksum.clone());
    current[0]["fields"]["resOriginalRes"]["value"]["size"] = json!(MEDIA.len());
    let mut delta = pair(
        "delta-master",
        "delta.JPG",
        &format!("{}/delta-current", server.uri()),
        42,
    );
    delta[0]["fields"]["resOriginalRes"]["value"]["fileChecksum"] = json!(checksum.clone());
    delta[0]["fields"]["resOriginalRes"]["value"]["size"] = json!(MEDIA.len());
    for record in [&mut current, &mut delta] {
        record[1]["fields"]["masterRef"]["value"]["zoneID"] =
            json!({"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"});
    }

    let app_config = make_c13_album_app_config();
    let selection = &app_config.filters.selection;
    let (_session_dir, shared) = make_shared_session_for_run_cycle().await;
    let requested = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let pages_served = Arc::new(AtomicUsize::new(0));
    let records_served = Arc::new(AtomicUsize::new(0));
    let pages = 2;
    let token_key = sync_token_key("PrimarySync");

    let make_library = |cycle: usize| {
        let session = C13AlbumSession {
            zone: "PrimarySync",
            album_record: "album-c13",
            album_name: "Synthetic Album".to_string(),
            inventory: Arc::new(current.clone()),
            delta: Arc::new(delta.clone()),
            cycle,
            pages,
            pages_served: Arc::clone(&pages_served),
            records_served: Arc::clone(&records_served),
            requested: Arc::clone(&requested),
            release: Arc::clone(&release),
        };
        async move {
            PhotoLibrary::new(
                "https://synthetic.example/database/1/photos/production/private".to_string(),
                Arc::new(std::collections::HashMap::new()),
                Box::new(session),
                Arc::new(json!({"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"})),
                "private".into(),
                crate::retry::RetryConfig::default(),
            )
            .await
            .unwrap()
        }
    };

    // Cycle 0: ordinary first baseline through the real album resolver.
    let db = Arc::new(
        SqliteStateDb::open_owned(&db_path, &baseline_owner())
            .await
            .unwrap(),
    );
    let library = make_library(0).await;
    let mut library = library;
    library.shadow_capture = Some(crate::icloud::photos::inbox::ShadowCapture::new(
        db.clone(),
        baseline_owner(),
        "com",
    ));
    let selected_zones = crate::commands::zone_name_set(std::slice::from_ref(&library));
    let collection_zones = selected_zones.clone();
    let scope = crate::commands::pass_scope_for_zone(
        selection,
        "PrimarySync",
        &selected_zones,
        &collection_zones,
    );
    assert!(scope.include_albums);
    assert!(!scope.include_unfiled);
    let context =
        crate::commands::build_collection_context(selection, std::slice::from_ref(&library))
            .await
            .unwrap();
    let plan = crate::commands::resolve_passes_for_scope(&library, selection, scope, &context, &[])
        .await
        .unwrap();
    assert_eq!(plan.passes.len(), 1, "single selected album");
    assert_eq!(plan.passes[0].kind, PassKind::Album);
    assert_eq!(plan.passes[0].album.name.as_ref(), "Synthetic Album");
    assert_eq!(
        plan.passes[0].album.container_id(),
        Some("album-c13"),
        "relation routing uses the CloudKit folder record id"
    );
    let mut selected =
        make_run_cycle_library_state_with_passes("PrimarySync", &token_key, plan.passes.clone());
    selected.library = library;
    let base = make_run_cycle_download_config_builder(&root, db.clone());
    let builder = |mode, exclude, groups, zone| {
        let mut config = (*base(mode, exclude, groups, zone)).clone();
        config.folder_structure = "none".into();
        Arc::new(config)
    };
    let result = run_cycle(
        &[&selected],
        &app_config,
        Some(db.as_ref()),
        false,
        &builder,
        download::DownloadControls::download_hidden(),
        &shared,
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(result.failed_count, 0, "{result:?}");
    assert!(result.db_sync_token_advance_safe, "{result:?}");
    let prior_token = db.get_metadata(&token_key).await.unwrap();
    assert!(prior_token.is_some(), "baseline token");
    let prior_config = db.get_metadata("config_hash").await.unwrap();
    assert!(prior_config.is_some(), "baseline config hash");
    assert_eq!(
        std::fs::read(&old_path).unwrap(),
        old_bytes,
        "protected bytes"
    );
    assert_eq!(protected_snapshot(), protected_before);

    // Cycle 1: later delta enters the paginated album relation scan and waits.
    let library = make_library(1).await;
    let mut library = library;
    library.shadow_capture = Some(crate::icloud::photos::inbox::ShadowCapture::new(
        db.clone(),
        baseline_owner(),
        "com",
    ));
    let cycle_selected_zones = crate::commands::zone_name_set(std::slice::from_ref(&library));
    let cycle_collection_zones = cycle_selected_zones.clone();
    let cycle_scope = crate::commands::pass_scope_for_zone(
        selection,
        "PrimarySync",
        &cycle_selected_zones,
        &cycle_collection_zones,
    );
    let cycle_context =
        crate::commands::build_collection_context(selection, std::slice::from_ref(&library))
            .await
            .unwrap();
    let cycle_plan = crate::commands::resolve_passes_for_scope(
        &library,
        selection,
        cycle_scope,
        &cycle_context,
        &[],
    )
    .await
    .unwrap();
    let mut selected =
        make_run_cycle_library_state_with_passes("PrimarySync", &token_key, cycle_plan.passes);
    selected.library = library;
    let base = make_run_cycle_download_config_builder(&root, db.clone());
    let builder = |mode, exclude, groups, zone| {
        let mut config = (*base(mode, exclude, groups, zone)).clone();
        config.folder_structure = "none".into();
        Arc::new(config)
    };
    let (layer, diagnostics) = crate::support::test_capture::test_layer();
    let _default = tracing::subscriber::set_default(tracing_subscriber::registry().with(layer));
    let _worker_callsite_registration = tracing::Dispatch::new(tracing_subscriber::registry());
    let cancel = CancellationToken::new();
    let states = [&selected];
    let mut cycle = Box::pin(run_cycle(
        &states,
        &app_config,
        Some(db.as_ref()),
        false,
        &builder,
        download::DownloadControls::download_hidden(),
        &shared,
        &cancel,
    ));
    tokio::select! {
        () = requested.notified() => {}
        result = cycle.as_mut() => panic!("relation gate was not reached: {result:?}"),
        () = tokio::time::sleep(Duration::from_secs(5)) => {
            release.notify_one();
            panic!("relation gate was not reached within the fixture bound");
        }
    }
    cancel.cancel();
    let result = tokio::time::timeout(Duration::from_secs(5), cycle.as_mut()).await;
    let drained = result.is_ok();
    release.notify_one();
    let result = match result {
        Ok(result) => result,
        Err(_) => tokio::time::timeout(Duration::from_secs(5), cycle.as_mut())
            .await
            .expect("released relation scan must finish"),
    }
    .expect("cancelled relation cycle returns its checkpoint hold");
    assert!(drained, "cancel must drain cooperatively");
    assert!(result.stats.interrupted);
    assert!(!result.db_sync_token_advance_safe);
    assert_eq!(
        db.get_metadata(&token_key).await.unwrap(),
        prior_token,
        "prior current token preserved"
    );
    assert_eq!(
        db.get_metadata("config_hash").await.unwrap(),
        prior_config,
        "prior current config preserved"
    );
    assert_eq!(
        pages_served.load(std::sync::atomic::Ordering::SeqCst),
        pages
    );
    assert_eq!(
        records_served.load(std::sync::atomic::Ordering::SeqCst),
        pages * 3,
        "bounded relation records observed during the wait"
    );
    let observations = diagnostics();
    assert!(
        observations.iter().any(|event| {
            event["kind"] == "support_hydration_wait_v1"
                && event["fields"]["phase"] == "album_relation"
                && event["fields"]["outcome"] == "cancelled"
        }),
        "observable album_relation phase cancellation: {observations:?}"
    );
    let encoded = serde_json::to_string(&observations).unwrap();
    for private in [
        "album-c13",
        "Synthetic Album",
        "asset-delta-master",
        "example.invalid",
    ] {
        assert!(!encoded.contains(private), "anonymous progress only");
    }
    assert_eq!(protected_snapshot(), protected_before);
    assert_eq!(std::fs::read(&old_path).unwrap(), old_bytes);
    assert_eq!(
        std::fs::read(&old_sidecar).unwrap(),
        b"protected C13 sidecar"
    );
    assert_eq!(
        (
            std::fs::metadata(&old_path).unwrap().modified().unwrap(),
            std::fs::metadata(&old_sidecar).unwrap().modified().unwrap(),
        ),
        old_mtime
    );
    drop(cycle);
    drop(selected);
    drop(db);

    // Reopen: valid response, current same-library item and fresh proof.
    let mut receipts_after_recovery: Option<Vec<Vec<rusqlite::types::Value>>> = None;
    for cycle in 2..5 {
        let db = Arc::new(
            SqliteStateDb::open_owned(&db_path, &baseline_owner())
                .await
                .unwrap(),
        );
        let library = make_library(cycle).await;
        let mut library = library;
        library.shadow_capture = Some(crate::icloud::photos::inbox::ShadowCapture::new(
            db.clone(),
            baseline_owner(),
            "com",
        ));
        let selected_zones = crate::commands::zone_name_set(std::slice::from_ref(&library));
        let collection_zones = selected_zones.clone();
        let scope = crate::commands::pass_scope_for_zone(
            selection,
            "PrimarySync",
            &selected_zones,
            &collection_zones,
        );
        let context =
            crate::commands::build_collection_context(selection, std::slice::from_ref(&library))
                .await
                .unwrap();
        let cycle_plan =
            crate::commands::resolve_passes_for_scope(&library, selection, scope, &context, &[])
                .await
                .unwrap();
        let mut selected =
            make_run_cycle_library_state_with_passes("PrimarySync", &token_key, cycle_plan.passes);
        selected.library = library;
        let base = make_run_cycle_download_config_builder(&root, db.clone());
        let builder = |mode, exclude, groups, zone| {
            let mut config = (*base(mode, exclude, groups, zone)).clone();
            config.folder_structure = "none".into();
            Arc::new(config)
        };
        let result = run_cycle(
            &[&selected],
            &app_config,
            Some(db.as_ref()),
            false,
            &builder,
            download::DownloadControls::download_hidden(),
            &shared,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(result.failed_count, 0, "cycle {cycle}: {result:?}");
        assert!(result.db_sync_token_advance_safe, "cycle {cycle}");
        if cycle == 2 {
            assert_eq!(result.stats.downloaded, 1, "delta media once");
            let completed = db.get_downloaded_page(0, 100).await.unwrap();
            let row = completed
                .iter()
                .find(|row| row.id.as_ref() == "asset-delta-master")
                .expect("delta current media");
            assert_eq!(
                std::fs::read(row.local_path.as_ref().unwrap()).unwrap(),
                MEDIA
            );
            let observation: i64 = db
                .acquire_lock("fresh proof")
                .unwrap()
                .query_row(
                    "SELECT COUNT(*) FROM metadata WHERE key LIKE 'observed_selection:v27:%PrimarySync'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(observation, 1, "fresh current observation proof");
            receipts_after_recovery = Some(rows(
                &db_path,
                "SELECT * FROM download_receipts ORDER BY id",
            ));
        } else {
            assert_eq!(result.stats.downloaded, 0, "cycle {cycle}: quiet");
            assert_eq!(
                rows(&db_path, "SELECT * FROM download_receipts ORDER BY id"),
                receipts_after_recovery.clone().unwrap(),
                "cycle {cycle}: no receipt churn"
            );
        }
        assert_eq!(protected_snapshot(), protected_before);
        assert_eq!(std::fs::read(&old_path).unwrap(), old_bytes);
    }
    let requests = server.received_requests().await.unwrap();
    let count = |path: &str| {
        requests
            .iter()
            .filter(|request| request.method.as_str() == "GET" && request.url.path() == path)
            .count()
    };
    assert_eq!(count("/album-current"), 1);
    assert_eq!(count("/delta-current"), 1);
    server.verify().await;
    eprintln!(
        "RELEASED_C13_REACHED pages={} records={} protected_rows={}",
        pages_served.load(std::sync::atomic::Ordering::SeqCst),
        records_served.load(std::sync::atomic::Ordering::SeqCst),
        protected_snapshot().len()
    );
}

#[tokio::test]
async fn released_relation_cancel_and_normal_replay_28() {
    super::PRESERVE_SHARED_CLAIMS
        .scope(true, Box::pin(exercise_released_relation_cancel()))
        .await;
}

fn make_primary_unfiled_app_config() -> crate::config::Config {
    let data_dir = tempfile::tempdir().unwrap();
    let globals = crate::config::GlobalArgs {
        username: Some("baseline@example.invalid".to_string()),
        domain: None,
        data_dir: Some(data_dir.path().to_string_lossy().into_owned()),
    };
    let mut sync = crate::cli::SyncArgs::default();
    sync.config_overrides.albums = vec!["none".to_string()];
    sync.config_overrides.libraries = vec!["primary".to_string()];
    sync.config_overrides.unfiled = Some(true);
    crate::config::Config::build(&globals, &crate::cli::PasswordArgs::default(), sync, None)
        .unwrap()
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Stock28Catalog {
    DownloadedCaptureHistory,
    InconclusivePendingDownloads,
}

async fn exercise_stock28_old_debt(
    old_assets: usize,
    sparse_rows: usize,
    capture_rows: usize,
    catalog: Stock28Catalog,
) {
    let fixture_start = std::time::Instant::now();
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("photos");
    let protected = root.join("protected");
    std::fs::create_dir_all(&protected).unwrap();
    let old_path = protected.join("old.JPG");
    let old_sidecar = protected.join("old.JPG.xmp");
    let old_bytes = MEDIA.to_vec();
    std::fs::write(&old_path, &old_bytes).unwrap();
    std::fs::write(&old_sidecar, b"protected stock28 sidecar").unwrap();
    let old_mtime = (
        std::fs::metadata(&old_path).unwrap().modified().unwrap(),
        std::fs::metadata(&old_sidecar).unwrap().modified().unwrap(),
    );
    let old_bytes_sha = sha256_hex(&old_bytes);
    let mut old_catalog_files = Vec::new();

    let source = dir.path().join("released.db");
    let db_path = dir.path().join("state.db");
    {
        let conn = rusqlite::Connection::open(&source).unwrap();
        crate::state::schema::migrate_genuine_v28(&conn).unwrap();
        assert_eq!(
            conn.query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            28
        );
        assert_eq!(
            conn.query_row(
                "SELECT COUNT(*) FROM metadata WHERE key LIKE 'sync_token%'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
            0
        );
        assert_eq!(
            conn.query_row(
                "SELECT COUNT(*) FROM metadata WHERE key='sync_epoch:v1' OR key LIKE 'sync_epoch_library:v1:%'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
            0
        );
        insert_released_asset(
            &conn,
            "PrimarySync",
            "old-protected-master",
            "original",
            "old.JPG",
            &old_path,
            &old_bytes,
            &signature(&old_bytes),
            true,
        );
        conn.execute_batch(
            "INSERT INTO asset_master_mappings VALUES ('PrimarySync','old-protected-child','old-protected-master',1700000000);
             INSERT INTO asset_metadata_capture_revisions VALUES ('PrimarySync','old-protected-master',1,1700000000);",
        )
        .unwrap();
        let tx = conn.unchecked_transaction().unwrap();
        {
            for index in 0..old_assets {
                let id = format!("old-catalog-{index}");
                let filename = format!("old-{index}.JPG");
                if catalog == Stock28Catalog::InconclusivePendingDownloads {
                    tx.execute(
                        "INSERT INTO assets(library,id,version_size,checksum,filename,created_at,size_bytes,media_type,status,last_seen_at,source) \
                         VALUES('PrimarySync',?1,'original',?2,?3,1700000000,1,'photo','pending',1700000000,'icloud')",
                        rusqlite::params![id, format!("old-provider-{index}"), filename],
                    ).unwrap();
                } else {
                    let path = protected.join(&filename);
                    std::fs::write(&path, &old_bytes).unwrap();
                    let modified = std::fs::metadata(&path).unwrap().modified().unwrap();
                    insert_released_asset(
                        &tx,
                        "PrimarySync",
                        &id,
                        "original",
                        &filename,
                        &path,
                        &old_bytes,
                        &signature(&old_bytes),
                        true,
                    );
                    old_catalog_files.push((path, modified));
                }
            }
        }
        {
            let mut sparse = tx
                .prepare(
                    "INSERT INTO unresolved_sparse_identities(library,source_record_name,original_evidence,observed_evidence,generation,first_seen_at,last_outcome) \
                     VALUES('PrimarySync',?1,'[malformed legacy evidence]','[malformed legacy evidence]',7,1000,'[invalid outcome')",
                )
                .unwrap();
            for index in 0..sparse_rows {
                sparse
                    .execute(rusqlite::params![format!("old-sparse-{index}")])
                    .unwrap();
            }
        }
        {
            for index in 0..capture_rows {
                let (id, revision, evidence, last, next) =
                    if catalog == Stock28Catalog::InconclusivePendingDownloads {
                        (
                            format!("old-capture-{index}"),
                            0,
                            "opaque old capture evidence".to_string(),
                            1_700_000_000,
                            1_700_003_600,
                        )
                    } else {
                        let id = format!("old-catalog-{}", index % old_assets);
                        let evidence = json!([
                            id,
                            null,
                            [[
                                "original",
                                signature(&old_bytes),
                                old_bytes.len(),
                                1_700_000_002_000_i64
                            ]]
                        ])
                        .to_string();
                        (id, 1, evidence, 1_700_000_000, 1_700_003_600)
                    };
                tx.execute(
                    "INSERT INTO metadata_capture_retries(library,asset_id,target_revision,evidence,generation,attempts,last_attempt_at,next_retry_at) \
                     VALUES('PrimarySync',?1,?2,?3,1,1,?4,?5)",
                    rusqlite::params![id, revision, evidence, last, next],
                ).unwrap();
            }
        }
        tx.commit().unwrap();
        if catalog == Stock28Catalog::DownloadedCaptureHistory {
            assert_eq!(conn.query_row("SELECT COUNT(*) FROM assets WHERE library='PrimarySync' AND id LIKE 'old-catalog-%' AND status='downloaded'", [], |r| r.get::<_, i64>(0)).unwrap(), i64::try_from(old_assets).unwrap());
            assert_eq!(conn.query_row("SELECT COUNT(*) FROM asset_metadata_capture_revisions WHERE library='PrimarySync' AND asset_id LIKE 'old-catalog-%'", [], |r| r.get::<_, i64>(0)).unwrap(), 0);
            assert_eq!(conn.query_row("SELECT COUNT(*) FROM metadata_capture_retries WHERE library='PrimarySync' AND asset_id LIKE 'old-catalog-%'", [], |r| r.get::<_, i64>(0)).unwrap(), i64::try_from(capture_rows).unwrap());
        }
    }
    let mut columns = released_columns(&source);
    {
        let conn = rusqlite::Connection::open(&source).unwrap();
        for table in [
            "unresolved_sparse_identities",
            "metadata_capture_retries",
            "metadata_capture_state",
        ] {
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
            columns.push((table.to_owned(), names));
        }
    }
    let legacy_rows = released_rows(&source, &columns);
    eprintln!(
        "C04_PHASE setup elapsed_ms={} old_catalog={old_assets} sparse={sparse_rows} protected_capture_retries={capture_rows} old_media_sha256={} sparse_payload_sha256={}",
        fixture_start.elapsed().as_millis(),
        old_bytes_sha,
        sha256_hex(b"[malformed legacy evidence]")
    );
    let adoption_start = std::time::Instant::now();
    crate::state::db::account::adopt_legacy(
        &source,
        &db_path,
        &baseline_owner(),
        "baseline@example.invalid",
        "com",
    )
    .await
    .unwrap();
    eprintln!(
        "C04_PHASE owned_adoption elapsed_ms={}",
        adoption_start.elapsed().as_millis()
    );
    assert_eq!(released_rows(&db_path, &columns), legacy_rows);

    let old_snapshot = || {
        rows(
            &db_path,
            "SELECT * FROM assets WHERE library IN ('PrimarySync','PrimaryLegacy') AND id LIKE 'old-%' ORDER BY library,id,version_size",
        )
        .into_iter()
        .chain(rows(
            &db_path,
            "SELECT * FROM unresolved_sparse_identities WHERE library='PrimarySync' AND source_record_name LIKE 'old-sparse-%' ORDER BY source_record_name",
        ))
        .chain(rows(
            &db_path,
            "SELECT * FROM metadata_capture_retries WHERE library='PrimarySync' AND asset_id LIKE 'old-%' ORDER BY asset_id",
        ))
        .chain(rows(&db_path, "SELECT * FROM asset_metadata_paths WHERE library='PrimarySync' AND id LIKE 'old-%' ORDER BY id,version_size"))
        .chain(rows(&db_path, "SELECT * FROM asset_master_mappings WHERE library='PrimarySync' AND master_record_name LIKE 'old-%' ORDER BY master_record_name"))
        .chain(rows(&db_path, "SELECT * FROM asset_metadata_capture_revisions WHERE library='PrimarySync' AND asset_id LIKE 'old-%' ORDER BY asset_id"))
        .collect::<Vec<_>>()
    };
    let old_before = old_snapshot();

    let server = crate::test_helpers::start_wiremock_or_skip("stock28 old debt")
        .await
        .expect("acceptance server must bind");
    for (endpoint, expected) in [
        ("/current-new", 1),
        (
            "/bridge-new",
            u64::from(catalog == Stock28Catalog::DownloadedCaptureHistory),
        ),
    ] {
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
    let checksum = signature(MEDIA);
    let mut current = pair(
        "current-new-master",
        "current-new.JPG",
        &format!("{}/current-new", server.uri()),
        51,
    );
    current[0]["fields"]["resOriginalRes"]["value"]["fileChecksum"] = json!(checksum.clone());
    current[0]["fields"]["resOriginalRes"]["value"]["size"] = json!(MEDIA.len());
    let mut bridge = pair(
        "bridge-new-master",
        "bridge-new.JPG",
        &format!("{}/bridge-new", server.uri()),
        52,
    );
    bridge[0]["fields"]["resOriginalRes"]["value"]["fileChecksum"] = json!(checksum.clone());
    bridge[0]["fields"]["resOriginalRes"]["value"]["size"] = json!(MEDIA.len());
    for record in [&mut current, &mut bridge] {
        record[1]["fields"]["masterRef"]["value"]["zoneID"] =
            json!({"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"});
    }

    let token_key = sync_token_key("PrimarySync");
    let app_config = make_primary_unfiled_app_config();
    let selection = &app_config.filters.selection;
    let (_session_dir, shared) = make_shared_session_for_run_cycle().await;
    let inventory = Arc::new(current);
    let incremental = Arc::new(bridge);
    let mut current_paths = BTreeMap::new();
    let mut config_after = None;
    for cycle in 0..4 {
        let db = Arc::new(
            SqliteStateDb::open_owned(&db_path, &baseline_owner())
                .await
                .unwrap(),
        );
        let inventories = Arc::new(AtomicUsize::new(0));
        let session = ProtectedSession {
            records: Arc::clone(&inventory),
            incremental: Arc::clone(&incremental),
            zone_name: "PrimarySync".to_string(),
            cycle,
            incremental_cycle: if catalog == Stock28Catalog::InconclusivePendingDownloads {
                1
            } else {
                0
            },
            inventories: Arc::clone(&inventories),
        };
        let mut library = PhotoLibrary::new(
            "https://synthetic.example/database/1/photos/production/private".to_string(),
            Arc::new(std::collections::HashMap::new()),
            Box::new(session.clone()),
            Arc::new(json!({"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"})),
            "private".into(),
            crate::retry::RetryConfig::default(),
        )
        .await
        .unwrap();
        library.shadow_capture = Some(crate::icloud::photos::inbox::ShadowCapture::new(
            db.clone(),
            baseline_owner(),
            "com",
        ));
        let selected_zones = crate::commands::zone_name_set(std::slice::from_ref(&library));
        let collection_zones = selected_zones.clone();
        let scope = crate::commands::pass_scope_for_zone(
            selection,
            "PrimarySync",
            &selected_zones,
            &collection_zones,
        );
        let context =
            crate::commands::build_collection_context(selection, std::slice::from_ref(&library))
                .await
                .unwrap();
        let plan =
            crate::commands::resolve_passes_for_scope(&library, selection, scope, &context, &[])
                .await
                .unwrap();
        let mut selected =
            make_run_cycle_library_state_with_passes("PrimarySync", &token_key, plan.passes);
        selected.library = library;
        let base = make_run_cycle_download_config_builder(&root, db.clone());
        let builder = |mode, exclude, groups, zone| {
            let mut config = (*base(mode, exclude, groups, zone)).clone();
            config.folder_structure = "none".into();
            config.file_match_policy = crate::types::FileMatchPolicy::NameId7;
            config.retry.max_retries = 0;
            Arc::new(config)
        };
        let cycle_start = std::time::Instant::now();
        let result = run_cycle(
            &[&selected],
            &app_config,
            Some(db.as_ref()),
            false,
            &builder,
            download::DownloadControls::download_hidden(),
            &shared,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        eprintln!(
            "C04_PHASE run_cycle cycle={cycle} elapsed_ms={}",
            cycle_start.elapsed().as_millis()
        );

        if catalog == Stock28Catalog::InconclusivePendingDownloads {
            assert_eq!(cycle, 0);
            assert_eq!(result.failed_count, 2 * old_assets, "{result:?}");
            assert_eq!(result.stats.downloaded, 1, "{result:?}");
        } else {
            assert_eq!(result.failed_count, 0, "cycle {cycle}: {result:?}");
            assert_eq!(
                result.stats.downloaded,
                if cycle == 0 { 2 } else { 0 },
                "cycle {cycle}: {result:?}"
            );
        }
        assert!(result.db_sync_token_advance_safe, "cycle {cycle}");
        assert_eq!(
            db.get_metadata(&token_key).await.unwrap().as_deref(),
            Some(format!("protected-after-{cycle}").as_str())
        );
        assert_eq!(
            db.get_metadata("sync_token:PrimarySync").await.unwrap(),
            None
        );
        let enum_hash = db.get_metadata("enum_config_hash").await.unwrap();
        assert_eq!(
            enum_hash.as_deref(),
            Some(crate::download::compute_config_hash(&app_config).as_str())
        );
        let config = db
            .get_metadata("config_hash")
            .await
            .unwrap()
            .expect("normal path configuration checkpoint");
        if let Some(prior) = &config_after {
            assert_eq!(&config, prior, "quiet configuration checkpoint changed");
        } else {
            config_after = Some(config);
        }
        if catalog == Stock28Catalog::DownloadedCaptureHistory && cycle == 0 {
            assert!(
                inventories.load(std::sync::atomic::Ordering::SeqCst) > 0,
                "real selected inventory must run"
            );
            let conn = rusqlite::Connection::open(&db_path).unwrap();
            let epoch = crate::state::db::sync_epoch::read_library(&conn, "PrimarySync")
                .unwrap()
                .expect("normal enrollment");
            assert_eq!(epoch.reason, "existing_upgrade");
            assert_eq!(epoch.state, "complete");
            assert_eq!(epoch.baseline_scope, selected.library.baseline_scope());
        }
        assert_eq!(
            old_snapshot(),
            old_before,
            "cycle {cycle}: old catalog/sparse/capture debt changed"
        );
        assert_eq!(
            sha256_hex(&std::fs::read(&old_path).unwrap()),
            old_bytes_sha
        );
        assert_eq!(
            std::fs::read(&old_sidecar).unwrap(),
            b"protected stock28 sidecar"
        );
        assert_eq!(
            (
                std::fs::metadata(&old_path).unwrap().modified().unwrap(),
                std::fs::metadata(&old_sidecar).unwrap().modified().unwrap(),
            ),
            old_mtime
        );
        for (path, modified) in &old_catalog_files {
            assert_eq!(std::fs::read(path).unwrap(), old_bytes);
            assert_eq!(
                std::fs::metadata(path).unwrap().modified().unwrap(),
                *modified
            );
        }
        // Released downloaded rows precede the two newly published rows by
        // rowid. Keep the proof bounded when the catalogue is large.
        let offset = if catalog == Stock28Catalog::DownloadedCaptureHistory {
            u64::try_from(old_assets).unwrap()
        } else {
            0
        };
        let completed = db.get_downloaded_page(offset, 100).await.unwrap();
        if catalog == Stock28Catalog::InconclusivePendingDownloads {
            let current = completed
                .iter()
                .find(|row| row.id.as_ref() == "asset-current-new-master")
                .expect("current media is durable despite retained pending failures");
            assert_eq!(
                std::fs::read(current.local_path.as_ref().unwrap()).unwrap(),
                MEDIA
            );
            drop(db);
            let reopened = SqliteStateDb::open_owned(&db_path, &baseline_owner())
                .await
                .unwrap();
            assert!(reopened.get_metadata(&token_key).await.unwrap().is_some());
            assert_eq!(old_snapshot(), old_before);
            break;
        }
        if cycle < 2 {
            for id in ["asset-current-new-master", "asset-bridge-new-master"] {
                let row = completed
                    .iter()
                    .find(|row| row.id.as_ref() == id)
                    .unwrap_or_else(|| panic!("cycle {cycle}: missing {id}"));
                let path = row.local_path.clone().unwrap();
                let modified = std::fs::metadata(&path).unwrap().modified().unwrap();
                let (prior_path, prior_modified) = current_paths
                    .entry(id)
                    .or_insert_with(|| (path.clone(), modified));
                assert_eq!(&path, prior_path);
                assert_eq!(modified, *prior_modified);
                assert_eq!(std::fs::read(path).unwrap(), MEDIA);
            }
        } else {
            assert_eq!(result.stats.downloaded, 0, "cycle {cycle}: quiet");
            for (id, (prior, modified)) in &current_paths {
                assert!(completed.iter().any(|row| row.id.as_ref() == *id && row.local_path.as_ref() == Some(prior)));
                assert_eq!(std::fs::read(prior).unwrap(), MEDIA);
                assert_eq!(
                    std::fs::metadata(prior).unwrap().modified().unwrap(),
                    *modified
                );
            }
        }
    }
    server.verify().await;
    eprintln!(
        "C04_PHASE fixture_total elapsed_ms={}",
        fixture_start.elapsed().as_millis()
    );
}

#[tokio::test]
async fn released_stock28_small_old_debt_current_progress() {
    super::PRESERVE_SHARED_CLAIMS
        .scope(
            true,
            Box::pin(exercise_stock28_old_debt(
                8,
                4,
                2,
                Stock28Catalog::DownloadedCaptureHistory,
            )),
        )
        .await;
}

#[tokio::test]
async fn released_stock28_inconclusive_pending_downloads_remain_partial() {
    super::PRESERVE_SHARED_CLAIMS
        .scope(
            true,
            Box::pin(exercise_stock28_old_debt(
                8,
                4,
                8,
                Stock28Catalog::InconclusivePendingDownloads,
            )),
        )
        .await;
}

/// Synthetic stock-28 scale probe; not reporter data or a backlog-repair test.
#[tokio::test]
#[ignore = "representative optimized cost probe; run explicitly and measure externally"]
async fn released_stock28_representative_first_adoption_cost() {
    super::PRESERVE_SHARED_CLAIMS
        .scope(
            true,
            Box::pin(exercise_stock28_old_debt(
                61_098,
                481,
                2,
                Stock28Catalog::DownloadedCaptureHistory,
            )),
        )
        .await;
}
