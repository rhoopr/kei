use std::path::{Path, PathBuf};
use std::sync::Arc;

use base64::Engine as _;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tempfile::TempDir;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::download::{DownloadConfig, DownloadOutcome};
use crate::state::SqliteStateDb;

use super::{SyncMode, cycle, fixture, records, sha256};

struct CurrentView {
    session: Box<dyn crate::icloud::photos::PhotosSession>,
    records: Vec<Value>,
}

#[async_trait::async_trait]
impl crate::icloud::photos::PhotosSession for CurrentView {
    async fn post(
        &self,
        url: &str,
        body: String,
        headers: &[(&str, &str)],
    ) -> anyhow::Result<Value> {
        if !url.contains("/records/lookup?") {
            return self.session.post(url, body, headers).await;
        }
        let request: Value = serde_json::from_str(&body)?;
        assert_eq!(
            request["zoneID"],
            json!({"zoneName":"PrimarySync", "ownerRecordName":"_defaultOwner"})
        );
        let names = request["records"].as_array().unwrap();
        let mut selected = Vec::new();
        for requested in names {
            let name = requested["recordName"].as_str().unwrap();
            assert!(
                !selected
                    .iter()
                    .any(|record: &Value| record["recordName"] == name)
            );
            selected.push(
                self.records
                    .iter()
                    .find(|record| record["recordName"] == name)
                    .expect("known synthetic record")
                    .clone(),
            );
        }
        Ok(json!({"records": selected}))
    }
    fn clone_box(&self) -> Box<dyn crate::icloud::photos::PhotosSession> {
        Box::new(Self {
            session: self.session.clone_box(),
            records: self.records.clone(),
        })
    }
}

pub(super) struct Harness {
    _dir: TempDir,
    db_path: PathBuf,
    db: Option<Arc<SqliteStateDb>>,
    owner: Option<crate::state::db::account::AccountOwner>,
    pub config: DownloadConfig,
    pub server: MockServer,
}

impl Harness {
    pub async fn new() -> Self {
        Self::new_with_owner(false).await
    }

    pub async fn new_owned() -> Self {
        Self::new_with_owner(true).await
    }

    async fn new_with_owner(owned: bool) -> Self {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("state.db");
        let owner = owned.then(|| {
            crate::state::db::account::AccountOwner::authenticated(
                "fixture@example.invalid",
                "com",
                &serde_json::from_value(json!({"dsInfo":{"dsid":"fixture"}})).unwrap(),
            )
            .unwrap()
        });
        let db = Arc::new(match &owner {
            Some(owner) => SqliteStateDb::open_owned(&db_path, owner).await.unwrap(),
            None => SqliteStateDb::open(&db_path).await.unwrap(),
        });
        let mut config = DownloadConfig::test_default();
        config.directory = Arc::from(dir.path().join("media"));
        config.folder_structure = String::new();
        config.folder_structure_albums = Arc::from("");
        config.state_db = Some(db.clone());
        config.sync_mode = SyncMode::Full;
        config.retry.max_retries = 0;
        Self {
            _dir: dir,
            db_path,
            db: Some(db),
            owner,
            config,
            server: MockServer::start().await,
        }
    }

    pub fn db(&self) -> &SqliteStateDb {
        self.db.as_ref().unwrap()
    }

    pub async fn reopen(&mut self) {
        self.config.state_db = None;
        drop(self.db.take());
        let db = Arc::new(match &self.owner {
            Some(owner) => SqliteStateDb::open_owned(&self.db_path, owner)
                .await
                .unwrap(),
            None => SqliteStateDb::open(&self.db_path).await.unwrap(),
        });
        self.config.state_db = Some(db.clone());
        self.db = Some(db);
    }

