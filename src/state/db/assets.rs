//! Asset lifecycle, download finalization, retry eligibility, and source-state transitions.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
#[cfg(test)]
use rusqlite::OptionalExtension;

use super::asset_writes::{
    DownloadChecksums, ensure_asset_has_no_prepared_capture_repair,
    ensure_master_family_has_no_prepared_capture_repair, record_metadata_capture_revision,
    update_status_to_downloaded, upsert_asset_row,
};
use super::membership::refresh_asset_album_groupings_tx;
#[cfg(test)]
use super::rows::{ASSET_COLUMNS, row_to_asset_record};
use super::{
    AssetVerificationState, DownloadContextStateStore, DownloadStateStore, DownloadedFileRecord,
    ReportStateStore, RetryErrorRetention, SqliteStateDb,
};
use crate::state::error::StateError;
#[cfg(test)]
use crate::state::types::AssetStatus;
use crate::state::types::{AssetRecord, METADATA_CAPTURE_REVISION, VersionSizeKey};

impl SqliteStateDb {
    #[cfg(test)]
    pub(crate) async fn should_download(
        &self,
        library: &str,
        id: &str,
        version_size: &str,
        checksum: &str,
        local_path: &Path,
    ) -> Result<bool, StateError> {
        if checksum.is_empty() {
            tracing::warn!(
                id,
                version_size,
                "Empty remote checksum cannot be trusted for state skip decisions"
            );
            return Ok(true);
        }

        let library_owned = library.to_owned();
        let id_owned = id.to_owned();
        let version_size_owned = version_size.to_owned();
        let result: Option<(String, String, Option<String>)> = self
            .with_conn("should_download", move |conn| {
                conn.query_row(
                    "SELECT status, checksum, local_path FROM assets \
                     WHERE library = ?1 AND id = ?2 AND version_size = ?3",
                    [&library_owned, &id_owned, &version_size_owned],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .optional()
                .map_err(|e| StateError::query("should_download", e))
            })
            .await?;

        match result {
            None => {
                // Not in database — should download
                Ok(true)
            }
            Some((status_str, stored_checksum, stored_path_opt)) => {
                let status = AssetStatus::from_str(&status_str).unwrap_or(AssetStatus::Pending);

                // Checksum changed — re-download
                if stored_checksum != checksum {
                    tracing::debug!(
                        id = %id,
                        "Asset checksum changed, will re-download"
                    );
                    return Ok(true);
                }

                match status {
                    AssetStatus::Downloaded => {
                        // Check if file still exists (async to avoid blocking)
                        let path_to_check: PathBuf = stored_path_opt
                            .map(PathBuf::from)
                            .unwrap_or_else(|| local_path.to_path_buf());
                        match tokio::fs::try_exists(&path_to_check).await {
                            Ok(true) => Ok(false),
                            Ok(false) => {
                                tracing::debug!(
                                    id = %id,
                                    path = %path_to_check.display(),
                                    "Downloaded file missing, will re-download"
                                );
                                Ok(true)
                            }
                            Err(e) => {
                                tracing::warn!(
                                    id = %id,
                                    path = %path_to_check.display(),
                                    error = %e,
                                    "Failed to check file existence, assuming missing"
                                );
                                Ok(true)
                            }
                        }
                    }
                    AssetStatus::Pending | AssetStatus::PolicyExcluded | AssetStatus::Failed => {
                        Ok(true)
                    }
                }
            }
        }
    }

    pub(crate) async fn upsert_seen(&self, record: &AssetRecord) -> Result<(), StateError> {
        let record = record.clone();
        self.with_conn_mut("upsert_seen", move |conn| {
            let tx = conn.transaction().map_err(|e| StateError::query("upsert_seen::begin", e))?;
            upsert_asset_row(&tx, &record, Utc::now().timestamp())?;
            refresh_asset_album_groupings_tx(&tx, &record.library, &record.id, None)?;
            // Membership may precede the first state row. Keep a retry marker
            // until the end-of-cycle writer sees all selected album producers.
            tx.execute(
                "UPDATE assets SET metadata_write_failed_at = COALESCE(metadata_write_failed_at, ?3) \
                 WHERE library = ?1 AND id = ?2 AND status = 'pending' AND EXISTS ( \
                     SELECT 1 FROM asset_albums WHERE library = ?1 AND asset_id = ?2)",
                rusqlite::params![&record.library, &record.id, Utc::now().timestamp()],
            ).map_err(|e| StateError::query("upsert_seen::album_marker", e))?;
            tx.commit().map_err(|e| StateError::query("upsert_seen::commit", e))
        })
        .await
    }

    pub(crate) async fn mark_downloaded(
        &self,
        library: &str,
        id: &str,
        version_size: &str,
        local_path: &Path,
        local_checksum: &str,
        download_checksum: Option<&str>,
    ) -> Result<(), StateError> {
        self.mark_downloaded_with_capture_repair(
            library,
            id,
            version_size,
            local_path,
            local_checksum,
            download_checksum,
            false,
        )
        .await
    }

    pub(crate) async fn mark_downloaded_with_capture_repair(
        &self,
        library: &str,
        id: &str,
        version_size: &str,
        local_path: &Path,
        local_checksum: &str,
        download_checksum: Option<&str>,
        mark_capture_repair: bool,
    ) -> Result<(), StateError> {
        self.mark_downloaded_with_checksums(
            library,
            id,
            version_size,
            local_path,
            DownloadChecksums {
                local: local_checksum,
                downloaded: download_checksum,
                source: None,
            },
            mark_capture_repair,
        )
        .await
    }

    async fn mark_downloaded_with_checksums(
        &self,
        library: &str,
        id: &str,
        version_size: &str,
        local_path: &Path,
        checksums: DownloadChecksums<'_>,
        mark_capture_repair: bool,
    ) -> Result<(), StateError> {
        let downloaded_at = Utc::now().timestamp();
        let library = library.to_owned();
        let id = id.to_owned();
        let version_size = version_size.to_owned();
        let local_path = local_path.to_path_buf();
        let local_checksum = checksums.local.to_owned();
        let download_checksum = checksums.downloaded.map(str::to_owned);
        let source_checksum = checksums.source.map(str::to_owned);

        self.with_conn_mut("mark_downloaded", move |conn| {
            let tx = conn
                .transaction()
                .map_err(|e| StateError::query("mark_downloaded::begin", e))?;
            let rows = update_status_to_downloaded(
                &tx,
                &library,
                &id,
                &version_size,
                &local_path,
                &local_checksum,
                download_checksum.as_deref(),
                mark_capture_repair,
                downloaded_at,
            )?;

            if rows == 0 {
                crate::metrics::MARK_DOWNLOADED_ZERO_ROWS.inc();
                return Err(StateError::AssetRowMissing {
                    asset_id: id,
                    version_size,
                });
            }

            if let Some(source_checksum) = &source_checksum {
                // CONTRACT: XMP_GPS_ACCURACY_REQUIRES_MATCHING_LOCATION
                // Source provenance and downloaded state commit together.
                tx.execute(
                    "UPDATE asset_metadata_paths SET source_checksum = ?5 \
                     WHERE library = ?1 AND id = ?2 AND version_size = ?3 AND local_path = ?4",
                    rusqlite::params![
                        library,
                        id,
                        version_size,
                        local_path.to_string_lossy(),
                        source_checksum
                    ],
                )
                .map_err(|e| StateError::query("mark_downloaded::source_checksum", e))?;
            }

            record_metadata_capture_revision(
                &tx,
                &library,
                &id,
                METADATA_CAPTURE_REVISION,
                downloaded_at,
            )?;

            tx.commit()
                .map_err(|e| StateError::query("mark_downloaded::commit", e))?;
            Ok(())
        })
        .await
    }

    pub(crate) async fn mark_failed(
        &self,
        library: &str,
        id: &str,
        version_size: &str,
        error: &str,
    ) -> Result<(), StateError> {
        let library = library.to_owned();
        let id = id.to_owned();
        let version_size = version_size.to_owned();
        let error = error.to_owned();

        self.with_conn("mark_failed", move |conn| {
            let rows = conn
                .execute(
                    "UPDATE assets SET status = 'failed', download_attempts = download_attempts + 1, \
                     last_error = ?1 WHERE library = ?2 AND id = ?3 AND version_size = ?4",
                    rusqlite::params![&error, &library, &id, &version_size],
                )
                .map_err(|e| StateError::query("mark_failed", e))?;

            if rows == 0 {
                tracing::error!(
                    id = %id,
                    version_size = %version_size,
                    "mark_failed matched 0 rows; caller must upsert_seen before mark_failed \
                     (producer-dispatch invariant). Failure not persisted"
                );
                crate::metrics::MARK_FAILED_ZERO_ROWS.inc();
                return Err(StateError::Invariant {
                    operation: "mark_failed",
                    detail: format!(
                        "library={library} id={id} version_size={version_size} \
                         not present; upsert_seen must run before mark_failed"
                    ),
                });
            }

            Ok(())
        })
        .await
    }

    #[cfg(test)]
    pub(crate) async fn get_pending(&self) -> Result<Vec<AssetRecord>, StateError> {
        self.with_conn("get_pending", move |conn| {
            let sql = format!(
                "SELECT {ASSET_COLUMNS} FROM assets WHERE status = 'pending' AND is_deleted = 0 \
                 ORDER BY last_seen_at DESC",
            );
            let mut stmt = conn
                .prepare(&sql)
                .map_err(|e| StateError::query("get_pending", e))?;

            let records = stmt
                .query_map([], row_to_asset_record)
                .map_err(|e| StateError::query("get_pending", e))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| StateError::query("get_pending", e))?;

            Ok(records)
        })
        .await
    }

