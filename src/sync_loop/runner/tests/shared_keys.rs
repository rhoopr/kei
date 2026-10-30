//! Bounded proof for schema-28 upgrade plus selected Shared-key materialization.
//! Synthetic accounts, bytes, and provider records only.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use base64::Engine as _;
use rusqlite::Connection;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{method, path};
use wiremock::{Mock, ResponseTemplate};

use crate::icloud::photos::{PhotoLibrary, PhotosSession};
use crate::sync_loop::test_support::{
    RunCycleDownloadConfigOptions, album_count_response, full_album_page_with_download,
    make_run_cycle_config, make_run_cycle_download_config_builder,
    make_run_cycle_download_config_builder_with_options, make_run_cycle_library_state_with_album,
    make_shared_session_for_run_cycle, run_cycle_expected_date_dir,
};
use crate::{download, state};

const PRIMARY: &str = "PrimarySync";
const SHARED: &str = "SharedSync-SYNTHETIC-KEYS";
const SHARED_TOKEN: &str = "sync_token:SharedSync-SYNTHETIC-KEYS";

fn scoped_shared_album(session: Box<dyn PhotosSession>) -> crate::icloud::photos::PhotoAlbum {
    crate::icloud::photos::PhotoAlbum::new(
        crate::icloud::photos::PhotoAlbumConfig {
            params: Arc::new(std::collections::HashMap::new()),
            service_endpoint: "https://synthetic.example/database/1/photos/production/shared"
                .into(),
            name: "TestAlbum".into(),
            list_type: "CPLAssetAndMasterByAssetDateWithoutHiddenOrDeleted".into(),
            obj_type: "CPLAssetByAssetDateWithoutHiddenOrDeleted".into(),
            query_filter: None,
            page_size: 100,
            zone_id: Arc::new(json!({"zoneName":SHARED,"ownerRecordName":"_defaultOwner"})),
            retry_config: crate::retry::RetryConfig::default(),
            container_id: None,
            cross_zone_sources: Vec::new(),
        },
        session,
    )
}

#[derive(Clone, Debug)]
struct Provider {
    zone: String,
    records: Vec<Value>,
}

#[async_trait::async_trait]
impl PhotosSession for Provider {
    async fn post(&self, url: &str, body: String, _: &[(&str, &str)]) -> anyhow::Result<Value> {
        let request: Value = serde_json::from_str(&body)?;
        if request["query"]["recordType"] == "CheckIndexingState" {
            return Ok(
                json!({"records":[{"recordName":"index-state","recordType":"CheckIndexingState",
                "fields":{"state":{"value":"FINISHED"}}}]}),
            );
        }
        if url.contains("/zones/list") {
            return Ok(json!({"zones":[{
                "zoneID":{"zoneName":self.zone,"ownerRecordName":"_defaultOwner"},
                "syncToken":"shared-before-inventory"
            }]}));
        }
        if url.contains("/records/query/batch") {
            return Ok(album_count_response(
                self.records
                    .iter()
                    .filter(|record| record["recordType"] == "CPLAsset")
                    .count() as u64,
            ));
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
                self.records.clone()
            } else {
                Vec::new()
            };
            return Ok(json!({"records":records,"syncToken":"shared-inventory"}));
        }
        if url.contains("/records/lookup?") {
            let requested = request["records"].as_array().unwrap();
            let records: Vec<_> = self
                .records
                .iter()
                .filter(|record| {
                    requested
                        .iter()
                        .any(|item| item["recordName"] == record["recordName"])
                })
                .cloned()
                .collect();
            return Ok(json!({"records":records}));
        }
        assert!(
            url.contains("/changes/zone?"),
            "unexpected synthetic provider endpoint: {url}"
        );
        let delta = request["zones"][0]["syncToken"].is_string();
        let records = if delta {
            Vec::new()
        } else {
            self.records.clone()
        };
        Ok(json!({"zones":[{
            "zoneID":{"zoneName":self.zone,"ownerRecordName":"_defaultOwner"},
            "syncToken":if delta {"shared-delta-after"} else {"shared-inventory-after"},
            "moreComing":false,
            "records":records
        }]}))
    }

    fn clone_box(&self) -> Box<dyn PhotosSession> {
        Box::new(self.clone())
    }
}

fn page_records(
    zone: &str,
    master: &str,
    child: &str,
    filename: &str,
    url: &str,
    bytes: &[u8],
    checksum: &str,
) -> Vec<Value> {
    let page =
        full_album_page_with_download(zone, master, "inventory", url, bytes.len() as u64, checksum);
    let mut master_record = page["records"][0].clone();
    master_record["recordName"] = json!(master);
    master_record["fields"]["filenameEnc"] = json!({
        "value": filename,
        "type": "STRING",
    });
    let mut child_record = page["records"][1].clone();
    child_record["recordName"] = json!(child);
    child_record["fields"]["masterRef"]["value"]["recordName"] = json!(master);
    child_record["fields"]["masterRef"]["value"]["zoneID"]["zoneName"] = json!(zone);
    vec![master_record, child_record]
}

