use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use anyhow::Result;
use tokio_util::sync::CancellationToken;

use crate::download::DownloadControls;
use crate::download::pipeline::outcome::{StreamingResult, build_download_outcome};
use crate::state::error::StateError;
use crate::state::types::SyncSummary;
use crate::state::{
    AssetRecord, DownloadStateStore, ImportStateStore, MembershipStore, MetadataRewriteStore,
    ReportStateStore, SyncRunStats, SyncTokenStore, TempFileOwnershipStore,
};

/// Exercise transfer, prepared publication, and finalization for an explicitly
/// bound retry task. The caller supplies expected bytes; only the real transfer
/// computes and persists publication evidence.
pub(crate) async fn run_bound_publication_for_test(
    db: Arc<crate::state::SqliteStateDb>,
    expected: &crate::state::db::PreparedPublication,
    url: &str,
    asset: &crate::icloud::photos::PhotoAsset,
) -> (usize, usize) {
    let binding = expected.binding.as_ref().expect("bound retry fixture");
    assert_eq!(asset.asset_record_name(), binding.child);
    assert_eq!(asset.id(), binding.master);
    assert_eq!(asset.state_id(), expected.id);
    let mut config = crate::download::DownloadConfig::test_default();
    config.directory = expected
        .destination
        .to_path()
        .parent()
        .unwrap()
        .to_path_buf()
        .into();
    config.folder_structure = "none".into();
    config.library = expected.library.as_str().into();
    let paths = crate::download::filter::derive_expected_paths(asset, &config);
    let motion = paths
        .iter()
        .find(|path| path.version_size == crate::state::VersionSizeKey::LiveOriginal)
        .unwrap();
    assert_eq!(motion.path, expected.destination.to_path());
    assert_eq!(motion.checksum.as_ref(), expected.provider_checksum);
    assert_eq!(motion.size, expected.provider_size);
    let task = crate::download::filter::DownloadTask {
        require_created_entry: false,
        url: url.into(),
        download_path: expected.destination.to_path(),
        replacement_fingerprint: None,
        pending_cross_parent_root: None,
        superseded_requested_path: None,
        publication_binding: Some(Arc::new(binding.clone())),
        checksum: expected.provider_checksum.clone().into(),
        asset_id: expected.id.as_str().into(),
        asset_record_name: binding.child.as_str().into(),
        library: expected.library.as_str().into(),
        metadata: Arc::default(),
        size: expected.provider_size,
        created_local: chrono::DateTime::from_timestamp(1_700_000_000, 0)
            .unwrap()
            .fixed_offset(),
        version_size: crate::state::VersionSizeKey::LiveOriginal,
        media_type: crate::state::MediaType::LivePhotoVideo,
    };
    let client = reqwest::Client::new();
    let retry = crate::retry::RetryConfig {
        max_retries: 0,
        ..Default::default()
    };
    let result = super::run_download_pass(
        super::PassConfig {
            prior_auth_errors: 0,
            url_obtained_at: Default::default(),
            client: &client,
            retry_config: &retry,
            metadata: super::MetadataFlags::empty(),
            mark_capture_repair_after_download: false,
            concurrency: 1,
            reporting: crate::download::DownloadReporting::hidden(),
            temp_suffix: ".family-custody".into(),
            shutdown_token: CancellationToken::new(),
            state_db: Some(db.clone()),
            rate_limit_counter: Arc::default(),
            bandwidth_limiter: None,
            library: expected.library.as_str().into(),
            selection_run: None,
            selection_pass: None,
        },
        vec![task],
    )
    .await;
    assert_eq!(result.state_write_failures, 0);
    if !result.failed.is_empty() {
        let error: Option<String> = db
            .acquire_lock("bound retry failure evidence")
            .unwrap()
            .query_row(
                "SELECT last_error FROM assets WHERE library=?1 AND id=?2 AND version_size=?3",
                rusqlite::params![expected.library, expected.id, expected.version],
                |row| row.get(0),
            )
            .unwrap();
        eprintln!("R2_REAL_PIPELINE failure={error:?}");
    }
    (result.downloaded, result.failed.len())
}