    pub(crate) async fn get_policy_excluded_ids_for_revalidation(
        &self,
        library: &str,
    ) -> Result<Vec<String>, StateError> {
        let library = library.to_owned();
        self.with_conn("get_policy_excluded_ids_for_revalidation", move |conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT DISTINCT id FROM assets \
                     WHERE status = 'policy_excluded' AND is_deleted = 0 AND library = ?1 \
                     ORDER BY id",
                )
                .map_err(|e| StateError::query("get_policy_excluded_ids_for_revalidation", e))?;

            let records = stmt
                .query_map(rusqlite::params![library], |row| row.get(0))
                .map_err(|e| StateError::query("get_policy_excluded_ids_for_revalidation", e))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| StateError::query("get_policy_excluded_ids_for_revalidation", e))?;

            Ok(records)
        })
        .await
    }

    pub(crate) async fn reset_failed(&self) -> Result<u64, StateError> {
        let (failed, _, _) = self
            .prepare_for_retry(None, RetryErrorRetention::Clear)
            .await?;
        Ok(failed)
    }

    pub(crate) async fn prepare_for_retry(
        &self,
        library: Option<&str>,
        error_retention: RetryErrorRetention,
    ) -> Result<(u64, u64, u64), StateError> {
        let library = library.map(ToOwned::to_owned);
        self.with_conn("prepare_for_retry", move |conn| {
            let failed = match (library.as_deref(), error_retention) {
                (Some(library), RetryErrorRetention::Clear) => conn.execute(
                    "UPDATE assets SET status = 'pending', download_attempts = 0, last_error = NULL \
                     WHERE status = 'failed' AND is_deleted = 0 AND library = ?1",
                    rusqlite::params![library],
                ),
                (None, RetryErrorRetention::Clear) => conn.execute(
                    "UPDATE assets SET status = 'pending', download_attempts = 0, last_error = NULL \
                     WHERE status = 'failed' AND is_deleted = 0",
                    [],
                ),
                (Some(library), RetryErrorRetention::Preserve(reason)) => conn.execute(
                    "UPDATE assets SET status = 'pending', download_attempts = 0, \
                     last_error = CASE WHEN last_error = ?2 THEN last_error ELSE NULL END \
                     WHERE status = 'failed' AND is_deleted = 0 AND library = ?1",
                    rusqlite::params![library, reason],
                ),
                (None, RetryErrorRetention::Preserve(reason)) => conn.execute(
                    "UPDATE assets SET status = 'pending', download_attempts = 0, \
                     last_error = CASE WHEN last_error = ?1 THEN last_error ELSE NULL END \
                     WHERE status = 'failed' AND is_deleted = 0",
                    rusqlite::params![reason],
                ),
            }
            .map_err(|e| StateError::query("prepare_for_retry", e))?
                as u64;

            let pending = match (library.as_deref(), error_retention) {
                (Some(library), RetryErrorRetention::Clear) => conn.execute(
                    "UPDATE assets SET download_attempts = 0, last_error = NULL \
                     WHERE status = 'pending' AND is_deleted = 0 AND download_attempts > 0 AND library = ?1",
                    rusqlite::params![library],
                ),
                (None, RetryErrorRetention::Clear) => conn.execute(
                    "UPDATE assets SET download_attempts = 0, last_error = NULL \
                     WHERE status = 'pending' AND is_deleted = 0 AND download_attempts > 0",
                    [],
                ),
                (Some(library), RetryErrorRetention::Preserve(reason)) => conn.execute(
                    "UPDATE assets SET download_attempts = 0, \
                     last_error = CASE WHEN last_error = ?2 THEN last_error ELSE NULL END \
                     WHERE status = 'pending' AND is_deleted = 0 AND download_attempts > 0 AND library = ?1",
                    rusqlite::params![library, reason],
                ),
                (None, RetryErrorRetention::Preserve(reason)) => conn.execute(
                    "UPDATE assets SET download_attempts = 0, \
                     last_error = CASE WHEN last_error = ?1 THEN last_error ELSE NULL END \
                     WHERE status = 'pending' AND is_deleted = 0 AND download_attempts > 0",
                    rusqlite::params![reason],
                ),
            }
            .map_err(|e| StateError::query("prepare_for_retry", e))?
                as u64;

            let total_pending: i64 = if let Some(library) = library.as_deref() {
                conn.query_row(
                    "SELECT COUNT(*) FROM assets WHERE status = 'pending' AND is_deleted = 0 AND library = ?1",
                    rusqlite::params![library],
                    |row| row.get(0),
                )
            } else {
                conn.query_row(
                    "SELECT COUNT(*) FROM assets WHERE status = 'pending' AND is_deleted = 0",
                    [],
                    |row| row.get(0),
                )
            }
            .map_err(|e| StateError::query("prepare_for_retry", e))?;
            #[allow(clippy::cast_sign_loss, reason = "SQL COUNT(*) is always non-negative")]
            let total_pending = total_pending as u64;

            Ok((failed, pending, total_pending))
        })
        .await
    }

    pub(crate) async fn prune_source_deleted_retries(
        &self,
        _library: Option<&str>,
    ) -> Result<u64, StateError> {
        Ok(0)
    }

    pub(crate) async fn promote_pending_to_failed(
        &self,
        seen_since: i64,
    ) -> Result<u64, StateError> {
        self.with_conn("promote_pending_to_failed", move |conn| {
            // Only promote assets the producer dispatched this sync (last_seen_at
            // was bumped by upsert_seen at or after sync_started_at) that never
            // reached mark_downloaded or mark_failed. See the trait doc comment
            // and issue #211 for the rationale.
            let promoted = conn
                .execute(
                    "UPDATE assets SET status = 'failed', last_error = 'Not resolved during sync' \
                     WHERE status = 'pending' AND last_seen_at >= ?1 \
                       AND NOT EXISTS ( \
                         SELECT 1 FROM asset_verifications AS verification \
                         WHERE verification.library = assets.library \
                           AND verification.id = assets.id \
                           AND verification.version_size = assets.version_size \
                       )",
                    rusqlite::params![seen_since],
                )
                .map_err(|e| StateError::query("promote_pending_to_failed", e))?
                as u64;

            Ok(promoted)
        })
        .await
    }

    pub(crate) async fn prune_stale_pending_not_seen_since(
        &self,
        library: &str,
        seen_since: i64,
    ) -> Result<u64, StateError> {
        let library = library.to_string();
        self.with_conn("prune_stale_pending_not_seen_since", move |conn| {
            let pruned = conn
                .execute(
                    "DELETE FROM assets \
                     WHERE library = ?1 AND status = 'pending' AND last_seen_at < ?2",
                    rusqlite::params![library, seen_since],
                )
                .map_err(|e| StateError::query("prune_stale_pending_not_seen_since", e))?
                as u64;

            Ok(pruned)
        })
        .await
    }

    pub(crate) async fn prune_pending_asset_versions(
        &self,
        library: &str,
        asset_versions: &[(String, String)],
    ) -> Result<u64, StateError> {
        if asset_versions.is_empty() {
            return Ok(0);
        }
        let library = library.to_string();
        let asset_versions = asset_versions.to_vec();
        self.with_conn_mut("prune_pending_asset_versions", move |conn| {
            let tx = conn
                .transaction()
                .map_err(|e| StateError::query("prune_pending_asset_versions::begin", e))?;
            let pruned = {
                let mut stmt = tx
                    .prepare_cached(
                        "DELETE FROM assets \
                         WHERE library = ?1 AND id = ?2 AND version_size = ?3 \
                           AND status = 'pending'",
                    )
                    .map_err(|e| StateError::query("prune_pending_asset_versions::prepare", e))?;
                let mut pruned = 0u64;
                for (asset_id, version_size) in &asset_versions {
                    pruned += stmt
                        .execute(rusqlite::params![&library, asset_id, version_size])
                        .map_err(|e| {
                            StateError::query("prune_pending_asset_versions::execute", e)
                        })? as u64;
                }
                pruned
            };
            tx.commit()
                .map_err(|e| StateError::query("prune_pending_asset_versions::commit", e))?;
            Ok(pruned)
        })
        .await
    }

    pub(crate) async fn get_downloaded_ids(
        &self,
    ) -> Result<HashSet<(String, String, String)>, StateError> {
        self.with_conn("get_downloaded_ids", move |conn| {
            let count: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM assets WHERE status = 'downloaded'",
                    [],
                    |row| row.get(0),
                )
                .map_err(|e| StateError::query("get_downloaded_ids", e))?;
            let count = usize::try_from(count).unwrap_or(0);

            let mut stmt = conn
                .prepare_cached(
                    "SELECT library, id, version_size FROM assets WHERE status = 'downloaded'",
                )
                .map_err(|e| StateError::query("get_downloaded_ids", e))?;

            let mut ids = HashSet::with_capacity(count);
            let rows = stmt
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                })
                .map_err(|e| StateError::query("get_downloaded_ids", e))?;
            for row in rows {
                ids.insert(row.map_err(|e| StateError::query("get_downloaded_ids", e))?);
            }

            Ok(ids)
        })
        .await
    }

    pub(crate) async fn get_downloaded_file_records(
        &self,
    ) -> Result<Vec<DownloadedFileRecord>, StateError> {
        self.with_conn("get_downloaded_file_records", move |conn| {
            let mut stmt = conn
                .prepare_cached(
                    "SELECT library, id, version_size, checksum, local_path, \
                            local_checksum, download_checksum \
                     FROM assets WHERE status = 'downloaded'",
                )
                .map_err(|e| StateError::query("get_downloaded_file_records", e))?;

            stmt.query_map([], |row| {
                let version_size: String = row.get(2)?;
                let local_path: Option<String> = row.get(4)?;
                Ok(DownloadedFileRecord {
                    library: row.get(0)?,
                    id: row.get(1)?,
                    version_size: VersionSizeKey::from_str(&version_size)
                        .unwrap_or(VersionSizeKey::Original),
                    checksum: row.get(3)?,
                    local_path: local_path.map(PathBuf::from),
                    local_checksum: row.get(5)?,
                    download_checksum: row.get(6)?,
                })
            })
            .map_err(|e| StateError::query("get_downloaded_file_records", e))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| StateError::query("get_downloaded_file_records", e))
        })
        .await
    }

    /// Assets that still hold `status = 'downloaded'` rows after the provider
    /// reported them deleted. Soft deletion flips every version of an asset
    /// together, so this is keyed by asset rather than by version.
    pub(crate) async fn get_soft_deleted_downloaded_ids(
        &self,
    ) -> Result<HashSet<(String, String)>, StateError> {
        self.with_conn("get_soft_deleted_downloaded_ids", move |conn| {
            let mut stmt = conn
                .prepare_cached(
                    "SELECT DISTINCT library, id FROM assets \
                     WHERE status = 'downloaded' AND is_deleted = 1",
                )
                .map_err(|e| StateError::query("get_soft_deleted_downloaded_ids", e))?;

            let mut ids = HashSet::new();
            let rows = stmt
                .query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })
                .map_err(|e| StateError::query("get_soft_deleted_downloaded_ids", e))?;
            for row in rows {
                ids.insert(
                    row.map_err(|e| StateError::query("get_soft_deleted_downloaded_ids", e))?,
                );
            }

            Ok(ids)
        })
        .await
    }

    pub(crate) async fn get_all_known_ids(&self) -> Result<HashSet<(String, String)>, StateError> {
        self.with_conn("get_all_known_ids", move |conn| {
            let mut stmt = conn
                .prepare_cached("SELECT DISTINCT library, id FROM assets")
                .map_err(|e| StateError::query("get_all_known_ids", e))?;

            let ids = stmt
                .query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })
                .map_err(|e| StateError::query("get_all_known_ids", e))?
                .collect::<Result<HashSet<_>, _>>()
                .map_err(|e| StateError::query("get_all_known_ids", e))?;

            Ok(ids)
        })
        .await
    }

    pub(crate) async fn get_downloaded_checksums(
        &self,
    ) -> Result<HashMap<(String, String, String), String>, StateError> {
        self.with_conn("get_downloaded_checksums", move |conn| {
            let count: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM assets WHERE status = 'downloaded'",
                    [],
                    |row| row.get(0),
                )
                .map_err(|e| StateError::query("get_downloaded_checksums", e))?;
            let count = usize::try_from(count).unwrap_or(0);

            let mut stmt = conn
                .prepare_cached(
                    "SELECT library, id, version_size, checksum FROM assets \
                     WHERE status = 'downloaded'",
                )
                .map_err(|e| StateError::query("get_downloaded_checksums", e))?;

            let mut checksums = HashMap::with_capacity(count);
            let rows = stmt
                .query_map([], |row| {
                    Ok((
                        (
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, String>(2)?,
                        ),
                        row.get::<_, String>(3)?,
                    ))
                })
                .map_err(|e| StateError::query("get_downloaded_checksums", e))?;
            for row in rows {
                let (key, val) =
                    row.map_err(|e| StateError::query("get_downloaded_checksums", e))?;
                checksums.insert(key, val);
            }

            Ok(checksums)
        })
        .await
    }

    pub(crate) async fn get_downloaded_local_paths(
        &self,
    ) -> Result<HashMap<(String, String, String), PathBuf>, StateError> {
        self.with_conn("get_downloaded_local_paths", move |conn| {
            let count: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM assets \
                     WHERE status = 'downloaded' AND local_path IS NOT NULL",
                    [],
                    |row| row.get(0),
                )
                .map_err(|e| StateError::query("get_downloaded_local_paths", e))?;
            let count = usize::try_from(count).unwrap_or(0);

            let mut stmt = conn
                .prepare_cached(
                    "SELECT library, id, version_size, local_path FROM assets \
                     WHERE status = 'downloaded' AND local_path IS NOT NULL",
                )
                .map_err(|e| StateError::query("get_downloaded_local_paths", e))?;

            let mut paths = HashMap::with_capacity(count);
            let rows = stmt
                .query_map([], |row| {
                    let local_path: String = row.get(3)?;
                    Ok((
                        (
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, String>(2)?,
                        ),
                        PathBuf::from(local_path),
                    ))
                })
                .map_err(|e| StateError::query("get_downloaded_local_paths", e))?;
            for row in rows {
                let (key, val) =
                    row.map_err(|e| StateError::query("get_downloaded_local_paths", e))?;
                paths.insert(key, val);
            }

            Ok(paths)
        })
        .await
    }

    pub(crate) async fn get_attempt_counts(
        &self,
    ) -> Result<HashMap<(String, String), u32>, StateError> {
        self.with_conn("get_attempt_counts", move |conn| {
            let mut stmt = conn
                .prepare_cached(
                    "SELECT library, id, MAX(download_attempts) FROM assets \
                     WHERE download_attempts > 0 GROUP BY library, id",
                )
                .map_err(|e| StateError::query("get_attempt_counts", e))?;

            let counts = stmt
                .query_map([], |row| {
                    let library: String = row.get(0)?;
                    let id: String = row.get(1)?;
                    let count: i64 = row.get(2)?;
                    Ok(((library, id), u32::try_from(count).unwrap_or(u32::MAX)))
                })
                .map_err(|e| StateError::query("get_attempt_counts", e))?
                .collect::<Result<HashMap<_, _>, _>>()
                .map_err(|e| StateError::query("get_attempt_counts", e))?;

            Ok(counts)
        })
        .await
    }

    pub(crate) async fn touch_last_seen_many(
        &self,
        library: &str,
        asset_ids: &[&str],
    ) -> Result<(), StateError> {
        if asset_ids.is_empty() {
            return Ok(());
        }
        let library = library.to_owned();
        let ids: Vec<String> = asset_ids.iter().map(|s| (*s).to_owned()).collect();
        self.with_conn_mut("touch_last_seen_many", move |conn| {
            let now = Utc::now().timestamp();
            let tx = conn
                .transaction()
                .map_err(|e| StateError::query("touch_last_seen_many::begin", e))?;
            {
                let mut stmt = tx
                    .prepare_cached(
                        "UPDATE assets SET last_seen_at = ?1 WHERE library = ?2 AND id = ?3",
                    )
                    .map_err(|e| StateError::query("touch_last_seen_many::prepare", e))?;
                for id in &ids {
                    stmt.execute(rusqlite::params![now, &library, id])
                        .map_err(|e| StateError::query("touch_last_seen_many::execute", e))?;
                }
            }
            tx.commit()
                .map_err(|e| StateError::query("touch_last_seen_many::commit", e))?;
            Ok(())
        })
        .await
    }

    pub(crate) async fn set_asset_verification(
        &self,
        library: &str,
        id: &str,
        version_size: &str,
        state: AssetVerificationState,
        reason: &str,
    ) -> Result<(), StateError> {
        let library = library.to_owned();
        let id = id.to_owned();
        let version_size = version_size.to_owned();
        let reason = reason.to_owned();
        self.with_conn("set_asset_verification", move |conn| {
            conn.execute(
                "INSERT INTO asset_verifications \
                    (library, id, version_size, state, reason, checked_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6) \
                 ON CONFLICT(library, id, version_size) DO UPDATE SET \
                    state = excluded.state, reason = excluded.reason, \
                    checked_at = excluded.checked_at",
                rusqlite::params![
                    library,
                    id,
                    version_size,
                    state.as_str(),
                    reason,
                    Utc::now().timestamp()
                ],
            )
            .map_err(|e| StateError::query("set_asset_verification", e))?;
            Ok(())
        })
        .await
    }

    pub(crate) async fn clear_asset_verification(
        &self,
        library: &str,
        id: &str,
        version_size: &str,
    ) -> Result<(), StateError> {
        let library = library.to_owned();
        let id = id.to_owned();
        let version_size = version_size.to_owned();
        self.with_conn("clear_asset_verification", move |conn| {
            conn.execute(
                "DELETE FROM asset_verifications \
                 WHERE library = ?1 AND id = ?2 AND version_size = ?3",
                rusqlite::params![library, id, version_size],
            )
            .map_err(|e| StateError::query("clear_asset_verification", e))?;
            Ok(())
        })
        .await
    }

    pub(crate) async fn mark_policy_excluded(
        &self,
        library: &str,
        id: &str,
        version_size: &str,
    ) -> Result<bool, StateError> {
        let library = library.to_owned();
        let id = id.to_owned();
        let version_size = version_size.to_owned();
        self.with_conn_mut("mark_policy_excluded", move |conn| {
            let tx = conn
                .transaction()
                .map_err(|e| StateError::query("mark_policy_excluded::begin", e))?;
            let affected = tx
                .execute(
                    "UPDATE assets SET status = 'policy_excluded', last_error = NULL \
                     WHERE library = ?1 AND id = ?2 AND version_size = ?3 \
                       AND status = 'pending' AND is_deleted = 0",
                    rusqlite::params![library, id, version_size],
                )
                .map_err(|e| StateError::query("mark_policy_excluded::update", e))?;
            if affected > 0 {
                tx.execute(
                    "DELETE FROM asset_verifications \
                     WHERE library = ?1 AND id = ?2 AND version_size = ?3",
                    rusqlite::params![library, id, version_size],
                )
                .map_err(|e| StateError::query("mark_policy_excluded::verification", e))?;
            }
            tx.commit()
                .map_err(|e| StateError::query("mark_policy_excluded::commit", e))?;
            Ok(affected > 0)
        })
        .await
    }

    pub(crate) async fn mark_soft_deleted(
        &self,
        library: &str,
        asset_id: &str,
        deleted_at: Option<DateTime<Utc>>,
    ) -> Result<usize, StateError> {
        let library = library.to_owned();
        let asset_id = asset_id.to_owned();
        self.with_conn("mark_soft_deleted", move |conn| {
            ensure_asset_has_no_prepared_capture_repair(
                conn,
                &library,
                &asset_id,
                "mark_soft_deleted",
            )?;
            let updated = conn
                .execute(
                    "UPDATE assets SET is_deleted = 1, deleted_at = COALESCE(?1, deleted_at) \
                 WHERE library = ?2 AND id = ?3",
                    rusqlite::params![deleted_at.map(|dt| dt.timestamp()), &library, &asset_id],
                )
                .map_err(|e| StateError::query("mark_soft_deleted", e))?;
            Ok(updated)
        })
        .await
    }

    pub(crate) async fn resolve_source_deleted(
        &self,
        library: &str,
        asset_id: &str,
        deleted_at: Option<DateTime<Utc>>,
    ) -> Result<usize, StateError> {
        let library = library.to_owned();
        let asset_id = asset_id.to_owned();
        self.with_conn_mut("resolve_source_deleted", move |conn| {
            let tx = conn
                .transaction()
                .map_err(|e| StateError::query("resolve_source_deleted::begin", e))?;
            ensure_asset_has_no_prepared_capture_repair(
                &tx,
                &library,
                &asset_id,
                "resolve_source_deleted",
            )?;
            let marked = tx
                .execute(
                    "UPDATE assets SET is_deleted = 1, deleted_at = COALESCE(?1, deleted_at) \
                     WHERE library = ?2 AND id = ?3",
                    rusqlite::params![deleted_at.map(|dt| dt.timestamp()), library, asset_id],
                )
                .map_err(|e| StateError::query("resolve_source_deleted::mark", e))?;
            tx.execute(
                "DELETE FROM asset_verifications WHERE library = ?1 AND id = ?2",
                rusqlite::params![&library, &asset_id],
            )
            .map_err(|e| StateError::query("resolve_source_deleted::clear_verification", e))?;
            tx.commit()
                .map_err(|e| StateError::query("resolve_source_deleted::commit", e))?;
            Ok(marked)
        })
        .await
    }

    pub(crate) async fn mark_master_family_soft_deleted(
        &self,
        library: &str,
        master_record_name: &str,
        deleted_at: Option<DateTime<Utc>>,
    ) -> Result<usize, StateError> {
        let library = library.to_owned();
        let master_record_name = master_record_name.to_owned();
        self.with_conn("mark_master_family_soft_deleted", move |conn| {
            ensure_master_family_has_no_prepared_capture_repair(
                conn,
                &library,
                &master_record_name,
                "mark_master_family_soft_deleted",
            )?;
            let updated = conn
                .execute(
                    "UPDATE assets SET is_deleted = 1, deleted_at = COALESCE(?1, deleted_at) \
                     WHERE library = ?2 AND (id = ?3 OR id IN ( \
                        SELECT asset_record_name FROM asset_master_mappings \
                        WHERE library = ?2 AND master_record_name = ?3 \
                     ))",
                    rusqlite::params![
                        deleted_at.map(|dt| dt.timestamp()),
                        &library,
                        &master_record_name
                    ],
                )
                .map_err(|e| StateError::query("mark_master_family_soft_deleted", e))?;
            Ok(updated)
        })
        .await
    }

    pub(crate) async fn resolve_master_family_source_deleted(
        &self,
        library: &str,
        master_record_name: &str,
        deleted_at: Option<DateTime<Utc>>,
    ) -> Result<usize, StateError> {
        let library = library.to_owned();
        let master_record_name = master_record_name.to_owned();
        self.with_conn_mut("resolve_master_family_source_deleted", move |conn| {
            let tx = conn
                .transaction()
                .map_err(|e| StateError::query("resolve_master_family_source_deleted::begin", e))?;
            ensure_master_family_has_no_prepared_capture_repair(
                &tx,
                &library,
                &master_record_name,
                "resolve_master_family_source_deleted",
            )?;
            let marked = tx
                .execute(
                    "UPDATE assets SET is_deleted = 1, deleted_at = COALESCE(?1, deleted_at) \
                     WHERE library = ?2 AND (id = ?3 OR id IN ( \
                        SELECT asset_record_name FROM asset_master_mappings \
                        WHERE library = ?2 AND master_record_name = ?3 \
                     ))",
                    rusqlite::params![
                        deleted_at.map(|dt| dt.timestamp()),
                        library,
                        master_record_name
                    ],
                )
                .map_err(|e| StateError::query("resolve_master_family_source_deleted::mark", e))?;
            tx.execute(
                "DELETE FROM asset_verifications \
                 WHERE library = ?1 AND (id = ?2 OR id IN ( \
                    SELECT asset_record_name FROM asset_master_mappings \
                    WHERE library = ?1 AND master_record_name = ?2 \
                 ))",
                rusqlite::params![&library, &master_record_name],
            )
            .map_err(|e| {
                StateError::query(
                    "resolve_master_family_source_deleted::clear_verification",
                    e,
                )
            })?;
            tx.commit().map_err(|e| {
                StateError::query("resolve_master_family_source_deleted::commit", e)
            })?;
            Ok(marked)
        })
        .await
    }

    pub(crate) async fn mark_hidden_at_source(
        &self,
        library: &str,
        asset_id: &str,
    ) -> Result<usize, StateError> {
        let library = library.to_owned();
        let asset_id = asset_id.to_owned();
        self.with_conn("mark_hidden_at_source", move |conn| {
            let updated = conn
                .execute(
                    "UPDATE assets SET is_hidden = 1 WHERE library = ?1 AND id = ?2",
                    rusqlite::params![library, asset_id],
                )
                .map_err(|e| StateError::query("mark_hidden_at_source", e))?;
            Ok(updated)
        })
        .await
    }
}

