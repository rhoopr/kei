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
    lookups: Arc<Mutex<Vec<serde_json::Value>>>,
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
            self.lookups.lock().unwrap().push(request.clone());
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
                    companion["value"]["downloadURL"] = json!(self.fresh_motion_url);
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

pub(in crate::sync_loop::runner::tests) async fn exercise_released_populated(
    case: ReleasedPopulatedCase,
) {
    let (trace, _trace_guard) = crate::test_helpers::TracingCapture::install();
    let motion_refresh = matches!(case, ReleasedPopulatedCase::MotionRefresh36);
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
            ReleasedPopulatedCase::MotionRefresh36 => {
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
            "INSERT INTO legacy_master_state_owners VALUES ('{ZONE}','old-master','asset-current-master',1700000000);
             INSERT INTO asset_master_mappings VALUES ('{ZONE}','asset-current-master','current-master',1700000000);
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
        receipt_rows(&db_path, "asset-current-master").is_empty(),
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
                "SELECT * FROM assets WHERE library='{ZONE}' AND id='old-master' AND version_size='original'"
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
        "current-master",
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
    let mut inventory = existing;
    inventory.extend(pair(
        "new-master",
        "new.JPG",
        &format!("{}/new", server.uri()),
        2,
    ));
    // A genuinely new item arrives on the concurrent bridge rather than the
    // first inventory page, so the baseline proves current progress as well as
    // byte-level reuse. The ordinary baseline provider's count side-channel
    // expects two initial assets and one concurrent addition.
    let concurrent = pair(
        "during-master",
        "during.JPG",
        &format!("{}/during", server.uri()),
        3,
    );

    let token_key = sync_token_key(ZONE);
    let (_session_dir, shared) = make_shared_session_for_run_cycle().await;
    let pass_scope = json!({"zone":zone(),"name":"",
        "list_type":"CPLAssetAndMasterByAssetDateWithoutHiddenOrDeleted",
        "obj_type":"CPLAssetByAssetDateWithoutHiddenOrDeleted",
        "query_filter":null,"container_id":null,"cross_zone_sources":[]});
    let lookups = Arc::new(Mutex::new(Vec::new()));
    let expected_tree = BTreeMap::from([
        ("shared-only.JPG".to_owned(), provider_still.clone()),
        ("shared-only.MOV".to_owned(), MOTION.to_vec()),
        ("new_YXNzZXQ.JPG".to_owned(), MEDIA.to_vec()),
        ("during_YXNzZXQ.JPG".to_owned(), MEDIA.to_vec()),
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
                    lookups: Arc::clone(&lookups),
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

        let completed = db.get_downloaded_page(0, 100).await.unwrap();
        for (id, version, path) in [
            ("asset-current-master", "original", still_path.as_path()),
            (
                "asset-current-master",
                "live_original",
                motion_path.as_path(),
            ),
            (
                "asset-new-master",
                "original",
                root.join("new_YXNzZXQ.JPG").as_path(),
            ),
            (
                "asset-during-master",
                "original",
                root.join("during_YXNzZXQ.JPG").as_path(),
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
        let receipts = receipt_rows(&db_path, "asset-current-master");
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
        assert_eq!(lookups.len(), 1, "one bounded exact cleanup lookup");
        let body = lookups[0].to_string();
        assert!(body.contains("asset-current"), "exact child lookup: {body}");
        assert!(
            body.contains("current-master"),
            "exact master lookup: {body}"
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