pub(crate) async fn recover_bound_publication_for_test(
    db: &crate::state::SqliteStateDb,
    expected: &crate::state::db::PreparedPublication,
    asset: &crate::icloud::photos::PhotoAsset,
) {
    let binding = expected.binding.as_ref().expect("bound retry fixture");
    assert_eq!(asset.asset_record_name(), binding.child);
    assert_eq!(asset.id(), binding.master);
    assert_eq!(asset.state_id(), expected.id);
    let destination = expected.destination.to_path();
    let mut config = crate::download::DownloadConfig::test_default();
    config.directory = destination.parent().unwrap().to_path_buf().into();
    config.folder_structure = "none".into();
    config.library = expected.library.as_str().into();
    config.ownership_check = Some(
        crate::download::planner::OwnershipCheck::inspect(db)
            .await
            .unwrap(),
    );
    let mut planner = crate::download::planner::TaskPlanner::for_download(Some(db))
        .await
        .unwrap();
    let result = super::recover_current_pending_publication(
        db,
        &config,
        asset,
        &mut planner,
        crate::state::VersionSizeKey::LiveOriginal,
        None,
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(
        result,
        Some(super::adoption::PendingOnDiskAdoption::Adopted(destination))
    );
}

pub(super) async fn build_zero_download_outcome(
    streaming_result: StreamingResult,
    controls: DownloadControls,
) -> (crate::download::DownloadOutcome, crate::download::SyncStats) {
    let client = reqwest::Client::new();
    let config = Arc::new(crate::download::DownloadConfig::test_default());
    build_download_outcome(
        &client,
        &[],
        &config,
        controls,
        streaming_result,
        Instant::now(),
        CancellationToken::new(),
    )
    .await
    .expect("zero-download outcome should build")
}

/// T-6: All pending state writes from the download loop are retained and
/// re-flushed. Even with multiple records and transient failures, every
/// write that eventually succeeds reaches the DB.
/// A download-store stub where `mark_downloaded` fails a configurable number
/// of times before succeeding. All other methods panic (unused).
pub(super) struct FailingDownloadStore {
    pub(super) remaining_failures: AtomicUsize,
    pub(super) calls: AtomicUsize,
    pub(super) successes: AtomicUsize,
    pub(super) failed_calls: AtomicUsize,
    pub(super) downloaded_state_loads: AtomicUsize,
    pub(super) track_failed_calls: bool,
    pub(super) fail_complete_sync_run: bool,
    pub(super) publication_db: Option<Arc<crate::state::SqliteStateDb>>,
}

impl FailingDownloadStore {
    pub(super) fn new(fail_count: usize) -> Self {
        Self {
            remaining_failures: AtomicUsize::new(fail_count),
            calls: AtomicUsize::new(0),
            successes: AtomicUsize::new(0),
            failed_calls: AtomicUsize::new(0),
            downloaded_state_loads: AtomicUsize::new(0),
            track_failed_calls: false,
            fail_complete_sync_run: false,
            publication_db: None,
        }
    }

    pub(super) fn with_mark_failed_tracking() -> Self {
        let mut s = Self::new(0);
        s.track_failed_calls = true;
        s
    }

    pub(super) fn with_failing_complete_sync_run() -> Self {
        let mut s = Self::new(0);
        s.fail_complete_sync_run = true;
        s
    }

    pub(super) fn success_count(&self) -> usize {
        self.successes.load(Ordering::Relaxed)
    }

    pub(super) fn call_count(&self) -> usize {
        self.calls.load(Ordering::Relaxed)
    }

    pub(super) fn downloaded_state_load_count(&self) -> usize {
        self.downloaded_state_loads.load(Ordering::Relaxed)
    }

    pub(super) fn failed_call_count(&self) -> usize {
        self.failed_calls.load(Ordering::Relaxed)
    }
}

#[async_trait::async_trait]
impl crate::state::SparseIdentityStore for FailingDownloadStore {
    async fn sparse_identities(
        &self,
        _library: &str,
    ) -> Result<Vec<crate::state::SparseIdentity>, crate::state::error::StateError> {
        unimplemented!()
    }
    async fn observe_sparse_identity(
        &self,
        _library: &str,
        _source: &crate::state::SparseSourceId,
        _evidence: &crate::state::SparseEvidence,
        _now: chrono::DateTime<chrono::Utc>,
    ) -> Result<crate::state::SparseIdentity, crate::state::error::StateError> {
        unimplemented!()
    }
    async fn record_sparse_attempt(
        &self,
        _identity: &crate::state::SparseIdentity,
        _outcome: crate::state::SparseAttemptOutcome,
        _now: chrono::DateTime<chrono::Utc>,
    ) -> Result<crate::state::SparseIdentity, crate::state::error::StateError> {
        unimplemented!()
    }
}
#[async_trait::async_trait]
impl DownloadStateStore for FailingDownloadStore {
    #[cfg(test)]
    async fn should_download(
        &self,
        _: &str,
        _: &str,
        _: &str,
        _: &str,
        _: &Path,
    ) -> Result<bool, StateError> {
        unimplemented!()
    }

    async fn upsert_seen(&self, _: &AssetRecord) -> Result<(), StateError> {
        unimplemented!()
    }

    async fn mark_downloaded(
        &self,
        _: &str,
        _: &str,
        _: &str,
        _: &Path,
        _: &str,
        _: Option<&str>,
    ) -> Result<(), StateError> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        let prev = self.remaining_failures.fetch_sub(1, Ordering::Relaxed);
        if prev > 0 {
            Err(StateError::LockPoisoned("simulated failure".into()))
        } else {
            self.remaining_failures.store(0, Ordering::Relaxed);
            self.successes.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }
    }

    async fn mark_failed(
        &self,
        library: &str,
        id: &str,
        version: &str,
        error: &str,
    ) -> Result<(), StateError> {
        if self.track_failed_calls {
            self.failed_calls.fetch_add(1, Ordering::Relaxed);
            if let Some(db) = &self.publication_db {
                return db.mark_failed(library, id, version, error).await;
            }
            Ok(())
        } else {
            unimplemented!()
        }
    }

    #[cfg(test)]
    async fn get_pending(&self) -> Result<Vec<AssetRecord>, StateError> {
        Ok(Vec::new())
    }

    async fn get_policy_excluded_ids_for_revalidation(
        &self,
        _: &str,
    ) -> Result<Vec<String>, StateError> {
        Ok(Vec::new())
    }

    async fn reset_failed(&self) -> Result<u64, StateError> {
        unimplemented!()
    }

    async fn prepare_for_retry(
        &self,
        _library: Option<&str>,
        _error_retention: crate::state::RetryErrorRetention,
    ) -> Result<(u64, u64, u64), StateError> {
        Ok((0, 0, 0))
    }

    async fn promote_pending_to_failed(&self, _seen_since: i64) -> Result<u64, StateError> {
        Ok(0)
    }

    async fn get_downloaded_ids(&self) -> Result<HashSet<(String, String, String)>, StateError> {
        Ok(HashSet::new())
    }

    async fn get_soft_deleted_downloaded_ids(
        &self,
    ) -> Result<HashSet<(String, String)>, StateError> {
        Ok(HashSet::new())
    }

    async fn get_all_known_ids(&self) -> Result<HashSet<(String, String)>, StateError> {
        Ok(HashSet::new())
    }

    async fn get_downloaded_checksums(
        &self,
    ) -> Result<HashMap<(String, String, String), String>, StateError> {
        Ok(HashMap::new())
    }

    async fn get_attempt_counts(&self) -> Result<HashMap<(String, String), u32>, StateError> {
        Ok(HashMap::new())
    }

    async fn touch_last_seen_many(&self, _: &str, _: &[&str]) -> Result<(), StateError> {
        Ok(())
    }

    async fn mark_policy_excluded(&self, _: &str, _: &str, _: &str) -> Result<bool, StateError> {
        unimplemented!()
    }

    async fn mark_soft_deleted(
        &self,
        _: &str,
        _: &str,
        _: Option<chrono::DateTime<chrono::Utc>>,
    ) -> Result<(), StateError> {
        Ok(())
    }

    async fn mark_hidden_at_source(&self, _: &str, _: &str) -> Result<(), StateError> {
        Ok(())
    }
}