fn query(conn: &Connection, sql: &str) -> Vec<Vec<rusqlite::types::Value>> {
    let mut stmt = conn.prepare(sql).unwrap();
    let columns = stmt.column_count();
    stmt.query_map([], |row| {
        (0..columns)
            .map(|index| row.get::<_, rusqlite::types::Value>(index))
            .collect()
    })
    .unwrap()
    .collect::<Result<_, _>>()
    .unwrap()
}

fn hex_sha256(bytes: &[u8]) -> String {
    data_encoding::HEXLOWER.encode(&Sha256::digest(bytes))
}

fn keys(conn: &Connection) -> Vec<Vec<rusqlite::types::Value>> {
    query(
        conn,
        "SELECT library,id,version_size FROM assets ORDER BY library,id,version_size",
    )
}

fn baseline_six_fields(conn: &Connection) -> Vec<Vec<rusqlite::types::Value>> {
    query(
        conn,
        "SELECT a.library,a.id,a.version_size,a.status,a.checksum,\
                a.local_path,a.downloaded_at,a.size_bytes,a.added_at \
         FROM assets a LEFT JOIN asset_metadata_paths p \
           ON p.library=a.library AND p.id=a.id AND p.version_size=a.version_size \
          AND p.local_path=a.local_path \
         ORDER BY a.library,a.id,a.version_size,p.local_path",
    )
}

fn existing_baseline_fields(
    conn: &Connection,
    before: &[Vec<rusqlite::types::Value>],
) -> Vec<Vec<rusqlite::types::Value>> {
    baseline_six_fields(conn)
        .into_iter()
        .filter(|row| before.iter().any(|baseline| baseline[..3] == row[..3]))
        .collect()
}

fn mappings(conn: &Connection) -> Vec<(String, String, String)> {
    conn.prepare(
        "SELECT library,asset_record_name,master_record_name \
         FROM asset_master_mappings ORDER BY library,asset_record_name",
    )
    .unwrap()
    .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
    .unwrap()
    .collect::<Result<_, _>>()
    .unwrap()
}

fn media_files(root: &Path) -> Vec<PathBuf> {
    fn walk(dir: &Path, found: &mut Vec<PathBuf>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                walk(&path, found);
            } else {
                found.push(path);
            }
        }
    }
    let mut found = Vec::new();
    walk(root, &mut found);
    found.sort();
    found
}

fn seed_genuine_v28(
    database: &Path,
    protected: &Path,
) -> (
    Vec<Vec<rusqlite::types::Value>>,
    Vec<Vec<rusqlite::types::Value>>,
) {
    let conn = Connection::open(database).unwrap();
    crate::state::schema::migrate_genuine_v28(&conn).unwrap();
    assert_eq!(
        conn.query_row("PRAGMA user_version", [], |row| row.get::<_, i32>(0))
            .unwrap(),
        28
    );
    conn.execute(
        "INSERT INTO assets(library,id,version_size,checksum,filename,created_at,\
             added_at,size_bytes,media_type,status,downloaded_at,local_path,last_seen_at,source) \
         VALUES('PrimarySync','asset-shared-collision','original','primary-provider-checksum',\
             'collision.jpg',1700000000,1690000000,42,'photo','downloaded',1700000001,?1,\
             1700000002,'icloud')",
        rusqlite::params![protected.to_string_lossy()],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO asset_metadata_paths(library,id,version_size,local_path,\
             provider_checksum,local_checksum,download_checksum,source_checksum) \
         VALUES('PrimarySync','asset-shared-collision','original',?1,\
             'primary-provider-checksum','primary-local','primary-download','primary-source')",
        rusqlite::params![protected.to_string_lossy()],
    )
    .unwrap();
    conn.execute_batch(
        "INSERT INTO asset_master_mappings VALUES \
         ('PrimarySync','asset-shared-collision','primary-master',1700000000);",
    )
    .unwrap();
    let shared_baseline = protected.with_file_name("shared-baseline.jpg");
    conn.execute(
        "INSERT INTO assets(library,id,version_size,checksum,filename,created_at,\
             added_at,size_bytes,media_type,status,downloaded_at,local_path,last_seen_at,source) \
         VALUES('SharedSync-SYNTHETIC-KEYS','asset-shared-baseline','original',\
             'shared-baseline-provider','baseline.jpg',1700000000,1690000000,84,'photo',\
             'downloaded',1700000001,?1,1700000002,'icloud')",
        rusqlite::params![shared_baseline.to_string_lossy()],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO asset_metadata_paths(library,id,version_size,local_path,\
             provider_checksum,local_checksum,download_checksum,source_checksum) \
         VALUES('SharedSync-SYNTHETIC-KEYS','asset-shared-baseline','original',?1,\
             'shared-baseline-provider','shared-baseline-local','shared-baseline-download',\
             'shared-baseline-source')",
        rusqlite::params![shared_baseline.to_string_lossy()],
    )
    .unwrap();
    conn.execute_batch(
        "INSERT INTO asset_master_mappings VALUES \
         ('SharedSync-SYNTHETIC-KEYS','asset-shared-baseline','shared-baseline-master',\
          1700000000);",
    )
    .unwrap();
    let before_keys = keys(&conn);
    let before_six = baseline_six_fields(&conn);
    assert_eq!(before_keys.len(), 2);
    assert_eq!(before_six.len(), 2);
    (before_keys, before_six)
}