#[cfg(test)]
impl SqliteStateDb {
    /// Overwrite `last_seen_at` for a specific asset in `PrimarySync`. Used
    /// by tests that need to simulate a pending row carried over from a
    /// prior sync. Callable from any test module in the crate so
    /// cross-module state tests (e.g. pipeline-level ghost-loop regression)
    /// don't have to reach for raw `rusqlite::Connection` plumbing.
    pub(crate) fn backdate_last_seen(&self, asset_id: &str, ts: i64) {
        self.backdate_last_seen_in(crate::icloud::photos::PRIMARY_ZONE_NAME, asset_id, ts);
    }

    pub(crate) fn backdate_last_seen_in(&self, library: &str, asset_id: &str, ts: i64) {
        let conn = self.acquire_lock("test_backdate_last_seen").unwrap();
        conn.execute(
            "UPDATE assets SET last_seen_at = ?1 WHERE library = ?2 AND id = ?3",
            rusqlite::params![ts, library, asset_id],
        )
        .unwrap();
    }
}

#[async_trait]
impl DownloadStateStore for SqliteStateDb {
    #[cfg(test)]
    async fn should_download(
        &self,
        library: &str,
        id: &str,
        version_size: &str,
        checksum: &str,
        local_path: &Path,
    ) -> Result<bool, StateError> {
        SqliteStateDb::should_download(self, library, id, version_size, checksum, local_path).await
    }