#[async_trait::async_trait]
impl ImportStateStore for FailingDownloadStore {
    async fn import_adopt(
        &self,
        _: &AssetRecord,
        _: &Path,
        _: &str,
        _: u64,
        _: Option<i64>,
        _: Option<(&str, &str)>,
    ) -> Result<(), StateError> {
        unimplemented!()
    }

    async fn get_all_imported_records(
        &self,
        _: &str,
    ) -> Result<HashMap<(String, String), crate::state::ImportedRecord>, StateError> {
        Ok(HashMap::new())
    }
}

#[async_trait::async_trait]
impl crate::state::ReconciliationStateStore for FailingDownloadStore {
    async fn get_reconciliation_catalog_paths(
        &self,
    ) -> Result<Vec<crate::state::ReconciliationCatalogPath>, crate::state::error::StateError> {
        Ok(Vec::new())
    }

    async fn get_reconciliation_reservations(
        &self,
    ) -> Result<Vec<crate::state::ReconciliationReservation>, crate::state::error::StateError> {
        Ok(Vec::new())
    }

    async fn reserve_reconciliation_paths(
        &self,
        _reservations: &[crate::state::ReconciliationReservation],
        _actual_child: Option<&str>,
    ) -> Result<(), crate::state::error::StateError> {
        Err(StateError::Invariant {
            operation: "reserve_reconciliation_paths",
            detail: "test store does not support reconciliation".into(),
        })
    }
}

