//! C11 prerequisite: one released pending original under a distinct
//! Primary/Shared owner shape.
//!
//! Synthetic released representation, not official output. The test starts
//! from schema 36 rows, performs authenticated adoption/owned migration, uses
//! the real collection/pass resolver and normal run_cycle, and requires the
//! recorded original destination to be recovered exactly once. If production
//! cannot supply that authority, the test must fail rather than seed current
//! admission.

use super::{
    BaselineSession, MEDIA, OLD_TOKEN, ResetCycleSession, ZONE, baseline_owner, pair, zone,
};
use crate::commands::{CollectionContext, PassScope, resolve_passes_for_scope};
use crate::download;
use crate::icloud::photos::{PhotoLibrary, PhotosSession};
use crate::selection::Selection;
use crate::state::SqliteStateDb;
use crate::state::db::account::adopt_legacy;
use crate::sync_cycle::{run_cycle, sync_token_key};
use crate::sync_loop::test_support::{
    album_count_response, full_album_page_with_download, make_run_cycle_config,
    make_run_cycle_download_config_builder, make_run_cycle_library_state_with_passes,
    make_shared_session_for_run_cycle,
};
use base64::Engine as _;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{method, path};
use wiremock::{Mock, ResponseTemplate};

const SHARED: &str = "SharedSync-c11-history";
const SHARED_PATH: &str = "shared-history.JPG";
const SHARED_BYTES: &[u8] = b"shared distinct-owner history bytes";
const SHARED_NEW_BYTES: &[u8] = MEDIA;
const PRIMARY: &str = "PrimarySync";

fn hex(bytes: &[u8]) -> String {
    data_encoding::HEXLOWER.encode(&Sha256::digest(bytes))
}

fn provider_checksum(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(Sha256::digest(bytes))
}

fn sparse_rows(conn: &rusqlite::Connection) -> Vec<Vec<rusqlite::types::Value>> {
    let mut statement = conn
        .prepare(
            "SELECT library,source_record_name,original_evidence,observed_evidence,\
                    generation,first_seen_at,last_outcome \
             FROM unresolved_sparse_identities ORDER BY library,source_record_name",
        )
        .unwrap();
    let columns = statement.column_count();
    statement
        .query_map([], |row| {
            (0..columns)
                .map(|index| row.get::<_, rusqlite::types::Value>(index))
                .collect::<rusqlite::Result<Vec<_>>>()
        })
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap()
}

fn shared_rows(conn: &rusqlite::Connection) -> Vec<Vec<rusqlite::types::Value>> {
    let mut rows = Vec::new();
    for query in [
        "SELECT library,id,version_size,checksum,filename,created_at,added_at,size_bytes,\
                media_type,status,downloaded_at,local_path,last_seen_at,local_checksum,\
                download_checksum,source,is_deleted,deleted_at \
         FROM assets WHERE library=?1 ORDER BY id,version_size",
        "SELECT library,id,version_size,local_path,provider_checksum,local_checksum,\
                download_checksum,source_checksum \
         FROM asset_metadata_paths WHERE library=?1 ORDER BY id,version_size,local_path",
        "SELECT library,master_record_name,asset_record_name,claimed_at \
         FROM legacy_master_state_owners WHERE library=?1 ORDER BY master_record_name",
    ] {
        let mut statement = conn.prepare(query).unwrap();
        let columns = statement.column_count();
        rows.extend(
            statement
                .query_map([SHARED], |row| {
                    (0..columns)
                        .map(|index| row.get::<_, rusqlite::types::Value>(index))
                        .collect::<rusqlite::Result<Vec<_>>>()
                })
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap(),
        );
    }
    rows
}

