//! Historical certificates are evidence, never current-sync authority.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use base64::Engine as _;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::icloud::photos::inbox::ShadowCapture;
use crate::icloud::photos::{PhotoLibrary, PhotosSession};
use crate::state::db::account::AccountOwner;
use crate::state::{MetadataRewriteStore, SqliteStateDb, VersionSizeKey};
use crate::sync_cycle::{CycleResult, ENUM_CONFIG_HASH_KEY, run_cycle};
use crate::sync_loop::test_support::{
    RunCycleDownloadConfigOptions, album_count_response, full_album_page_with_download,
    make_full_album_with_boxed_session, make_run_cycle_config,
    make_run_cycle_download_config_builder_with_options, make_run_cycle_library_state_with_album,
    make_shared_session_for_run_cycle,
};
use crate::{config, download, state};

const ZONE: &str = "PrimarySync";
const CURSOR: &str = "sync_token:PrimarySync";
const BYTES: &[u8] = include_bytes!("../../../../tests/data/media/pattern.jpg");
const MOTION: &[u8] = b"\0\0\0\x14ftypqt  \0\0\0\0qt  ";

#[derive(Clone, Default)]
struct Session {
    zone: String,
    records: Vec<Value>,
    delta: Option<Vec<Value>>,
    reject: bool,
    cancel: Option<CancellationToken>,
    trace: Arc<Mutex<Vec<String>>>,
}

#[async_trait::async_trait]
impl PhotosSession for Session {
    async fn post(&self, url: &str, body: String, _: &[(&str, &str)]) -> anyhow::Result<Value> {
        let body: Value = serde_json::from_str(&body)?;
        if url.contains("/changes/zone?") {
            let cursor = body["zones"][0]["syncToken"].as_str();
            self.trace
                .lock()
                .unwrap()
                .push(format!("delta:{}", cursor.unwrap_or("none")));
            if let Some(cancel) = &self.cancel {
                cancel.cancel();
            }
            return Ok(
                json!({"zones":[{"zoneID":{"zoneName":self.zone,"ownerRecordName":"_defaultOwner"},
                "syncToken":"after", "moreComing": if self.reject {Value::Null} else {json!(false)},
                "records": self.delta.as_ref().unwrap_or_else(|| if cursor == Some("before") {&self.records} else {&EMPTY})}]}),
            );
        }
        if url.contains("/records/lookup?") {
            self.trace.lock().unwrap().push("lookup".into());
            let requested = body["records"].as_array().unwrap();
            let records: Vec<_> = self
                .records
                .iter()
                .filter(|record| {
                    requested
                        .iter()
                        .any(|name| name["recordName"] == record["recordName"])
                })
                .collect();
            return Ok(json!({"records":records}));
        }
        if url.contains("/records/query/batch?") {
            self.trace.lock().unwrap().push("count".into());
            return Ok(album_count_response(
                self.records
                    .iter()
                    .filter(|record| record["recordType"] == "CPLAsset")
                    .count() as u64,
            ));
        }
        assert!(
            url.contains("/records/query?"),
            "unexpected synthetic endpoint"
        );
        self.trace.lock().unwrap().push("rank".into());
        let offset = body["query"]["filterBy"]
            .as_array()
            .unwrap()
            .iter()
            .find(|field| field["fieldName"] == "startRank")
            .unwrap()["fieldValue"]["value"]
            .as_u64()
            .unwrap();
        Ok(
            json!({"records":if offset == 0 {self.records.clone()} else {Vec::new()},"syncToken":"inventory"}),
        )
    }
    fn clone_box(&self) -> Box<dyn PhotosSession> {
        Box::new(self.clone())
    }
}
static EMPTY: Vec<Value> = Vec::new();