    async fn upsert_seen(&self, record: &AssetRecord) -> Result<(), StateError> {
        SqliteStateDb::upsert_seen(self, record).await
    }

    async fn mark_downloaded(
        &self,
        library: &str,
        id: &str,
        version_size: &str,
        local_path: &Path,
        local_checksum: &str,
        download_checksum: Option<&str>,
    ) -> Result<(), StateError> {
        SqliteStateDb::mark_downloaded(
            self,
            library,
            id,
            version_size,
            local_path,
            local_checksum,
            download_checksum,
        )
        .await
    }

    async fn mark_downloaded_with_capture_repair(
        &self,
        library: &str,
        id: &str,
        version_size: &str,
        local_path: &Path,
        local_checksum: &str,
        download_checksum: Option<&str>,
        mark_capture_repair: bool,
    ) -> Result<(), StateError> {
        SqliteStateDb::mark_downloaded_with_capture_repair(
            self,
            library,
            id,
            version_size,
            local_path,
            local_checksum,
            download_checksum,
            mark_capture_repair,
        )
        .await
    }

    async fn mark_verified_download(
        &self,
        library: &str,
        id: &str,
        version_size: &str,
        local_path: &Path,
        local_checksum: &str,
        download_checksum: Option<&str>,
        mark_capture_repair: bool,
    ) -> Result<(), StateError> {
        self.mark_downloaded_with_checksums(
            library,
            id,
            version_size,
            local_path,
            DownloadChecksums {
                local: local_checksum,
                downloaded: download_checksum,
                source: download_checksum,
            },
            mark_capture_repair,
        )
        .await
    }