    pub async fn owned_cycle(&self, records: Vec<Value>) -> super::super::models::SyncResult {
        let count = records
            .iter()
            .filter(|record| record["recordType"] == "CPLMaster")
            .count() as u64;
        let current = records.clone();
        let session = crate::test_helpers::MockPhotosFlow::new()
            .album_count(count)
            .query_page(records, Some("fixture-token"))
            .empty_query_page(Some("fixture-token"))
            .build();
        let mut album = crate::icloud::photos::PhotoAlbum::new(
            crate::icloud::photos::PhotoAlbumConfig {
                params: Arc::new(std::collections::HashMap::new()),
                service_endpoint: Arc::from("https://example.com"),
                name: Arc::from(""),
                list_type: Arc::from("CPLAssetAndMasterByAssetDateWithoutHiddenOrDeleted"),
                obj_type: Arc::from("CPLAssetByAssetDateWithoutHiddenOrDeleted"),
                query_filter: None,
                page_size: 100,
                zone_id: Arc::new(
                    json!({"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"}),
                ),
                retry_config: crate::retry::RetryConfig::default(),
                container_id: None,
                cross_zone_sources: Vec::new(),
            },
            Box::new(CurrentView {
                session: Box::new(session),
                records: current,
            }),
        );
        album.set_shadow_capture(
            crate::icloud::photos::inbox::ShadowCapture::new(
                self.db.as_ref().unwrap().clone(),
                self.owner.as_ref().unwrap().clone(),
                "com",
            ),
            Arc::from("private"),
        );
        let pass = crate::commands::AlbumPass {
            kind: crate::commands::PassKind::Unfiled,
            album,
            exclude_ids: Arc::default(),
        };
        super::cycle_passes(&self.config, &[pass]).await
    }

    pub async fn resource(&self, endpoint: &str, source: &str, expected_requests: u64) -> Value {
        let bytes = fixture(source);
        Mock::given(method("GET"))
            .and(path(format!("/{endpoint}")))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(bytes.clone()))
            .expect(expected_requests)
            .mount(&self.server)
            .await;
        json!({"value": {"downloadURL": format!("{}/{endpoint}", self.server.uri()), "size": bytes.len(), "fileChecksum": base64::engine::general_purpose::STANDARD.encode(Sha256::digest(&bytes))}})
    }

    pub async fn asset(
        &self,
        id: &str,
        name: &str,
        uti: &str,
        source: &str,
        requests: u64,
    ) -> Vec<Value> {
        let bytes = fixture(source);
        let mut pair = records(name, uti, &self.server, &bytes);
        pair[0]["recordName"] = json!(id);
        pair[1]["recordName"] = json!(format!("asset-{id}"));
        pair[1]["fields"]["masterRef"]["value"]["recordName"] = json!(id);
        pair[0]["fields"]["resOriginalRes"] = self.resource(id, source, requests).await;
        pair
    }

    pub async fn assert_files(&self, expected: &[(&str, &str)]) {
        let rows = self.db().get_downloaded_page(0, 100).await.unwrap();
        assert_eq!(rows.len(), expected.len());
        let mut actual: Vec<_> = rows
            .iter()
            .map(|row| {
                row.local_path
                    .as_ref()
                    .unwrap()
                    .strip_prefix(&self.config.directory)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/")
            })
            .collect();
        actual.sort();
        let mut names: Vec<_> = expected.iter().map(|(name, _)| name.to_string()).collect();
        names.sort();
        assert_eq!(actual, names);
        for (name, source) in expected {
            let bytes = fixture(source);
            let path = self.config.directory.join(name);
            assert!(
                std::fs::read(&path).unwrap() == bytes,
                "changed fixture {name}"
            );
            let row = rows
                .iter()
                .find(|row| row.local_path.as_ref() == Some(&path))
                .unwrap();
            assert_eq!(
                row.download_checksum.as_deref(),
                Some(sha256(&bytes).as_str())
            );
            assert_eq!(row.local_checksum, row.download_checksum);
        }
        assert_eq!(
            files(&self.config.directory).len(),
            expected.len(),
            "unexpected temporary or sidecar file"
        );
    }

    pub async fn stable(&mut self, records: Vec<Value>, expected: &[(&str, &str)]) {
        let result = cycle(&self.config, records.clone()).await;
        assert!(
            matches!(result.outcome, DownloadOutcome::Success),
            "{result:?}"
        );
        assert_eq!(result.stats.downloaded, expected.len());
        self.assert_files(expected).await;
        self.reopen().await;
        let result = cycle(&self.config, records).await;
        assert!(
            matches!(result.outcome, DownloadOutcome::Success),
            "{result:?}"
        );
        assert_eq!(result.stats.downloaded, 0);
        self.assert_files(expected).await;
        assert!(self.db().get_failed().await.unwrap().is_empty());
        assert!(self.db().get_pending().await.unwrap().is_empty());
        self.server.verify().await;
    }
}

pub(super) fn files(root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    if root.is_dir() {
        for entry in std::fs::read_dir(root).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                files.extend(self::files(&path));
            } else {
                files.push(path);
            }
        }
    }
    files.sort();
    files
}
