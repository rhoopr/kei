//! Status, sync-run history, paged asset reports, and manifest reads.

use std::collections::BTreeMap;
use std::path::PathBuf;

use async_trait::async_trait;
use chrono::{DateTime, TimeZone, Utc};
use rusqlite::OptionalExtension;

use super::metadata::{metadata_capture_remaining, metadata_capture_status};
use super::rows::{
    ASSET_COLUMNS, decode_asset_date, optional_ts_to_utc, row_to_asset_record, ts_to_utc,
};
use super::{ManifestAssetRow, ReportStateStore, SqliteStateDb};
use crate::state::error::StateError;
use crate::state::types::{AssetRecord, METADATA_CAPTURE_REVISION, SyncRunStats, SyncSummary};

struct ManifestJoinedRow {
    asset: ManifestAssetRow,
    album_name: Option<String>,
}

fn manifest_joined_row_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ManifestJoinedRow> {
    let library: String = row.get(0)?;
    let asset_id: String = row.get(1)?;
    let version: String = row.get(2)?;
    let filename: String = row.get(3)?;
    let local_path: Option<String> = row.get(4)?;
    let checksum: String = row.get(5)?;
    let local_checksum: Option<String> = row.get(6)?;
    let download_checksum: Option<String> = row.get(7)?;
    let size_bytes: i64 = row.get(8)?;
    let created_at = decode_asset_date(row.get(9)?, 9)?;
    let added_at = row
        .get::<_, Option<f64>>(10)?
        .map(|value| decode_asset_date(value, 10))
        .transpose()?;
    let downloaded_at_ts: Option<i64> = row.get(11)?;
    let last_seen_at_ts: i64 = row.get(12)?;
    let media_type: String = row.get(13)?;
    let status: String = row.get(14)?;
    let album_name: Option<String> = row.get(15)?;

    Ok(ManifestJoinedRow {
        asset: ManifestAssetRow {
            library,
            asset_id,
            version,
            filename,
            local_path: local_path.map(PathBuf::from),
            checksum,
            local_checksum,
            download_checksum,
            size_bytes: u64::try_from(size_bytes).unwrap_or(0),
            created_at,
            added_at,
            downloaded_at: optional_ts_to_utc(downloaded_at_ts),
            last_seen_at: ts_to_utc(last_seen_at_ts),
            media_type,
            status,
            albums: Vec::new(),
        },
        album_name,
    })
}