fn rows(database: &Path, sql: &str) -> Vec<Vec<rusqlite::types::Value>> {
    let conn = rusqlite::Connection::open(database).unwrap();
    let mut statement = conn.prepare(sql).unwrap();
    let count = statement.column_count();
    statement
        .query_map([], |row| (0..count).map(|index| row.get(index)).collect())
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

fn files(root: &Path) -> std::collections::BTreeMap<PathBuf, (Vec<u8>, std::time::SystemTime)> {
    let mut files = std::collections::BTreeMap::new();
    let mut directories = vec![root.to_path_buf()];
    while let Some(directory) = directories.pop() {
        for entry in std::fs::read_dir(directory).unwrap() {
            let path = entry.unwrap().path();
            let metadata = std::fs::metadata(&path).unwrap();
            if metadata.is_dir() {
                directories.push(path);
            } else {
                files.insert(
                    path.clone(),
                    (std::fs::read(&path).unwrap(), metadata.modified().unwrap()),
                );
            }
        }
    }
    files
}

struct Fixture {
    _root: tempfile::TempDir,
    database: PathBuf,
    media: PathBuf,
    original: PathBuf,
    sidecar: PathBuf,
    owner: AccountOwner,
    config: config::Config,
    session: Session,
    server: MockServer,
    second_zone: bool,
    collecting: bool,
    history: Vec<Vec<Vec<rusqlite::types::Value>>>,
}

const HISTORY: &[&str] = &[
    "SELECT * FROM unattributed_legacy ORDER BY library,asset_id",
    "SELECT * FROM unattributed_legacy_paths ORDER BY library,asset_id,path_key",
    "SELECT * FROM unattributed_legacy_proofs ORDER BY library,asset_id,generation",
    "SELECT * FROM assets WHERE id='historical-master'",
    "SELECT * FROM asset_metadata_paths WHERE id='historical-master'",
    "SELECT * FROM asset_metadata_capture_revisions WHERE asset_id='historical-master'",
    "SELECT * FROM metadata_capture_retries WHERE asset_id='historical-master'",
];

impl Fixture {
    async fn new(certificate: &str) -> Self {
        let root = tempfile::tempdir().unwrap();
        let database = root.path().join("owned.db");
        let media = root.path().join("media");
        std::fs::create_dir(&media).unwrap();
        let original = media.join("photo.jpg");
        let sidecar = media.join("photo.jpg.xmp");
        std::fs::write(&original, b"preserved historical original").unwrap();
        std::fs::write(&sidecar, b"private user sidecar, not Kei metadata").unwrap();
        let owner = AccountOwner::authenticated(
            "history@example.invalid",
            "com",
            &serde_json::from_value(json!({"dsInfo":{"dsid":"synthetic-history"}})).unwrap(),
        )
        .unwrap();
        let db = SqliteStateDb::open_owned(&database, &owner).await.unwrap();
        db.upsert_seen(
            &crate::test_helpers::TestAssetRecord::new("historical-master")
                .filename("photo.jpg")
                .build(),
        )
        .await
        .unwrap();
        db.mark_downloaded(
            ZONE,
            "historical-master",
            "original",
            &original,
            "historical-local",
            None,
        )
        .await
        .unwrap();
        db.set_metadata_capture_revision_for_test(ZONE, "historical-master", 0);
        db.begin_metadata_capture_revision(ZONE, state::METADATA_CAPTURE_REVISION)
            .await
            .unwrap();
        for id in ["historical-child-a", "historical-child-b"] {
            db.upsert_asset_master_mapping(ZONE, id, "historical-master")
                .await
                .unwrap();
        }
        let candidate = db
            .get_metadata_capture_candidates(ZONE, state::METADATA_CAPTURE_REVISION, 10)
            .await
            .unwrap()
            .remove(0);
        assert!(
            db.defer_metadata_capture_ambiguity(&candidate, state::METADATA_CAPTURE_REVISION)
                .await
                .unwrap()
        );
        {
            let conn = db.acquire_lock("initial historical evidence").unwrap();
            let files: Vec<_> = [&original, &sidecar]
                .into_iter()
                .map(|path| state::LegacyFileEvidence {
                    root: media.clone(),
                    path: path.clone(),
                    sha256: Some(
                        data_encoding::HEXLOWER
                            .encode(&Sha256::digest(std::fs::read(path).unwrap())),
                    ),
                    size: Some(std::fs::metadata(path).unwrap().len()),
                })
                .collect();
            conn.execute(
                "INSERT INTO unattributed_legacy VALUES(?1,'historical-master',1,?2,?3,7,?4)",
                rusqlite::params![
                    ZONE,
                    "retained original evidence: exact bytes",
                    serde_json::to_string(&files).unwrap(),
                    if certificate == "missing" {
                        None
                    } else {
                        Some(7)
                    }
                ],
            )
            .unwrap();
            for file in files {
                conn.execute(
                    "INSERT INTO unattributed_legacy_paths VALUES(?1,'historical-master',?2,?3)",
                    rusqlite::params![
                        ZONE,
                        crate::fs_util::confined_path_key(&file.path).unwrap(),
                        file.path.to_str().unwrap()
                    ],
                )
                .unwrap();
            }
            if certificate != "missing" {
                // Simulate retained evidence from a future writer without
                // changing the schema or relaxing production validation.
                conn.pragma_update(None, "ignore_check_constraints", true)
                    .unwrap();
                conn.execute("INSERT INTO unattributed_legacy_proofs VALUES(?1,'historical-master',7,?2,'obsolete dependencies','opaque historical provider bytes','old configuration','old cursor','old successor',7)", rusqlite::params![ZONE,if certificate == "future" {99} else {1}]).unwrap();
                conn.pragma_update(None, "ignore_check_constraints", false)
                    .unwrap();
            }
        }
        db.set_metadata(CURSOR, "before").await.unwrap();
        drop(db);
        let history = HISTORY.iter().map(|sql| rows(&database, sql)).collect();
        let server = MockServer::start().await;
        let mut records = Vec::new();
        let checksum = base64::engine::general_purpose::STANDARD.encode(Sha256::digest(BYTES));
        for master in ["current", "healthy"] {
            let mut page = full_album_page_with_download(
                ZONE,
                master,
                "after",
                &format!("{}/{master}.jpg", server.uri()),
                BYTES.len() as u64,
                &checksum,
            );
            page["records"][0]["fields"]["filenameEnc"] =
                json!({"value":if master == "current" {"photo.jpg"} else {"healthy.jpg"}});
            if master == "current" {
                page["records"][0]["fields"]["resOriginalVidComplRes"] = json!({"value":{"downloadURL":format!("{}/motion.mov",server.uri()),"size":MOTION.len(),"fileChecksum":base64::engine::general_purpose::STANDARD.encode(Sha256::digest(MOTION))}});
                page["records"][0]["fields"]["resOriginalVidComplFileType"] =
                    json!({"value":"com.apple.quicktime-movie"});
            }
            records.extend(page["records"].as_array().unwrap().clone());
        }
        let fixture = Self {
            _root: root,
            database,
            media,
            original,
            sidecar,
            owner,
            config: make_run_cycle_config(),
            session: Session {
                zone: ZONE.into(),
                records,
                ..Default::default()
            },
            server,
            second_zone: false,
            collecting: false,
            history,
        };
        fixture.responses(false).await;
        fixture
    }

    async fn responses(&self, fail_current: bool) {
        self.server.reset().await;
        for (route, bytes) in [
            ("current.jpg", BYTES),
            ("healthy.jpg", BYTES),
            ("motion.mov", MOTION),
        ] {
            Mock::given(method("GET"))
                .and(path(format!("/{route}")))
                .respond_with(
                    ResponseTemplate::new(if fail_current && route == "current.jpg" {
                        500
                    } else {
                        200
                    })
                    .set_body_bytes(bytes),
                )
                .mount(&self.server)
                .await;
        }
    }
    async fn open(&self) -> Arc<SqliteStateDb> {
        Arc::new(
            SqliteStateDb::open_owned(&self.database, &self.owner)
                .await
                .unwrap(),
        )
    }
    fn assert_history(&self) {
        assert_eq!(
            HISTORY
                .iter()
                .map(|sql| rows(&self.database, sql))
                .collect::<Vec<_>>(),
            self.history
        );
        assert_eq!(
            std::fs::read(&self.sidecar).unwrap(),
            b"private user sidecar, not Kei metadata"
        );
    }
    async fn cycle(
        &self,
        cancel: CancellationToken,
        stale: bool,
        second_zone: bool,
    ) -> CycleResult {
        self.try_cycle(cancel, stale, second_zone).await.unwrap()
    }
    async fn try_cycle(
        &self,
        cancel: CancellationToken,
        stale: bool,
        second_zone: bool,
    ) -> anyhow::Result<CycleResult> {
        let db = self.open().await;
        let capture = ShadowCapture::new(db.clone(), self.owner.clone(), "com");
        let mut album = make_full_album_with_boxed_session(ZONE, Box::new(self.session.clone()));
        album.set_shadow_capture(capture.clone(), Arc::from("private"));
        let mut selected = make_run_cycle_library_state_with_album(ZONE, CURSOR, album);
        if self.collecting {
            // The empty named album selects collecting; current records remain
            // unfiled and retain their existing destination.
            selected.plan.passes.push(crate::commands::AlbumPass {
                kind: crate::commands::PassKind::Album,
                album: crate::icloud::photos::PhotoAlbum::new(
                    crate::icloud::photos::PhotoAlbumConfig {
                        params: Arc::new(std::collections::HashMap::new()),
                        service_endpoint: Arc::from("https://example.invalid"),
                        name: Arc::from("Empty"),
                        list_type: Arc::from("CPLAssetAndMasterByAssetDateWithoutHiddenOrDeleted"),
                        obj_type: Arc::from("CPLAssetByAssetDateWithoutHiddenOrDeleted"),
                        query_filter: None,
                        page_size: 100,
                        zone_id: Arc::new(json!({"zoneName":ZONE})),
                        retry_config: crate::retry::RetryConfig::default(),
                        container_id: Some(Arc::from("empty")),
                        cross_zone_sources: Vec::new(),
                    },
                    Box::new(self.session.clone()),
                ),
                exclude_ids: Arc::new(rustc_hash::FxHashSet::default()),
            });
            selected.pass_scope.include_albums = true;
        }
        selected.library = PhotoLibrary::new_stub_with_zone(Box::new(self.session.clone()), ZONE);
        selected.library.shadow_capture = Some(capture);
        selected.plan_is_stale = stale;
        let second_records = full_album_page_with_download(
            "SharedSync-HISTORY",
            "second-current",
            "after",
            &format!("{}/healthy.jpg", self.server.uri()),
            BYTES.len() as u64,
            &base64::engine::general_purpose::STANDARD.encode(Sha256::digest(BYTES)),
        )["records"]
            .as_array()
            .unwrap()
            .clone();
        let other_session = Session {
            zone: "SharedSync-HISTORY".into(),
            records: second_records,
            ..Default::default()
        };
        let mut other = make_run_cycle_library_state_with_album(
            "SharedSync-HISTORY",
            "sync_token:SharedSync-HISTORY",
            make_full_album_with_boxed_session(
                "SharedSync-HISTORY",
                Box::new(other_session.clone()),
            ),
        );
        other.library =
            PhotoLibrary::new_stub_with_zone(Box::new(other_session), "SharedSync-HISTORY");
        let libraries = if second_zone {
            vec![&selected, &other]
        } else {
            vec![&selected]
        };
        let store = db.clone() as Arc<dyn download::DownloadStore>;
        let builder = make_run_cycle_download_config_builder_with_options(
            &self.media,
            store.clone(),
            RunCycleDownloadConfigOptions {
                concurrent_downloads: Some(1),
                #[cfg(feature = "xmp")]
                xmp_sidecar: self.config.metadata.xmp_sidecar,
                ..Default::default()
            },
        );
        let build = |mode, excluded, groups, library| {
            let mut built = builder(mode, excluded, groups, library);
            Arc::make_mut(&mut built).retry.max_retries = 0;
            built
        };
        let (_session_dir, session) = make_shared_session_for_run_cycle().await;
        let result = Box::pin(run_cycle(
            &libraries,
            &self.config,
            Some(store.as_ref()),
            false,
            &build,
            download::DownloadControls::download_hidden(),
            &session,
            &cancel,
        ))
        .await;
        self.assert_history();
        result
    }
    async fn healthy(&self) {
        let result = self.cycle(CancellationToken::new(), false, false).await;
        assert_eq!(result.failed_count, 0, "{result:?}");
        assert!(result.can_advance_database_checkpoint(), "{result:?}");
        assert_eq!(result.stats.unattributed_legacy_assets, 1);
        assert_eq!(
            result.stats.unattributed_legacy_pending,
            usize::from(self.history[2].is_empty()) as u64
        );
        let db = self.open().await;
        let capture = db
            .get_summary()
            .await
            .unwrap()
            .metadata_capture
            .into_iter()
            .find(|capture| capture.library == ZONE)
            .unwrap();
        assert_eq!(capture.pending_revision, None);
        assert_eq!(capture.remaining_assets, 0);
        assert!(
            !db.has_metadata_capture_work(&[ZONE], state::METADATA_CAPTURE_REVISION)
                .await
                .unwrap()
        );
        assert_eq!(
            db.get_metadata(CURSOR).await.unwrap().as_deref(),
            Some("after")
        );
        let downloaded = db.get_downloaded_page(0, 100).await.unwrap();
        for id in ["asset-current", "asset-healthy"] {
            for row in downloaded.iter().filter(|row| row.id.as_ref() == id) {
                let file = row.local_path.as_ref().unwrap();
                assert_ne!(file, &self.original);
                assert_eq!(
                    std::fs::read(file).unwrap(),
                    if row.version_size == VersionSizeKey::LiveOriginal {
                        MOTION
                    } else {
                        BYTES
                    }
                );
                assert!(row.local_checksum.is_some());
            }
        }
        assert_eq!(
            downloaded
                .iter()
                .filter(|row| row.id.as_ref() == "asset-current")
                .count(),
            2
        );
        assert_eq!(
            downloaded
                .iter()
                .filter(|row| row.id.as_ref() == "asset-healthy")
                .count(),
            1
        );
    }
    async fn quiet_twice(&self) {
        let before = rows(
            &self.database,
            "SELECT library,id,version_size,status,is_deleted,local_path,local_checksum,download_checksum,downloaded_at,download_attempts,last_error FROM assets ORDER BY library,id,version_size",
        );
        let receipts = rows(
            &self.database,
            "SELECT * FROM asset_metadata_paths ORDER BY library,id,version_size,local_path",
        );
        let media = files(&self.media);
        let checkpoints = rows(
            &self.database,
            "SELECT key,value FROM metadata WHERE key LIKE 'sync_token:%' OR key IN ('enum_config_hash','pending_enum_config_hash','config_hash','pending_download_config_hash') ORDER BY key",
        );
        let requests = self.server.received_requests().await.unwrap().len();
        for _ in 0..2 {
            self.session.trace.lock().unwrap().clear();
            let result = self
                .cycle(CancellationToken::new(), false, self.second_zone)
                .await;
            assert_eq!(result.failed_count, 0, "{result:?}");
            assert_eq!(result.stats.downloaded, 0);
            assert_eq!(
                self.server.received_requests().await.unwrap().len(),
                requests
            );
            assert_eq!(*self.session.trace.lock().unwrap(), ["delta:after"]);
            assert_eq!(
                rows(
                    &self.database,
                    "SELECT library,id,version_size,status,is_deleted,local_path,local_checksum,download_checksum,downloaded_at,download_attempts,last_error FROM assets ORDER BY library,id,version_size"
                ),
                before
            );
            assert_eq!(
                rows(
                    &self.database,
                    "SELECT * FROM asset_metadata_paths ORDER BY library,id,version_size,local_path"
                ),
                receipts
            );
            assert_eq!(
                files(&self.media),
                media,
                "all media and sidecars stay byte/path/timestamp stable"
            );
            assert_eq!(
                rows(
                    &self.database,
                    "SELECT key,value FROM metadata WHERE key LIKE 'sync_token:%' OR key IN ('enum_config_hash','pending_enum_config_hash','config_hash','pending_download_config_hash') ORDER BY key"
                ),
                checkpoints
            );
            let db = self.open().await;
            assert_eq!(db.get_summary().await.unwrap().pending, 0);
            assert_eq!(db.get_summary().await.unwrap().failed, 0);
            assert_eq!(
                db.get_metadata(CURSOR).await.unwrap().as_deref(),
                Some("after")
            );
        }
    }
}

#[tokio::test]
async fn historical_certificates_missing_stale_and_future_do_not_gate_current_live_photos() {
    for certificate in ["missing", "stale", "future"] {
        let f = Fixture::new(certificate).await;
        f.healthy().await;
        assert_eq!(f.server.received_requests().await.unwrap().len(), 3);
        f.quiet_twice().await;
    }
}

#[tokio::test]
async fn historical_changed_or_missing_originals_do_not_recertify_or_release_reserved_names() {
    for missing in [false, true] {
        let f = Fixture::new("stale").await;
        f.healthy().await;
        if missing {
            std::fs::remove_file(&f.original).unwrap();
        } else {
            std::fs::write(&f.original, b"external historical edit").unwrap();
        }
        let db = f.open().await;
        assert_eq!(
            crate::sync_loop::reconcile::run_bounded_local_drift_probe(db.as_ref(), 1)
                .await
                .marked_failed,
            0,
            "historical drift cannot create current retry work"
        );
        drop(db);
        f.quiet_twice().await;
        assert_eq!(f.original.exists(), !missing);
        if !missing {
            assert_eq!(
                std::fs::read(&f.original).unwrap(),
                b"external historical edit"
            );
        }
        let db = f.open().await;
        assert!(
            db.protected_legacy_paths()
                .await
                .unwrap()
                .contains(&f.original)
        );
    }
}

#[tokio::test]
async fn historical_records_allow_current_config_activation_in_same_and_second_zone() {
    for second in [false, true] {
        let mut f = Fixture::new("missing").await;
        f.second_zone = second;
        let db = f.open().await;
        db.set_metadata(ENUM_CONFIG_HASH_KEY, "previous-current-selection")
            .await
            .unwrap();
        if second {
            db.set_metadata("sync_token:SharedSync-HISTORY", "before")
                .await
                .unwrap();
        }
        drop(db);
        f.session.delta = Some(Vec::new());
        let result = f.cycle(CancellationToken::new(), false, second).await;
        assert_eq!(result.failed_count, 0, "{result:?}");
        assert!(result.can_advance_database_checkpoint(), "{result:?}");
        let db = f.open().await;
        assert_eq!(
            db.get_metadata(ENUM_CONFIG_HASH_KEY).await.unwrap(),
            Some(download::compute_config_hash(&f.config))
        );
        assert!(
            db.get_metadata("pending_enum_config_hash")
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(
            db.get_metadata(CURSOR).await.unwrap().as_deref(),
            Some("after")
        );
        if second {
            assert_eq!(
                db.get_metadata("sync_token:SharedSync-HISTORY")
                    .await
                    .unwrap()
                    .as_deref(),
                Some("after")
            );
        }
        if second {
            assert!(
                db.get_downloaded_page(0, 100)
                    .await
                    .unwrap()
                    .iter()
                    .any(|row| row.library.as_ref() == "SharedSync-HISTORY"
                        && row.id.as_ref() == "asset-second-current")
            );
        }
        drop(db);
        f.session.delta = None;
        f.quiet_twice().await;
    }
}

#[tokio::test]
async fn historical_records_do_not_hide_explicit_child_or_master_family_deletion() {
    for master in [false, true] {
        let mut f = Fixture::new("future").await;
        f.healthy().await;
        let db = f.open().await;
        let files: Vec<_> = db
            .get_downloaded_page(0, 100)
            .await
            .unwrap()
            .into_iter()
            .filter(|row| row.id.as_ref() == "asset-current")
            .map(|row| row.local_path.unwrap())
            .collect();
        drop(db);
        f.session.delta = Some(vec![
            json!({"recordName":if master {"current"} else {"asset-current"},"recordType":if master {"CPLMaster"} else {"CPLAsset"},"deleted":true}),
        ]);
        let result = f.cycle(CancellationToken::new(), false, false).await;
        assert_eq!(result.failed_count, 0, "{result:?}");
        assert_eq!(
            rows(
                &f.database,
                "SELECT status,is_deleted FROM assets WHERE id='asset-current' ORDER BY version_size"
            ),
            vec![
                vec![
                    rusqlite::types::Value::Text("downloaded".into()),
                    rusqlite::types::Value::Integer(1)
                ];
                2
            ],
            "master={master}; trace={:?}; {result:?}",
            f.session.trace.lock().unwrap()
        );
        for file in files {
            assert!(!std::fs::read(file).unwrap().is_empty());
        }
        f.session.delta = None;
        f.quiet_twice().await;
    }
}

#[tokio::test]
async fn historical_records_retain_current_transfer_finalization_and_missing_file_failures() {
    for fault in ["transfer", "finalization", "missing"] {
        let mut f = Fixture::new("missing").await;
        if fault != "missing" {
            f.collecting = true;
            f.config.filters.selection.albums = crate::selection::AlbumSelector::Named {
                included: ["Empty".to_owned()].into_iter().collect(),
                excluded: Default::default(),
            };
            f.config.filters.selection.albums_explicit = true;
            let mut observations = f.session.records.clone();
            observations.insert(2, observations[1].clone());
            f.session.delta = Some(observations);
            let db = f.open().await;
            db.upsert_album_container(ZONE, "empty", "Empty", "album")
                .await
                .unwrap();
            let snapshot = db
                .start_album_membership_snapshot(ZONE, "empty", None)
                .await
                .unwrap();
            db.complete_album_membership_snapshot(ZONE, "empty", snapshot)
                .await
                .unwrap();
            db.set_metadata(
                ENUM_CONFIG_HASH_KEY,
                &download::compute_config_hash(&f.config),
            )
            .await
            .unwrap();
            let store = db.clone() as Arc<dyn download::DownloadStore>;
            let builder = make_run_cycle_download_config_builder_with_options(
                &f.media,
                store,
                RunCycleDownloadConfigOptions {
                    concurrent_downloads: Some(1),
                    ..Default::default()
                },
            );
            let probe = builder(
                download::SyncMode::Full,
                Arc::new(Default::default()),
                Arc::new(download::AssetGroupings::default()),
                Arc::from(ZONE),
            );
            db.set_metadata("config_hash", &download::hash_download_config(&probe))
                .await
                .unwrap();
        }
        if fault == "missing" {
            f.healthy().await;
            let db = f.open().await;
            let file = db
                .get_downloaded_page(0, 100)
                .await
                .unwrap()
                .into_iter()
                .find(|row| {
                    row.id.as_ref() == "asset-current"
                        && row.version_size == VersionSizeKey::Original
                })
                .unwrap()
                .local_path
                .unwrap();
            std::fs::remove_file(file).unwrap();
            assert_eq!(
                crate::sync_loop::reconcile::run_bounded_local_drift_probe(db.as_ref(), 1)
                    .await
                    .marked_failed,
                1
            );
        }
        if fault == "finalization" {
            rusqlite::Connection::open(&f.database).unwrap().execute_batch("CREATE TRIGGER fail_current_finalization BEFORE UPDATE OF status ON assets WHEN NEW.id='asset-current' AND NEW.version_size='original' AND NEW.status='downloaded' BEGIN SELECT RAISE(ABORT,'synthetic current finalization failure'); END;").unwrap();
        } else {
            f.responses(true).await;
        }
        let result = f.cycle(CancellationToken::new(), false, false).await;
        assert!(result.failed_count > 0, "{fault}: {result:?}");
        if fault != "missing" {
            assert!(
                result.stats.full_enumeration_reason.is_none(),
                "duplicate failure must use collecting incremental: {result:?}"
            );
            let current_gets = f
                .server
                .received_requests()
                .await
                .unwrap()
                .iter()
                .filter(|request| request.url.path() == "/current.jpg")
                .count();
            assert_eq!(
                current_gets, 1,
                "{fault}: duplicate input must not create a second transfer"
            );
        }
        let db = f.open().await;
        assert!(db.get_summary().await.unwrap().failed > 0, "{fault}");
        assert!(
            db.get_downloaded_page(0, 100)
                .await
                .unwrap()
                .iter()
                .any(|row| row.id.as_ref() == "asset-healthy")
        );
        drop(db);
        if fault == "finalization" {
            rusqlite::Connection::open(&f.database)
                .unwrap()
                .execute_batch("DROP TRIGGER fail_current_finalization;")
                .unwrap();
        }
        f.responses(false).await;
        f.session.delta = None;
        f.healthy().await;
        if fault != "missing" {
            assert_eq!(rows(&f.database, "SELECT local_path FROM asset_metadata_paths WHERE id='asset-current' AND version_size='original'").len(), 1, "{fault}: retry has exactly one current receipt");
        }
        f.quiet_twice().await;
    }
}

#[cfg(feature = "xmp")]
#[tokio::test]
async fn historical_records_retain_selected_metadata_failure_until_quiet_retry() {
    let mut f = Fixture::new("stale").await;
    f.healthy().await;
    let db = f.open().await;
    let current = db
        .get_downloaded_page(0, 100)
        .await
        .unwrap()
        .into_iter()
        .find(|row| {
            row.id.as_ref() == "asset-current" && row.version_size == VersionSizeKey::Original
        })
        .unwrap()
        .local_path
        .unwrap();
    drop(db);
    let mut sidecar = current.as_os_str().to_owned();
    sidecar.push(".xmp");
    let sidecar = PathBuf::from(sidecar);
    std::fs::write(&sidecar, b"unowned conflicting packet").unwrap();
    f.config.metadata.xmp_sidecar = true;
    f.session.records[1]["fields"]["isFavorite"] = json!({"value":1});
    f.session.delta = Some(f.session.records.clone());
    let result = f.cycle(CancellationToken::new(), false, false).await;
    assert!(result.failed_count > 0, "{result:?}");
    assert_eq!(std::fs::read(&current).unwrap(), BYTES);
    assert_eq!(
        std::fs::read(&sidecar).unwrap(),
        b"unowned conflicting packet"
    );
    assert!(!rows(&f.database,"SELECT id FROM assets WHERE id='asset-current' AND metadata_write_failed_at IS NOT NULL").is_empty());
    std::fs::remove_file(&sidecar).unwrap();
    f.session.delta = None;
    f.healthy().await;
    assert!(rows(&f.database,"SELECT id FROM assets WHERE id='asset-current' AND metadata_write_failed_at IS NOT NULL").is_empty());
    assert!(sidecar.is_file());
    f.quiet_twice().await;
}

#[tokio::test]
async fn historical_records_do_not_relax_current_identity_enumeration_cancellation_or_stale_plan() {
    for fault in ["identity", "enumeration", "cancel", "stale"] {
        let mut f = Fixture::new("future").await;
        if fault == "identity" {
            f.session.delta = Some(vec![
                json!({"recordName":"unknown-current-child","recordType":"CPLAsset","fields":{}}),
            ]);
        }
        if fault == "enumeration" {
            f.session.reject = true;
        }
        let cancel = CancellationToken::new();
        if fault == "cancel" {
            f.session.cancel = Some(cancel.clone());
        }
        let result = f.try_cycle(cancel, fault == "stale", false).await;
        if fault == "enumeration" {
            assert!(
                result.is_err(),
                "malformed current page must be refused: {result:?}"
            );
        } else {
            assert!(
                !result.as_ref().unwrap().can_advance_database_checkpoint(),
                "{fault}: {result:?}"
            );
        }
        let db = f.open().await;
        assert_eq!(
            db.get_metadata(CURSOR).await.unwrap().as_deref(),
            Some("before")
        );
        if fault == "identity" {
            assert!(result.as_ref().unwrap().failed_count > 0);
            assert!(
                db.get_metadata(&state::unresolved_identity_key(ZONE))
                    .await
                    .unwrap()
                    .is_some()
            );
        }
        drop(db);
        f.session.delta = None;
        f.session.reject = false;
        f.session.cancel = None;
        if fault == "identity" {
            // Explicit deletion, not inventory absence, resolves the held source.
            let mut records = f.session.records.clone();
            records.push(json!({"recordName":"unknown-current-child","recordType":"CPLAsset","deleted":true}));
            f.session.delta = Some(records);
        }
        f.healthy().await;
        f.session.delta = None;
        f.quiet_twice().await;
    }
}

#[tokio::test]
async fn historical_records_preserve_checkpoint_and_configuration_transaction_atomicity() {
    for fault in ["cursor", "activation"] {
        let mut f = Fixture::new("stale").await;
        let db = f.open().await;
        if fault == "activation" {
            db.set_metadata(ENUM_CONFIG_HASH_KEY, "previous-selection")
                .await
                .unwrap();
            f.session.delta = Some(Vec::new());
        }
        drop(db);
        let sql = if fault == "cursor" {
            "CREATE TRIGGER fail_transition BEFORE UPDATE ON metadata WHEN NEW.key='sync_token:PrimarySync' BEGIN SELECT RAISE(ABORT,'synthetic cursor failure'); END;"
        } else {
            "CREATE TRIGGER fail_transition BEFORE UPDATE ON metadata WHEN NEW.key='enum_config_hash' BEGIN SELECT RAISE(ABORT,'synthetic activation failure'); END;"
        };
        rusqlite::Connection::open(&f.database)
            .unwrap()
            .execute_batch(sql)
            .unwrap();
        let result = f.cycle(CancellationToken::new(), false, false).await;
        assert!(!result.can_advance_database_checkpoint(), "{result:?}");
        let db = f.open().await;
        assert_eq!(
            db.get_metadata(CURSOR).await.unwrap().as_deref(),
            Some("before")
        );
        if fault == "activation" {
            assert_eq!(
                db.get_metadata(ENUM_CONFIG_HASH_KEY)
                    .await
                    .unwrap()
                    .as_deref(),
                Some("previous-selection")
            );
            assert!(
                db.get_metadata("pending_enum_config_hash")
                    .await
                    .unwrap()
                    .is_some()
            );
        }
        drop(db);
        rusqlite::Connection::open(&f.database)
            .unwrap()
            .execute_batch("DROP TRIGGER fail_transition;")
            .unwrap();
        f.session.delta = None;
        f.healthy().await;
        f.quiet_twice().await;
    }
}
