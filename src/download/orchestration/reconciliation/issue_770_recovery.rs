//! Durable pending recovery retains recorded choices without changing reconciliation policy.
use super::{
    DownloadControls, DownloadOutcome, album_with_session, download_photos_with_sync,
    incremental_photo_records_with_url, reconcile_catalog_paths, test_config,
};
use crate::commands::{AlbumPass, PassKind};
use crate::download::planner::TaskPlanner;
use crate::download::retry::build_pending_retry_download_tasks;
use crate::download::{DownloadConfig, DownloadRunMode, file};
use crate::icloud::photos::{PhotoAsset, PhotosSession};
use crate::state::{ReconciliationContent, ReconciliationPathKey, ReconciliationReservation};
use crate::state::{ReconciliationStateStore, SqliteStateDb, VersionSizeKey};
use crate::test_helpers::TestAssetRecord;
use crate::types::AssetVersionSize;
use base64::Engine as _;
use chrono::{TimeZone, Utc};
use reqwest::Client;
use rustc_hash::FxHashSet;
use serde_json::Value;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::sync::Arc;
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn issue_770_pass(kind: PassKind, records: &[Value]) -> AlbumPass {
    AlbumPass {
        kind,
        album: album_with_session(
            "PrimarySync",
            if kind == PassKind::SmartFolder {
                "Hidden"
            } else if kind == PassKind::Album {
                "Album"
            } else {
                ""
            },
            Box::new(RecoveryLookupSession {
                records: Arc::new(records.to_vec()),
                enumerate: false,
            }),
        ),
        exclude_ids: Arc::new(FxHashSet::default()),
    }
}

#[derive(Clone)]
struct RecoveryLookupSession {
    records: Arc<Vec<Value>>,
    enumerate: bool,
}

#[async_trait::async_trait]
impl PhotosSession for RecoveryLookupSession {
    async fn post(
        &self,
        url: &str,
        body: String,
        _headers: &[(&str, &str)],
    ) -> anyhow::Result<Value> {
        if url.contains("/records/lookup?") {
            return Ok(json!({"records": self.records.as_ref()}));
        }
        if url.contains("/records/query/batch?") {
            let body: Value = serde_json::from_str(&body)?;
            let batch = body["batch"]
                .as_array()
                .unwrap()
                .iter()
                .map(|_| json!({"records": [{"fields": {"itemCount": {"value": usize::from(self.enumerate)}}}]}))
                .collect::<Vec<_>>();
            return Ok(json!({"batch": batch}));
        }
        if url.contains("/records/query?") {
            if self.enumerate {
                return Ok(
                    json!({"records": self.records.as_ref(), "syncToken": "enumerated-current-query"}),
                );
            }
            return Ok(json!({"records": [], "syncToken": "empty-current-query"}));
        }
        if url.contains("/changes/zone?") {
            return Ok(json!({"zones": [{"zoneID": {"zoneName": "PrimarySync"},
                "syncToken": "empty-current-delta", "moreComing": false, "records": []}]}));
        }
        anyhow::bail!("unexpected synthetic Photos request: {url}")
    }

    fn clone_box(&self) -> Box<dyn PhotosSession> {
        Box::new(self.clone())
    }
}