impl SqliteStateDb {
    #[cfg(test)]
    pub(crate) async fn get_failed(&self) -> Result<Vec<AssetRecord>, StateError> {
        self.with_conn("get_failed", move |conn| {
            let sql = format!(
                "SELECT {ASSET_COLUMNS} FROM assets WHERE status = 'failed' AND is_deleted = 0 \
                 ORDER BY last_seen_at DESC",
            );
            let mut stmt = conn
                .prepare(&sql)
                .map_err(|e| StateError::query("get_failed", e))?;

            let records = stmt
                .query_map([], row_to_asset_record)
                .map_err(|e| StateError::query("get_failed", e))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| StateError::query("get_failed", e))?;

            Ok(records)
        })
        .await
    }

    pub(crate) async fn get_failed_sample(
        &self,
        limit: u32,
    ) -> Result<(Vec<AssetRecord>, u64), StateError> {
        self.with_conn("get_failed_sample", move |conn| {
            let total: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM assets WHERE status = 'failed' AND is_deleted = 0",
                    [],
                    |row| row.get(0),
                )
                .map_err(|e| StateError::query("get_failed_sample", e))?;

            let sql = format!(
                "SELECT {ASSET_COLUMNS} FROM assets WHERE status = 'failed' AND is_deleted = 0 \
                 ORDER BY last_seen_at DESC LIMIT ?1",
            );
            let mut stmt = conn
                .prepare(&sql)
                .map_err(|e| StateError::query("get_failed_sample", e))?;

            let records = stmt
                .query_map([i64::from(limit)], row_to_asset_record)
                .map_err(|e| StateError::query("get_failed_sample", e))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| StateError::query("get_failed_sample", e))?;

            #[allow(
                clippy::cast_sign_loss,
                reason = ".max(0) clamps any negative COUNT(*) result to 0 before the cast"
            )]
            let total_u64 = total.max(0) as u64;
            Ok((records, total_u64))
        })
        .await
    }

    pub(crate) async fn get_failed_page(
        &self,
        offset: u64,
        limit: u32,
    ) -> Result<Vec<AssetRecord>, StateError> {
        self.with_conn("get_failed_page", move |conn| {
            let sql = format!(
                "SELECT {ASSET_COLUMNS} FROM assets WHERE status = 'failed' AND is_deleted = 0 \
                 ORDER BY last_seen_at DESC LIMIT ?1 OFFSET ?2",
            );
            let mut stmt = conn
                .prepare(&sql)
                .map_err(|e| StateError::query("get_failed_page", e))?;

            #[allow(
                clippy::cast_possible_wrap,
                reason = "offset is bounded by the failed-row count and well below i64::MAX"
            )]
            let offset_i = offset as i64;
            let records = stmt
                .query_map(
                    rusqlite::params![i64::from(limit), offset_i],
                    row_to_asset_record,
                )
                .map_err(|e| StateError::query("get_failed_page", e))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| StateError::query("get_failed_page", e))?;

            Ok(records)
        })
        .await
    }

    pub(crate) async fn get_pending_page(
        &self,
        offset: u64,
        limit: u32,
    ) -> Result<Vec<AssetRecord>, StateError> {
        self.with_conn("get_pending_page", move |conn| {
            let sql = format!(
                "SELECT {ASSET_COLUMNS} FROM assets WHERE status = 'pending' AND is_deleted = 0 \
                 ORDER BY last_seen_at DESC LIMIT ?1 OFFSET ?2",
            );
            let mut stmt = conn
                .prepare(&sql)
                .map_err(|e| StateError::query("get_pending_page", e))?;

            #[allow(
                clippy::cast_possible_wrap,
                reason = "offset is bounded by the pending-row count and well below i64::MAX"
            )]
            let offset_i = offset as i64;
            let records = stmt
                .query_map(
                    rusqlite::params![i64::from(limit), offset_i],
                    row_to_asset_record,
                )
                .map_err(|e| StateError::query("get_pending_page", e))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| StateError::query("get_pending_page", e))?;

            Ok(records)
        })
        .await
    }

    pub(crate) async fn get_summary(&self) -> Result<SyncSummary, StateError> {
        self.with_conn("get_summary", move |conn| {
            let (
                total_assets,
                downloaded,
                pending,
                policy_excluded,
                failed,
                source_deleted,
                downloaded_bytes,
            ) = conn.query_row(
                    "SELECT \
                         COUNT(*), \
                         COUNT(CASE WHEN status = 'downloaded' THEN 1 END), \
                         COUNT(CASE WHEN status = 'pending' AND is_deleted = 0 THEN 1 END), \
                         COUNT(CASE WHEN status = 'policy_excluded' AND is_deleted = 0 THEN 1 END), \
                         COUNT(CASE WHEN status = 'failed' AND is_deleted = 0 THEN 1 END), \
                         COUNT(CASE WHEN is_deleted = 1 THEN 1 END), \
                         COALESCE(SUM(CASE WHEN status = 'downloaded' THEN size_bytes ELSE 0 END), 0) \
                     FROM assets",
                    [],
                    |row| {
                        Ok((
                            row.get::<_, i64>(0)?,
                            row.get::<_, i64>(1)?,
                            row.get::<_, i64>(2)?,
                            row.get::<_, i64>(3)?,
                            row.get::<_, i64>(4)?,
                            row.get::<_, i64>(5)?,
                            row.get::<_, i64>(6)?,
                        ))
                    },
                )
                .map(|(t, d, p, e, f, s, b)| {
                    (
                        u64::try_from(t).unwrap_or(0),
                        u64::try_from(d).unwrap_or(0),
                        u64::try_from(p).unwrap_or(0),
                        u64::try_from(e).unwrap_or(0),
                        u64::try_from(f).unwrap_or(0),
                        u64::try_from(s).unwrap_or(0),
                        u64::try_from(b).unwrap_or(0),
                    )
                })
                .map_err(|e| StateError::query("get_summary", e))?;
            let awaiting_provider_verification: u64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM asset_verifications AS verification \
                     JOIN assets \
                       ON assets.library = verification.library \
                      AND assets.id = verification.id \
                      AND assets.version_size = verification.version_size \
                     WHERE assets.status IN ('pending', 'failed') AND assets.is_deleted = 0",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .map(|count| u64::try_from(count).unwrap_or(0))
                .map_err(|e| StateError::query("get_summary::provider_verification", e))?;
            let oldest_provider_verification_at = conn
                .query_row(
                    "SELECT MIN(verification.checked_at) FROM asset_verifications AS verification \
                     JOIN assets \
                       ON assets.library = verification.library \
                      AND assets.id = verification.id \
                      AND assets.version_size = verification.version_size \
                     WHERE assets.status IN ('pending', 'failed') AND assets.is_deleted = 0",
                    [],
                    |row| row.get::<_, Option<i64>>(0),
                )
                .map_err(|e| StateError::query("get_summary::oldest_provider_verification", e))?
                .and_then(|timestamp| Utc.timestamp_opt(timestamp, 0).single());
            let metadata_value = |key: &str| {
                conn.query_row("SELECT value FROM metadata WHERE key = ?1", [key], |row| {
                    row.get::<_, String>(0)
                })
                .optional()
            };
            let mut provider_checkpoint_status = metadata_value("last_checkpoint_status")
                .map_err(|e| StateError::query("get_summary::checkpoint_status", e))?;
            if provider_checkpoint_status.is_none() {
                let token_exists = conn
                    .prepare("SELECT 1 FROM metadata WHERE key LIKE 'sync_token:%' LIMIT 1")
                    .and_then(|mut stmt| stmt.exists([]))
                    .map_err(|e| StateError::query("get_summary::checkpoint_exists", e))?;
                provider_checkpoint_status = token_exists.then(|| "current".to_owned());
            }
            let last_recovery_action = metadata_value("last_recovery_action")
                .map_err(|e| StateError::query("get_summary::recovery_action", e))?;
            let last_full_enumeration_reason = metadata_value("last_full_enumeration_reason")
                .map_err(|e| StateError::query("get_summary::full_enumeration_reason", e))?;

            type LastSyncRow = (
                Option<i64>,
                Option<i64>,
                Option<String>,
                i64,
                i64,
                i32,
                Option<i64>,
                i32,
                i32,
                Option<i64>,
                Option<i64>,
                Option<String>,
            );
            let last_sync: Option<LastSyncRow> = conn
                .query_row(
                    "SELECT started_at, completed_at, \
                            status, assets_failed, enumeration_errors, interrupted, \
                            api_total_at_start, api_total_at_start_partial, inventory_drop_detected, \
                            inventory_drop_previous_total, inventory_drop_current_total, \
                            inventory_drop_library \
                     FROM sync_runs ORDER BY id DESC LIMIT 1",
                    [],
                    |row| {
                        Ok((
                            row.get(0)?,
                            row.get(1)?,
                            row.get(2)?,
                            row.get(3)?,
                            row.get(4)?,
                            row.get(5)?,
                            row.get(6)?,
                            row.get(7)?,
                            row.get(8)?,
                            row.get(9)?,
                            row.get(10)?,
                            row.get(11)?,
                        ))
                    },
                )
                .optional()
                .map_err(|e| StateError::query("get_summary", e))?;

            let (
                last_sync_started,
                last_sync_completed,
                last_sync_status,
                last_sync_assets_failed,
                last_sync_enumeration_errors,
                last_sync_interrupted,
                last_api_total_at_start,
                last_api_total_at_start_partial,
                last_inventory_drop_detected,
                last_inventory_drop_previous_total,
                last_inventory_drop_current_total,
                last_inventory_drop_library,
            ) = match last_sync {
                Some((
                    started,
                    completed,
                    status,
                    assets_failed,
                    enumeration_errors,
                    interrupted,
                    api_total,
                    api_total_partial,
                    drop_detected,
                    drop_previous,
                    drop_current,
                    drop_library,
                )) => (
                    started.and_then(|ts| Utc.timestamp_opt(ts, 0).single()),
                    completed.and_then(|ts| Utc.timestamp_opt(ts, 0).single()),
                    status,
                    u64::try_from(assets_failed).unwrap_or(0),
                    u64::try_from(enumeration_errors).unwrap_or(0),
                    interrupted != 0,
                    api_total.and_then(|n| u64::try_from(n).ok()),
                    api_total_partial != 0,
                    drop_detected != 0,
                    drop_previous.and_then(|n| u64::try_from(n).ok()),
                    drop_current.and_then(|n| u64::try_from(n).ok()),
                    drop_library,
                ),
                None => (None, None, None, 0, 0, false, None, false, false, None, None, None),
            };

            let active_sync_started: Option<DateTime<Utc>> = conn
                .query_row(
                    "SELECT started_at FROM sync_runs \
                     WHERE status = 'running' ORDER BY id DESC LIMIT 1",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .optional()
                .map_err(|e| StateError::query("get_summary", e))?
                .and_then(|ts| Utc.timestamp_opt(ts, 0).single());

            let mut enum_stmt = conn
                .prepare(
                    "SELECT key FROM metadata \
                     WHERE key LIKE 'enum_in_progress:%' ORDER BY key",
                )
                .map_err(|e| StateError::query("get_summary", e))?;
            let enum_rows = enum_stmt
                .query_map([], |row| row.get::<_, String>(0))
                .map_err(|e| StateError::query("get_summary", e))?;
            let mut active_enumeration_zones = Vec::new();
            for row in enum_rows {
                let key = row.map_err(|e| StateError::query("get_summary", e))?;
                if let Some(zone) = key.strip_prefix("enum_in_progress:") {
                    active_enumeration_zones.push(zone.to_string());
                }
            }

            let mut capture_stmt = conn
                .prepare(
                    "SELECT library FROM metadata_capture_state \
                     UNION SELECT DISTINCT library FROM assets \
                     ORDER BY library",
                )
                .map_err(|e| StateError::query("get_summary::metadata_capture", e))?;
            let capture_libraries = capture_stmt
                .query_map([], |row| row.get::<_, String>(0))
                .map_err(|e| StateError::query("get_summary::metadata_capture", e))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| StateError::query("get_summary::metadata_capture", e))?;
            let mut metadata_capture = Vec::with_capacity(capture_libraries.len());
            for library in capture_libraries {
                let remaining =
                    metadata_capture_remaining(conn, &library, METADATA_CAPTURE_REVISION)?;
                metadata_capture.push(metadata_capture_status(
                    conn,
                    &library,
                    METADATA_CAPTURE_REVISION,
                    remaining,
                )?);
            }

            Ok(SyncSummary {
                total_assets,
                downloaded,
                pending,
                policy_excluded,
                failed,
                awaiting_provider_verification,
                source_deleted,
                oldest_provider_verification_at,
                provider_checkpoint_status,
                last_recovery_action,
                last_full_enumeration_reason,
                downloaded_bytes,
                active_sync_started,
                active_enumeration_zones,
                last_sync_completed,
                last_sync_started,
                last_sync_status,
                last_sync_assets_failed,
                last_sync_enumeration_errors,
                last_sync_interrupted,
                last_api_total_at_start,
                last_api_total_at_start_partial,
                last_inventory_drop_detected,
                last_inventory_drop_previous_total,
                last_inventory_drop_current_total,
                last_inventory_drop_library,
                metadata_capture,
            })
        })
        .await
    }

    pub(crate) async fn get_downloaded_page(
        &self,
        offset: u64,
        limit: u32,
    ) -> Result<Vec<AssetRecord>, StateError> {
        self.with_conn("get_downloaded_page", move |conn| {
            let sql = format!(
                "SELECT {ASSET_COLUMNS} FROM assets WHERE status = 'downloaded' \
                 ORDER BY rowid LIMIT ?1 OFFSET ?2",
            );
            let mut stmt = conn
                .prepare(&sql)
                .map_err(|e| StateError::query("get_downloaded_page", e))?;

            let records = stmt
                .query_map(
                    rusqlite::params![i64::from(limit), offset as i64],
                    row_to_asset_record,
                )
                .map_err(|e| StateError::query("get_downloaded_page", e))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| StateError::query("get_downloaded_page", e))?;

            Ok(records)
        })
        .await
    }

    pub(crate) async fn get_manifest_assets(&self) -> Result<Vec<ManifestAssetRow>, StateError> {
        self.with_conn("get_manifest_assets", move |conn| {
            let mut stmt = conn
                .prepare(
                    r"
                    SELECT
                        a.library,
                        a.id,
                        a.version_size,
                        a.filename,
                        a.local_path,
                        a.checksum,
                        a.local_checksum,
                        a.download_checksum,
                        a.size_bytes,
                        a.created_at,
                        a.added_at,
                        a.downloaded_at,
                        a.last_seen_at,
                        a.media_type,
                        a.status,
                        aa.album_name
                    FROM assets a
                    LEFT JOIN asset_albums aa
                        ON aa.library = a.library
                       AND aa.asset_id = a.id
                    ORDER BY a.library, a.id, a.version_size, aa.album_name
                    ",
                )
                .map_err(|e| StateError::query("get_manifest_assets::prepare", e))?;

            let rows = stmt
                .query_map([], manifest_joined_row_from_row)
                .map_err(|e| StateError::query("get_manifest_assets::query", e))?;

            let mut assets: BTreeMap<(String, String, String), ManifestAssetRow> = BTreeMap::new();
            for row in rows {
                let joined = row.map_err(|e| StateError::query("get_manifest_assets::row", e))?;
                let key = (
                    joined.asset.library.clone(),
                    joined.asset.asset_id.clone(),
                    joined.asset.version.clone(),
                );
                let asset = assets.entry(key).or_insert(joined.asset);
                if let Some(album) = joined.album_name {
                    asset.albums.push(album);
                }
            }

            Ok(assets.into_values().collect())
        })
        .await
    }

    pub(crate) async fn start_sync_run(&self) -> Result<i64, StateError> {
        self.start_sync_run_at(Utc::now()).await
    }

    pub(crate) async fn start_sync_run_at(
        &self,
        started_at: DateTime<Utc>,
    ) -> Result<i64, StateError> {
        let started_at = started_at.timestamp();
        self.with_conn("start_sync_run", move |conn| {
            conn.execute(
                "INSERT INTO sync_runs (started_at, status) VALUES (?1, 'running')",
                [started_at],
            )
            .map_err(|e| StateError::query("start_sync_run", e))?;

            Ok(conn.last_insert_rowid())
        })
        .await
    }

    pub(crate) async fn complete_sync_run(
        &self,
        run_id: i64,
        stats: &SyncRunStats,
    ) -> Result<(), StateError> {
        let completed_at = Utc::now().timestamp();
        let assets_seen = i64::try_from(stats.assets_seen).unwrap_or(i64::MAX);
        let assets_downloaded = i64::try_from(stats.assets_downloaded).unwrap_or(i64::MAX);
        let assets_failed = i64::try_from(stats.assets_failed).unwrap_or(i64::MAX);
        let enumeration_errors = i64::try_from(stats.enumeration_errors).unwrap_or(i64::MAX);
        let api_total_at_start = stats
            .api_total_at_start
            .map(|n| i64::try_from(n).unwrap_or(i64::MAX));
        let api_total_at_start_partial = i32::from(stats.api_total_at_start_partial);
        let inventory_drop_detected = i32::from(stats.inventory_drop_warnings > 0);
        let inventory_drop_previous_total = stats
            .inventory_drop_previous_total
            .map(|n| i64::try_from(n).unwrap_or(i64::MAX));
        let inventory_drop_current_total = stats
            .inventory_drop_current_total
            .map(|n| i64::try_from(n).unwrap_or(i64::MAX));
        let inventory_drop_library = stats.inventory_drop_library.clone();
        let interrupted_i32 = i32::from(stats.interrupted);
        let status = if stats.interrupted {
            "interrupted"
        } else {
            "complete"
        };

        self.with_conn("complete_sync_run", move |conn| {
            let rows = conn.execute(
                "UPDATE sync_runs SET completed_at = ?1, assets_seen = ?2, assets_downloaded = ?3, \
                 assets_failed = ?4, interrupted = ?5, status = ?6, enumeration_errors = ?7, \
                 api_total_at_start = ?8, api_total_at_start_partial = ?9, \
                 inventory_drop_detected = ?10, inventory_drop_previous_total = ?11, \
                 inventory_drop_current_total = ?12, inventory_drop_library = ?13 \
                 WHERE id = ?14",
                rusqlite::params![
                    completed_at,
                    assets_seen,
                    assets_downloaded,
                    assets_failed,
                    interrupted_i32,
                    status,
                    enumeration_errors,
                    api_total_at_start,
                    api_total_at_start_partial,
                    inventory_drop_detected,
                    inventory_drop_previous_total,
                    inventory_drop_current_total,
                    inventory_drop_library,
                    run_id
                ],
            )
            .map_err(|e| StateError::query("complete_sync_run", e))?;
            if rows == 0 {
                return Err(StateError::Invariant {
                    operation: "complete_sync_run",
                    detail: format!("no sync_runs row for id {run_id}"),
                });
            }

            Ok(())
        })
        .await
    }

    #[cfg(test)]
    pub(crate) fn sync_run_snapshot_for_test(
        &self,
        run_id: i64,
    ) -> Result<(String, i64, i64, i64, i32), StateError> {
        let conn = self.acquire_lock("sync_run_snapshot_for_test")?;
        conn.query_row(
            "SELECT status, assets_seen, assets_failed, enumeration_errors, interrupted \
             FROM sync_runs WHERE id = ?1",
            [run_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, i32>(4)?,
                ))
            },
        )
        .map_err(|e| StateError::query("sync_run_snapshot_for_test", e))
    }

    pub(crate) async fn promote_orphaned_sync_runs(&self) -> Result<u64, StateError> {
        self.with_conn("promote_orphaned_sync_runs", move |conn| {
            let rows = conn
                .execute(
                    "UPDATE sync_runs SET status = 'interrupted', interrupted = 1 \
                     WHERE status = 'running'",
                    [],
                )
                .map_err(|e| StateError::query("promote_orphaned_sync_runs", e))?;
            Ok(rows as u64)
        })
        .await
    }
}