async fn open_current(database: &Path) -> Arc<state::SqliteStateDb> {
    let db = Arc::new(state::SqliteStateDb::open(database).await.unwrap());
    {
        let conn = db.acquire_lock("schema proof").unwrap();
        assert_eq!(
            conn.query_row("PRAGMA user_version", [], |row| row.get::<_, i32>(0))
                .unwrap(),
            crate::state::schema::SCHEMA_VERSION
        );
    }
    db
}

#[tokio::test]
async fn selected_shared_cycle_from_genuine_v28_adds_exact_child_keys_without_collapsing_baseline()
{
    let server = wiremock::MockServer::start().await;
    let bytes = include_bytes!("../../../../tests/data/media/pattern.jpg");
    let checksum = base64::engine::general_purpose::STANDARD.encode(Sha256::digest(bytes));
    let expected_hex = hex_sha256(bytes);
    Mock::given(method("GET"))
        .and(path("/shared-collision.jpg"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(bytes.as_slice())
                .insert_header("content-type", "image/jpeg"),
        )
        .expect(2)
        .mount(&server)
        .await;

    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("state.db");
    let media = directory.path().join("media");
    std::fs::create_dir(&media).unwrap();
    let protected = media.join("protected-primary.jpg");
    let protected_sidecar = media.join("protected-primary.xmp");
    std::fs::write(&protected, bytes.as_slice()).unwrap();
    std::fs::write(&protected_sidecar, b"synthetic independent sidecar").unwrap();
    let protected_before = (
        std::fs::read(&protected).unwrap(),
        std::fs::metadata(&protected).unwrap().modified().unwrap(),
        std::fs::read(&protected_sidecar).unwrap(),
        std::fs::metadata(&protected_sidecar)
            .unwrap()
            .modified()
            .unwrap(),
    );

    let (before_keys, before_six) = seed_genuine_v28(&database, &protected);
    let db = open_current(&database).await;
    {
        let conn = db.acquire_lock("pre-dispatch proof").unwrap();
        assert_eq!(
            keys(&conn),
            before_keys,
            "opening schema 28 at current must create no catalogue keys before provider dispatch"
        );
        assert_eq!(
            baseline_six_fields(&conn),
            before_six,
            "schema 28 -> current must preserve the six baseline fields"
        );
        assert_eq!(mappings(&conn).len(), 2);
    }

    let url = format!("{}/shared-collision.jpg", server.uri());
    let mut records = page_records(
        SHARED,
        "shared-collision-master",
        "asset-shared-collision",
        "collision.jpg",
        &url,
        bytes,
        &checksum,
    );
    records.extend(page_records(
        SHARED,
        "shared-distinct-master",
        "asset-shared-distinct",
        "collision.jpg",
        &url,
        bytes,
        &checksum,
    ));

    let config = make_run_cycle_config();
    let (_session_dir, shared_session) = make_shared_session_for_run_cycle().await;
    let mut shared_paths = None;
    let mut files_after_first = None;
    for cycle in 0..2 {
        let inner = open_current(&database).await;
        let store: Arc<dyn download::DownloadStore> = inner.clone();
        let provider = Provider {
            zone: SHARED.to_string(),
            records: records.clone(),
        };
        let mut shared = make_run_cycle_library_state_with_album(
            SHARED,
            SHARED_TOKEN,
            scoped_shared_album(Box::new(provider.clone())),
        );
        shared.library = PhotoLibrary::new(
            "https://synthetic.example/database/1/photos/production/shared".into(),
            Arc::new(std::collections::HashMap::new()),
            Box::new(provider),
            Arc::new(json!({"zoneName":SHARED,"ownerRecordName":"_defaultOwner"})),
            "shared".into(),
            crate::retry::RetryConfig::default(),
        )
        .await
        .unwrap();
        let builder = make_run_cycle_download_config_builder_with_options(
            &media,
            store.clone(),
            RunCycleDownloadConfigOptions {
                #[cfg(feature = "xmp")]
                xmp_sidecar: true,
                ..Default::default()
            },
        );
        let result = crate::sync_cycle::run_cycle(
            &[&shared],
            &config,
            Some(store.as_ref()),
            false,
            &builder,
            download::DownloadControls::download_hidden(),
            &shared_session,
            &CancellationToken::new(),
        )
        .await
        .unwrap();

        assert_eq!(
            result.stats.downloaded,
            if cycle == 0 { 2 } else { 0 },
            "cycle {cycle}: {result:?}"
        );
        let conn = inner.acquire_lock("cycle proof").unwrap();
        assert_eq!(
            existing_baseline_fields(&conn, &before_six),
            before_six,
            "cycle {cycle}: an existing Primary or Shared baseline tuple changed"
        );
        let shared_rows = query(
            &conn,
            "SELECT a.library,a.id,a.version_size,a.status,a.checksum,a.filename,a.size_bytes,\
                    a.source,a.local_path,p.provider_checksum,p.local_checksum,p.source_checksum,\
                    p.local_path \
             FROM assets a JOIN asset_metadata_paths p \
               ON p.library=a.library AND p.id=a.id AND p.version_size=a.version_size \
              AND p.local_path=a.local_path \
             WHERE a.library='SharedSync-SYNTHETIC-KEYS' ORDER BY a.id",
        );
        assert_eq!(
            shared_rows.len(),
            3,
            "cycle {cycle}: selected Shared baseline plus exactly two new child keys"
        );
        assert_eq!(
            &shared_rows[0][..4],
            [
                rusqlite::types::Value::Text(SHARED.into()),
                rusqlite::types::Value::Text("asset-shared-baseline".into()),
                rusqlite::types::Value::Text("original".into()),
                rusqlite::types::Value::Text("downloaded".into()),
            ],
            "cycle {cycle}: existing Shared baseline tuple changed"
        );
        let mut paths = std::collections::BTreeSet::new();
        let mut filenames = std::collections::BTreeSet::new();
        for (row, expected_id) in shared_rows[1..]
            .iter()
            .zip(["asset-shared-collision", "asset-shared-distinct"])
        {
            assert_eq!(
                &row[..4],
                [
                    rusqlite::types::Value::Text(SHARED.into()),
                    rusqlite::types::Value::Text(expected_id.into()),
                    rusqlite::types::Value::Text("original".into()),
                    rusqlite::types::Value::Text("downloaded".into()),
                ],
                "cycle {cycle}: exact selected Shared child identity"
            );
            assert_eq!(
                row[4],
                rusqlite::types::Value::Text(checksum.clone()),
                "{expected_id}: provider source checksum must be the base64 provider checksum"
            );
            let filename = match &row[5] {
                rusqlite::types::Value::Text(filename) => filename.clone(),
                other => panic!("{expected_id}: expected text filename, got {other:?}"),
            };
            let expected_filename = if expected_id == "asset-shared-collision" {
                "collision.JPG"
            } else {
                "collision-asset-shared-distinct.JPG"
            };
            assert_eq!(
                filename, expected_filename,
                "{expected_id}: exact collision filename"
            );
            assert!(
                filenames.insert(filename),
                "{expected_id}: children must not share a local filename"
            );
            assert_eq!(
                row[6],
                rusqlite::types::Value::Integer(bytes.len() as i64),
                "{expected_id}: exact size"
            );
            assert_eq!(
                row[7],
                rusqlite::types::Value::Text("icloud".into()),
                "{expected_id}: source provenance"
            );
            assert_eq!(
                row[9],
                rusqlite::types::Value::Text(checksum.clone()),
                "{expected_id}: receipt provider checksum"
            );
            assert_eq!(
                row[10],
                rusqlite::types::Value::Text(expected_hex.clone()),
                "{expected_id}: exact local HEX SHA-256"
            );
            assert_eq!(
                row[11],
                rusqlite::types::Value::Text(expected_hex.clone()),
                "{expected_id}: exact untouched-source HEX SHA-256"
            );
            assert_eq!(
                row[8], row[12],
                "{expected_id}: receipt path must match catalogue path"
            );
            let path = match &row[8] {
                rusqlite::types::Value::Text(path) => PathBuf::from(path),
                other => panic!("{expected_id}: expected text path, got {other:?}"),
            };
            assert_ne!(
                path, protected,
                "{expected_id}: shared child adopted the protected original"
            );
            assert!(
                paths.insert(path.clone()),
                "{expected_id}: child aliases {path:?}"
            );
            assert_eq!(std::fs::read(&path).unwrap(), bytes, "{expected_id}");
        }
        assert_eq!(
            keys(&conn).len(),
            before_keys.len() + 2,
            "cycle {cycle}: no shared keys outside the two exact children"
        );
        if cycle == 0 {
            shared_paths = Some(paths);
            files_after_first = Some(media_files(&media));
        } else {
            assert_eq!(
                Some(paths),
                shared_paths,
                "reopened cycle moved shared paths"
            );
            assert_eq!(
                media_files(&media),
                files_after_first.clone().unwrap(),
                "reopened cycle wrote extra files"
            );
        }
    }
    server.verify().await;

    assert_eq!(
        (
            std::fs::read(&protected).unwrap(),
            std::fs::metadata(&protected).unwrap().modified().unwrap(),
            std::fs::read(&protected_sidecar).unwrap(),
            std::fs::metadata(&protected_sidecar)
                .unwrap()
                .modified()
                .unwrap(),
        ),
        protected_before,
        "protected Primary original or sidecar changed"
    );

    let conn = Connection::open(&database).unwrap();
    assert_eq!(
        keys(&conn),
        vec![
            vec![
                rusqlite::types::Value::Text(PRIMARY.into()),
                rusqlite::types::Value::Text("asset-shared-collision".into()),
                rusqlite::types::Value::Text("original".into()),
            ],
            vec![
                rusqlite::types::Value::Text(SHARED.into()),
                rusqlite::types::Value::Text("asset-shared-baseline".into()),
                rusqlite::types::Value::Text("original".into()),
            ],
            vec![
                rusqlite::types::Value::Text(SHARED.into()),
                rusqlite::types::Value::Text("asset-shared-collision".into()),
                rusqlite::types::Value::Text("original".into()),
            ],
            vec![
                rusqlite::types::Value::Text(SHARED.into()),
                rusqlite::types::Value::Text("asset-shared-distinct".into()),
                rusqlite::types::Value::Text("original".into()),
            ],
        ],
        "same child id and identical bytes/name/size must remain scoped keys"
    );
    assert_eq!(
        mappings(&conn),
        vec![
            (
                PRIMARY.to_string(),
                "asset-shared-collision".to_string(),
                "primary-master".to_string()
            ),
            (
                SHARED.to_string(),
                "asset-shared-baseline".to_string(),
                "shared-baseline-master".to_string()
            ),
            (
                SHARED.to_string(),
                "asset-shared-collision".to_string(),
                "shared-collision-master".to_string()
            ),
            (
                SHARED.to_string(),
                "asset-shared-distinct".to_string(),
                "shared-distinct-master".to_string()
            ),
        ],
        "shared children must not acquire the baseline owner mapping"
    );
}

// Unselected Shared targets remain covered by
// sparse_recovery::sparse_target_selected_materializes_and_reopened_cycle_is_quiet,
// which uses a real Primary sparse child and asserts no target lookup or key materialization.

#[tokio::test]
async fn existing_exact_selected_shared_child_is_not_redownloaded() {
    let server = wiremock::MockServer::start().await;
    let bytes = include_bytes!("../../../../tests/data/media/pattern.jpg");
    let checksum = base64::engine::general_purpose::STANDARD.encode(Sha256::digest(bytes));
    let expected_hex = hex_sha256(bytes);
    Mock::given(method("GET"))
        .and(path("/existing.jpg"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(bytes.as_slice())
                .insert_header("content-type", "image/jpeg"),
        )
        .expect(0)
        .mount(&server)
        .await;

    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("state.db");
    let media = directory.path().join("media");
    std::fs::create_dir(&media).unwrap();
    let protected = media.join("protected-primary.jpg");
    std::fs::write(&protected, b"synthetic primary original").unwrap();
    let _seed_baseline = seed_genuine_v28(&database, &protected);
    let existing = media
        .join(run_cycle_expected_date_dir())
        .join("collision.JPG");
    std::fs::create_dir_all(existing.parent().unwrap()).unwrap();
    std::fs::write(&existing, bytes.as_slice()).unwrap();
    let existing_before = (
        std::fs::read(&existing).unwrap(),
        std::fs::metadata(&existing).unwrap().modified().unwrap(),
    );
    let (before_keys, before_six) = {
        let conn = Connection::open(&database).unwrap();
        conn.execute(
            "INSERT INTO assets(library,id,version_size,checksum,filename,created_at,\
                 added_at,size_bytes,media_type,status,downloaded_at,local_path,last_seen_at,\
                 source,local_checksum) \
             VALUES(?1,'asset-shared-existing','original',?2,'collision.jpg',1700000000,\
                 1700000000,?3,'photo','downloaded',1700000001,?4,1700000002,'icloud',?5)",
            rusqlite::params![
                SHARED,
                checksum,
                bytes.len() as i64,
                existing.to_string_lossy(),
                expected_hex
            ],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO asset_metadata_paths(library,id,version_size,local_path,\
                 provider_checksum,local_checksum,download_checksum,source_checksum) \
             VALUES(?1,'asset-shared-existing','original',?2,?3,?4,?4,?4)",
            rusqlite::params![SHARED, existing.to_string_lossy(), checksum, expected_hex],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO asset_master_mappings VALUES(?1,'asset-shared-existing',\
                 'shared-existing-master',1700000000)",
            rusqlite::params![SHARED],
        )
        .unwrap();
        (keys(&conn), baseline_six_fields(&conn))
    };
    let db = open_current(&database).await;
    {
        let conn = db.acquire_lock("existing exact child proof").unwrap();
        assert_eq!(
            keys(&conn),
            before_keys,
            "opening schema 28 at current must not alter existing keys"
        );
        assert_eq!(baseline_six_fields(&conn), before_six);
        let row = query(
            &conn,
            "SELECT a.library,a.id,a.version_size,a.status,a.checksum,a.local_path,\
                    p.provider_checksum,p.local_checksum,p.download_checksum,p.source_checksum,\
                    p.local_path \
             FROM assets a JOIN asset_metadata_paths p USING(library,id,version_size,local_path) \
             WHERE a.library='SharedSync-SYNTHETIC-KEYS' AND a.id='asset-shared-existing'",
        );
        assert_eq!(row.len(), 1);
        assert_eq!(
            row[0],
            vec![
                rusqlite::types::Value::Text(SHARED.into()),
                rusqlite::types::Value::Text("asset-shared-existing".into()),
                rusqlite::types::Value::Text("original".into()),
                rusqlite::types::Value::Text("downloaded".into()),
                rusqlite::types::Value::Text(checksum.clone()),
                rusqlite::types::Value::Text(existing.to_string_lossy().into_owned()),
                rusqlite::types::Value::Text(checksum.clone()),
                rusqlite::types::Value::Text(expected_hex.clone()),
                rusqlite::types::Value::Text(expected_hex.clone()),
                rusqlite::types::Value::Text(expected_hex.clone()),
                rusqlite::types::Value::Text(existing.to_string_lossy().into_owned()),
            ],
            "existing exact child receipt seed must survive migration"
        );
    }

    let url = format!("{}/existing.jpg", server.uri());
    let records = page_records(
        SHARED,
        "shared-existing-master",
        "asset-shared-existing",
        "collision.jpg",
        &url,
        bytes,
        &checksum,
    );
    let config = make_run_cycle_config();
    let (_session_dir, shared_session) = make_shared_session_for_run_cycle().await;
    for cycle in 0..2 {
        let inner = open_current(&database).await;
        let store: Arc<dyn download::DownloadStore> = inner.clone();
        let provider = Provider {
            zone: SHARED.to_string(),
            records: records.clone(),
        };
        let mut shared = make_run_cycle_library_state_with_album(
            SHARED,
            SHARED_TOKEN,
            scoped_shared_album(Box::new(provider.clone())),
        );
        shared.library = PhotoLibrary::new(
            "https://synthetic.example/database/1/photos/production/shared".into(),
            Arc::new(std::collections::HashMap::new()),
            Box::new(provider),
            Arc::new(json!({"zoneName":SHARED,"ownerRecordName":"_defaultOwner"})),
            "shared".into(),
            crate::retry::RetryConfig::default(),
        )
        .await
        .unwrap();
        let builder = make_run_cycle_download_config_builder(&media, store.clone());
        let result = crate::sync_cycle::run_cycle(
            &[&shared],
            &config,
            Some(store.as_ref()),
            false,
            &builder,
            download::DownloadControls::download_hidden(),
            &shared_session,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(
            result.stats.downloaded, 0,
            "cycle {cycle}: already downloaded exact child must not download again: {result:?}"
        );
        let conn = inner.acquire_lock("existing exact child cycle").unwrap();
        assert_eq!(
            keys(&conn),
            before_keys,
            "cycle {cycle}: exact existing child must not create extra keys"
        );
        assert_eq!(
            existing_baseline_fields(&conn, &before_six),
            before_six,
            "cycle {cycle}: existing tuple fields changed"
        );
        let row = query(
            &conn,
            "SELECT a.status,a.checksum,p.provider_checksum,p.local_checksum,p.source_checksum \
             FROM assets a JOIN asset_metadata_paths p USING(library,id,version_size,local_path) \
             WHERE a.library='SharedSync-SYNTHETIC-KEYS' AND a.id='asset-shared-existing'",
        );
        assert_eq!(
            row,
            vec![vec![
                rusqlite::types::Value::Text("downloaded".into()),
                rusqlite::types::Value::Text(checksum.clone()),
                rusqlite::types::Value::Text(checksum.clone()),
                rusqlite::types::Value::Text(expected_hex.clone()),
                rusqlite::types::Value::Text(expected_hex.clone()),
            ]],
            "cycle {cycle}: existing exact child receipt changed"
        );
        assert_eq!(
            (
                std::fs::read(&existing).unwrap(),
                std::fs::metadata(&existing).unwrap().modified().unwrap(),
            ),
            existing_before,
            "cycle {cycle}: existing selected child bytes or mtime changed"
        );
    }
    server.verify().await;
    let conn = Connection::open(&database).unwrap();
    assert_eq!(keys(&conn), before_keys);
    assert_eq!(baseline_six_fields(&conn), before_six);
}

#[tokio::test]
async fn distinct_provider_resources_with_colliding_names_publish_independent_files() {
    Box::pin(super::distinct_resource_proof::observe_before_receipt_timing(false)).await;
    use crate::commands::{AlbumPass, PassKind};
    use crate::sync_loop::test_support::make_run_cycle_library_state_with_passes;

    let server = wiremock::MockServer::start().await;
    let first = include_bytes!("../../../../tests/data/media/pattern.jpg").as_slice();
    let second = include_bytes!("../../../../tests/data/media/metadata.jpg").as_slice();
    let hashes = [
        "1f7b7dd82b9a019b98add756d74e4cfdfb952bb8a1a010869c80de0fd8e5c215",
        "37fc601b7ed94c703635d331cac5705d610ccbb01321935f20577458a141f6b1",
    ];
    let payloads = [first, second];
    // Independently calculated MMCS v1 fileChecksum values for these exact bytes.
    let checksums = [
        "ATLF2mUdMsryhdyIJgwrpiBlt1z2".to_string(),
        "AV/Hz7fW6Rc+X19s6b+avvkD/0Sf".to_string(),
    ];
    for (bytes, expected) in payloads.iter().zip(&checksums) {
        let mut signature = sha1::Sha1::new();
        signature.update(b"com.apple.XattrObjectSalt\0com.apple.DataObjectSalt\0");
        signature.update(bytes);
        let actual = base64::engine::general_purpose::STANDARD
            .encode([vec![1], signature.finalize().to_vec()].concat());
        assert_eq!(&actual, expected);
    }
    assert_ne!(first, second);
    for (index, endpoint) in ["/distinct-first.jpg", "/distinct-second.jpg"]
        .iter()
        .enumerate()
    {
        assert_eq!(hex_sha256(payloads[index]), hashes[index]);
        Mock::given(method("GET"))
            .and(path(*endpoint))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_bytes(payloads[index])
                    .insert_header("content-type", "image/jpeg"),
            )
            .expect(1)
            .mount(&server)
            .await;
    }
    let directory = tempfile::tempdir_in(std::env::temp_dir()).unwrap().keep();
    eprintln!(
        "distinct-resource synthetic evidence: {}",
        directory.display()
    );
    let database = directory.join("state.db");
    let media = directory.join("media");
    std::fs::create_dir(&media).unwrap();
    let first_path = media
        .join(run_cycle_expected_date_dir())
        .join("collision.JPG");
    let second_path = media
        .join(run_cycle_expected_date_dir())
        .join(format!("collision-{}.JPG", second.len()));
    let _positive_timing =
        crate::download::planner::distinct_timing_test_hook::Control::install_positive(
            first_path,
            second_path,
        );
    let sentinel = media.join("independent.bin");
    std::fs::write(&sentinel, b"unrelated preserved bytes").unwrap();
    let sentinel_mtime = std::fs::metadata(&sentinel).unwrap().modified().unwrap();
    let mut records = page_records(
        SHARED,
        "distinct-master-a",
        "asset-distinct-a",
        "collision.jpg",
        &format!("{}/distinct-first.jpg", server.uri()),
        first,
        &checksums[0],
    );
    records.extend(page_records(
        SHARED,
        "distinct-master-b",
        "asset-distinct-b",
        "collision.jpg",
        &format!("{}/distinct-second.jpg", server.uri()),
        second,
        &checksums[1],
    ));
    let owner = crate::state::db::account::AccountOwner::authenticated(
        "distinct@example.invalid",
        "com",
        &serde_json::from_value(json!({"dsInfo":{"dsid":"synthetic-distinct"}})).unwrap(),
    )
    .unwrap();
    let (_session_dir, shared_session) = make_shared_session_for_run_cycle().await;
    let mut prior = None;
    for cycle in 0..3 {
        let db = Arc::new(
            state::SqliteStateDb::open_owned(&database, &owner)
                .await
                .unwrap(),
        );
        let provider = Provider {
            zone: SHARED.to_string(),
            records: records.clone(),
        };
        let mut library = PhotoLibrary::new(
            "https://synthetic.example/database/1/photos/production/shared".into(),
            Arc::new(std::collections::HashMap::new()),
            Box::new(provider),
            Arc::new(json!({"zoneName":SHARED,"ownerRecordName":"_defaultOwner"})),
            "shared".into(),
            crate::retry::RetryConfig::default(),
        )
        .await
        .unwrap();
        library.shadow_capture = Some(crate::icloud::photos::inbox::ShadowCapture::new(
            db.clone(),
            owner.clone(),
            "com",
        ));
        let passes = vec![AlbumPass {
            kind: PassKind::Unfiled,
            album: library.all(),
            exclude_ids: Arc::new(rustc_hash::FxHashSet::default()),
        }];
        let mut selected = make_run_cycle_library_state_with_passes(SHARED, SHARED_TOKEN, passes);
        selected.library = library;
        let builder = make_run_cycle_download_config_builder(&media, db.clone());
        let result = crate::sync_cycle::run_cycle(
            &[&selected],
            &make_run_cycle_config(),
            Some(db.as_ref()),
            false,
            &builder,
            download::DownloadControls::download_hidden(),
            &shared_session,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(result.failed_count, 0, "cycle {cycle}: {result:?}");
        assert_eq!(
            result.stats.skipped.uncertain, 0,
            "cycle {cycle}: {result:?}"
        );
        assert_eq!(
            result.stats.downloaded,
            if cycle == 0 { 2 } else { 0 },
            "cycle {cycle}: {result:?}"
        );
        let conn = db.acquire_lock("distinct resources proof").unwrap();
        let owner_keys = owner.authenticated_keys().unwrap();
        let receipt_owners: Vec<(String, String)> = conn
            .prepare("SELECT account_key,provider_key FROM download_receipts ORDER BY child")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(
            receipt_owners,
            vec![(owner_keys.0.to_string(), owner_keys.1.to_string()); 2]
        );
        let rows = query(
            &conn,
            "SELECT id,version_size,status,checksum,created_at,added_at,local_path FROM assets ORDER BY id,version_size",
        );
        assert_eq!(rows.len(), 2, "exact current asset count");
        assert_eq!(
            mappings(&conn),
            vec![
                (
                    SHARED.into(),
                    "asset-distinct-a".into(),
                    "distinct-master-a".into()
                ),
                (
                    SHARED.into(),
                    "asset-distinct-b".into(),
                    "distinct-master-b".into()
                )
            ]
        );
        let mut files = vec![sentinel.clone()];
        let mut snapshot = Vec::new();
        for (i, (id, master)) in [
            ("asset-distinct-a", "distinct-master-a"),
            ("asset-distinct-b", "distinct-master-b"),
        ]
        .iter()
        .enumerate()
        {
            let row = &rows[i];
            assert_eq!(
                &row[..6],
                &[
                    rusqlite::types::Value::Text((*id).into()),
                    rusqlite::types::Value::Text("original".into()),
                    rusqlite::types::Value::Text("downloaded".into()),
                    rusqlite::types::Value::Text(checksums[i].clone()),
                    rusqlite::types::Value::Integer(1_700_000_000),
                    rusqlite::types::Value::Integer(1_700_000_000)
                ]
            );
            let file = match &row[6] {
                rusqlite::types::Value::Text(s) => PathBuf::from(s),
                value => panic!("invalid path: {value:?}"),
            };
            let expected_name = if i == 0 {
                "collision.JPG".to_string()
            } else {
                format!("collision-{}.JPG", second.len())
            };
            assert_eq!(
                file,
                media
                    .join(run_cycle_expected_date_dir())
                    .join(expected_name)
            );
            let bytes = std::fs::read(&file).unwrap();
            assert_eq!(bytes, payloads[i]);
            assert_eq!(hex_sha256(&bytes), hashes[i]);
            let path_text = file.to_string_lossy().into_owned();
            let paths: Vec<(String, String, String, String)> = conn.prepare("SELECT local_path,provider_checksum,local_checksum,source_checksum FROM asset_metadata_paths WHERE library=?1 AND id=?2 AND version_size='original'").unwrap().query_map(rusqlite::params![SHARED, id], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).unwrap().collect::<Result<_,_>>().unwrap();
            assert_eq!(
                paths,
                vec![(
                    path_text,
                    checksums[i].clone(),
                    hashes[i].into(),
                    hashes[i].into()
                )]
            );
            type ReceiptRow = (
                String,
                String,
                String,
                String,
                String,
                i64,
                String,
                String,
                String,
            );
            let receipts: Vec<ReceiptRow> = conn.prepare("SELECT scope,child,master,state_id,provider_checksum,provider_size,native_path,source_checksum,local_checksum FROM download_receipts WHERE library=?1 AND child=?2 AND version_size='original'").unwrap().query_map(rusqlite::params![SHARED,id], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?,r.get(6)?,r.get(7)?,r.get(8)?))).unwrap().collect::<Result<_,_>>().unwrap();
            assert_eq!(receipts.len(), 1, "exact receipt for {id}");
            let receipt = &receipts[0];
            let scope: Value = serde_json::from_str(&receipt.0).unwrap();
            assert_eq!(scope["zone"]["zoneName"], SHARED);
            assert_eq!(scope["database"], "shared");
            assert_eq!(
                (
                    &receipt.1, &receipt.2, &receipt.3, &receipt.4, receipt.5, &receipt.7,
                    &receipt.8
                ),
                (
                    &id.to_string(),
                    &master.to_string(),
                    &id.to_string(),
                    &checksums[i],
                    payloads[i].len() as i64,
                    &hashes[i].to_string(),
                    &hashes[i].to_string()
                )
            );
            let native: Value = serde_json::from_str(&receipt.6).unwrap();
            assert_eq!(native["path"], file.to_string_lossy().as_ref());
            files.push(file.clone());
            snapshot.push((
                file.clone(),
                bytes,
                std::fs::metadata(file).unwrap().modified().unwrap(),
            ));
        }
        assert_ne!(snapshot[0].0, snapshot[1].0);
        assert_eq!(query(&conn, "SELECT id FROM download_receipts").len(), 2);
        assert_eq!(query(&conn, "SELECT id FROM asset_metadata_paths").len(), 2);
        files.sort();
        assert_eq!(media_files(&media), files, "exact file set");
        assert_eq!(
            std::fs::read(&sentinel).unwrap(),
            b"unrelated preserved bytes"
        );
        assert_eq!(
            std::fs::metadata(&sentinel).unwrap().modified().unwrap(),
            sentinel_mtime
        );
        if let Some(previous) = &prior {
            assert_eq!(&snapshot, previous, "reopened files changed");
        }
        prior = Some(snapshot);
    }
    server.verify().await;
}