#[async_trait::async_trait]
impl crate::state::DownloadContextStateStore for FailingDownloadStore {
    async fn get_downloaded_file_records(
        &self,
    ) -> Result<Vec<crate::state::DownloadedFileRecord>, StateError> {
        self.downloaded_state_loads.fetch_add(1, Ordering::Relaxed);
        Ok(Vec::new())
    }
}

#[async_trait::async_trait]
impl TempFileOwnershipStore for FailingDownloadStore {
    async fn prepare_temp_publication(
        &self,
        part: &Path,
        proof: &crate::state::db::PreparedPublication,
    ) -> Result<(), StateError> {
        match &self.publication_db {
            Some(db) => db.prepare_temp_publication(part, proof).await,
            None => Err(StateError::Invariant {
                operation: "prepare_temp_publication",
                detail: "store cannot retain prepared publication evidence".into(),
            }),
        }
    }

    async fn claim_temp_file(&self, path: &Path) -> Result<(), StateError> {
        match &self.publication_db {
            Some(db) => db.claim_temp_file(path).await,
            None => Ok(()),
        }
    }

    async fn get_owned_temp_files_before(
        &self,
        _: i64,
    ) -> Result<Vec<crate::state::OwnedTempFile>, StateError> {
        Ok(Vec::new())
    }

    async fn retire_temp_files(&self, paths: &[PathBuf]) -> Result<u64, StateError> {
        match &self.publication_db {
            Some(db) => db.retire_temp_files(paths).await,
            None => Ok(0),
        }
    }
}

#[async_trait::async_trait]
impl ReportStateStore for FailingDownloadStore {
    #[cfg(test)]
    async fn get_failed(&self) -> Result<Vec<AssetRecord>, StateError> {
        unimplemented!()
    }

    async fn get_failed_sample(&self, _limit: u32) -> Result<(Vec<AssetRecord>, u64), StateError> {
        Ok((Vec::new(), 0))
    }

    async fn get_failed_page(
        &self,
        _offset: u64,
        _limit: u32,
    ) -> Result<Vec<AssetRecord>, StateError> {
        unimplemented!()
    }

