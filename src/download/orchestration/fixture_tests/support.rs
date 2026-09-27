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

pub(super) struct Harness {
    _dir: TempDir,
    db_path: PathBuf,
    db: Option<Arc<SqliteStateDb>>,
    pub config: DownloadConfig,
    pub server: MockServer,
}

impl Harness {
    pub async fn new() -> Self {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("state.db");
        let db = Arc::new(SqliteStateDb::open(&db_path).await.unwrap());
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
        let db = Arc::new(SqliteStateDb::open(&self.db_path).await.unwrap());
        self.config.state_db = Some(db.clone());
        self.db = Some(db);
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