    async fn mark_failed(
        &self,
        library: &str,
        id: &str,
        version_size: &str,
        error: &str,
    ) -> Result<(), StateError> {
        SqliteStateDb::mark_failed(self, library, id, version_size, error).await
    }

    async fn get_pending(&self) -> Result<Vec<AssetRecord>, StateError> {
        <SqliteStateDb as ReportStateStore>::get_pending_page(self, 0, u32::MAX).await
    }

    async fn get_policy_excluded_ids_for_revalidation(
        &self,
        library: &str,
    ) -> Result<Vec<String>, StateError> {
        SqliteStateDb::get_policy_excluded_ids_for_revalidation(self, library).await
    }

    async fn reset_failed(&self) -> Result<u64, StateError> {
        SqliteStateDb::reset_failed(self).await
    }

    async fn prepare_for_retry(
        &self,
        library: Option<&str>,
        error_retention: RetryErrorRetention,
    ) -> Result<(u64, u64, u64), StateError> {
        SqliteStateDb::prepare_for_retry(self, library, error_retention).await
    }

    async fn prune_source_deleted_retries(&self, library: Option<&str>) -> Result<u64, StateError> {
        SqliteStateDb::prune_source_deleted_retries(self, library).await
    }

    async fn promote_pending_to_failed(&self, seen_since: i64) -> Result<u64, StateError> {
        SqliteStateDb::promote_pending_to_failed(self, seen_since).await
    }