    async fn get_pending_page(
        &self,
        _offset: u64,
        _limit: u32,
    ) -> Result<Vec<AssetRecord>, StateError> {
        unimplemented!()
    }

    async fn get_summary(&self) -> Result<SyncSummary, StateError> {
        unimplemented!()
    }

    async fn get_downloaded_page(
        &self,
        _offset: u64,
        _limit: u32,
    ) -> Result<Vec<AssetRecord>, StateError> {
        Ok(Vec::new())
    }

    async fn start_sync_run_at(&self, _: chrono::DateTime<chrono::Utc>) -> Result<i64, StateError> {
        Ok(1)
    }

    async fn start_sync_run(&self) -> Result<i64, StateError> {
        Ok(1)
    }

    async fn complete_sync_run(&self, _: i64, _: &SyncRunStats) -> Result<(), StateError> {
        if self.fail_complete_sync_run {
            Err(StateError::LockPoisoned(
                "simulated complete_sync_run failure".into(),
            ))
        } else {
            Ok(())
        }
    }

    async fn promote_orphaned_sync_runs(&self) -> Result<u64, StateError> {
        Ok(0)
    }
}

#[async_trait::async_trait]
impl SyncTokenStore for FailingDownloadStore {
    async fn get_metadata(&self, _: &str) -> Result<Option<String>, StateError> {
        Ok(None)
    }

    async fn set_metadata(&self, _: &str, _: &str) -> Result<(), StateError> {
        Ok(())
    }

    async fn delete_metadata_by_prefix(&self, _: &str) -> Result<u64, StateError> {
        Ok(0)
    }

    async fn begin_enum_progress(&self, _zone: &str) -> Result<(), StateError> {
        Ok(())
    }

    async fn end_enum_progress(&self, _zone: &str) -> Result<(), StateError> {
        Ok(())
    }

    async fn list_interrupted_enumerations(&self) -> Result<Vec<String>, StateError> {
        Ok(Vec::new())
    }
}

#[async_trait::async_trait]
impl MembershipStore for FailingDownloadStore {
    async fn add_asset_album(&self, _: &str, _: &str, _: &str, _: &str) -> Result<(), StateError> {
        Ok(())
    }

    async fn get_all_asset_albums(&self, _: &str) -> Result<Vec<(String, String)>, StateError> {
        Ok(Vec::new())
    }

    async fn get_all_asset_people(&self, _: &str) -> Result<Vec<(String, String)>, StateError> {
        Ok(Vec::new())
    }
}

#[async_trait::async_trait]
impl MetadataRewriteStore for FailingDownloadStore {
    async fn record_metadata_write_failure(
        &self,
        _: &str,
        _: &str,
        _: &str,
    ) -> Result<(), StateError> {
        Ok(())
    }

    async fn refresh_downloaded_asset_metadata(
        &self,
        _: &str,
        _: &str,
        _: (
            &crate::state::MetadataCapture,
            chrono::DateTime<chrono::Utc>,
            Option<chrono::DateTime<chrono::Utc>>,
        ),
        _: bool,
        _: bool,
        _: i64,
    ) -> Result<usize, StateError> {
        Ok(0)
    }

    async fn get_downloaded_metadata_hashes(
        &self,
    ) -> Result<HashMap<(String, String, String), String>, StateError> {
        Ok(HashMap::new())
    }

    async fn get_metadata_retry_markers(
        &self,
    ) -> Result<HashSet<(String, String, String)>, StateError> {
        Ok(HashSet::new())
    }

    async fn get_pending_metadata_rewrites_page_for_queue(
        &self,
        _: crate::state::db::MetadataRewriteQueue,
        _: Option<&[&str]>,
        _: usize,
        _: usize,
    ) -> Result<Vec<crate::state::db::PendingMetadataRewrite>, StateError> {
        Ok(Vec::new())
    }