#[tokio::test]
async fn issue_770_recovery_enumerated_cross_parent_pending_cannot_certify_unproven_bytes() {
    let server = crate::start_wiremock_or_skip!();
    let fixture = RecoveryFixture::new(&server).await;
    let mut bytes = fixture.bytes.clone();
    *bytes.last_mut().unwrap() ^= 1;
    std::fs::create_dir_all(fixture.destination.parent().unwrap()).unwrap();
    std::fs::write(&fixture.destination, &bytes).unwrap();
    let mut config = fixture.config.clone();
    config.folder_structure = "old-album".into();
    let mut pass = issue_770_pass(PassKind::Unfiled, &fixture.records);
    pass.album = album_with_session(
        "PrimarySync",
        "",
        Box::new(RecoveryLookupSession {
            records: Arc::new(fixture.records.clone()),
            enumerate: true,
        }),
    );
    let asset = PhotoAsset::new(fixture.records[0].clone(), fixture.records[1].clone());
    let result = crate::download::pipeline::stream_and_download_from_stream(
        &Client::new(),
        futures_util::stream::iter(vec![Ok(asset)]),
        &Arc::new(config.with_pass(&pass)),
        DownloadControls::download_hidden(),
        1,
        CancellationToken::new(),
        crate::download::pipeline::StreamRuntime::new(None, None),
    )
    .await
    .unwrap();
    assert_eq!(result.assets_seen, 1, "must exercise nonempty enumeration");
    assert!(
        fixture
            .db
            .get_downloaded_page(0, 10)
            .await
            .unwrap()
            .is_empty(),
        "enumeration must not mint ownership proof for conflicting reserved bytes"
    );
    assert_eq!(std::fs::read(&fixture.destination).unwrap(), bytes);
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn issue_770_recovery_nonempty_source_enumeration_retains_unproven_debt_in_every_order() {
    for kinds in [
        vec![PassKind::SmartFolder, PassKind::Unfiled],
        vec![PassKind::Unfiled, PassKind::SmartFolder],
        vec![PassKind::Album, PassKind::SmartFolder, PassKind::Unfiled],
    ] {
        for conflicting in [false, true] {
            let server = crate::start_wiremock_or_skip!();
            let mut fixture = RecoveryFixture::new(&server).await;
            let mut bytes = fixture.bytes.clone();
            if conflicting {
                *bytes.last_mut().unwrap() ^= 1;
            }
            std::fs::create_dir_all(fixture.destination.parent().unwrap()).unwrap();
            std::fs::write(&fixture.destination, &bytes).unwrap();
            fixture.config.folder_structure = "old-album".into();
            fixture.config.folder_structure_albums = "old-album".into();
            for _ in 0..2 {
                fixture.reopen().await;
                let mut passes = fixture.passes(&kinds);
                for pass in &mut passes {
                    pass.album = album_with_session(
                        "PrimarySync",
                        &pass.album.name,
                        Box::new(RecoveryLookupSession {
                            records: Arc::new(fixture.records.clone()),
                            enumerate: true,
                        }),
                    );
                }
                let result = download_photos_with_sync(
                    &Client::new(),
                    &passes,
                    Arc::new(fixture.config.clone()),
                    DownloadControls::download_hidden(),
                    CancellationToken::new(),
                )
                .await
                .unwrap();
                assert!(
                    result.stats.assets_seen > 0,
                    "must enumerate source assets: {result:?}"
                );
                assert_eq!(result.stats.downloaded, 0, "{result:?}");
                assert!(
                    fixture
                        .db
                        .get_downloaded_page(0, 10)
                        .await
                        .unwrap()
                        .is_empty()
                );
                assert!(
                    !fixture.db.get_pending().await.unwrap().is_empty()
                        || !fixture.db.get_failed().await.unwrap().is_empty()
                );
                assert_eq!(std::fs::read(&fixture.destination).unwrap(), bytes);
                assert!(server.received_requests().await.unwrap().is_empty());
                fixture.assert_preserved().await;
            }
        }
    }
}

#[cfg(unix)]
#[tokio::test]
async fn issue_770_recovery_nonempty_enumeration_cannot_adopt_linked_reserved_files() {
    for ancestor in [false, true] {
        let server = crate::start_wiremock_or_skip!();
        let mut fixture = RecoveryFixture::new(&server).await;
        let outside = TempDir::new().unwrap();
        let outside_path = outside.path().join("pending.jpg");
        std::fs::write(&outside_path, &fixture.bytes).unwrap();
        if ancestor {
            std::os::unix::fs::symlink(outside.path(), fixture.destination.parent().unwrap())
                .unwrap();
        } else {
            std::fs::create_dir_all(fixture.destination.parent().unwrap()).unwrap();
            std::os::unix::fs::symlink(&outside_path, &fixture.destination).unwrap();
        }
        fixture.config.folder_structure = "old-album".into();
        for _ in 0..2 {
            fixture.reopen().await;
            let asset = PhotoAsset::new(fixture.records[0].clone(), fixture.records[1].clone());
            let result = crate::download::pipeline::stream_and_download_from_stream(
                &Client::new(),
                futures_util::stream::iter(vec![Ok(asset)]),
                &Arc::new(fixture.config.clone()),
                DownloadControls::download_hidden(),
                1,
                CancellationToken::new(),
                crate::download::pipeline::StreamRuntime::new(None, None),
            )
            .await
            .unwrap();
            assert_eq!(result.assets_seen, 1);
            assert!(
                fixture
                    .db
                    .get_downloaded_page(0, 10)
                    .await
                    .unwrap()
                    .is_empty()
            );
            assert_eq!(std::fs::read(&outside_path).unwrap(), fixture.bytes);
            assert!(server.received_requests().await.unwrap().is_empty());
            fixture.assert_preserved().await;
        }
    }
}

fn expected_hidden_original_request_key(config: &DownloadConfig, records: &[Value]) -> String {
    let asset = PhotoAsset::new(
        records.first().expect("fixture master").clone(),
        records.get(1).expect("fixture child").clone(),
    )
    .with_state_record_name(Arc::from("PENDING"));
    let hidden = issue_770_pass(PassKind::SmartFolder, records);
    let requested = crate::download::filter::expected_paths_for(&asset, &config.with_pass(&hidden))
        .into_iter()
        .find(|path| path.version_size == VersionSizeKey::Original)
        .expect("fixture original Hidden request");
    crate::fs_util::confined_path_key(&requested.path).unwrap()
}

async fn issue_770_seed(root: &TempDir) -> (DownloadConfig, Vec<Value>, std::path::PathBuf) {
    let records = incremental_photo_records_with_url(
        "PENDING",
        "pending.jpg",
        "http://127.0.0.1:9/never-requested.jpg",
        1024,
    );
    issue_770_seed_records(root, records).await
}

async fn issue_770_seed_records(
    root: &TempDir,
    records: Vec<Value>,
) -> (DownloadConfig, Vec<Value>, std::path::PathBuf) {
    let db_path = root.path().join("state.db");
    let db = Arc::new(SqliteStateDb::open(&db_path).await.unwrap());
    let asset = PhotoAsset::new(records[0].clone(), records[1].clone());
    let version = asset
        .versions()
        .iter()
        .find(|(version, _)| *version == AssetVersionSize::Original)
        .unwrap()
        .1
        .clone();
    let old_path = root.path().join("old-album/pending.jpg");
    db.upsert_seen(
        &TestAssetRecord::new("PENDING")
            .filename("pending.jpg")
            .checksum(&version.checksum)
            .size(version.size)
            .added_at(Utc.timestamp_opt(1_700_000_000, 0).unwrap())
            .build(),
    )
    .await
    .unwrap();
    db.mark_downloaded(
        "PrimarySync",
        "PENDING",
        "original",
        &old_path,
        "historical-local-checksum",
        None,
    )
    .await
    .unwrap();
    db.mark_failed(
        "PrimarySync",
        "PENDING",
        "original",
        "synthetic earlier transfer failure",
    )
    .await
    .unwrap();
    db.prepare_for_retry(
        Some("PrimarySync"),
        crate::state::RetryErrorRetention::Clear,
    )
    .await
    .unwrap();
    db.upsert_asset_master_mapping("PrimarySync", "asset-PENDING", "PENDING")
        .await
        .unwrap();
    db.set_metadata("sync_token:PrimarySync", "checkpoint-before")
        .await
        .unwrap();
    db.set_metadata("config_hash", "active-before")
        .await
        .unwrap();
    db.set_metadata("pending_download_config_hash", "staged-after")
        .await
        .unwrap();
    // A valid unrelated choice activates the existing reserved-download mode.
    let held = root.path().join("held.jpg");
    let key = crate::fs_util::confined_path_key(&held).unwrap();
    db.reserve_reconciliation_paths(&[ReconciliationReservation {
        library: Arc::from("PrimarySync"),
        asset_id: "UNRELATED".into(),
        version_size: VersionSizeKey::Original,
        content: Some(ReconciliationContent {
            checksum: "held-content".into(),
            size: 1024,
        }),
        requested_path_key: ReconciliationPathKey(key.clone()),
        destination_path_key: ReconciliationPathKey(key),
        destination_path: held,
    }])
    .await
    .unwrap();
    let mut config = test_config();
    config.directory = Arc::from(root.path());
    config.folder_structure = "%Y/%m".into();
    config.folder_structure_smart_folders = Arc::from("Hidden/%Y/%m");
    config.file_match_policy = crate::types::FileMatchPolicy::NameId7;
    config.state_db = Some(db);
    (config, records, old_path)
}

#[tokio::test]
async fn issue_770_recovery_recorded_choice_replays_across_sqlite_reopen() {
    let root = TempDir::new().unwrap();
    let (mut config, records, old_path) = issue_770_seed(&root).await;
    let hidden = vec![issue_770_pass(PassKind::SmartFolder, &records)];
    let first = build_pending_retry_download_tasks(
        &hidden,
        &config,
        DownloadRunMode::Download,
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(first.tasks.len(), 1);
    assert_eq!(first.tasks[0].download_path, old_path);
    assert!(matches!(
        first.tasks[0].publication(),
        file::FinalPublication::NoReplace
    ));
    let db = config.state_db.take().unwrap();
    let before = db.get_reconciliation_reservations().await.unwrap();
    let reservation = before
        .iter()
        .find(|r| r.asset_id.as_ref() == "PENDING")
        .unwrap();
    assert_eq!(
        reservation.requested_path_key.0,
        expected_hidden_original_request_key(&config, &records)
    );
    assert_eq!(reservation.destination_path, old_path);
    assert!(!old_path.exists());
    drop(db);
    // No manual insertion of the problematic row: the retry owner wrote it.
    for cycle in 0..2 {
        config.state_db = Some(Arc::new(
            SqliteStateDb::open(&root.path().join("state.db"))
                .await
                .unwrap(),
        ));
        let replay = build_pending_retry_download_tasks(
            &hidden,
            &config,
            DownloadRunMode::Download,
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(replay.tasks.len(), 1);
        assert_eq!(replay.tasks[0].download_path, old_path);
        assert!(replay.unmatched_targets.is_empty());
        let db = config.state_db.take().unwrap();
        assert_eq!(db.get_reconciliation_reservations().await.unwrap(), before);
        let pending = db.get_pending().await.unwrap();
        assert_eq!(pending.len(), 1, "cycle {cycle}");
        assert_eq!(pending[0].local_path.as_ref(), Some(&old_path));
        assert!(!pending[0].metadata.is_hidden);
        assert_eq!(
            db.get_metadata("sync_token:PrimarySync")
                .await
                .unwrap()
                .as_deref(),
            Some("checkpoint-before")
        );
        assert_eq!(
            db.get_metadata("config_hash").await.unwrap().as_deref(),
            Some("active-before")
        );
        assert_eq!(
            db.get_metadata("pending_download_config_hash")
                .await
                .unwrap()
                .as_deref(),
            Some("staged-after")
        );
        assert!(!old_path.exists());
    }
}

#[tokio::test]
async fn issue_770_recovery_both_pass_orders_match_without_ledger_changes() {
    let root = TempDir::new().unwrap();
    let (mut config, records, old_path) = issue_770_seed(&root).await;
    let hidden = issue_770_pass(PassKind::SmartFolder, &records);
    build_pending_retry_download_tasks(
        std::slice::from_ref(&hidden),
        &config,
        DownloadRunMode::Download,
        CancellationToken::new(),
    )
    .await
    .unwrap();
    config.state_db = None;
    config.state_db = Some(Arc::new(
        SqliteStateDb::open(&root.path().join("state.db"))
            .await
            .unwrap(),
    ));
    let before = config
        .state_db
        .as_ref()
        .unwrap()
        .get_reconciliation_reservations()
        .await
        .unwrap();
    let unfiled = issue_770_pass(PassKind::Unfiled, &records);
    // Read-only modes must select the exact destination without changing the ledger.
    for passes in [
        vec![hidden.clone(), unfiled.clone()],
        vec![unfiled.clone(), hidden],
    ] {
        let replay = build_pending_retry_download_tasks(
            &passes,
            &config,
            DownloadRunMode::DryRun,
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(replay.tasks.len(), 1);
        assert!(replay.unmatched_targets.is_empty());
        assert_eq!(replay.tasks[0].download_path, old_path);
    }
    let control = build_pending_retry_download_tasks(
        &[unfiled],
        &config,
        DownloadRunMode::DryRun,
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(control.tasks.len(), 1);
    assert!(control.unmatched_targets.is_empty());
    assert_eq!(control.tasks[0].download_path, old_path);
    assert_eq!(
        config
            .state_db
            .as_ref()
            .unwrap()
            .get_reconciliation_reservations()
            .await
            .unwrap(),
        before
    );
    assert_eq!(
        config
            .state_db
            .as_ref()
            .unwrap()
            .get_pending()
            .await
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn issue_770_recovery_preserves_smart_selection_completeness_veto() {
    let root = TempDir::new().unwrap();
    let (config, records, old_path) = issue_770_seed(&root).await;
    std::fs::create_dir_all(old_path.parent().unwrap()).unwrap();
    std::fs::write(&old_path, vec![7u8; 1024]).unwrap();
    let local = file::compute_sha256(&old_path).await.unwrap();
    let db = config.state_db.as_ref().unwrap();
    db.mark_downloaded(
        "PrimarySync",
        "PENDING",
        "original",
        &old_path,
        &local,
        None,
    )
    .await
    .unwrap();
    let unfiled = issue_770_pass(PassKind::Unfiled, &records);
    let hidden = issue_770_pass(PassKind::SmartFolder, &records);
    for _ in 0..2 {
        let result = reconcile_catalog_paths(
            &[unfiled.clone(), hidden.clone()],
            Arc::new(config.clone()),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(!result.complete);
        assert!(result.complete_after_smart_query);
        assert_eq!(result.stats.failed, 0);
        assert_eq!(std::fs::read(&old_path).unwrap(), vec![7u8; 1024]);
    }
    let control = reconcile_catalog_paths(&[unfiled], Arc::new(config), CancellationToken::new())
        .await
        .unwrap();
    assert!(control.complete, "{control:?}");
    assert!(!control.complete_after_smart_query);
    assert_eq!(control.stats.failed, 0);
    assert_eq!(control.stats.downloaded, 0);
}

struct RecoveryFixture {
    root: TempDir,
    db: Arc<SqliteStateDb>,
    config: DownloadConfig,
    records: Vec<Value>,
    destination: std::path::PathBuf,
    ledger: Vec<ReconciliationReservation>,
    bytes: Vec<u8>,
}

enum PublicationProof {
    Download,
    Adoption,
}

impl RecoveryFixture {
    async fn new(server: &MockServer) -> Self {
        let bytes = include_bytes!("../../../../tests/data/media/pattern.jpg").to_vec();
        // MMCS identifies a generation and is deliberately not this file's SHA-256.
        let checksum = base64::engine::general_purpose::STANDARD.encode([0xa7u8; 32]);
        let mut records = incremental_photo_records_with_url(
            "PENDING",
            "pending.jpg",
            &format!("{}/pending.jpg", server.uri()),
            bytes.len() as u64,
        );
        records[0]["fields"]["resOriginalRes"]["value"]["fileChecksum"] = json!(checksum);
        let root = TempDir::new().unwrap();
        let (mut config, records, destination) = issue_770_seed_records(&root, records).await;
        let hidden = issue_770_pass(PassKind::SmartFolder, &records);
        let first = build_pending_retry_download_tasks(
            &[hidden],
            &config,
            DownloadRunMode::Download,
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(first.tasks.len(), 1);
        assert_eq!(first.tasks[0].download_path, destination);
        let expected_requested_key = expected_hidden_original_request_key(&config, &records);
        let ledger = config
            .state_db
            .as_ref()
            .unwrap()
            .get_reconciliation_reservations()
            .await
            .unwrap();
        assert!(ledger.iter().any(|row| row.asset_id.as_ref() == "PENDING"
            && row.requested_path_key.0 == expected_requested_key
            && row.destination_path == destination));
        let db = Arc::new(
            SqliteStateDb::open(&root.path().join("state.db"))
                .await
                .unwrap(),
        );
        config.state_db = Some(db.clone());
        Self {
            root,
            db,
            config,
            records,
            destination,
            ledger,
            bytes,
        }
    }

    async fn reopen(&mut self) {
        self.config.state_db = None;
        self.db = Arc::new(
            SqliteStateDb::open(&self.root.path().join("state.db"))
                .await
                .unwrap(),
        );
        self.config.state_db = Some(self.db.clone());
    }

    fn passes(&self, kinds: &[PassKind]) -> Vec<AlbumPass> {
        kinds
            .iter()
            .map(|kind| issue_770_pass(*kind, &self.records))
            .collect()
    }

    async fn run(&self, kinds: &[PassKind], token: CancellationToken) -> super::SyncResult {
        download_photos_with_sync(
            &Client::new(),
            &self.passes(kinds),
            Arc::new(self.config.clone()),
            DownloadControls::download_hidden(),
            token,
        )
        .await
        .unwrap()
    }

    async fn assert_preserved(&self) {
        let db = self.config.state_db.as_ref().unwrap();
        let ledger = db.get_reconciliation_reservations().await.unwrap();
        assert!(self.ledger.iter().all(|choice| ledger.contains(choice)));
        for added in ledger.iter().filter(|choice| !self.ledger.contains(choice)) {
            assert_eq!(added.asset_id.as_ref(), "PENDING");
            assert_eq!(added.library.as_ref(), "PrimarySync");
            assert_eq!(added.version_size, VersionSizeKey::Original);
            assert_eq!(added.destination_path, self.destination);
            assert_eq!(
                added.content,
                self.ledger
                    .iter()
                    .find(|choice| choice.asset_id.as_ref() == "PENDING")
                    .unwrap()
                    .content
            );
        }
        for (key, value) in [
            ("sync_token:PrimarySync", "checkpoint-before"),
            ("config_hash", "active-before"),
            ("pending_download_config_hash", "staged-after"),
        ] {
            assert_eq!(db.get_metadata(key).await.unwrap().as_deref(), Some(value));
        }
    }

    async fn assert_downloaded(&self, proof: PublicationProof) {
        let db = self.config.state_db.as_ref().unwrap();
        assert!(db.get_pending().await.unwrap().is_empty());
        assert!(db.get_failed().await.unwrap().is_empty());
        let rows = db.get_downloaded_page(0, 10).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].local_path.as_ref(), Some(&self.destination));
        let local = data_encoding::HEXLOWER.encode(&Sha256::digest(&self.bytes));
        assert_eq!(rows[0].local_checksum.as_deref(), Some(local.as_str()));
        let expected_download_checksum = match proof {
            PublicationProof::Download => Some(local.as_str()),
            PublicationProof::Adoption => None,
        };
        assert_eq!(
            rows[0].download_checksum.as_deref(),
            expected_download_checksum
        );
        let receipts = db.get_downloaded_path_records().await.unwrap();
        assert!(!receipts.is_empty());
        if matches!(proof, PublicationProof::Download) {
            assert_eq!(receipts.len(), 1);
        }
        for receipt in &receipts {
            // Equivalent root spellings can retain SQLite TEXT aliases for
            // one filesystem destination. They must never authorize another file.
            assert_eq!(
                crate::fs_util::confined_path_key(receipt.local_path.as_ref().unwrap()).unwrap(),
                crate::fs_util::confined_path_key(&self.destination).unwrap()
            );
            assert_eq!(receipt.library, "PrimarySync");
            assert_eq!(receipt.id, "PENDING");
            assert_eq!(receipt.version_size, VersionSizeKey::Original);
            assert_eq!(receipt.checksum, rows[0].checksum.as_ref());
            assert_eq!(receipt.local_checksum.as_deref(), Some(local.as_str()));
            assert_eq!(
                receipt.download_checksum.as_deref(),
                expected_download_checksum
            );
        }
        assert_eq!(std::fs::read(&self.destination).unwrap(), self.bytes);
        self.assert_preserved().await;
    }

    async fn receipt_snapshot(&self) -> Value {
        let rows = self
            .config
            .state_db
            .as_ref()
            .unwrap()
            .get_downloaded_path_records()
            .await
            .unwrap();
        Value::Array(rows.into_iter().map(|row| json!({
            "added_at": row.added_at, "is_current_path": row.is_current_path,
            "library": row.library, "id": row.id, "version_size": row.version_size.as_str(),
            "checksum": row.checksum, "local_path": row.local_path,
            "local_checksum": row.local_checksum, "download_checksum": row.download_checksum,
        })).collect())
    }
}

#[tokio::test]
async fn issue_770_recovery_publishes_exact_choice_and_reaches_quiet_restart() {
    for kinds in [
        vec![PassKind::SmartFolder, PassKind::Unfiled],
        vec![PassKind::Unfiled, PassKind::SmartFolder],
        vec![PassKind::Album, PassKind::SmartFolder, PassKind::Unfiled],
    ] {
        let server = crate::start_wiremock_or_skip!();
        let mut fixture = RecoveryFixture::new(&server).await;
        Mock::given(method("GET"))
            .and(path("/pending.jpg"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(fixture.bytes.clone()))
            .expect(1)
            .mount(&server)
            .await;
        let historical = fixture.root.path().join("historical.jpg");
        let sidecar = fixture.destination.with_extension("jpg.xmp");
        std::fs::write(&historical, b"retained earlier media").unwrap();
        std::fs::create_dir_all(sidecar.parent().unwrap()).unwrap();
        std::fs::write(&sidecar, b"custom unrelated metadata").unwrap();
        fixture.reopen().await;
        let result = fixture.run(&kinds, CancellationToken::new()).await;
        assert!(
            matches!(result.outcome, DownloadOutcome::Success),
            "{result:?}"
        );
        assert_eq!(result.stats.downloaded, 1);
        fixture.reopen().await;
        fixture.assert_downloaded(PublicationProof::Download).await;
        let completed_ledger = fixture
            .config
            .state_db
            .as_ref()
            .unwrap()
            .get_reconciliation_reservations()
            .await
            .unwrap();
        let receipt = fixture.receipt_snapshot().await;
        let mtime = std::fs::metadata(&fixture.destination)
            .unwrap()
            .modified()
            .unwrap();
        let quiet = fixture.run(&kinds, CancellationToken::new()).await;
        assert!(
            matches!(quiet.outcome, DownloadOutcome::Success),
            "{quiet:?}"
        );
        assert_eq!(quiet.stats.downloaded, 0);
        fixture.reopen().await;
        fixture.assert_downloaded(PublicationProof::Download).await;
        assert_eq!(
            fixture
                .config
                .state_db
                .as_ref()
                .unwrap()
                .get_reconciliation_reservations()
                .await
                .unwrap(),
            completed_ledger
        );
        assert_eq!(fixture.receipt_snapshot().await, receipt);
        assert_eq!(
            std::fs::metadata(&fixture.destination)
                .unwrap()
                .modified()
                .unwrap(),
            mtime
        );
        assert_eq!(
            std::fs::read(&historical).unwrap(),
            b"retained earlier media"
        );
        assert_eq!(
            std::fs::read(&sidecar).unwrap(),
            b"custom unrelated metadata"
        );
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }
}

#[tokio::test]
async fn issue_770_recovery_refuses_ordinary_cross_parent_replay_and_outside_root() {
    let root = TempDir::new().unwrap();
    let (mut config, records, destination) = issue_770_seed(&root).await;
    let hidden = issue_770_pass(PassKind::SmartFolder, &records);
    build_pending_retry_download_tasks(
        std::slice::from_ref(&hidden),
        &config,
        DownloadRunMode::Download,
        CancellationToken::new(),
    )
    .await
    .unwrap();
    let asset = PhotoAsset::new(records[0].clone(), records[1].clone())
        .with_state_record_name(Arc::from("PENDING"));
    let hidden_config = config.with_pass(&hidden);
    let db = config.state_db.as_ref().unwrap();
    let ledger = db.get_reconciliation_reservations().await.unwrap();
    for reconciliation in [false, true] {
        let mut planner = TaskPlanner::for_download(Some(db.as_ref())).await.unwrap();
        let plan = if reconciliation {
            planner
                .plan_reconciliation_asset(&asset, &hidden_config)
                .await
        } else {
            planner.plan_download_asset(&asset, &hidden_config).await
        };
        let error = plan.err().unwrap();
        assert!(
            error.to_string().contains("left its requested directory"),
            "{error:#}"
        );
    }
    // Change only the configured root; the stored request must still match
    // to exercise the new confinement check rather than a different slot.
    let new_root = root.path().join("Hidden");
    config.directory = Arc::from(new_root);
    config.folder_structure_smart_folders = Arc::from("%Y/%m");
    let error = build_pending_retry_download_tasks(
        &[hidden],
        &config,
        DownloadRunMode::Download,
        CancellationToken::new(),
    )
    .await
    .err()
    .unwrap();
    assert!(
        error
            .to_string()
            .contains("outside the current download root"),
        "{error:#}"
    );
    assert_eq!(db.get_reconciliation_reservations().await.unwrap(), ledger);
    assert_eq!(db.get_pending().await.unwrap().len(), 1);
    assert!(!destination.exists());
}

#[tokio::test]
async fn issue_770_recovery_finalization_failure_retains_unreceipted_bytes_and_restart_debt() {
    let server = crate::start_wiremock_or_skip!();
    let mut fixture = RecoveryFixture::new(&server).await;
    Mock::given(method("GET"))
        .and(path("/pending.jpg"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(fixture.bytes.clone()))
        .expect(1)
        .mount(&server)
        .await;
    fixture.db.acquire_lock("inject exact finalization failure").unwrap()
        .execute_batch("CREATE TEMP TRIGGER issue_770_fail BEFORE UPDATE OF status ON assets WHEN NEW.id = 'PENDING' AND NEW.status = 'downloaded' BEGIN SELECT RAISE(FAIL, 'injected issue 770 finalization failure'); END;").unwrap();
    let kinds = [PassKind::SmartFolder, PassKind::Unfiled];
    let failed = fixture.run(&kinds, CancellationToken::new()).await;
    assert!(failed.stats.state_write_failures > 0, "{failed:?}");
    assert_eq!(std::fs::read(&fixture.destination).unwrap(), fixture.bytes);
    fixture.assert_preserved().await;
    assert!(
        !fixture
            .config
            .state_db
            .as_ref()
            .unwrap()
            .get_pending()
            .await
            .unwrap()
            .is_empty()
            || !fixture
                .config
                .state_db
                .as_ref()
                .unwrap()
                .get_failed()
                .await
                .unwrap()
                .is_empty()
    );
    fixture.reopen().await;
    for kinds in [
        vec![PassKind::SmartFolder, PassKind::Unfiled],
        vec![PassKind::Unfiled, PassKind::SmartFolder],
        vec![PassKind::Album, PassKind::SmartFolder, PassKind::Unfiled],
    ] {
        fixture.reopen().await;
        let refused = fixture.run(&kinds, CancellationToken::new()).await;
        assert!(
            matches!(refused.outcome, DownloadOutcome::PartialFailure { .. }),
            "{refused:?}"
        );
        assert_eq!(refused.stats.downloaded, 0);
        assert_eq!(std::fs::read(&fixture.destination).unwrap(), fixture.bytes);
        fixture.assert_preserved().await;
        assert!(
            fixture
                .db
                .get_downloaded_page(0, 10)
                .await
                .unwrap()
                .is_empty()
        );
        assert!(
            !fixture.db.get_pending().await.unwrap().is_empty()
                || !fixture.db.get_failed().await.unwrap().is_empty()
        );
    }
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn issue_770_recovery_adopts_only_durable_exact_hash_proof_in_every_pass_order() {
    for kinds in [
        vec![PassKind::SmartFolder, PassKind::Unfiled],
        vec![PassKind::Unfiled, PassKind::SmartFolder],
        vec![PassKind::Album, PassKind::SmartFolder, PassKind::Unfiled],
    ] {
        for equivalent_root in [false, true] {
            let server = crate::start_wiremock_or_skip!();
            let mut fixture = RecoveryFixture::new(&server).await;
            std::fs::create_dir_all(fixture.destination.parent().unwrap()).unwrap();
            std::fs::write(&fixture.destination, &fixture.bytes).unwrap();
            let checksum = file::compute_sha256(&fixture.destination).await.unwrap();
            fixture
                .db
                .mark_downloaded(
                    "PrimarySync",
                    "PENDING",
                    "original",
                    &fixture.destination,
                    &checksum,
                    None,
                )
                .await
                .unwrap();
            fixture
                .db
                .mark_failed("PrimarySync", "PENDING", "original", "synthetic retry")
                .await
                .unwrap();
            fixture
                .db
                .prepare_for_retry(
                    Some("PrimarySync"),
                    crate::state::RetryErrorRetention::Clear,
                )
                .await
                .unwrap();
            assert!(
                fixture.db.get_pending().await.unwrap()[0]
                    .downloaded_at
                    .is_some()
            );
            if equivalent_root {
                fixture.config.directory = Arc::from(fixture.root.path().join("."));
            }
            fixture.reopen().await;
            let adopted = fixture.run(&kinds, CancellationToken::new()).await;
            assert!(
                matches!(adopted.outcome, DownloadOutcome::Success),
                "{adopted:?}"
            );
            assert_eq!(adopted.stats.downloaded, 0);
            fixture.assert_downloaded(PublicationProof::Adoption).await;
            let receipts = fixture.receipt_snapshot().await;
            fixture.reopen().await;
            assert_eq!(
                fixture
                    .run(&kinds, CancellationToken::new())
                    .await
                    .stats
                    .downloaded,
                0
            );
            fixture.assert_downloaded(PublicationProof::Adoption).await;
            assert_eq!(fixture.receipt_snapshot().await, receipts);
            assert!(server.received_requests().await.unwrap().is_empty());
        }
    }
}

#[tokio::test]
async fn issue_770_recovery_failed_adoption_write_rechecks_proof_after_restart() {
    let server = crate::start_wiremock_or_skip!();
    let mut fixture = RecoveryFixture::new(&server).await;
    std::fs::create_dir_all(fixture.destination.parent().unwrap()).unwrap();
    std::fs::write(&fixture.destination, &fixture.bytes).unwrap();
    let checksum = file::compute_sha256(&fixture.destination).await.unwrap();
    fixture
        .db
        .mark_downloaded(
            "PrimarySync",
            "PENDING",
            "original",
            &fixture.destination,
            &checksum,
            None,
        )
        .await
        .unwrap();
    fixture
        .db
        .mark_failed("PrimarySync", "PENDING", "original", "synthetic retry")
        .await
        .unwrap();
    fixture
        .db
        .prepare_for_retry(
            Some("PrimarySync"),
            crate::state::RetryErrorRetention::Clear,
        )
        .await
        .unwrap();
    fixture.db.acquire_lock("inject adoption finalization failure").unwrap()
        .execute_batch("CREATE TEMP TRIGGER issue_770_fail BEFORE UPDATE OF status ON assets WHEN NEW.id = 'PENDING' AND NEW.status = 'downloaded' BEGIN SELECT RAISE(FAIL, 'injected adoption failure'); END;").unwrap();
    let kinds = [PassKind::SmartFolder, PassKind::Unfiled];
    let failed = fixture.run(&kinds, CancellationToken::new()).await;
    assert!(
        matches!(failed.outcome, DownloadOutcome::PartialFailure { .. }),
        "{failed:?}"
    );
    assert!(
        fixture
            .db
            .get_downloaded_page(0, 10)
            .await
            .unwrap()
            .is_empty()
    );
    fixture.assert_preserved().await;
    let mut conflicting = fixture.bytes.clone();
    *conflicting.last_mut().unwrap() ^= 1;
    std::fs::write(&fixture.destination, &conflicting).unwrap();
    fixture.reopen().await;
    let refused = fixture.run(&kinds, CancellationToken::new()).await;
    assert!(
        matches!(refused.outcome, DownloadOutcome::PartialFailure { .. }),
        "{refused:?}"
    );
    assert!(
        fixture
            .db
            .get_downloaded_page(0, 10)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(std::fs::read(&fixture.destination).unwrap(), conflicting);
    fixture.assert_preserved().await;
    // Explicit synthetic fixture restoration supplies the original proved bytes.
    std::fs::write(&fixture.destination, &fixture.bytes).unwrap();
    fixture.reopen().await;
    let recovered = fixture.run(&kinds, CancellationToken::new()).await;
    assert!(
        matches!(recovered.outcome, DownloadOutcome::Success),
        "{recovered:?}"
    );
    fixture.assert_downloaded(PublicationProof::Adoption).await;
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn issue_770_recovery_hash_proof_cannot_transfer_from_another_path_or_absence() {
    for wrong_path in [false, true] {
        let server = crate::start_wiremock_or_skip!();
        let mut fixture = RecoveryFixture::new(&server).await;
        std::fs::create_dir_all(fixture.destination.parent().unwrap()).unwrap();
        std::fs::write(&fixture.destination, &fixture.bytes).unwrap();
        let checksum = file::compute_sha256(&fixture.destination).await.unwrap();
        let other = fixture.root.path().join("other-receipt.jpg");
        std::fs::write(&other, &fixture.bytes).unwrap();
        let recorded_path = if wrong_path {
            &other
        } else {
            &fixture.destination
        };
        fixture
            .db
            .mark_downloaded(
                "PrimarySync",
                "PENDING",
                "original",
                recorded_path,
                &checksum,
                None,
            )
            .await
            .unwrap();
        fixture
            .db
            .mark_failed("PrimarySync", "PENDING", "original", "synthetic retry")
            .await
            .unwrap();
        fixture
            .db
            .prepare_for_retry(
                Some("PrimarySync"),
                crate::state::RetryErrorRetention::Clear,
            )
            .await
            .unwrap();
        if !wrong_path {
            fixture
                .db
                .acquire_lock("inject missing durable hash")
                .unwrap()
                .execute(
                    "UPDATE assets SET local_checksum = NULL WHERE id = 'PENDING'",
                    [],
                )
                .unwrap();
        }
        for kinds in [
            vec![PassKind::SmartFolder, PassKind::Unfiled],
            vec![PassKind::Unfiled, PassKind::SmartFolder],
            vec![PassKind::Album, PassKind::SmartFolder, PassKind::Unfiled],
        ] {
            fixture.reopen().await;
            let refused = fixture.run(&kinds, CancellationToken::new()).await;
            assert!(
                matches!(refused.outcome, DownloadOutcome::PartialFailure { .. }),
                "{refused:?}"
            );
            assert!(
                fixture
                    .db
                    .get_downloaded_page(0, 10)
                    .await
                    .unwrap()
                    .is_empty()
            );
            assert!(
                !fixture.db.get_pending().await.unwrap().is_empty()
                    || !fixture.db.get_failed().await.unwrap().is_empty()
            );
            assert_eq!(std::fs::read(&fixture.destination).unwrap(), fixture.bytes);
            assert_eq!(std::fs::read(&other).unwrap(), fixture.bytes);
            fixture.assert_preserved().await;
            assert!(server.received_requests().await.unwrap().is_empty());
        }
    }
}

#[tokio::test]
async fn issue_770_recovery_durable_metadata_hash_preserves_size_and_download_provenance() {
    let server = crate::start_wiremock_or_skip!();
    let mut fixture = RecoveryFixture::new(&server).await;
    let provider_size = fixture.bytes.len();
    let source_hash = data_encoding::HEXLOWER.encode(&Sha256::digest(&fixture.bytes));
    // Synthetic already-completed metadata rewrite, independently recorded
    // before this retry. Its final bytes legitimately differ from provider size.
    fixture
        .bytes
        .extend_from_slice(b"previously written metadata");
    std::fs::create_dir_all(fixture.destination.parent().unwrap()).unwrap();
    std::fs::write(&fixture.destination, &fixture.bytes).unwrap();
    let local_hash = file::compute_sha256(&fixture.destination).await.unwrap();
    assert_ne!(source_hash, local_hash);
    fixture
        .db
        .mark_downloaded(
            "PrimarySync",
            "PENDING",
            "original",
            &fixture.destination,
            &local_hash,
            Some(&source_hash),
        )
        .await
        .unwrap();
    fixture
        .db
        .mark_failed("PrimarySync", "PENDING", "original", "synthetic retry")
        .await
        .unwrap();
    fixture
        .db
        .prepare_for_retry(
            Some("PrimarySync"),
            crate::state::RetryErrorRetention::Clear,
        )
        .await
        .unwrap();
    fixture.reopen().await;
    let adopted = fixture
        .run(
            &[PassKind::Unfiled, PassKind::SmartFolder],
            CancellationToken::new(),
        )
        .await;
    assert!(
        matches!(adopted.outcome, DownloadOutcome::Success),
        "{adopted:?}"
    );
    assert_eq!(adopted.stats.downloaded, 0);
    assert!(fixture.db.get_pending().await.unwrap().is_empty());
    assert!(fixture.db.get_failed().await.unwrap().is_empty());
    let rows = fixture.db.get_downloaded_page(0, 10).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].size_bytes, provider_size as u64);
    assert_eq!(rows[0].local_checksum.as_deref(), Some(local_hash.as_str()));
    assert_eq!(
        rows[0].download_checksum.as_deref(),
        Some(source_hash.as_str())
    );
    assert_eq!(std::fs::read(&fixture.destination).unwrap(), fixture.bytes);
    fixture.assert_preserved().await;
    let receipts = fixture.receipt_snapshot().await;
    fixture.reopen().await;
    assert_eq!(
        fixture
            .run(
                &[PassKind::Unfiled, PassKind::SmartFolder],
                CancellationToken::new()
            )
            .await
            .stats
            .downloaded,
        0
    );
    assert_eq!(fixture.receipt_snapshot().await, receipts);
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn issue_770_recovery_reservation_failure_blocks_publication_then_recovers() {
    let server = crate::start_wiremock_or_skip!();
    let mut fixture = RecoveryFixture::new(&server).await;
    Mock::given(method("GET"))
        .and(path("/pending.jpg"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(fixture.bytes.clone()))
        .expect(1)
        .mount(&server)
        .await;
    fixture.db.acquire_lock("inject reservation write failure").unwrap()
        .execute_batch("CREATE TEMP TRIGGER issue_770_fail BEFORE INSERT ON reconciliation_paths WHEN NEW.id = 'PENDING' BEGIN SELECT RAISE(FAIL, 'injected issue 770 reservation failure'); END;").unwrap();
    let kinds = [PassKind::SmartFolder, PassKind::Unfiled];
    let failed = fixture.run(&kinds, CancellationToken::new()).await;
    assert!(
        matches!(failed.outcome, DownloadOutcome::PartialFailure { .. }),
        "{failed:?}"
    );
    assert!(!fixture.destination.exists());
    assert!(server.received_requests().await.unwrap().is_empty());
    fixture.assert_preserved().await;
    fixture.reopen().await;
    let recovered = fixture.run(&kinds, CancellationToken::new()).await;
    assert!(
        matches!(recovered.outcome, DownloadOutcome::Success),
        "{recovered:?}"
    );
    fixture.assert_downloaded(PublicationProof::Download).await;
    fixture.reopen().await;
    assert_eq!(
        fixture
            .run(&kinds, CancellationToken::new())
            .await
            .stats
            .downloaded,
        0
    );
    fixture.assert_downloaded(PublicationProof::Download).await;
}

#[tokio::test]
async fn issue_770_recovery_foreign_catalog_ownership_cannot_reuse_saved_choice() {
    for (library, id, version) in [
        ("OtherLibrary", "PENDING", VersionSizeKey::Original),
        ("PrimarySync", "OTHER", VersionSizeKey::Original),
        ("PrimarySync", "PENDING", VersionSizeKey::Adjusted),
    ] {
        let root = TempDir::new().unwrap();
        let (config, records, destination) = issue_770_seed(&root).await;
        let hidden = issue_770_pass(PassKind::SmartFolder, &records);
        build_pending_retry_download_tasks(
            std::slice::from_ref(&hidden),
            &config,
            DownloadRunMode::Download,
            CancellationToken::new(),
        )
        .await
        .unwrap();
        let db = Arc::new(
            SqliteStateDb::open(&root.path().join("state.db"))
                .await
                .unwrap(),
        );
        db.upsert_seen(
            &TestAssetRecord::new(id)
                .library(library)
                .version_size(version)
                .checksum("foreign-generation")
                .size(1024)
                .build(),
        )
        .await
        .unwrap();
        db.mark_downloaded(
            library,
            id,
            version.as_str(),
            &destination,
            "foreign-local-checksum",
            None,
        )
        .await
        .unwrap();
        let ledger = db.get_reconciliation_reservations().await.unwrap();
        let error = build_pending_retry_download_tasks(
            &[hidden],
            &config,
            DownloadRunMode::Download,
            CancellationToken::new(),
        )
        .await
        .err()
        .unwrap();
        assert!(error.to_string().contains("another owner"), "{error:#}");
        assert_eq!(db.get_reconciliation_reservations().await.unwrap(), ledger);
        assert_eq!(db.get_pending().await.unwrap().len(), 1);
        assert!(!destination.exists());
    }
}

#[tokio::test]
async fn issue_770_recovery_rejects_traversal_and_inconsistent_saved_keys() {
    for traversal in [false, true] {
        let root = TempDir::new().unwrap();
        let (config, records, destination) = issue_770_seed(&root).await;
        let hidden = issue_770_pass(PassKind::SmartFolder, &records);
        build_pending_retry_download_tasks(
            std::slice::from_ref(&hidden),
            &config,
            DownloadRunMode::Download,
            CancellationToken::new(),
        )
        .await
        .unwrap();
        let db = SqliteStateDb::open(&root.path().join("state.db"))
            .await
            .unwrap();
        // A damaged durable spelling is retained as evidence, never normalized
        // into permission to publish somewhere else.
        let damaged = if traversal {
            root.path().join("old-album/../pending.jpg")
        } else {
            root.path().join("different.jpg")
        };
        db.acquire_lock("seed damaged path spelling")
            .unwrap()
            .execute(
                "UPDATE reconciliation_paths SET destination_path = ?1 WHERE id = 'PENDING'",
                rusqlite::params![damaged.to_str().unwrap()],
            )
            .unwrap();
        let ledger = db.get_reconciliation_reservations().await.unwrap();
        let error = build_pending_retry_download_tasks(
            &[hidden],
            &config,
            DownloadRunMode::Download,
            CancellationToken::new(),
        )
        .await
        .err()
        .unwrap();
        assert!(
            error.to_string().contains(if traversal {
                "parent components"
            } else {
                "inconsistent destination key"
            }),
            "{error:#}"
        );
        assert_eq!(db.get_reconciliation_reservations().await.unwrap(), ledger);
        assert_eq!(db.get_pending().await.unwrap().len(), 1);
        assert!(!destination.exists());
        assert!(!damaged.exists());
    }
}

#[tokio::test]
async fn issue_770_recovery_changed_content_and_unknown_legacy_choices_stay_occupied() {
    for unknown in [false, true] {
        let root = TempDir::new().unwrap();
        let (config, mut records, destination) = issue_770_seed(&root).await;
        let hidden = issue_770_pass(PassKind::SmartFolder, &records);
        build_pending_retry_download_tasks(
            &[hidden],
            &config,
            DownloadRunMode::Download,
            CancellationToken::new(),
        )
        .await
        .unwrap();
        let db = SqliteStateDb::open(&root.path().join("state.db"))
            .await
            .unwrap();
        if unknown {
            db.acquire_lock("seed unknown legacy content").unwrap().execute_batch(
                "UPDATE reconciliation_paths SET provider_checksum = '', provider_size = -1 WHERE id = 'PENDING';"
            ).unwrap();
        } else {
            records[0]["fields"]["resOriginalRes"]["value"]["fileChecksum"] =
                json!("new-content-checksum");
        }
        let ledger = db.get_reconciliation_reservations().await.unwrap();
        let asset = PhotoAsset::new(records[0].clone(), records[1].clone())
            .with_state_record_name(Arc::from("PENDING"));
        let hidden_config = config.with_pass(&issue_770_pass(PassKind::SmartFolder, &records));
        let mut planner =
            TaskPlanner::for_download(Some(config.state_db.as_ref().unwrap().as_ref()))
                .await
                .unwrap();
        let plan = planner
            .plan_pending_retry_asset(&asset, &hidden_config)
            .await
            .unwrap();
        assert!(!plan.tasks.is_empty());
        assert!(
            plan.tasks
                .iter()
                .all(|task| task.download_path != destination)
        );
        assert!(!planner.retry_path_allowed(
            "PrimarySync",
            "PENDING",
            VersionSizeKey::Original,
            if unknown {
                "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA="
            } else {
                "new-content-checksum"
            },
            1024,
            &destination
        ));
        assert_eq!(db.get_reconciliation_reservations().await.unwrap(), ledger);
        assert!(!destination.exists());
    }
}

#[tokio::test]
async fn issue_770_recovery_cancellation_preserves_choice_and_recovers_after_restart() {
    let server = crate::start_wiremock_or_skip!();
    let mut fixture = RecoveryFixture::new(&server).await;
    let kinds = [PassKind::SmartFolder, PassKind::Unfiled];
    let cancelled = CancellationToken::new();
    cancelled.cancel();
    let before = fixture.run(&kinds, cancelled).await;
    assert!(before.checkpoint.interrupted, "{before:?}");
    assert!(server.received_requests().await.unwrap().is_empty());
    fixture.assert_preserved().await;
    assert!(!fixture.destination.exists());
    let requested = Arc::new(tokio::sync::Notify::new());
    let signal = requested.clone();
    let bytes = fixture.bytes.clone();
    let delayed = Mock::given(method("GET"))
        .and(path("/pending.jpg"))
        .respond_with(move |_: &wiremock::Request| {
            signal.notify_one();
            ResponseTemplate::new(200)
                .set_body_bytes(bytes.clone())
                .set_delay(std::time::Duration::from_secs(5))
        })
        .expect(1)
        .mount_as_scoped(&server)
        .await;
    let token = CancellationToken::new();
    {
        let running = fixture.run(&kinds, token.clone());
        tokio::pin!(running);
        tokio::select! {
            () = requested.notified() => token.cancel(),
            result = &mut running => panic!("recovery finished before injected cancellation: {result:?}"),
        }
        let stopped = running.await;
        assert!(stopped.checkpoint.interrupted, "{stopped:?}");
    }
    drop(delayed);
    assert!(!fixture.destination.exists());
    fixture.assert_preserved().await;
    assert!(
        !fixture
            .config
            .state_db
            .as_ref()
            .unwrap()
            .get_pending()
            .await
            .unwrap()
            .is_empty()
            || !fixture
                .config
                .state_db
                .as_ref()
                .unwrap()
                .get_failed()
                .await
                .unwrap()
                .is_empty()
    );
    Mock::given(method("GET"))
        .and(path("/pending.jpg"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(fixture.bytes.clone()))
        .expect(1)
        .mount(&server)
        .await;
    fixture.reopen().await;
    let recovered = fixture.run(&kinds, CancellationToken::new()).await;
    assert!(
        matches!(recovered.outcome, DownloadOutcome::Success),
        "{recovered:?}"
    );
    fixture.assert_downloaded(PublicationProof::Download).await;
    fixture.reopen().await;
    assert_eq!(
        fixture
            .run(&kinds, CancellationToken::new())
            .await
            .stats
            .downloaded,
        0
    );
    assert_eq!(server.received_requests().await.unwrap().len(), 2);
}

#[tokio::test]
async fn issue_770_recovery_publishes_reserved_still_and_motion_without_duplicate_retry() {
    let server = crate::start_wiremock_or_skip!();
    let mut fixture = RecoveryFixture::new(&server).await;
    let motion = include_bytes!("../../../../tests/data/media/pattern.mov");
    let motion_checksum = base64::engine::general_purpose::STANDARD.encode(Sha256::digest(motion));
    fixture.records[0]["fields"]["resOriginalVidComplRes"] = json!({"value": {
        "downloadURL": format!("{}/pending.mov", server.uri()),
        "size": motion.len(), "fileChecksum": motion_checksum,
    }});
    fixture.records[0]["fields"]["resOriginalVidComplFileType"] =
        json!({"value": "com.apple.quicktime-movie"});
    let motion_path = fixture.destination.with_extension("MOV");
    fixture
        .db
        .upsert_seen(
            &TestAssetRecord::new("PENDING")
                .version_size(VersionSizeKey::LiveOriginal)
                .filename("pending.MOV")
                .checksum(&motion_checksum)
                .size(motion.len() as u64)
                .media_type(crate::state::MediaType::LivePhotoVideo)
                .added_at(Utc.timestamp_opt(1_700_000_000, 0).unwrap())
                .build(),
        )
        .await
        .unwrap();
    fixture
        .db
        .mark_downloaded(
            "PrimarySync",
            "PENDING",
            VersionSizeKey::LiveOriginal.as_str(),
            &motion_path,
            "historical-motion-checksum",
            None,
        )
        .await
        .unwrap();
    fixture
        .db
        .mark_failed(
            "PrimarySync",
            "PENDING",
            VersionSizeKey::LiveOriginal.as_str(),
            "earlier companion failure",
        )
        .await
        .unwrap();
    fixture
        .db
        .prepare_for_retry(
            Some("PrimarySync"),
            crate::state::RetryErrorRetention::Clear,
        )
        .await
        .unwrap();
    let hidden = fixture.passes(&[PassKind::SmartFolder]);
    let plan = build_pending_retry_download_tasks(
        &hidden,
        &fixture.config,
        DownloadRunMode::Download,
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(plan.tasks.len(), 2);
    assert!(plan.unmatched_targets.is_empty());
    assert!(
        plan.tasks
            .iter()
            .any(|task| task.version_size == VersionSizeKey::Original
                && task.download_path == fixture.destination)
    );
    assert!(
        plan.tasks
            .iter()
            .any(|task| task.version_size == VersionSizeKey::LiveOriginal
                && task.download_path == motion_path)
    );
    fixture.ledger = fixture
        .config
        .state_db
        .as_ref()
        .unwrap()
        .get_reconciliation_reservations()
        .await
        .unwrap();
    for (endpoint, bytes) in [
        ("/pending.jpg", fixture.bytes.as_slice()),
        ("/pending.mov", motion.as_slice()),
    ] {
        Mock::given(method("GET"))
            .and(path(endpoint))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(bytes.to_vec()))
            .expect(1)
            .mount(&server)
            .await;
    }
    fixture.reopen().await;
    let kinds = [PassKind::SmartFolder, PassKind::Unfiled];
    let recovered = fixture.run(&kinds, CancellationToken::new()).await;
    assert!(
        matches!(recovered.outcome, DownloadOutcome::Success),
        "{recovered:?}"
    );
    assert_eq!(recovered.stats.downloaded, 2);
    assert_eq!(std::fs::read(&fixture.destination).unwrap(), fixture.bytes);
    assert_eq!(std::fs::read(&motion_path).unwrap(), motion);
    fixture.reopen().await;
    let rows = fixture
        .config
        .state_db
        .as_ref()
        .unwrap()
        .get_downloaded_page(0, 10)
        .await
        .unwrap();
    assert_eq!(rows.len(), 2);
    for (version, pathname, bytes) in [
        (
            VersionSizeKey::Original,
            &fixture.destination,
            fixture.bytes.as_slice(),
        ),
        (
            VersionSizeKey::LiveOriginal,
            &motion_path,
            motion.as_slice(),
        ),
    ] {
        let row = rows.iter().find(|row| row.version_size == version).unwrap();
        let checksum = data_encoding::HEXLOWER.encode(&Sha256::digest(bytes));
        assert_eq!(row.local_path.as_ref(), Some(pathname));
        assert_eq!(row.local_checksum.as_deref(), Some(checksum.as_str()));
        assert_eq!(row.download_checksum.as_deref(), Some(checksum.as_str()));
    }
    let receipts = fixture.receipt_snapshot().await;
    assert_eq!(receipts.as_array().unwrap().len(), 2);
    fixture.assert_preserved().await;
    assert_eq!(
        fixture
            .run(&kinds, CancellationToken::new())
            .await
            .stats
            .downloaded,
        0
    );
    fixture.reopen().await;
    assert_eq!(fixture.receipt_snapshot().await, receipts);
    fixture.assert_preserved().await;
    assert_eq!(server.received_requests().await.unwrap().len(), 2);
}

#[cfg(feature = "xmp")]
#[tokio::test]
async fn issue_770_recovery_enabled_sidecar_survives_quiet_restart() {
    let server = crate::start_wiremock_or_skip!();
    let mut fixture = RecoveryFixture::new(&server).await;
    fixture.config.metadata.xmp_sidecar = true;
    Mock::given(method("GET"))
        .and(path("/pending.jpg"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(fixture.bytes.clone()))
        .expect(1)
        .mount(&server)
        .await;
    let kinds = [PassKind::SmartFolder, PassKind::Unfiled];
    assert!(matches!(
        fixture.run(&kinds, CancellationToken::new()).await.outcome,
        DownloadOutcome::Success
    ));
    let sidecar = fixture.destination.with_extension("jpg.xmp");
    let text = std::fs::read_to_string(&sidecar).unwrap();
    let _: xmp_toolkit::XmpMeta = text.parse().unwrap();
    let metadata_mtime = std::fs::metadata(&sidecar).unwrap().modified().unwrap();
    fixture.reopen().await;
    fixture.assert_downloaded(PublicationProof::Download).await;
    assert_eq!(
        fixture
            .run(&kinds, CancellationToken::new())
            .await
            .stats
            .downloaded,
        0
    );
    fixture.reopen().await;
    assert_eq!(std::fs::read_to_string(&sidecar).unwrap(), text);
    assert_eq!(
        std::fs::metadata(&sidecar).unwrap().modified().unwrap(),
        metadata_mtime
    );
    fixture.assert_downloaded(PublicationProof::Download).await;
}

#[tokio::test]
async fn issue_770_recovery_conflicting_reserved_bytes_are_retained_until_fixture_resolution() {
    let server = crate::start_wiremock_or_skip!();
    let mut fixture = RecoveryFixture::new(&server).await;
    Mock::given(method("GET"))
        .and(path("/pending.jpg"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(fixture.bytes.clone()))
        .expect(1)
        .mount(&server)
        .await;
    let mut conflicting = fixture.bytes.clone();
    *conflicting.last_mut().unwrap() ^= 1;
    std::fs::create_dir_all(fixture.destination.parent().unwrap()).unwrap();
    std::fs::write(&fixture.destination, &conflicting).unwrap();
    let sidecar = fixture.destination.with_extension("jpg.xmp");
    std::fs::write(&sidecar, b"retained custom packet").unwrap();
    let kinds = [PassKind::SmartFolder, PassKind::Unfiled];
    for kinds in [
        vec![PassKind::SmartFolder, PassKind::Unfiled],
        vec![PassKind::Unfiled, PassKind::SmartFolder],
        vec![PassKind::Album, PassKind::SmartFolder, PassKind::Unfiled],
    ] {
        fixture.reopen().await;
        let refused = fixture.run(&kinds, CancellationToken::new()).await;
        assert!(
            matches!(refused.outcome, DownloadOutcome::PartialFailure { .. }),
            "{refused:?}"
        );
        assert_eq!(std::fs::read(&fixture.destination).unwrap(), conflicting);
        assert_eq!(std::fs::read(&sidecar).unwrap(), b"retained custom packet");
        fixture.assert_preserved().await;
        assert!(server.received_requests().await.unwrap().is_empty());
    }
    let retained = fixture.destination.with_extension("retained-conflict");
    std::fs::rename(&fixture.destination, &retained).unwrap();
    let repaired = fixture.run(&kinds, CancellationToken::new()).await;
    assert!(
        matches!(repaired.outcome, DownloadOutcome::Success),
        "{repaired:?}"
    );
    fixture.reopen().await;
    fixture.assert_downloaded(PublicationProof::Download).await;
    assert_eq!(std::fs::read(&retained).unwrap(), conflicting);
    assert_eq!(std::fs::read(&sidecar).unwrap(), b"retained custom packet");
    assert_eq!(
        fixture
            .run(&kinds, CancellationToken::new())
            .await
            .stats
            .downloaded,
        0
    );
}

#[cfg(unix)]
#[tokio::test]
async fn issue_770_recovery_reserved_links_cannot_publish_outside_root() {
    for parent_link in [false, true] {
        let server = crate::start_wiremock_or_skip!();
        let mut fixture = RecoveryFixture::new(&server).await;
        let outside = TempDir::new().unwrap();
        let target = outside.path().join("pending.jpg");
        std::fs::write(&target, vec![9u8; fixture.bytes.len()]).unwrap();
        std::fs::create_dir_all(fixture.destination.parent().unwrap()).unwrap();
        let link = if parent_link {
            let parent = fixture.destination.parent().unwrap().to_path_buf();
            std::fs::remove_dir(&parent).unwrap();
            std::os::unix::fs::symlink(outside.path(), &parent).unwrap();
            parent
        } else {
            std::os::unix::fs::symlink(&target, &fixture.destination).unwrap();
            fixture.destination.clone()
        };
        fixture.reopen().await;
        let refused = fixture
            .run(
                &[PassKind::SmartFolder, PassKind::Unfiled],
                CancellationToken::new(),
            )
            .await;
        assert!(
            matches!(refused.outcome, DownloadOutcome::PartialFailure { .. }),
            "{refused:?}"
        );
        fixture.assert_preserved().await;
        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(
            std::fs::read(&target).unwrap(),
            vec![9u8; fixture.bytes.len()]
        );
        assert_eq!(std::fs::read_dir(outside.path()).unwrap().count(), 1);
        assert!(server.received_requests().await.unwrap().is_empty());
    }
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
#[tokio::test]
async fn issue_770_recovery_case_equivalent_root_replays_absence_and_durable_proof() {
    for with_proof in [false, true] {
        let server = crate::start_wiremock_or_skip!();
        let fixture = RecoveryFixture::new(&server).await;
        if with_proof {
            std::fs::create_dir_all(fixture.destination.parent().unwrap()).unwrap();
            std::fs::write(&fixture.destination, &fixture.bytes).unwrap();
            let hash = file::compute_sha256(&fixture.destination).await.unwrap();
            fixture
                .db
                .mark_downloaded(
                    "PrimarySync",
                    "PENDING",
                    "original",
                    &fixture.destination,
                    &hash,
                    None,
                )
                .await
                .unwrap();
            fixture
                .db
                .mark_failed("PrimarySync", "PENDING", "original", "synthetic retry")
                .await
                .unwrap();
            fixture
                .db
                .prepare_for_retry(
                    Some("PrimarySync"),
                    crate::state::RetryErrorRetention::Clear,
                )
                .await
                .unwrap();
        }
        let root = fixture.root.path();
        let alternate = root.with_file_name(
            root.file_name()
                .unwrap()
                .to_string_lossy()
                .to_ascii_uppercase(),
        );
        assert_ne!(alternate, root);
        let mut config = fixture.config.clone();
        config.directory = Arc::from(alternate);
        let retry = build_pending_retry_download_tasks(
            &fixture.passes(&[PassKind::SmartFolder, PassKind::Unfiled]),
            &config,
            DownloadRunMode::Download,
            CancellationToken::new(),
        )
        .await
        .unwrap();
        if with_proof {
            assert!(retry.tasks.is_empty());
            assert!(retry.unmatched_targets.is_empty());
            let rows = fixture.db.get_downloaded_page(0, 10).await.unwrap();
            assert_eq!(rows.len(), 1);
            assert_eq!(
                crate::fs_util::confined_path_key(rows[0].local_path.as_ref().unwrap()).unwrap(),
                crate::fs_util::confined_path_key(&fixture.destination).unwrap()
            );
        } else {
            assert_eq!(retry.tasks.len(), 1);
            assert!(
                retry.tasks[0]
                    .download_path
                    .starts_with(config.directory.as_ref())
            );
            assert_eq!(
                crate::fs_util::confined_path_key(&retry.tasks[0].download_path).unwrap(),
                crate::fs_util::confined_path_key(&fixture.destination).unwrap()
            );
        }
        assert!(server.received_requests().await.unwrap().is_empty());
        fixture.assert_preserved().await;
    }
}

#[tokio::test]
async fn issue_770_recovery_enumerated_cleanup_preserves_required_publication_proof() {
    let server = crate::start_wiremock_or_skip!();
    let fixture = RecoveryFixture::new(&server).await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(503))
        .expect(1)
        .mount(&server)
        .await;
    let mut config = fixture.config.clone();
    config.folder_structure = "old-album".into();
    config.retry.max_retries = 0;
    let asset = PhotoAsset::new(fixture.records[0].clone(), fixture.records[1].clone());
    let first = crate::download::pipeline::stream_and_download_from_stream(
        &Client::new(),
        futures_util::stream::iter(vec![Ok(asset)]),
        &Arc::new(config.clone()),
        DownloadControls::download_hidden(),
        1,
        CancellationToken::new(),
        crate::download::pipeline::StreamRuntime::new(None, None),
    )
    .await
    .unwrap();
    assert_eq!(
        first.failed.len(),
        1,
        "synthetic non-expiry transfer failure"
    );
    let failed = first.failed;
    assert!(
        failed[0].pending_cross_parent_root.is_some(),
        "producer must require proof for the affected pending rendition"
    );
    let mut pass = issue_770_pass(PassKind::Unfiled, &fixture.records);
    pass.album = album_with_session(
        "PrimarySync",
        "",
        Box::new(RecoveryLookupSession {
            records: Arc::new(fixture.records.clone()),
            enumerate: true,
        }),
    );
    for mode in [
        crate::download::CleanupUrlRefresh::Enumerate,
        crate::download::CleanupUrlRefresh::Lookup,
    ] {
        let retry = crate::download::build_retry_download_tasks(
            &[pass.clone()],
            &config,
            &failed,
            mode,
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(retry.tasks.len(), 1);
        assert_eq!(retry.tasks[0].download_path, failed[0].download_path);
        assert_eq!(
            retry.tasks[0].pending_cross_parent_root, failed[0].pending_cross_parent_root,
            "both cleanup routes must retain the affected task's required publication proof"
        );
    }
    // Ordinary failures retain their ordinary finalization policy.
    let mut ordinary = failed.clone();
    ordinary[0].pending_cross_parent_root = None;
    let retry = crate::download::build_retry_download_tasks(
        &[pass.clone()],
        &config,
        &ordinary,
        crate::download::CleanupUrlRefresh::Enumerate,
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(retry.tasks.len(), 1);
    assert!(retry.tasks[0].pending_cross_parent_root.is_none());
    for changed_size in [false, true] {
        let mut records = fixture.records.clone();
        let resource = &mut records[0]["fields"]["resOriginalRes"]["value"];
        if changed_size {
            resource["size"] = json!(fixture.bytes.len() + 1);
        } else {
            resource["fileChecksum"] = json!("new-provider-generation");
        }
        let mut changed_pass = issue_770_pass(PassKind::Unfiled, &records);
        changed_pass.album = album_with_session(
            "PrimarySync",
            "",
            Box::new(RecoveryLookupSession {
                records: Arc::new(records.clone()),
                enumerate: true,
            }),
        );
        // Bypass saved-generation reservation refusal to test cleanup's own
        // generation boundary with an otherwise identical retry key.
        let mut fresh_config = config.clone();
        fresh_config.state_db = None;
        let asset = PhotoAsset::new(records[0].clone(), records[1].clone());
        let mut planner = TaskPlanner::for_download(None).await.unwrap();
        let plan = planner
            .plan_download_asset(&asset, &fresh_config)
            .await
            .unwrap();
        assert_eq!(plan.tasks.len(), 1);
        assert_eq!(plan.tasks[0].download_path, failed[0].download_path);
        let retry = crate::download::build_retry_download_tasks(
            &[changed_pass],
            &fresh_config,
            &failed,
            crate::download::CleanupUrlRefresh::Enumerate,
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(
            retry.tasks.is_empty(),
            "old failure cannot authorize changed provider content"
        );
    }
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
    let ledger = fixture.db.get_reconciliation_reservations().await.unwrap();
    assert!(fixture.ledger.iter().all(|row| ledger.contains(row)));
    assert!(
        fixture
            .db
            .get_downloaded_page(0, 10)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn issue_770_recovery_collecting_incremental_revalidates_deferred_publication() {
    for changed in [false, true] {
        let server = crate::start_wiremock_or_skip!();
        let fixture = RecoveryFixture::new(&server).await;
        Mock::given(method("GET"))
            .and(path("/pending.jpg"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_bytes(fixture.bytes.clone())
                    .insert_header("content-type", "image/jpeg"),
            )
            .expect(1)
            .mount(&server)
            .await;
        let mut config = fixture.config.clone();
        config.folder_structure = "old-album".into();
        let asset = PhotoAsset::new(fixture.records[0].clone(), fixture.records[1].clone());
        let mut planner = TaskPlanner::for_download(Some(fixture.db.as_ref()))
            .await
            .unwrap();
        let plan = planner.plan_download_asset(&asset, &config).await.unwrap();
        assert_eq!(plan.tasks.len(), 1);
        let published = plan.tasks[0].download_path.clone();
        let mut pass = issue_770_pass(PassKind::Unfiled, &fixture.records);
        pass.album = album_with_session(
            "PrimarySync",
            "",
            Box::new(
                crate::test_helpers::MockPhotosFlow::new()
                    .changes_zone_page(
                        fixture.records.clone(),
                        "synthetic-created-successor",
                        false,
                    )
                    .build(),
            ),
        );
        fixture.db.acquire_lock("770 collecting finalization fault").unwrap().execute_batch(
            "CREATE TEMP TRIGGER issue_770_collecting_fail BEFORE UPDATE OF status ON assets WHEN NEW.status = 'downloaded' BEGIN SELECT RAISE(FAIL, 'injected collecting finalization failure'); END;"
        ).unwrap();
        let probe =
            crate::download::finalize::finalization_probe::FailedFinalizationProbe::for_path(
                &published,
            );
        let watcher = async {
            tokio::time::timeout(std::time::Duration::from_secs(10), probe.observed())
                .await
                .expect("must observe the failed initial receipt transaction");
            let mut bytes = fixture.bytes.clone();
            if changed {
                *bytes.last_mut().unwrap() ^= 1;
            }
            assert_eq!(std::fs::read(&published).unwrap(), fixture.bytes);
            if changed {
                std::fs::write(&published, &bytes).unwrap();
            }
            fixture
                .db
                .acquire_lock("770 permit deferred retry")
                .unwrap()
                .execute_batch("DROP TRIGGER issue_770_collecting_fail;")
                .unwrap();
            probe.release();
            bytes
        };
        let passes = [pass];
        let config = Arc::new(config);
        let client = Client::new();
        let operation = crate::download::orchestration::incremental::download_photos_incremental_collecting_inner(
            &client, &passes, &config, "synthetic-created-predecessor",
            DownloadControls::download_hidden(), CancellationToken::new(),
            std::time::Duration::from_secs(3600),
        );
        let (result, bytes) = tokio::join!(operation, watcher);
        let result = result.unwrap();
        assert_eq!(std::fs::read(&published).unwrap(), bytes);
        assert_eq!(
            fixture.db.get_downloaded_page(0, 10).await.unwrap().len(),
            usize::from(!changed),
            "collecting incremental cannot finalize stale publication bytes: {result:?}"
        );
        if changed {
            assert!(result.stats.state_write_failures > 0);
            assert!(
                !fixture.db.get_pending().await.unwrap().is_empty()
                    || !fixture.db.get_failed().await.unwrap().is_empty()
            );
        }
        let ledger = fixture.db.get_reconciliation_reservations().await.unwrap();
        assert!(fixture.ledger.iter().all(|row| ledger.contains(row)));
    }
}

#[tokio::test]
async fn issue_770_recovery_collecting_incremental_retains_unreceipted_reserved_file() {
    let server = crate::start_wiremock_or_skip!();
    let mut fixture = RecoveryFixture::new(&server).await;
    std::fs::create_dir_all(fixture.destination.parent().unwrap()).unwrap();
    std::fs::write(&fixture.destination, &fixture.bytes).unwrap();
    fixture.config.folder_structure = "old-album".into();
    for _ in 0..2 {
        fixture.reopen().await;
        let mut pass = issue_770_pass(PassKind::Unfiled, &fixture.records);
        pass.album = album_with_session(
            "PrimarySync",
            "",
            Box::new(
                crate::test_helpers::MockPhotosFlow::new()
                    .changes_zone_page(
                        fixture.records.clone(),
                        "synthetic-created-successor",
                        false,
                    )
                    .build(),
            ),
        );
        let result = crate::download::orchestration::incremental::download_photos_incremental_collecting_inner(
            &Client::new(), &[pass], &Arc::new(fixture.config.clone()),
            "synthetic-created-predecessor", DownloadControls::download_hidden(),
            CancellationToken::new(), std::time::Duration::from_secs(3600),
        ).await.unwrap();
        assert_eq!(result.stats.downloaded, 0);
        assert!(result.stats.enumeration_errors > 0);
        assert_eq!(std::fs::read(&fixture.destination).unwrap(), fixture.bytes);
        assert!(server.received_requests().await.unwrap().is_empty());
        fixture.assert_preserved().await;
    }
}