    async fn prune_stale_pending_not_seen_since(
        &self,
        library: &str,
        seen_since: i64,
    ) -> Result<u64, StateError> {
        SqliteStateDb::prune_stale_pending_not_seen_since(self, library, seen_since).await
    }

    async fn prune_pending_asset_versions(
        &self,
        library: &str,
        asset_versions: &[(String, String)],
    ) -> Result<u64, StateError> {
        SqliteStateDb::prune_pending_asset_versions(self, library, asset_versions).await
    }

    async fn get_downloaded_ids(&self) -> Result<HashSet<(String, String, String)>, StateError> {
        SqliteStateDb::get_downloaded_ids(self).await
    }

    async fn get_soft_deleted_downloaded_ids(
        &self,
    ) -> Result<HashSet<(String, String)>, StateError> {
        SqliteStateDb::get_soft_deleted_downloaded_ids(self).await
    }

    async fn get_all_known_ids(&self) -> Result<HashSet<(String, String)>, StateError> {
        SqliteStateDb::get_all_known_ids(self).await
    }

    async fn get_downloaded_checksums(
        &self,
    ) -> Result<HashMap<(String, String, String), String>, StateError> {
        SqliteStateDb::get_downloaded_checksums(self).await
    }

    async fn get_downloaded_local_paths(
        &self,
    ) -> Result<HashMap<(String, String, String), PathBuf>, StateError> {
        SqliteStateDb::get_downloaded_local_paths(self).await
    }