    async fn record_capture_repair_prepared(
        &self,
        _: &crate::state::db::PendingMetadataRewrite,
        _: &str,
        _: u64,
    ) -> Result<Option<crate::state::db::CaptureRepairReceipt>, StateError> {
        Ok(None)
    }

    async fn finish_metadata_rewrite(
        &self,
        _: &crate::state::db::PendingMetadataRewrite,
        selected_queue: crate::state::db::MetadataRewriteQueue,
        _: Option<&str>,
        _: Option<&str>,
        completion: crate::state::db::MetadataRewriteCompletion,
    ) -> Result<bool, StateError> {
        Ok(matches!(
            (selected_queue, completion),
            (
                crate::state::db::MetadataRewriteQueue::Ordinary,
                crate::state::db::MetadataRewriteCompletion::Ordinary
                    | crate::state::db::MetadataRewriteCompletion::Both
            ) | (
                crate::state::db::MetadataRewriteQueue::CaptureRepair,
                crate::state::db::MetadataRewriteCompletion::CaptureRepair
                    | crate::state::db::MetadataRewriteCompletion::Both
            )
        ))
    }
}

/// Minimal valid JPEG (SOI + APP0 JFIF + EOI). The XMP toolkit accepts it,
/// so a metadata write against it exercises the real writer.
#[cfg(any(unix, feature = "xmp"))]
pub(super) const MINIMAL_JPEG: [u8; 22] = [
    0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0x4A, 0x46, 0x49, 0x46, 0x00, 0x01, 0x01, 0x00, 0x00, 0x01,
    0x00, 0x01, 0x00, 0x00, 0xFF, 0xD9,
];

/// The `.xmp` sidecar kei writes beside a media file.
#[cfg(feature = "xmp")]
pub(super) fn sidecar_path_for(media_path: &std::path::Path) -> std::path::PathBuf {
    let mut name = media_path.file_name().unwrap().to_os_string();
    name.push(".xmp");
    media_path.with_file_name(name)
}

/// Current managed downloads have authenticated publication ownership, not
/// merely a legacy source-byte receipt. The mock cannot contact a live provider.
pub(super) async fn owned_download_fixture(
    root: &std::path::Path,
    library: &str,
) -> (
    Arc<crate::state::SqliteStateDb>,
    crate::download::DownloadConfig,
) {
    use crate::icloud::photos::{PhotoAlbum, PhotoAlbumConfig, inbox::ShadowCapture};
    use crate::state::db::account::AccountOwner;
    use serde_json::json;

    let owner = AccountOwner::authenticated(
        "writer@example.invalid",
        "com",
        &serde_json::from_value(json!({"dsInfo":{"dsid":"synthetic-writer"}})).unwrap(),
    )
    .unwrap();
    let db = Arc::new(
        crate::state::SqliteStateDb::open_owned(&root.join("state.db"), &owner)
            .await
            .unwrap(),
    );
    let mut album = PhotoAlbum::new(
        PhotoAlbumConfig {
            params: Arc::new(std::collections::HashMap::new()),
            service_endpoint: "https://invalid.example".into(),
            name: "selected".into(),
            list_type: "CPLAssetAndMasterByAssetDateWithoutHiddenOrDeleted".into(),
            obj_type: "CPLAssetByAssetDateWithoutHiddenOrDeleted".into(),
            query_filter: None,
            page_size: 1,
            zone_id: Arc::new(json!({"zoneName":library,"ownerRecordName":"_defaultOwner"})),
            retry_config: crate::retry::RetryConfig::default(),
            container_id: None,
            cross_zone_sources: Vec::new(),
        },
        Box::new(crate::test_helpers::MockPhotosSession::new()),
    );
    album.set_shadow_capture(
        ShadowCapture::new(db.clone(), owner, "com"),
        "private".into(),
    );
    let mut config = crate::download::DownloadConfig::test_default();
    config.directory = Arc::from(root);
    config.library = Arc::from(library);
    config.state_db = Some(db.clone());
    config.primary_layout_source = Some(Arc::new(album));
    config.primary_layout_pass = Some("selected-pass".into());
    (db, config)
}