#[derive(Clone)]
struct SharedOwnerProbe {
    probes: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl PhotosSession for SharedOwnerProbe {
    async fn post(
        &self,
        _url: &str,
        body: String,
        _headers: &[(&str, &str)],
    ) -> anyhow::Result<Value> {
        let request: Value = serde_json::from_str(&body)?;
        assert_eq!(
            request["zoneID"]["ownerRecordName"], "c11-shared-owner",
            "Shared probe must authenticate its distinct owner"
        );
        self.probes.fetch_add(1, Ordering::SeqCst);
        Ok(json!({"records":[{"fields":{"state":{"value":"FINISHED"}}}]}))
    }
    fn clone_box(&self) -> Box<dyn PhotosSession> {
        Box::new(self.clone())
    }
}

#[derive(Clone)]
struct SharedProgressSession {
    inventory: Arc<Vec<Value>>,
    queries: Arc<AtomicUsize>,
    full_queries: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl PhotosSession for SharedProgressSession {
    async fn post(
        &self,
        url: &str,
        body: String,
        _headers: &[(&str, &str)],
    ) -> anyhow::Result<Value> {
        self.respond(url, &body).await
    }
    fn clone_box(&self) -> Box<dyn PhotosSession> {
        Box::new(self.clone())
    }
}

impl SharedProgressSession {
    async fn respond(&self, url: &str, body: &str) -> anyhow::Result<Value> {
        let request: Value = serde_json::from_str(body)?;
        if url.ends_with("/zones/list") {
            return Ok(json!({"zones":[{"zoneID":zone_for(SHARED),
                "syncToken":"c11-shared-boundary","deleted":false}]}));
        }
        if url.contains("/records/query?") {
            assert_eq!(
                request["zoneID"]["ownerRecordName"], "c11-shared-owner",
                "Shared current progress must use its own authenticated owner"
            );
        }
        if request["query"]["recordType"] == "CheckIndexingState" {
            assert_eq!(
                request["zoneID"]["ownerRecordName"], "c11-shared-owner",
                "Shared readiness must use its own authenticated owner"
            );
            return Ok(json!({"records":[{"fields":{"state":{"value":"FINISHED"}}}]}));
        }
        if url.contains("/internal/records/query/batch") {
            return Ok(album_count_response(1));
        }
        if url.contains("/changes/zone?") {
            assert_eq!(
                request["zones"][0]["zoneID"]["ownerRecordName"], "c11-shared-owner",
                "Shared delta must use its own authenticated owner"
            );
            return Ok(json!({"zones":[{"zoneID":zone_for(SHARED),"records":[],
                "syncToken":"shared-progress-after","moreComing":false}]}));
        }
        if url.contains("/records/lookup?") {
            assert_eq!(
                request["zoneID"]["ownerRecordName"], "c11-shared-owner",
                "Shared lookup must use its own authenticated owner"
            );
            assert_eq!(
                request["zoneID"]["zoneName"], SHARED,
                "Shared lookup must use its own zone"
            );
            let requested: Vec<&str> = request["records"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|record| record["recordName"].as_str())
                .collect();
            let mut records = Vec::with_capacity(requested.len());
            for name in requested {
                let matches: Vec<&Value> = self
                    .inventory
                    .iter()
                    .filter(|record| record["recordName"].as_str() == Some(name))
                    .collect();
                assert_eq!(matches.len(), 1, "lookup must resolve exactly one {name}");
                records.push(matches[0].clone());
            }
            return Ok(json!({"records": records}));
        }
        assert!(url.contains("/records/query?"), "{url}");
        self.queries.fetch_add(1, Ordering::SeqCst);
        let offset = request["query"]["filterBy"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|filter| filter["fieldName"] == "startRank")
            .and_then(|filter| filter["fieldValue"]["value"].as_u64())
            .unwrap_or(0);
        let records = if offset == 0 {
            self.full_queries.fetch_add(1, Ordering::SeqCst);
            self.inventory.as_ref().clone()
        } else {
            Vec::new()
        };
        Ok(json!({
            "records": records,
            "syncToken": "shared-progress-page"
        }))
    }
}

fn zone_for(zone_name: &str) -> Value {
    json!({"zoneName":zone_name,"ownerRecordName":"c11-shared-owner"})
}

fn shared_pair(id: &str, filename: &str, url: &str, bytes: &[u8]) -> Vec<Value> {
    let mut records = full_album_page_with_download(
        SHARED,
        id,
        "unused",
        url,
        bytes.len() as u64,
        &provider_checksum(bytes),
    )["records"]
        .as_array()
        .unwrap()
        .clone();
    records[0]["fields"]["filenameEnc"] = json!({"value":filename,"type":"STRING"});
    records[1]["recordName"] = json!(format!("asset-{id}"));
    records[1]["fields"]["masterRef"]["value"]["zoneID"] = zone_for(SHARED);
    records
}

async fn run_c11_quiet(
    db: Arc<SqliteStateDb>,
    root: &std::path::Path,
    shared: &crate::auth::SharedSession,
    boundary: &'static str,
) -> (crate::sync_cycle::CycleResult, usize) {
    run_c11_quiet_in_zone(db, root, shared, boundary, ZONE).await
}

async fn run_c11_quiet_in_zone(
    db: Arc<SqliteStateDb>,
    root: &std::path::Path,
    shared: &crate::auth::SharedSession,
    boundary: &'static str,
    zone_name: &'static str,
) -> (crate::sync_cycle::CycleResult, usize) {
    let session = ResetCycleSession {
        zone: zone_name,
        inventory: Arc::new(Vec::new()),
        full_queries: Arc::new(AtomicUsize::new(0)),
        changes: Arc::new(AtomicUsize::new(0)),
        fail_full: false,
        auth_expired: false,
        boundary,
    };
    let full = Arc::clone(&session.full_queries);
    let mut library = PhotoLibrary::new(
        "https://synthetic.example/database/1/photos/production/private".into(),
        Arc::new(std::collections::HashMap::new()),
        Box::new(session),
        Arc::new(json!({"zoneName":zone_name,"ownerRecordName":"_defaultOwner"})),
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
    let mut state = make_run_cycle_library_state_with_passes(
        zone_name,
        &sync_token_key(zone_name),
        vec![crate::commands::AlbumPass {
            kind: crate::commands::PassKind::Unfiled,
            album: library.all(),
            exclude_ids: Arc::new(rustc_hash::FxHashSet::default()),
        }],
    );
    state.library = library;
    let base = make_run_cycle_download_config_builder(root, db.clone());
    let builder = |mode, exclude, groups, zone| {
        let mut config = (*base(mode, exclude, groups, zone)).clone();
        config.folder_structure = "none".into();
        Arc::new(config)
    };
    let result = run_cycle(
        &[&state],
        &make_run_cycle_config(),
        Some(db.as_ref()),
        false,
        &builder,
        download::DownloadControls::download_hidden(),
        shared,
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    (result, full.load(Ordering::SeqCst))
}

async fn run_shared_c11_quiet(
    db: Arc<SqliteStateDb>,
    root: &std::path::Path,
    shared: &crate::auth::SharedSession,
    session: SharedProgressSession,
    album: crate::icloud::photos::PhotoAlbum,
) -> (crate::sync_cycle::CycleResult, usize) {
    let full = Arc::clone(&session.full_queries);
    let capture =
        crate::icloud::photos::inbox::ShadowCapture::new(db.clone(), baseline_owner(), "com");
    let mut album = album;
    album.set_shadow_capture(capture.clone(), "shared".into());
    let mut library = PhotoLibrary::new(
        "https://synthetic.example/database/1/photos/production/shared".into(),
        Arc::new(std::collections::HashMap::new()),
        Box::new(session),
        Arc::new(zone_for(SHARED)),
        "shared".into(),
        crate::retry::RetryConfig::default(),
    )
    .await
    .unwrap();
    library.shadow_capture = Some(capture);
    let mut state = make_run_cycle_library_state_with_passes(
        SHARED,
        &sync_token_key(SHARED),
        vec![crate::commands::AlbumPass {
            kind: crate::commands::PassKind::Unfiled,
            album,
            exclude_ids: Arc::new(rustc_hash::FxHashSet::default()),
        }],
    );
    state.library = library;
    let base = make_run_cycle_download_config_builder(root, db.clone());
    let builder = |mode, exclude, groups, zone| {
        let mut config = (*base(mode, exclude, groups, zone)).clone();
        config.folder_structure = "none".into();
        Arc::new(config)
    };
    let result = run_cycle(
        &[&state],
        &make_run_cycle_config(),
        Some(db.as_ref()),
        false,
        &builder,
        download::DownloadControls::download_hidden(),
        shared,
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    (result, full.load(Ordering::SeqCst))
}

struct MissingSeed {
    _dir: tempfile::TempDir,
    root: std::path::PathBuf,
    recorded: std::path::PathBuf,
    shared_path: std::path::PathBuf,
    db_path: std::path::PathBuf,
    checksum: String,
    local: String,
    shared_mtime: std::time::SystemTime,
    sparse_before: Vec<Vec<rusqlite::types::Value>>,
    shared_before: Vec<Vec<rusqlite::types::Value>>,
}

async fn seed_missing_original() -> MissingSeed {
    seed_missing_originals(1, ZONE).await
}

fn original_identity(index: usize) -> (String, String, String) {
    if index == 0 {
        (
            "original-master".into(),
            "asset-original-child".into(),
            "original.JPG".into(),
        )
    } else {
        (
            format!("original-master-{index}"),
            format!("asset-original-child-{index}"),
            format!("original-{index}.JPG"),
        )
    }
}

async fn seed_missing_originals(count: usize, zone_name: &str) -> MissingSeed {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("photos");
    std::fs::create_dir_all(&root).unwrap();
    let recorded = root.join("original.JPG");
    let shared_path = root.join(SHARED_PATH);
    std::fs::write(&shared_path, SHARED_BYTES).unwrap();
    let shared_mtime = std::fs::metadata(&shared_path).unwrap().modified().unwrap();
    let source = dir.path().join("released.db");
    let db_path = dir.path().join("state.db");
    let checksum = provider_checksum(MEDIA);
    let local = hex(MEDIA);
    {
        let conn = rusqlite::Connection::open(&source).unwrap();
        crate::state::schema::migrate_genuine_v36(&conn).unwrap();
        for index in 0..count {
            let (master, child, filename) = original_identity(index);
            let path = root.join(&filename);
            conn.execute(
                "INSERT INTO assets(library,id,version_size,checksum,filename,created_at,added_at,\
             size_bytes,media_type,status,downloaded_at,local_path,last_seen_at,\
             local_checksum,download_checksum,download_attempts,last_error,source) \
             VALUES(?1,?2,'original',?3,?4,1700000000,1700000002,\
             ?5,'photo','pending',1700000003,?6,1700000004,?7,?7,1,\
             'interrupted publication','icloud')",
                rusqlite::params![
                    zone_name,
                    master,
                    checksum,
                    filename,
                    MEDIA.len() as i64,
                    path.to_str().unwrap(),
                    local
                ],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO asset_metadata_paths(library,id,version_size,local_path,\
             provider_checksum,local_checksum,download_checksum,source_checksum) \
             VALUES(?1,?2,'original',?3,?4,?5,?5,?5)",
                rusqlite::params![zone_name, master, path.to_str().unwrap(), checksum, local],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO asset_master_mappings VALUES (?1,?2,?3,1700000000)",
                rusqlite::params![zone_name, child, master],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO legacy_master_state_owners VALUES (?1,?2,?3,1700000000)",
                rusqlite::params![zone_name, master, child],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO asset_metadata_capture_revisions VALUES (?1,?2,1,1700000000)",
                rusqlite::params![zone_name, master],
            )
            .unwrap();
        }
        conn.execute(
            "INSERT INTO assets(library,id,version_size,checksum,filename,created_at,added_at,\
             size_bytes,media_type,status,downloaded_at,local_path,last_seen_at,\
             local_checksum,download_checksum,source) \
             VALUES(?1,'shared-old-master','original',?2,'shared-history.JPG',1700000000,1700000002,\
             ?3,'photo','downloaded',1700000003,?4,1700000004,?5,?5,'icloud')",
            rusqlite::params![
                SHARED,
                provider_checksum(SHARED_BYTES),
                SHARED_BYTES.len() as i64,
                shared_path.to_str().unwrap(),
                hex(SHARED_BYTES)
            ],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO asset_metadata_paths(library,id,version_size,local_path,\
             provider_checksum,local_checksum,download_checksum,source_checksum) \
             VALUES(?1,'shared-old-master','original',?2,?3,?4,?4,?4)",
            rusqlite::params![
                SHARED,
                shared_path.to_str().unwrap(),
                provider_checksum(SHARED_BYTES),
                hex(SHARED_BYTES)
            ],
        )
        .unwrap();
        conn.execute_batch(&format!(
            "INSERT INTO legacy_master_state_owners VALUES ('{SHARED}','shared-old-master','asset-shared-old-master',1700000000);
             INSERT INTO asset_master_mappings VALUES ('{SHARED}','asset-shared-old-master','shared-old-master',1700000000);"
        ))
        .unwrap();
        if count > 1 {
            let same_path = root.join("shared-same.JPG");
            std::fs::write(&same_path, MEDIA).unwrap();
            conn.execute(
                "INSERT INTO assets(library,id,version_size,checksum,filename,created_at,added_at,\
                 size_bytes,media_type,status,downloaded_at,local_path,last_seen_at,\
                 local_checksum,download_checksum,source) VALUES(?1,'shared-same-master',\
                 'original',?2,'shared-same.JPG',1700000000,1700000002,?3,'photo',\
                 'downloaded',1700000003,?4,1700000004,?5,?5,'icloud')",
                rusqlite::params![
                    SHARED,
                    checksum,
                    MEDIA.len() as i64,
                    same_path.to_str().unwrap(),
                    local
                ],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO asset_metadata_paths(library,id,version_size,local_path,\
                 provider_checksum,local_checksum,download_checksum,source_checksum) \
                 VALUES(?1,'shared-same-master','original',?2,?3,?4,?4,?4)",
                rusqlite::params![SHARED, same_path.to_str().unwrap(), checksum, local],
            )
            .unwrap();
        }
        for index in 0..100 {
            conn.execute(
                "INSERT INTO unresolved_sparse_identities(library,source_record_name,\
                 original_evidence,observed_evidence,generation,first_seen_at,last_outcome) \
                 VALUES(?1,?2,?3,?3,1,1000,NULL)",
                rusqlite::params![
                    zone_name,
                    format!("old-sparse-{index:03}"),
                    "[\"synthetic-released-shape\"]"
                ],
            )
            .unwrap();
        }
        conn.execute(
            "INSERT INTO metadata VALUES(?1,?2)",
            rusqlite::params![format!("sync_token:{zone_name}"), OLD_TOKEN],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO metadata VALUES(?1,?2)",
            rusqlite::params![format!("sync_token:{SHARED}"), "shared-old-before"],
        )
        .unwrap();
    }
    let (sparse_before, shared_before) = {
        let conn = rusqlite::Connection::open(&source).unwrap();
        (sparse_rows(&conn), shared_rows(&conn))
    };
    adopt_legacy(
        &source,
        &db_path,
        &baseline_owner(),
        "baseline@example.invalid",
        "com",
    )
    .await
    .unwrap();
    MissingSeed {
        _dir: dir,
        root,
        recorded,
        shared_path,
        db_path,
        checksum,
        local,
        shared_mtime,
        sparse_before,
        shared_before,
    }
}

#[tokio::test]
async fn released_pending_missing_original_downloads_once_with_new_primary_item() {
    let seed = seed_missing_original().await;
    assert!(
        !seed.recorded.exists(),
        "initial fixture must not create the missing original"
    );
    let server = crate::start_wiremock_or_skip!();
    for (endpoint, bytes) in [("/original", MEDIA), ("/new", MEDIA)] {
        Mock::given(method("GET"))
            .and(path(endpoint))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(bytes))
            .expect(1)
            .mount(&server)
            .await;
    }
    let mut original = pair(
        "original-master",
        "original.JPG",
        &format!("{}/original", server.uri()),
        31,
    );
    original[0]["fields"]["resOriginalRes"]["value"]["fileChecksum"] = json!(seed.checksum);
    original[0]["fields"]["resOriginalRes"]["value"]["size"] = json!(MEDIA.len());
    original[1]["recordName"] = json!("asset-original-child");
    let mut new_item = pair(
        "new-current",
        "new.JPG",
        &format!("{}/new", server.uri()),
        32,
    );
    new_item[0]["fields"]["resOriginalRes"]["value"]["fileChecksum"] = json!(seed.checksum);
    new_item[0]["fields"]["resOriginalRes"]["value"]["size"] = json!(MEDIA.len());
    let inventory = Arc::new(
        original
            .into_iter()
            .chain(new_item)
            .collect::<Vec<serde_json::Value>>(),
    );
    let db = Arc::new(
        SqliteStateDb::open_owned(&seed.db_path, &baseline_owner())
            .await
            .unwrap(),
    );
    let session = BaselineSession {
        database: "private",
        cycle: 0,
        inventory: Arc::clone(&inventory),
        concurrent: Arc::new(Vec::new()),
        incremental_addition: Arc::new(Vec::new()),
        requests: Arc::new(Mutex::new(Vec::new())),
        inventory_queries: Arc::new(AtomicUsize::new(0)),
        inventory_starts: Arc::new(AtomicUsize::new(0)),
        boundaries: Arc::new(AtomicUsize::new(0)),
        changes: Arc::new(AtomicUsize::new(0)),
    };
    let mut library = PhotoLibrary::new(
        "https://synthetic.example/database/1/photos/production/private".into(),
        Arc::new(std::collections::HashMap::new()),
        Box::new(session),
        Arc::new(zone()),
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
    let plan = resolve_passes_for_scope(
        &library,
        &Selection::default(),
        PassScope {
            include_albums: false,
            include_smart_folders: false,
            include_unfiled: true,
        },
        &CollectionContext {
            collection_album_names: BTreeSet::new(),
            selected_smart_folder_names: Vec::new(),
        },
        &[],
    )
    .await
    .unwrap();
    let mut selected =
        make_run_cycle_library_state_with_passes(ZONE, &sync_token_key(ZONE), plan.passes);
    selected.library = library;
    let base = make_run_cycle_download_config_builder(&seed.root, db.clone());
    let builder = |mode, exclude, groups, zone| {
        let mut config = (*base(mode, exclude, groups, zone)).clone();
        config.folder_structure = "none".into();
        Arc::new(config)
    };
    let (_session_dir, shared_session) = make_shared_session_for_run_cycle().await;
    let result = run_cycle(
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
    assert_eq!(result.failed_count, 0, "{result:?}");
    assert_eq!(result.stats.downloaded, 2, "missing original plus new item");
    assert_eq!(std::fs::read(&seed.recorded).unwrap(), MEDIA);
    let new_path = seed.root.join("new.JPG");
    assert_eq!(std::fs::read(&new_path).unwrap(), MEDIA);
    assert!(!seed.root.join("original_2.JPG").exists());
    {
        let conn = db
            .acquire_lock("inspect missing-original recovery")
            .unwrap();
        for (id, path) in [
            ("original-master", &seed.recorded),
            ("asset-new-current", &new_path),
        ] {
            let (status, local_path, checksum, size, local_checksum): (
                String,
                String,
                String,
                i64,
                String,
            ) = conn
                .query_row(
                    "SELECT status,local_path,checksum,size_bytes,local_checksum FROM assets \
                     WHERE library=?1 AND id=?2 AND version_size='original'",
                    rusqlite::params![ZONE, id],
                    |row| {
                        Ok((
                            row.get(0)?,
                            row.get(1)?,
                            row.get(2)?,
                            row.get(3)?,
                            row.get(4)?,
                        ))
                    },
                )
                .unwrap();
            assert_eq!(status, "downloaded");
            assert_eq!(local_path, path.to_str().unwrap());
            assert_eq!(checksum, seed.checksum);
            assert_eq!(size, MEDIA.len() as i64);
            assert_eq!(local_checksum, seed.local);
        }
        let token: Option<String> = conn
            .query_row(
                "SELECT value FROM metadata WHERE key=?1",
                [sync_token_key(ZONE)],
                |row| row.get(0),
            )
            .ok();
        assert!(token.is_some());
        assert_ne!(token.as_deref(), Some(OLD_TOKEN));
    }
    let sparse_after = {
        let conn = rusqlite::Connection::open(&seed.db_path).unwrap();
        sparse_rows(&conn)
    };
    let shared_after = {
        let conn = rusqlite::Connection::open(&seed.db_path).unwrap();
        shared_rows(&conn)
    };
    assert_eq!(sparse_after, seed.sparse_before);
    assert_eq!(shared_after, seed.shared_before);
    assert_eq!(std::fs::read(&seed.shared_path).unwrap(), SHARED_BYTES);
    assert_eq!(
        std::fs::metadata(&seed.shared_path)
            .unwrap()
            .modified()
            .unwrap(),
        seed.shared_mtime
    );
    let hashes_before_quiet = (
        db.get_metadata(crate::download::DOWNLOAD_CONFIG_HASH_KEY)
            .await
            .unwrap(),
        db.get_metadata(crate::sync_cycle::ENUM_CONFIG_HASH_KEY)
            .await
            .unwrap(),
    );
    drop(db);

    let db = Arc::new(
        SqliteStateDb::open_owned(&seed.db_path, &baseline_owner())
            .await
            .unwrap(),
    );
    let (first_quiet, first_full) = run_c11_quiet(
        Arc::clone(&db),
        &seed.root,
        &shared_session,
        "c11-missing-quiet-1",
    )
    .await;
    assert_eq!(first_quiet.failed_count, 0, "{first_quiet:?}");
    assert_eq!(first_full, 0, "{first_quiet:?}");
    assert_eq!(first_quiet.stats.downloaded, 0);
    assert_eq!(
        (
            db.get_metadata(crate::download::DOWNLOAD_CONFIG_HASH_KEY)
                .await
                .unwrap(),
            db.get_metadata(crate::sync_cycle::ENUM_CONFIG_HASH_KEY)
                .await
                .unwrap(),
        ),
        hashes_before_quiet
    );
    drop(db);

    let db = Arc::new(
        SqliteStateDb::open_owned(&seed.db_path, &baseline_owner())
            .await
            .unwrap(),
    );
    let (second_quiet, second_full) = run_c11_quiet(
        Arc::clone(&db),
        &seed.root,
        &shared_session,
        "c11-missing-quiet-2",
    )
    .await;
    assert_eq!(second_quiet.failed_count, 0, "{second_quiet:?}");
    assert_eq!(second_full, 0, "{second_quiet:?}");
    assert_eq!(second_quiet.stats.downloaded, 0);
    assert_eq!(
        (
            db.get_metadata(crate::download::DOWNLOAD_CONFIG_HASH_KEY)
                .await
                .unwrap(),
            db.get_metadata(crate::sync_cycle::ENUM_CONFIG_HASH_KEY)
                .await
                .unwrap(),
        ),
        hashes_before_quiet
    );
    drop(db);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OriginalProof {
    Complete,
    WrongSource,
}

#[derive(Clone)]
struct SixOriginalSession {
    inner: ResetCycleSession,
    requests: Arc<Mutex<Vec<(String, Value)>>>,
}

#[async_trait::async_trait]
impl PhotosSession for SixOriginalSession {
    async fn post(
        &self,
        url: &str,
        body: String,
        headers: &[(&str, &str)],
    ) -> anyhow::Result<Value> {
        let request: Value = serde_json::from_str(&body)?;
        assert!(url.contains("/private/"), "{url}");
        if url.contains("/records/query?") || url.contains("/records/lookup?") {
            assert_eq!(
                request["zoneID"],
                json!({"zoneName":PRIMARY,"ownerRecordName":"_defaultOwner"})
            );
        }
        if url.contains("/changes/zone?") {
            assert_eq!(
                request["zones"][0]["zoneID"],
                json!({"zoneName":PRIMARY,"ownerRecordName":"_defaultOwner"})
            );
            assert!(
                matches!(
                    request["zones"][0]["syncToken"].as_str(),
                    Some("c11-primary-before" | "reset-after-0")
                ),
                "baseline bridge must use the current Primary boundary: {request}"
            );
        }
        self.requests
            .lock()
            .unwrap()
            .push((url.to_owned(), request));
        self.inner.post(url, body, headers).await
    }

    fn clone_box(&self) -> Box<dyn PhotosSession> {
        Box::new(self.clone())
    }
}

fn primary_receipts(conn: &rusqlite::Connection) -> Vec<String> {
    conn.prepare("SELECT json_array(account_key,provider_key,scope,library,child,master,state_id,origin_pass,version_size,provider_checksum,provider_size,native_path,source_checksum,local_checksum,proof_hash) FROM download_receipts WHERE library=?1 ORDER BY child,native_path")
        .unwrap().query_map([PRIMARY], |row| row.get(0)).unwrap()
        .collect::<rusqlite::Result<_>>().unwrap()
}

fn assert_primary_ledger(
    conn: &rusqlite::Connection,
    seed: &MissingSeed,
    child: &str,
    master: &str,
    state_id: &str,
    path: &std::path::Path,
) {
    let native = crate::state::db::provider_selection::SelectionPath::from_path(path)
        .key()
        .unwrap();
    let owner = baseline_owner();
    let (account, provider) = owner.authenticated_keys().unwrap();
    let scope = serde_json::to_string(&json!({"format":1,"realm":"com","container":"com.apple.photos.cloud","environment":"production","database":"private","zone":{"zoneName":PRIMARY,"ownerRecordName":"_defaultOwner"}})).unwrap();
    let (generation, pass, specification, sealed, ready): (String, String, Vec<u8>, i64, i64) = conn.query_row(
        "SELECT g.id,d.pass_key,g.specification,g.sealed,g.checkpoint_ready FROM provider_active_generations g JOIN provider_active_decisions d ON d.generation=g.id JOIN provider_active_destinations w ON w.generation=d.generation AND w.pass_key=d.pass_key AND w.child=d.child JOIN download_receipts r ON r.library=w.library AND r.child=w.child AND r.master=w.master AND r.state_id=w.asset_id AND r.native_path=w.path AND r.origin_pass=d.pass_key WHERE g.account_key=?1 AND g.provider_key=?2 AND g.scope=?3 AND d.child=?4 AND d.outcome='selected' AND d.admission='admitted' AND w.library=?5 AND w.asset_id=?6 AND w.master=?7 AND w.version_size='original' AND w.path=?8 AND w.compat_path=?9 AND w.checksum=?10 AND w.size_bytes=?11 AND w.admitted=1 AND w.verified_media=1 AND w.local_checksum=?12 AND w.source_checksum=?12 AND r.account_key=?1 AND r.provider_key=?2 AND r.scope=?3 AND r.provider_checksum=?10 AND r.provider_size=?11 AND r.source_checksum=?12 AND r.local_checksum=?12",
        rusqlite::params![account,provider,scope,child,PRIMARY,state_id,master,native,path.to_str().unwrap(),seed.checksum,MEDIA.len() as i64,seed.local],
        |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?)),
    ).unwrap_or_else(|error| panic!("missing exact current ledger for {child}: {error}"));
    let spec: Value = serde_json::from_slice(&specification).unwrap();
    assert_eq!(spec["format"], 2, "{spec}");
    assert_eq!(
        spec["zone"],
        json!({"zoneName":PRIMARY,"ownerRecordName":"_defaultOwner"})
    );
    assert_eq!(spec["scope"], scope);
    assert!(
        spec["profile"]["passes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["key"] == pass && p["kind"] == "Unfiled"),
        "pass={pass} spec={spec}"
    );
    assert!(!generation.is_empty());
    assert_eq!((sealed, ready), (1, 1), "generation {generation}");
}

async fn six_originals_control(proof: OriginalProof) {
    let (trace, _trace_guard) = crate::test_helpers::TracingCapture::install();
    let seed = seed_missing_originals(6, PRIMARY).await;
    assert_eq!(seed.sparse_before.len(), 100);
    let same_path = seed.root.join("shared-same.JPG");
    let same_mtime = std::fs::metadata(&same_path).unwrap().modified().unwrap();
    assert_eq!(std::fs::read(&same_path).unwrap(), MEDIA);
    let server = crate::start_wiremock_or_skip!();
    let mut inventory = Vec::new();
    for index in 0..6 {
        let (master, child, filename) = original_identity(index);
        assert!(!seed.root.join(&filename).exists());
        let endpoint = format!("/original-{index}");
        Mock::given(method("GET"))
            .and(path(&endpoint))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(MEDIA))
            .expect(if proof == OriginalProof::WrongSource && index == 0 {
                0
            } else {
                1
            })
            .mount(&server)
            .await;
        let mut records = pair(
            &master,
            &filename,
            &format!("{}{endpoint}", server.uri()),
            31,
        );
        records[0]["fields"]["resOriginalRes"]["value"]["fileChecksum"] = json!(seed.checksum);
        records[0]["fields"]["resOriginalRes"]["value"]["size"] = json!(MEDIA.len());
        records[1]["recordName"] = json!(child);
        records[1]["fields"]["masterRef"]["value"]["zoneID"] =
            json!({"zoneName":PRIMARY,"ownerRecordName":"_defaultOwner"});
        if proof == OriginalProof::WrongSource && index == 0 {
            records[1]["fields"]["masterRef"]["value"]["zoneID"] = zone_for(SHARED);
        }
        inventory.extend(records);
    }
    Mock::given(method("GET"))
        .and(path("/new"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(MEDIA))
        .expect(1)
        .mount(&server)
        .await;
    let mut new_item = pair(
        "new-current",
        "new.JPG",
        &format!("{}/new", server.uri()),
        32,
    );
    new_item[1]["fields"]["masterRef"]["value"]["zoneID"] =
        json!({"zoneName":PRIMARY,"ownerRecordName":"_defaultOwner"});
    new_item[0]["fields"]["resOriginalRes"]["value"]["fileChecksum"] = json!(seed.checksum);
    new_item[0]["fields"]["resOriginalRes"]["value"]["size"] = json!(MEDIA.len());
    inventory.extend(new_item);
    let requests = Arc::new(Mutex::new(Vec::new()));
    let session = SixOriginalSession {
        inner: ResetCycleSession {
            zone: PRIMARY,
            inventory: Arc::new(inventory),
            full_queries: Arc::new(AtomicUsize::new(0)),
            changes: Arc::new(AtomicUsize::new(0)),
            fail_full: false,
            auth_expired: false,
            boundary: "c11-primary-before",
        },
        requests: Arc::clone(&requests),
    };
    let db = Arc::new(
        SqliteStateDb::open_owned(&seed.db_path, &baseline_owner())
            .await
            .unwrap(),
    );
    let old_pending = {
        let conn = db
            .acquire_lock("inspect six-original initial state")
            .unwrap();
        let receipts: i64 = conn
            .query_row("SELECT count(*) FROM download_receipts", [], |r| r.get(0))
            .unwrap();
        assert_eq!(receipts, 0);
        conn.query_row("SELECT status,local_path,checksum,size_bytes,local_checksum,download_checksum FROM assets WHERE library=?1 AND id='original-master' AND version_size='original'", [PRIMARY], |r| Ok((r.get::<_, String>(0)?,r.get::<_, String>(1)?,r.get::<_, String>(2)?,r.get::<_, i64>(3)?,r.get::<_, String>(4)?,r.get::<_, String>(5)?))).unwrap()
    };
    assert_eq!(
        db.get_metadata(&sync_token_key(PRIMARY)).await.unwrap(),
        None
    );
    let mut library = PhotoLibrary::new(
        "https://synthetic.example/database/1/photos/production/private".into(),
        Arc::new(std::collections::HashMap::new()),
        Box::new(session),
        Arc::new(json!({"zoneName":PRIMARY,"ownerRecordName":"_defaultOwner"})),
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
    let plan = resolve_passes_for_scope(
        &library,
        &Selection::default(),
        PassScope {
            include_albums: false,
            include_smart_folders: false,
            include_unfiled: true,
        },
        &CollectionContext {
            collection_album_names: BTreeSet::new(),
            selected_smart_folder_names: Vec::new(),
        },
        &[],
    )
    .await
    .unwrap();
    let mut state =
        make_run_cycle_library_state_with_passes(PRIMARY, &sync_token_key(PRIMARY), plan.passes);
    state.library = library;
    let (_session_dir, shared_session) = make_shared_session_for_run_cycle().await;
    let result = {
        let base = make_run_cycle_download_config_builder(&seed.root, db.clone());
        let builder = |mode, exclude, groups, zone| {
            let mut config = (*base(mode, exclude, groups, zone)).clone();
            config.folder_structure = "none".into();
            Arc::new(config)
        };
        run_cycle(
            &[&state],
            &make_run_cycle_config(),
            Some(db.as_ref()),
            false,
            &builder,
            download::DownloadControls::download_hidden(),
            &shared_session,
            &CancellationToken::new(),
        )
        .await
        .unwrap()
    };
    eprintln!(
        "six-original {proof:?}: downloaded={}, failed={}, state_write_failures={}",
        result.stats.downloaded, result.failed_count, result.stats.state_write_failures
    );
    if proof == OriginalProof::WrongSource {
        assert!(
            trace
                .events()
                .iter()
                .any(|event| event.level == tracing::Level::WARN
                    && event
                        .fields
                        .get("message")
                        .is_some_and(|message| message
                            .contains("Could not persist deferred current identity"))
                    && event.fields.get("state_error").is_some_and(
                        |error| error.contains("Ambiguous or mismatched current lookup scope")
                    ))
        );
    }
    assert_eq!(
        result.stats.downloaded,
        if proof == OriginalProof::Complete {
            7
        } else {
            6
        },
        "{result:?}"
    );
    let (current_token, receipts_before_quiet) = {
        let conn = db.acquire_lock("inspect six-original outcomes").unwrap();
        for index in 0..6 {
            let (master, child, filename) = original_identity(index);
            let path = seed.root.join(filename);
            if proof == OriginalProof::WrongSource && index == 0 {
                assert!(!path.exists());
                let status: String = conn.query_row("SELECT status FROM assets WHERE library=?1 AND id=?2 AND version_size='original'", rusqlite::params![PRIMARY, master], |r| r.get(0)).unwrap();
                assert_eq!(status, "pending");
                let receipts: i64 = conn
                    .query_row(
                        "SELECT count(*) FROM download_receipts WHERE library=?1 AND child=?2",
                        rusqlite::params![PRIMARY, child],
                        |r| r.get(0),
                    )
                    .unwrap();
                assert_eq!(receipts, 0);
                continue;
            }
            assert_eq!(std::fs::read(&path).unwrap(), MEDIA);
            let (status, saved_path): (String, String) = conn.query_row("SELECT status,local_path FROM assets WHERE library=?1 AND id=?2 AND version_size='original'", rusqlite::params![PRIMARY, master], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
            assert_eq!(status, "downloaded");
            assert_eq!(saved_path, path.to_str().unwrap());
            let native = crate::state::db::provider_selection::SelectionPath::from_path(&path)
                .key()
                .unwrap();
            let receipts: i64 = conn.query_row("SELECT count(*) FROM download_receipts WHERE library=?1 AND child=?2 AND master=?3 AND state_id=?3 AND version_size='original' AND native_path=?4 AND provider_checksum=?5 AND provider_size=?6 AND source_checksum=?7 AND local_checksum=?7", rusqlite::params![PRIMARY, child, master, native, seed.checksum, MEDIA.len() as i64, seed.local], |r| r.get(0)).unwrap();
            assert_eq!(receipts, 1, "receipt for {master}");
            if proof == OriginalProof::Complete {
                assert_primary_ledger(&conn, &seed, &child, &master, &master, &path);
            }
        }
        assert_eq!(std::fs::read(seed.root.join("new.JPG")).unwrap(), MEDIA);
        let new_native = crate::state::db::provider_selection::SelectionPath::from_path(
            &seed.root.join("new.JPG"),
        )
        .key()
        .unwrap();
        let new_receipts: i64 = conn.query_row("SELECT count(*) FROM download_receipts WHERE library=?1 AND child='asset-new-current' AND master='new-current' AND state_id='asset-new-current' AND version_size='original' AND native_path=?2 AND provider_checksum=?3 AND provider_size=?4 AND source_checksum=?5 AND local_checksum=?5", rusqlite::params![PRIMARY, new_native, seed.checksum, MEDIA.len() as i64, seed.local], |r| r.get(0)).unwrap();
        assert_eq!(
            new_receipts, 1,
            "new Primary item has its own current proof"
        );
        if proof == OriginalProof::Complete {
            assert_primary_ledger(
                &conn,
                &seed,
                "asset-new-current",
                "new-current",
                "asset-new-current",
                &seed.root.join("new.JPG"),
            );
        }
        let decisions: Vec<String> = conn.prepare("SELECT child||'|'||outcome||'|'||admission||'|'||reason FROM provider_active_decisions ORDER BY child").unwrap().query_map([], |row| row.get(0)).unwrap().collect::<rusqlite::Result<_>>().unwrap();
        let epoch: Option<String> = conn
            .query_row(
                "SELECT value FROM metadata WHERE key=?1",
                [crate::state::db::sync_epoch::library_key(PRIMARY)],
                |r| r.get(0),
            )
            .ok();
        let current_token: Option<String> = conn
            .query_row(
                "SELECT value FROM metadata WHERE key=?1",
                [sync_token_key(PRIMARY)],
                |r| r.get(0),
            )
            .ok();
        if proof == OriginalProof::Complete {
            assert_eq!(result.failed_count, 0, "{result:?}");
            assert_eq!(current_token.as_deref(), Some("reset-after-0"));
            let epoch: Value = serde_json::from_str(epoch.as_deref().unwrap()).unwrap();
            assert_eq!(epoch["library"], PRIMARY);
            assert_eq!(epoch["state"], "complete");
            assert_eq!(epoch["reason"], "existing_upgrade");
            assert_eq!(decisions.len(), 7);
        } else {
            assert!(!result.db_sync_token_advance_safe, "{result:?}");
            assert_eq!(current_token, None);
            assert!(requests.lock().unwrap().iter().any(|(url, body)| {
                url.contains("/records/lookup?")
                    && body["records"].as_array().is_some_and(|items| {
                        items
                            .iter()
                            .any(|item| item["recordName"] == "asset-original-child")
                    })
            }));
        }
        let old_sparse: Vec<_> = sparse_rows(&conn).into_iter().filter(|row| matches!(&row[1], rusqlite::types::Value::Text(name) if name.starts_with("old-sparse-"))).collect();
        assert_eq!(old_sparse, seed.sparse_before);
        if proof == OriginalProof::WrongSource {
            let current: Vec<_> = sparse_rows(&conn).into_iter().filter(|row| !matches!(&row[1], rusqlite::types::Value::Text(name) if name.starts_with("old-sparse-"))).collect();
            assert!(
                current.is_empty(),
                "foreign observation must not mint current sparse identity"
            );
            assert!(
                !decisions
                    .iter()
                    .any(|decision| decision.starts_with("asset-original-child|"))
            );
            assert!(result.failed_count > 0, "{result:?}");
            assert_eq!(result.stats.state_write_failures, 2, "{result:?}");
            assert!(result.stats.identity_incomplete, "{result:?}");
        }
        assert_eq!(shared_rows(&conn), seed.shared_before);
        (current_token, primary_receipts(&conn))
    };
    let hashes = (
        db.get_metadata(crate::download::DOWNLOAD_CONFIG_HASH_KEY)
            .await
            .unwrap(),
        db.get_metadata(crate::sync_cycle::ENUM_CONFIG_HASH_KEY)
            .await
            .unwrap(),
    );
    drop(state);
    drop(db);
    if proof == OriginalProof::Complete {
        for boundary in ["six-original-quiet-1", "six-original-quiet-2"] {
            let reopened = Arc::new(
                SqliteStateDb::open_owned(&seed.db_path, &baseline_owner())
                    .await
                    .unwrap(),
            );
            let (quiet, full) = run_c11_quiet_in_zone(
                Arc::clone(&reopened),
                &seed.root,
                &shared_session,
                boundary,
                PRIMARY,
            )
            .await;
            assert_eq!(quiet.failed_count, 0, "{quiet:?}");
            assert_eq!(quiet.stats.downloaded, 0, "{quiet:?}");
            assert_eq!(full, 0, "{quiet:?}");
            assert_eq!(
                (
                    reopened
                        .get_metadata(crate::download::DOWNLOAD_CONFIG_HASH_KEY)
                        .await
                        .unwrap(),
                    reopened
                        .get_metadata(crate::sync_cycle::ENUM_CONFIG_HASH_KEY)
                        .await
                        .unwrap()
                ),
                hashes
            );
            assert_eq!(
                reopened
                    .get_metadata(&sync_token_key(PRIMARY))
                    .await
                    .unwrap(),
                current_token
            );
            let conn = reopened
                .acquire_lock("inspect quiet primary receipts")
                .unwrap();
            assert_eq!(primary_receipts(&conn), receipts_before_quiet);
            assert_eq!(sparse_rows(&conn), seed.sparse_before);
            assert_eq!(shared_rows(&conn), seed.shared_before);
        }
    } else {
        let reopened = SqliteStateDb::open_owned(&seed.db_path, &baseline_owner())
            .await
            .unwrap();
        assert_eq!(
            reopened
                .get_metadata(&sync_token_key(PRIMARY))
                .await
                .unwrap(),
            None
        );
        let conn = reopened
            .acquire_lock("inspect retained foreign-source refusal")
            .unwrap();
        let retained = conn.query_row("SELECT status,local_path,checksum,size_bytes,local_checksum,download_checksum FROM assets WHERE library=?1 AND id='original-master' AND version_size='original'", [PRIMARY], |r| Ok((r.get::<_, String>(0)?,r.get::<_, String>(1)?,r.get::<_, String>(2)?,r.get::<_, i64>(3)?,r.get::<_, String>(4)?,r.get::<_, String>(5)?))).unwrap();
        assert_eq!(retained, old_pending);
        let epoch_state: String = conn
            .query_row(
                "SELECT json_extract(value,'$.state') FROM metadata WHERE key=?1",
                [crate::state::db::sync_epoch::library_key(PRIMARY)],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(epoch_state, "pending");
        let current: i64 = conn
            .query_row(
                "SELECT count(*) FROM provider_active_decisions WHERE child='asset-original-child'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(current, 0);
        let current: i64 = conn.query_row("SELECT count(*) FROM unresolved_sparse_identities WHERE library=?1 AND source_record_name='asset-original-child'", [PRIMARY], |r| r.get(0)).unwrap();
        assert_eq!(current, 0);
        let current: i64 = conn.query_row("SELECT count(*) FROM download_receipts WHERE library=?1 AND child='asset-original-child'", [PRIMARY], |r| r.get(0)).unwrap();
        assert_eq!(current, 0);
        assert!(!seed.recorded.exists());
        assert_eq!(sparse_rows(&conn), seed.sparse_before);
        assert_eq!(shared_rows(&conn), seed.shared_before);
    }
    assert_eq!(std::fs::read(&same_path).unwrap(), MEDIA);
    assert_eq!(
        std::fs::metadata(&same_path).unwrap().modified().unwrap(),
        same_mtime
    );
    assert_eq!(std::fs::read(&seed.shared_path).unwrap(), SHARED_BYTES);
    assert_eq!(
        std::fs::metadata(&seed.shared_path)
            .unwrap()
            .modified()
            .unwrap(),
        seed.shared_mtime
    );
    server.verify().await;
}

#[tokio::test]
async fn six_missing_originals_and_new_primary_recover() {
    six_originals_control(OriginalProof::Complete).await;
}

#[tokio::test]
async fn six_missing_originals_refuse_wrong_source_reference() {
    six_originals_control(OriginalProof::WrongSource).await;
}

#[tokio::test]
async fn shared_distinct_owner_current_progress_proceeds_separately() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("photos");
    std::fs::create_dir_all(&root).unwrap();
    let shared_path = root.join(SHARED_PATH);
    std::fs::write(&shared_path, SHARED_BYTES).unwrap();
    let shared_mtime = std::fs::metadata(&shared_path).unwrap().modified().unwrap();
    let source = dir.path().join("released.db");
    let db_path = dir.path().join("state.db");
    {
        let conn = rusqlite::Connection::open(&source).unwrap();
        crate::state::schema::migrate_genuine_v36(&conn).unwrap();
        conn.execute(
            "INSERT INTO assets(library,id,version_size,checksum,filename,created_at,added_at,\
             size_bytes,media_type,status,downloaded_at,local_path,last_seen_at,\
             local_checksum,download_checksum,source) \
             VALUES(?1,'shared-old-master','original',?2,'shared-history.JPG',1700000000,1700000002,\
             ?3,'photo','downloaded',1700000003,?4,1700000004,?5,?5,'icloud')",
            rusqlite::params![
                SHARED,
                provider_checksum(SHARED_BYTES),
                SHARED_BYTES.len() as i64,
                shared_path.to_str().unwrap(),
                hex(SHARED_BYTES)
            ],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO asset_metadata_paths(library,id,version_size,local_path,\
             provider_checksum,local_checksum,download_checksum,source_checksum) \
             VALUES(?1,'shared-old-master','original',?2,?3,?4,?4,?4)",
            rusqlite::params![
                SHARED,
                shared_path.to_str().unwrap(),
                provider_checksum(SHARED_BYTES),
                hex(SHARED_BYTES)
            ],
        )
        .unwrap();
        conn.execute_batch(&format!(
            "INSERT INTO legacy_master_state_owners VALUES ('{SHARED}','shared-old-master','asset-shared-old-master',1700000000);
             INSERT INTO asset_master_mappings VALUES ('{SHARED}','asset-shared-old-master','shared-old-master',1700000000);"
        ))
        .unwrap();
        conn.execute(
            "INSERT INTO metadata VALUES(?1,?2)",
            rusqlite::params![format!("sync_token:{SHARED}"), "shared-old-before"],
        )
        .unwrap();
    }
    adopt_legacy(
        &source,
        &db_path,
        &baseline_owner(),
        "baseline@example.invalid",
        "com",
    )
    .await
    .unwrap();
    let server = crate::start_wiremock_or_skip!();
    Mock::given(method("GET"))
        .and(path("/shared-new"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(SHARED_NEW_BYTES))
        .expect(1)
        .mount(&server)
        .await;
    let inventory = Arc::new(shared_pair(
        "shared-new-master",
        "shared-new.JPG",
        &format!("{}/shared-new", server.uri()),
        SHARED_NEW_BYTES,
    ));
    let queries = Arc::new(AtomicUsize::new(0));
    let full_queries = Arc::new(AtomicUsize::new(0));
    let session = SharedProgressSession {
        inventory,
        queries: Arc::clone(&queries),
        full_queries: Arc::clone(&full_queries),
    };
    let quiet_session = session.clone();
    let db = Arc::new(
        SqliteStateDb::open_owned(&db_path, &baseline_owner())
            .await
            .unwrap(),
    );
    {
        let conn = db.acquire_lock("inspect initial shared proof").unwrap();
        let count: i64 = conn
            .query_row("SELECT count(*) FROM download_receipts", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(
            count, 0,
            "the new Shared proof must be created by the cycle"
        );
    }
    let mut library = PhotoLibrary::new(
        "https://synthetic.example/database/1/photos/production/shared".into(),
        Arc::new(std::collections::HashMap::new()),
        Box::new(session),
        Arc::new(zone_for(SHARED)),
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
    let plan = resolve_passes_for_scope(
        &library,
        &Selection::default(),
        PassScope {
            include_albums: false,
            include_smart_folders: false,
            include_unfiled: true,
        },
        &CollectionContext {
            collection_album_names: BTreeSet::new(),
            selected_smart_folder_names: Vec::new(),
        },
        &[],
    )
    .await
    .unwrap();
    let mut state =
        make_run_cycle_library_state_with_passes(SHARED, &sync_token_key(SHARED), plan.passes);
    state.library = library;
    let base = make_run_cycle_download_config_builder(&root, db.clone());
    let builder = |mode, exclude, groups, zone| {
        let mut config = (*base(mode, exclude, groups, zone)).clone();
        config.folder_structure = "none".into();
        Arc::new(config)
    };
    let (_session_dir, shared_session) = make_shared_session_for_run_cycle().await;
    let (trace, _trace_guard) = crate::test_helpers::TracingCapture::install();
    let result = run_cycle(
        &[&state],
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
    if result.failed_count != 0 {
        for event in trace.events() {
            if matches!(
                event.level,
                tracing::Level::WARN | tracing::Level::ERROR | tracing::Level::INFO
            ) {
                eprintln!("SHARED FAIL {:?} {:?}", event.level, event.fields);
            }
        }
        let conn = db.acquire_lock("inspect shared failure").unwrap();
        let rows: Vec<String> = conn
            .prepare(
                "SELECT id||'|'||version_size||'|'||status||'|'||\
                 COALESCE(last_error,'') FROM assets WHERE library=?1 ORDER BY id",
            )
            .unwrap()
            .query_map([SHARED], |row| row.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        eprintln!("SHARED FAIL assets={rows:?}");
    }
    assert_eq!(result.failed_count, 0, "{result:?}");
    assert_eq!(result.stats.downloaded, 1, "{result:?}");
    assert!(queries.load(Ordering::SeqCst) > 0);
    assert_eq!(
        std::fs::read(root.join("shared-new.JPG")).unwrap(),
        SHARED_NEW_BYTES
    );
    assert_eq!(std::fs::read(&shared_path).unwrap(), SHARED_BYTES);
    assert_eq!(
        std::fs::metadata(&shared_path).unwrap().modified().unwrap(),
        shared_mtime
    );
    {
        let conn = db
            .acquire_lock("inspect shared progress checkpoint")
            .unwrap();
        let token: Option<String> = conn
            .query_row(
                "SELECT value FROM metadata WHERE key=?1",
                [sync_token_key(SHARED)],
                |row| row.get(0),
            )
            .ok();
        assert!(token.is_some());
        assert_ne!(token.as_deref(), Some("shared-old-before"));
        let epoch_state: String = conn
            .query_row(
                "SELECT json_extract(value,'$.state') FROM metadata WHERE key=?1",
                [crate::state::db::sync_epoch::library_key(SHARED)],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(epoch_state, "complete");
        let scope = serde_json::to_string(&json!({
            "format": 1,
            "realm": "com",
            "container": "com.apple.photos.cloud",
            "environment": "production",
            "database": "shared",
            "zone": zone_for(SHARED),
        }))
        .unwrap();
        let native_path = crate::state::db::provider_selection::SelectionPath::from_path(
            &root.join("shared-new.JPG"),
        )
        .key()
        .unwrap();
        let owner = baseline_owner();
        let (account_key, provider_key) = owner.authenticated_keys().unwrap();
        let count: i64 = conn
            .query_row(
                "SELECT count(*) FROM download_receipts WHERE account_key=?1 AND provider_key=?2 \
             AND scope=?3 AND library=?4 AND child='asset-shared-new-master' \
             AND master='shared-new-master' AND state_id='asset-shared-new-master' \
             AND version_size='original' AND provider_checksum=?5 AND provider_size=?6 \
             AND native_path=?7 AND source_checksum=?8 AND local_checksum=?8",
                rusqlite::params![
                    account_key,
                    provider_key,
                    scope,
                    SHARED,
                    provider_checksum(SHARED_NEW_BYTES),
                    SHARED_NEW_BYTES.len() as i64,
                    native_path,
                    hex(SHARED_NEW_BYTES)
                ],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1, "Shared transfer must create its own scoped proof");
    }
    let full_after_transfer = full_queries.load(Ordering::SeqCst);
    let shared_album = state.plan.passes.first().unwrap().album.clone();
    let hashes_before_quiet = (
        db.get_metadata(crate::download::DOWNLOAD_CONFIG_HASH_KEY)
            .await
            .unwrap(),
        db.get_metadata(crate::sync_cycle::ENUM_CONFIG_HASH_KEY)
            .await
            .unwrap(),
    );
    drop(db);

    let db = Arc::new(
        SqliteStateDb::open_owned(&db_path, &baseline_owner())
            .await
            .unwrap(),
    );
    let (first_quiet, first_full) = run_shared_c11_quiet(
        Arc::clone(&db),
        &root,
        &shared_session,
        quiet_session.clone(),
        shared_album.clone(),
    )
    .await;
    assert_eq!(first_quiet.failed_count, 0, "{first_quiet:?}");
    assert_eq!(first_quiet.stats.downloaded, 0, "{first_quiet:?}");
    assert_eq!(
        first_full, full_after_transfer,
        "first Shared quiet cycle must not rescan the full DATA stream: {first_quiet:?}"
    );
    let hashes_after_first = (
        db.get_metadata(crate::download::DOWNLOAD_CONFIG_HASH_KEY)
            .await
            .unwrap(),
        db.get_metadata(crate::sync_cycle::ENUM_CONFIG_HASH_KEY)
            .await
            .unwrap(),
    );
    assert_eq!(hashes_after_first, hashes_before_quiet);
    drop(db);

    let db = Arc::new(
        SqliteStateDb::open_owned(&db_path, &baseline_owner())
            .await
            .unwrap(),
    );
    let (second_quiet, second_full) = run_shared_c11_quiet(
        Arc::clone(&db),
        &root,
        &shared_session,
        quiet_session,
        shared_album,
    )
    .await;
    assert_eq!(second_quiet.failed_count, 0, "{second_quiet:?}");
    assert_eq!(second_quiet.stats.downloaded, 0, "{second_quiet:?}");
    assert_eq!(
        second_full, full_after_transfer,
        "reopened Shared quiet cycle must be incremental: {second_quiet:?}"
    );
    let hashes_after_second = (
        db.get_metadata(crate::download::DOWNLOAD_CONFIG_HASH_KEY)
            .await
            .unwrap(),
        db.get_metadata(crate::sync_cycle::ENUM_CONFIG_HASH_KEY)
            .await
            .unwrap(),
    );
    assert_eq!(hashes_after_second, hashes_before_quiet);
    drop(db);

    assert_eq!(std::fs::read(&shared_path).unwrap(), SHARED_BYTES);
    assert_eq!(
        std::fs::metadata(&shared_path).unwrap().modified().unwrap(),
        shared_mtime
    );
    assert_eq!(
        std::fs::read(root.join("shared-new.JPG")).unwrap(),
        SHARED_NEW_BYTES
    );
    server.verify().await;
}

#[tokio::test]
async fn released_pending_original_recovers_once_at_recorded_destination() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("photos");
    std::fs::create_dir_all(&root).unwrap();
    let recorded = root.join("original.JPG");
    std::fs::write(&recorded, MEDIA).unwrap();
    let recorded_mtime = std::fs::metadata(&recorded).unwrap().modified().unwrap();
    let shared_path = root.join(SHARED_PATH);
    std::fs::write(&shared_path, SHARED_BYTES).unwrap();
    let shared_mtime = std::fs::metadata(&shared_path).unwrap().modified().unwrap();
    let source = dir.path().join("released.db");
    let db_path = dir.path().join("state.db");
    let checksum = provider_checksum(MEDIA);
    let local = hex(MEDIA);

    {
        let conn = rusqlite::Connection::open(&source).unwrap();
        crate::state::schema::migrate_genuine_v36(&conn).unwrap();
        conn.execute(
            "INSERT INTO assets(library,id,version_size,checksum,filename,created_at,added_at,\
             size_bytes,media_type,status,downloaded_at,local_path,last_seen_at,\
             local_checksum,download_checksum,download_attempts,last_error,source) \
             VALUES(?1,'original-master','original',?2,'original.JPG',1700000000,1700000002,\
             ?3,'photo','pending',1700000003,?4,1700000004,?5,?5,1,\
             'interrupted publication','icloud')",
            rusqlite::params![
                ZONE,
                checksum,
                MEDIA.len() as i64,
                recorded.to_str().unwrap(),
                local
            ],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO asset_metadata_paths(library,id,version_size,local_path,\
             provider_checksum,local_checksum,download_checksum,source_checksum) \
             VALUES(?1,'original-master','original',?2,?3,?4,?4,?4)",
            rusqlite::params![ZONE, recorded.to_str().unwrap(), checksum, local],
        )
        .unwrap();
        conn.execute_batch(&format!(
            "INSERT INTO asset_master_mappings VALUES ('{ZONE}','asset-original-child','original-master',1700000000);
             INSERT INTO legacy_master_state_owners VALUES ('{ZONE}','original-master','asset-original-child',1700000000);
             INSERT INTO asset_metadata_capture_revisions VALUES ('{ZONE}','original-master',1,1700000000);"
        ))
        .unwrap();

        conn.execute(
            "INSERT INTO assets(library,id,version_size,checksum,filename,created_at,added_at,\
             size_bytes,media_type,status,downloaded_at,local_path,last_seen_at,\
             local_checksum,download_checksum,source) \
             VALUES(?1,'shared-old-master','original',?2,'shared-history.JPG',1700000000,1700000002,\
             ?3,'photo','downloaded',1700000003,?4,1700000004,?5,?5,'icloud')",
            rusqlite::params![
                SHARED,
                provider_checksum(SHARED_BYTES),
                SHARED_BYTES.len() as i64,
                shared_path.to_str().unwrap(),
                hex(SHARED_BYTES)
            ],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO asset_metadata_paths(library,id,version_size,local_path,\
             provider_checksum,local_checksum,download_checksum,source_checksum) \
             VALUES(?1,'shared-old-master','original',?2,?3,?4,?4,?4)",
            rusqlite::params![
                SHARED,
                shared_path.to_str().unwrap(),
                provider_checksum(SHARED_BYTES),
                hex(SHARED_BYTES)
            ],
        )
        .unwrap();
        conn.execute_batch(&format!(
            "INSERT INTO legacy_master_state_owners VALUES ('{SHARED}','shared-old-master','asset-shared-old-master',1700000000);
             INSERT INTO asset_master_mappings VALUES ('{SHARED}','asset-shared-old-master','shared-old-master',1700000000);"
        ))
        .unwrap();

        for index in 0..100 {
            conn.execute(
                "INSERT INTO unresolved_sparse_identities(library,source_record_name,\
                 original_evidence,observed_evidence,generation,first_seen_at,last_outcome) \
                 VALUES(?1,?2,?3,?3,1,1000,NULL)",
                rusqlite::params![
                    ZONE,
                    format!("old-sparse-{index:03}"),
                    "[\"synthetic-released-shape\"]"
                ],
            )
            .unwrap();
        }
        conn.execute(
            "INSERT INTO metadata VALUES(?1,?2)",
            rusqlite::params![format!("sync_token:{ZONE}"), OLD_TOKEN],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO metadata VALUES(?1,?2)",
            rusqlite::params![format!("sync_token:{SHARED}"), "shared-old-before"],
        )
        .unwrap();
    }
    let sparse_before = {
        let conn = rusqlite::Connection::open(&source).unwrap();
        sparse_rows(&conn)
    };
    let shared_before = {
        let conn = rusqlite::Connection::open(&source).unwrap();
        shared_rows(&conn)
    };
    adopt_legacy(
        &source,
        &db_path,
        &baseline_owner(),
        "baseline@example.invalid",
        "com",
    )
    .await
    .unwrap();

    let shared_probes = Arc::new(AtomicUsize::new(0));
    let shared_library = PhotoLibrary::new(
        "https://synthetic.example/database/1/photos/production/shared".into(),
        Arc::new(std::collections::HashMap::new()),
        Box::new(SharedOwnerProbe {
            probes: Arc::clone(&shared_probes),
        }),
        Arc::new(json!({
            "zoneName": SHARED,
            "ownerRecordName": "c11-shared-owner"
        })),
        "shared".into(),
        crate::retry::RetryConfig::default(),
    )
    .await
    .unwrap();
    assert!(shared_library.check_indexing().await.unwrap());
    assert!(shared_probes.load(Ordering::SeqCst) >= 1);
    assert_ne!(
        "c11-shared-owner",
        zone()["ownerRecordName"],
        "Primary and Shared owners must be deliberately distinct"
    );

    let server = crate::start_wiremock_or_skip!();
    Mock::given(method("GET"))
        .and(path("/original"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(MEDIA))
        .expect(0)
        .mount(&server)
        .await;
    let mut inventory = pair(
        "original-master",
        "original.JPG",
        &format!("{}/original", server.uri()),
        31,
    );
    inventory[0]["fields"]["resOriginalRes"]["value"]["fileChecksum"] = json!(checksum);
    inventory[0]["fields"]["resOriginalRes"]["value"]["size"] = json!(MEDIA.len());
    inventory[1]["recordName"] = json!("asset-original-child");
    inventory = inventory.into_iter().collect();

    let db = Arc::new(
        SqliteStateDb::open_owned(&db_path, &baseline_owner())
            .await
            .unwrap(),
    );
    {
        let conn = db
            .acquire_lock("inspect released pending original")
            .unwrap();
        let record = crate::state::db::sync_epoch::read_library(&conn, ZONE).unwrap();
        assert!(
            record.is_none(),
            "enrollment must happen in the normal cycle, not test setup"
        );
        let status: String = conn
            .query_row(
                "SELECT status FROM assets WHERE library=?1 AND id='original-master' \
                 AND version_size='original'",
                [ZONE],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(status, "pending");
        let pending_path: String = conn
            .query_row(
                "SELECT local_path FROM assets WHERE library=?1 AND id='original-master' \
                 AND version_size='original'",
                [ZONE],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(pending_path, recorded.to_str().unwrap());
    }

    let session = BaselineSession {
        database: "private",
        cycle: 0,
        inventory: Arc::new(inventory),
        concurrent: Arc::new(Vec::new()),
        incremental_addition: Arc::new(Vec::new()),
        requests: Arc::new(Mutex::new(Vec::new())),
        inventory_queries: Arc::new(AtomicUsize::new(0)),
        inventory_starts: Arc::new(AtomicUsize::new(0)),
        boundaries: Arc::new(AtomicUsize::new(0)),
        changes: Arc::new(AtomicUsize::new(0)),
    };
    let counters = session.clone();
    let mut library = PhotoLibrary::new(
        "https://synthetic.example/database/1/photos/production/private".into(),
        Arc::new(std::collections::HashMap::new()),
        Box::new(session),
        Arc::new(zone()),
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
    let selection = Selection::default();
    let scope = PassScope {
        include_albums: false,
        include_smart_folders: false,
        include_unfiled: true,
    };
    let collection = CollectionContext {
        collection_album_names: BTreeSet::new(),
        selected_smart_folder_names: Vec::new(),
    };
    let plan = resolve_passes_for_scope(&library, &selection, scope, &collection, &[])
        .await
        .unwrap();
    assert_eq!(plan.passes.len(), 1);
    assert_eq!(plan.passes[0].kind, crate::commands::PassKind::Unfiled);
    assert!(plan.passes[0].exclude_ids.is_empty());
    let mut selected =
        make_run_cycle_library_state_with_passes(ZONE, &sync_token_key(ZONE), plan.passes);
    selected.library = library;
    let base = make_run_cycle_download_config_builder(&root, db.clone());
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
    assert_eq!(result.failed_count, 0, "{result:?}");
    assert_eq!(result.stats.downloaded, 0, "recovery must not re-download");
    assert!(result.stats.full_enumeration_reason.is_some(), "{result:?}");
    assert!(counters.inventory_queries.load(Ordering::SeqCst) > 0);
    assert!(counters.boundaries.load(Ordering::SeqCst) > 0);
    {
        let requests = counters.requests.lock().unwrap();
        assert!(
            requests.iter().any(|(url, _)| url.contains("/zones/list")),
            "normal run_cycle must establish a fresh source boundary"
        );
        assert!(
            requests
                .iter()
                .any(|(url, _)| url.contains("/records/query?")),
            "normal run_cycle must issue a current inventory query"
        );
    }
    server.verify().await;

    let (status, local_path, local_checksum, stored_checksum, stored_size, token, epoch_state) = {
        let conn = db.acquire_lock("inspect recovered original").unwrap();
        let (status, local_path, local_checksum, stored_checksum, stored_size): (
            String,
            String,
            String,
            String,
            i64,
        ) = conn
            .query_row(
                "SELECT status,local_path,local_checksum,checksum,size_bytes FROM assets \
                 WHERE library=?1 AND id='original-master' AND version_size='original'",
                [ZONE],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                },
            )
            .unwrap();
        let token: Option<String> = conn
            .query_row(
                "SELECT value FROM metadata WHERE key=?1",
                [sync_token_key(ZONE)],
                |row| row.get(0),
            )
            .ok();
        let epoch_state: String = conn
            .query_row(
                "SELECT json_extract(value,'$.state') FROM metadata WHERE key=?1",
                [crate::state::db::sync_epoch::library_key(ZONE)],
                |row| row.get(0),
            )
            .unwrap();
        (
            status,
            local_path,
            local_checksum,
            stored_checksum,
            stored_size,
            token,
            epoch_state,
        )
    };
    assert_eq!(status, "downloaded");
    assert_eq!(local_path, recorded.to_str().unwrap());
    assert_eq!(local_checksum, local);
    assert_eq!(stored_checksum, checksum);
    assert_eq!(stored_size, MEDIA.len() as i64);
    assert_ne!(token.as_deref(), Some(OLD_TOKEN));
    assert!(token.is_some());
    assert_eq!(epoch_state, "complete");
    assert_eq!(std::fs::read(&recorded).unwrap(), MEDIA);
    assert_eq!(
        std::fs::metadata(&recorded).unwrap().modified().unwrap(),
        recorded_mtime
    );
    assert!(!root.join("original_2.JPG").exists());
    assert!(!root.join("original (1).JPG").exists());
    assert_eq!(
        db.get_metadata(&format!("sync_token:{ZONE}"))
            .await
            .unwrap(),
        None,
        "adopted enrollment clears the released legacy checkpoint key"
    );

    let (sparse_after, shared_after) = {
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        (sparse_rows(&conn), shared_rows(&conn))
    };
    assert_eq!(
        sparse_after, sparse_before,
        "old sparse history must be inert"
    );
    assert_eq!(
        shared_after, shared_before,
        "Shared history must not be borrowed"
    );
    assert_eq!(std::fs::read(&shared_path).unwrap(), SHARED_BYTES);
    assert_eq!(
        std::fs::metadata(&shared_path).unwrap().modified().unwrap(),
        shared_mtime
    );
    assert_eq!(
        db.get_metadata(&format!("sync_token:{SHARED}"))
            .await
            .unwrap()
            .as_deref(),
        Some("shared-old-before"),
        "the distinct-owner Shared checkpoint is not rewritten"
    );
    drop(db);

    let db = Arc::new(
        SqliteStateDb::open_owned(&db_path, &baseline_owner())
            .await
            .unwrap(),
    );
    let (first_quiet, first_full) =
        run_c11_quiet(Arc::clone(&db), &root, &shared, "c11-quiet-1").await;
    assert_eq!(first_quiet.stats.downloaded, 0, "{first_quiet:?}");
    if first_full > 0 {
        assert_eq!(
            first_quiet.stats.full_enumeration_reason,
            Some(download::FullEnumerationReason::DownloadConfigHashDrift),
            "the first quiet cycle is a labeled config reconciliation, not cadence proof"
        );
    }
    drop(db);

    let db = Arc::new(
        SqliteStateDb::open_owned(&db_path, &baseline_owner())
            .await
            .unwrap(),
    );
    let (second_quiet, second_full) =
        run_c11_quiet(Arc::clone(&db), &root, &shared, "c11-quiet-2").await;
    assert_eq!(second_full, 0, "reopened quiet cycle must be incremental");
    assert_eq!(second_quiet.stats.downloaded, 0, "{second_quiet:?}");
    assert_eq!(std::fs::read(&recorded).unwrap(), MEDIA);
}