    async fn get_attempt_counts(&self) -> Result<HashMap<(String, String), u32>, StateError> {
        SqliteStateDb::get_attempt_counts(self).await
    }

    async fn touch_last_seen_many(
        &self,
        library: &str,
        asset_ids: &[&str],
    ) -> Result<(), StateError> {
        SqliteStateDb::touch_last_seen_many(self, library, asset_ids).await
    }

    async fn upsert_asset_master_mapping(
        &self,
        library: &str,
        asset_record_name: &str,
        master_record_name: &str,
    ) -> Result<(), StateError> {
        SqliteStateDb::upsert_asset_master_mapping(
            self,
            library,
            asset_record_name,
            master_record_name,
        )
        .await
    }

    async fn get_master_record_name_for_asset(
        &self,
        library: &str,
        asset_record_name: &str,
    ) -> Result<Option<String>, StateError> {
        SqliteStateDb::get_master_record_name_for_asset(self, library, asset_record_name).await
    }

    async fn get_asset_record_names_for_master(
        &self,
        library: &str,
        master_record_name: &str,
    ) -> Result<Vec<String>, StateError> {
        SqliteStateDb::get_asset_record_names_for_master(self, library, master_record_name).await
    }

    async fn get_asset_master_mappings(
        &self,
    ) -> Result<HashSet<(String, String, String)>, StateError> {
        SqliteStateDb::get_asset_master_mappings(self).await
    }