#[async_trait]
impl ReportStateStore for SqliteStateDb {
    async fn get_failed(&self) -> Result<Vec<AssetRecord>, StateError> {
        <SqliteStateDb as ReportStateStore>::get_failed_page(self, 0, u32::MAX).await
    }

    async fn get_failed_sample(&self, limit: u32) -> Result<(Vec<AssetRecord>, u64), StateError> {
        SqliteStateDb::get_failed_sample(self, limit).await
    }

    async fn get_failed_page(
        &self,
        offset: u64,
        limit: u32,
    ) -> Result<Vec<AssetRecord>, StateError> {
        SqliteStateDb::get_failed_page(self, offset, limit).await
    }

    async fn get_pending_page(
        &self,
        offset: u64,
        limit: u32,
    ) -> Result<Vec<AssetRecord>, StateError> {
        SqliteStateDb::get_pending_page(self, offset, limit).await
    }

    async fn get_summary(&self) -> Result<SyncSummary, StateError> {
        SqliteStateDb::get_summary(self).await
    }

    async fn get_downloaded_page(
        &self,
        offset: u64,
        limit: u32,
    ) -> Result<Vec<AssetRecord>, StateError> {
        SqliteStateDb::get_downloaded_page(self, offset, limit).await
    }

    async fn start_sync_run_at(&self, started_at: DateTime<Utc>) -> Result<i64, StateError> {
        SqliteStateDb::start_sync_run_at(self, started_at).await
    }

    async fn start_sync_run(&self) -> Result<i64, StateError> {
        SqliteStateDb::start_sync_run(self).await
    }

    async fn complete_sync_run(&self, run_id: i64, stats: &SyncRunStats) -> Result<(), StateError> {
        SqliteStateDb::complete_sync_run(self, run_id, stats).await
    }

    async fn promote_orphaned_sync_runs(&self) -> Result<u64, StateError> {
        SqliteStateDb::promote_orphaned_sync_runs(self).await
    }
}

#[cfg(test)]
mod tests;