    async fn get_legacy_master_state_owners(
        &self,
    ) -> Result<HashSet<(String, String, String)>, StateError> {
        SqliteStateDb::get_legacy_master_state_owners(self).await
    }

    async fn claim_legacy_master_state_owner(
        &self,
        library: &str,
        master_record_name: &str,
        asset_record_name: &str,
    ) -> Result<bool, StateError> {
        SqliteStateDb::claim_legacy_master_state_owner(
            self,
            library,
            master_record_name,
            asset_record_name,
        )
        .await
    }

    async fn set_asset_verification(
        &self,
        library: &str,
        id: &str,
        version_size: &str,
        state: AssetVerificationState,
        reason: &str,
    ) -> Result<(), StateError> {
        SqliteStateDb::set_asset_verification(self, library, id, version_size, state, reason).await
    }

    async fn clear_asset_verification(
        &self,
        library: &str,
        id: &str,
        version_size: &str,
    ) -> Result<(), StateError> {
        SqliteStateDb::clear_asset_verification(self, library, id, version_size).await
    }

    async fn mark_policy_excluded(
        &self,
        library: &str,
        id: &str,
        version_size: &str,
    ) -> Result<bool, StateError> {
        SqliteStateDb::mark_policy_excluded(self, library, id, version_size).await
    }

    async fn backfill_asset_master_mappings_from_album_memberships(
        &self,
    ) -> Result<u64, StateError> {
        SqliteStateDb::backfill_asset_master_mappings_from_album_memberships(self).await
    }

    async fn mark_soft_deleted(
        &self,
        library: &str,
        asset_id: &str,
        deleted_at: Option<DateTime<Utc>>,
    ) -> Result<(), StateError> {
        SqliteStateDb::mark_soft_deleted(self, library, asset_id, deleted_at)
            .await
            .map(|_| ())
    }

    async fn mark_soft_deleted_affected(
        &self,
        library: &str,
        asset_id: &str,
        deleted_at: Option<DateTime<Utc>>,
    ) -> Result<usize, StateError> {
        SqliteStateDb::mark_soft_deleted(self, library, asset_id, deleted_at).await
    }

    async fn resolve_source_deleted_affected(
        &self,
        library: &str,
        asset_id: &str,
        deleted_at: Option<DateTime<Utc>>,
    ) -> Result<usize, StateError> {
        SqliteStateDb::resolve_source_deleted(self, library, asset_id, deleted_at).await
    }

    async fn mark_master_family_soft_deleted_affected(
        &self,
        library: &str,
        master_record_name: &str,
        deleted_at: Option<DateTime<Utc>>,
    ) -> Result<usize, StateError> {
        SqliteStateDb::mark_master_family_soft_deleted(
            self,
            library,
            master_record_name,
            deleted_at,
        )
        .await
    }

    async fn resolve_master_family_source_deleted_affected(
        &self,
        library: &str,
        master_record_name: &str,
        deleted_at: Option<DateTime<Utc>>,
    ) -> Result<usize, StateError> {
        SqliteStateDb::resolve_master_family_source_deleted(
            self,
            library,
            master_record_name,
            deleted_at,
        )
        .await
    }

    async fn mark_hidden_at_source(&self, library: &str, asset_id: &str) -> Result<(), StateError> {
        SqliteStateDb::mark_hidden_at_source(self, library, asset_id)
            .await
            .map(|_| ())
    }

    async fn mark_hidden_at_source_affected(
        &self,
        library: &str,
        asset_id: &str,
    ) -> Result<usize, StateError> {
        SqliteStateDb::mark_hidden_at_source(self, library, asset_id).await
    }
}

#[async_trait]
impl DownloadContextStateStore for SqliteStateDb {
    async fn get_downloaded_file_records(&self) -> Result<Vec<DownloadedFileRecord>, StateError> {
        SqliteStateDb::get_downloaded_file_records(self).await
    }
}

#[cfg(test)]
mod tests;
