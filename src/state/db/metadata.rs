//! Metadata capture revisions, path-specific rewrite debt, and repair receipts.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension};

use super::asset_writes::{
    MetadataPathWrite, ensure_asset_has_no_prepared_capture_repair, metadata_rewrite_source_sql,
    record_metadata_capture_revision, record_metadata_path,
};
use super::rows::{
    ASSET_COLUMN_COUNT, ASSET_COLUMNS, encode_asset_date, row_to_asset_record, sqlite_placeholders,
    unique_sorted_strings,
};
use super::{
    CaptureRepairReceipt, MetadataRewriteCompletion, MetadataRewriteQueue, MetadataRewriteStore,
    PendingMetadataRewrite, SqliteStateDb,
};
use crate::state::error::StateError;
use crate::state::types::{
    AssetRecord, MetadataCapture, MetadataCaptureCandidate, MetadataCaptureStatus,
    MetadataCaptureVersionEvidence, VersionSizeKey,
};

#[derive(Clone, Copy, PartialEq, Eq)]
enum MetadataRewriteTarget {
    Catalogue,
    AdditionalPath,
}

impl MetadataRewriteTarget {
    fn sql(self) -> (&'static str, &'static str) {
        match self {
            Self::Catalogue => (
                "assets",
                "status = 'downloaded' AND is_deleted = 0 AND metadata_hash IS ?4",
            ),
            Self::AdditionalPath => (
                "asset_metadata_paths",
                "EXISTS (SELECT 1 FROM assets a \
                WHERE a.library = asset_metadata_paths.library AND a.id = asset_metadata_paths.id \
                  AND a.version_size = asset_metadata_paths.version_size \
                  AND a.checksum = asset_metadata_paths.provider_checksum \
                  AND a.status = 'downloaded' AND a.is_deleted = 0 AND a.metadata_hash IS ?4)",
            ),
        }
    }
}

/// Select a path-specific compare-and-swap target without changing sync ownership.
fn metadata_rewrite_target(
    conn: &Connection,
    library: &str,
    id: &str,
    version_size: &str,
    path: &Path,
) -> Result<MetadataRewriteTarget, StateError> {
    let primary = conn.query_row(
        "SELECT local_path IS ?4 FROM assets WHERE library = ?1 AND id = ?2 AND version_size = ?3",
        rusqlite::params![library, id, version_size, path.to_string_lossy()], |row| row.get::<_, bool>(0),
    ).optional().map_err(|e| StateError::query("metadata_rewrite_target", e))?.unwrap_or(false);
    Ok(if primary {
        MetadataRewriteTarget::Catalogue
    } else {
        MetadataRewriteTarget::AdditionalPath
    })
}

pub(super) fn metadata_capture_remaining(
    conn: &Connection,
    library: &str,
    target_revision: i64,
) -> Result<u64, StateError> {
    conn.query_row(
        "SELECT COUNT(*) FROM ( \
             SELECT assets.id FROM assets \
             LEFT JOIN asset_metadata_capture_revisions AS revisions \
               ON revisions.library = assets.library AND revisions.asset_id = assets.id \
             WHERE assets.library = ?1 AND assets.status = 'downloaded' \
               AND assets.is_deleted = 0 \
               AND (revisions.revision IS NULL OR revisions.revision < ?2) \
             GROUP BY assets.id \
         )",
        rusqlite::params![library, target_revision],
        |row| row.get::<_, i64>(0),
    )
    .map(|count| u64::try_from(count).unwrap_or(0))
    .map_err(|e| StateError::query("metadata_capture_remaining", e))
}

pub(super) fn metadata_capture_status(
    conn: &Connection,
    library: &str,
    target_revision: i64,
    remaining_assets: u64,
) -> Result<MetadataCaptureStatus, StateError> {
    let row = conn
        .query_row(
            "SELECT active_revision, pending_revision, processed_assets, \
                    failed_assets, last_error \
             FROM metadata_capture_state WHERE library = ?1",
            [library],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, Option<i64>>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, Option<String>>(4)?,
                ))
            },
        )
        .optional()
        .map_err(|e| StateError::query("metadata_capture_status", e))?;
    let (active_revision, pending_revision, processed, failed, last_error) = row.unwrap_or({
        if remaining_assets > 0 {
            (0, Some(target_revision), 0, 0, None)
        } else {
            (target_revision, None, 0, 0, None)
        }
    });
    Ok(MetadataCaptureStatus {
        library: library.to_owned(),
        active_revision,
        pending_revision,
        processed_assets: u64::try_from(processed).unwrap_or(0),
        failed_assets: u64::try_from(failed).unwrap_or(0),
        remaining_assets,
        last_error,
    })
}

fn query_pending_metadata_rewrites(
    conn: &Connection,
    queue: MetadataRewriteQueue,
    libraries: Option<&[String]>,
    offset: usize,
    limit: usize,
) -> Result<Vec<PendingMetadataRewrite>, StateError> {
    let (queue_predicate, order) = match queue {
        MetadataRewriteQueue::Ordinary => (
            "metadata_write_failed_at IS NOT NULL",
            "metadata_write_failed_at ASC, library, id, version_size",
        ),
        MetadataRewriteQueue::CaptureRepair => (
            "(capture_repair_metadata_hash IS NOT NULL \
              OR capture_repair_output_checksum IS NOT NULL \
              OR capture_repair_output_size IS NOT NULL)",
            "library, id, version_size",
        ),
    };
    let scope = libraries
        .map(|libraries| format!(" AND library IN ({})", sqlite_placeholders(libraries.len())))
        .unwrap_or_default();
    let source = metadata_rewrite_source_sql();
    let sql = format!(
        "SELECT {ASSET_COLUMNS}, capture_repair_metadata_hash, \
            capture_repair_output_checksum, capture_repair_output_size, source_checksum \
         FROM ({source}) WHERE {queue_predicate} \
           AND status = 'downloaded' AND is_deleted = 0 AND local_path IS NOT NULL \
           {scope} ORDER BY {order}, local_path LIMIT ? OFFSET ?"
    );
    let limit = i64::try_from(limit).unwrap_or(i64::MAX);
    let offset = i64::try_from(offset).unwrap_or(i64::MAX);
    let mut params: Vec<&dyn rusqlite::ToSql> =
        Vec::with_capacity(libraries.map_or(2, |values| values.len() + 2));
    if let Some(libraries) = libraries {
        for library in libraries {
            params.push(library);
        }
    }
    params.push(&limit);
    params.push(&offset);
    let mut stmt = conn
        .prepare(&sql)
        .map_err(|e| StateError::query("get_pending_metadata_rewrites_for_queue", e))?;
    let rows = stmt
        .query_map(
            rusqlite::params_from_iter(params),
            row_to_pending_metadata_rewrite,
        )
        .map_err(|e| StateError::query("get_pending_metadata_rewrites_for_queue", e))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| StateError::query("get_pending_metadata_rewrites_for_queue", e))
}

fn row_to_pending_metadata_rewrite(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<PendingMetadataRewrite> {
    let metadata_hash: Option<String> = row.get(ASSET_COLUMN_COUNT)?;
    let output_checksum: Option<String> = row.get(ASSET_COLUMN_COUNT + 1)?;
    let output_size: Option<i64> = row.get(ASSET_COLUMN_COUNT + 2)?;
    let capture_repair_receipt = match (metadata_hash, output_checksum, output_size) {
        (None, None, None) => None,
        (Some(metadata_hash), None, None) if !metadata_hash.is_empty() => {
            Some(CaptureRepairReceipt::Pending { metadata_hash })
        }
        (Some(metadata_hash), Some(output_checksum), Some(output_size))
            if !metadata_hash.is_empty() && !output_checksum.is_empty() && output_size >= 0 =>
        {
            Some(CaptureRepairReceipt::Prepared {
                metadata_hash,
                output_checksum,
                output_size: u64::try_from(output_size).unwrap_or(0),
            })
        }
        state => {
            return Err(rusqlite::Error::FromSqlConversionFailure(
                ASSET_COLUMN_COUNT,
                rusqlite::types::Type::Null,
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("malformed capture-repair receipt: {state:?}"),
                )
                .into(),
            ));
        }
    };
    Ok(PendingMetadataRewrite {
        asset: row_to_asset_record(row)?,
        capture_repair_receipt,
        source_checksum: row.get(ASSET_COLUMN_COUNT + 3)?,
    })
}

impl SqliteStateDb {
    pub(crate) async fn record_metadata_write_failure(
        &self,
        library: &str,
        asset_id: &str,
        version_size: &str,
    ) -> Result<(), StateError> {
        let ts = Utc::now().timestamp();
        let library = library.to_owned();
        let asset_id = asset_id.to_owned();
        let version_size = version_size.to_owned();
        self.with_conn_mut("record_metadata_write_failure", move |conn| {
            let tx = conn
                .transaction()
                .map_err(|e| StateError::query("record_metadata_write_failure::begin", e))?;
            tx.execute(
                "UPDATE assets SET metadata_write_failed_at = ?1 \
                 WHERE library = ?2 AND id = ?3 AND version_size = ?4",
                rusqlite::params![ts, &library, &asset_id, &version_size],
            )
            .map_err(|e| StateError::query("record_metadata_write_failure", e))?;
            tx.execute(
                "UPDATE asset_metadata_paths SET metadata_write_failed_at = ?1 \
                 WHERE library = ?2 AND id = ?3 AND version_size = ?4",
                rusqlite::params![ts, library, asset_id, version_size],
            )
            .map_err(|e| StateError::query("record_metadata_write_failure::paths", e))?;
            tx.commit()
                .map_err(|e| StateError::query("record_metadata_write_failure::commit", e))?;
            Ok(())
        })
        .await
    }

    pub(crate) async fn record_capture_repair_prepared(
        &self,
        pending: &PendingMetadataRewrite,
        output_checksum: &str,
        output_size: u64,
    ) -> Result<Option<CaptureRepairReceipt>, StateError> {
        if output_checksum.is_empty() {
            return Err(StateError::Invariant {
                operation: "record_capture_repair_prepared",
                detail: "capture-repair output checksum is empty".into(),
            });
        }
        let receipt =
            pending
                .capture_repair_receipt
                .as_ref()
                .ok_or_else(|| StateError::Invariant {
                    operation: "record_capture_repair_prepared",
                    detail: "the selected row has no capture-repair receipt".into(),
                });
        let receipt = receipt?;
        let metadata_hash = receipt.metadata_hash();
        let (selected_output_checksum, selected_output_size) = match receipt {
            CaptureRepairReceipt::Pending { .. } => (None, None),
            CaptureRepairReceipt::Prepared {
                output_checksum,
                output_size,
                ..
            } => (
                Some(output_checksum.to_owned()),
                Some(
                    i64::try_from(*output_size).map_err(|_error| StateError::Invariant {
                        operation: "record_capture_repair_prepared",
                        detail: "selected capture-repair output size exceeds SQLite INTEGER".into(),
                    })?,
                ),
            ),
        };
        let Some(input_checksum) = pending.asset.local_checksum.as_deref() else {
            return Ok(None);
        };
        let library = pending.asset.library.to_string();
        let asset_id = pending.asset.id.to_string();
        let version_size = pending.asset.version_size.as_str().to_owned();
        let metadata_hash = metadata_hash.to_owned();
        let input_checksum = input_checksum.to_owned();
        let output_checksum = output_checksum.to_owned();
        let output_size_sql =
            i64::try_from(output_size).map_err(|_error| StateError::Invariant {
                operation: "record_capture_repair_prepared",
                detail: "capture-repair output size exceeds SQLite INTEGER".into(),
            })?;
        let Some(path) = pending.asset.local_path.clone() else {
            return Ok(None);
        };
        self.with_conn_mut("record_capture_repair_prepared", move |conn| {
            let tx = conn
                .transaction()
                .map_err(|e| StateError::query("record_capture_repair_prepared::begin", e))?;
            let target = metadata_rewrite_target(&tx, &library, &asset_id, &version_size, &path)?;
            let (table, evidence) = target.sql();

            let updated = tx
                .execute(
                    &format!(
                        "UPDATE {table} SET capture_repair_output_checksum = ?8, \
                        capture_repair_output_size = ?9 \
                     WHERE library = ?1 AND id = ?2 AND version_size = ?3 \
                       AND {evidence} AND local_checksum IS ?5 \
                       AND capture_repair_metadata_hash IS ?4 \
                       AND capture_repair_output_checksum IS ?6 \
                       AND capture_repair_output_size IS ?7 AND local_path IS ?10"
                    ),
                    rusqlite::params![
                        &library,
                        &asset_id,
                        &version_size,
                        metadata_hash,
                        input_checksum,
                        selected_output_checksum,
                        selected_output_size,
                        output_checksum,
                        output_size_sql,
                        path.to_string_lossy()
                    ],
                )
                .map_err(|e| StateError::query("record_capture_repair_prepared", e))?;
            if updated > 0 && target == MetadataRewriteTarget::Catalogue {
                record_metadata_path(
                    &tx,
                    &library,
                    &asset_id,
                    &version_size,
                    MetadataPathWrite::Rewritten,
                )?;
            }
            tx.commit()
                .map_err(|e| StateError::query("record_capture_repair_prepared::commit", e))?;
            Ok((updated > 0).then_some(CaptureRepairReceipt::Prepared {
                metadata_hash,
                output_checksum,
                output_size,
            }))
        })
        .await
    }

    pub(crate) async fn refresh_downloaded_asset_metadata(
        &self,
        library: &str,
        asset_id: &str,
        capture: (&MetadataCapture, DateTime<Utc>, Option<DateTime<Utc>>),
        mark_for_rewrite: bool,
        mark_capture_repair: bool,
        capture_revision: i64,
    ) -> Result<usize, StateError> {
        let library = library.to_owned();
        let asset_id = asset_id.to_owned();
        let (capture, created_at, added_at) = capture;
        let capture = capture.clone();
        let created_at = encode_asset_date(created_at);
        let added_at = added_at.map(encode_asset_date);
        let rewrite_at = Utc::now().timestamp();
        self.with_conn_mut("refresh_downloaded_asset_metadata", move |connection| {
            let tx = connection.transaction().map_err(|e| StateError::query("refresh_downloaded_asset_metadata::begin", e))?;
            let conn = &tx;
            let source = metadata_rewrite_source_sql();
            let mut rows = conn.prepare(
                "SELECT version_size, status = 'downloaded', metadata_hash, checksum, \
                        created_at IS ?3 AND added_at IS ?4 FROM assets \
                 WHERE library = ?1 AND id = ?2 AND is_deleted = 0",
            ).and_then(|mut stmt| {
                stmt.query_map(rusqlite::params![&library, &asset_id, created_at, added_at], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, bool>(1)?, row.get::<_, Option<String>>(2)?, row.get::<_, String>(3)?, row.get::<_, bool>(4)?))
                })?.collect::<Result<Vec<_>, _>>()
            }).map_err(|e| StateError::query("refresh_downloaded_asset_metadata::versions", e))?;
            if rows.iter().any(|(version, _, _, _, _)| VersionSizeKey::from_str(version).is_none()) {
                return Err(StateError::Invariant {
                    operation: "refresh_downloaded_asset_metadata",
                    detail: "unknown stored rendition".into(),
                });
            }
            let has_downloaded = rows.iter().any(|(_, downloaded, _, _, _)| *downloaded);
            rows.retain(|(_, downloaded, _, _, _)| !has_downloaded || *downloaded);
            let mut snapshots = HashMap::with_capacity(rows.len());
            for (version, _, _, checksum, _) in &rows {
                let key = VersionSizeKey::from_str(version).ok_or_else(|| StateError::Invariant {
                    operation: "refresh_downloaded_asset_metadata",
                    detail: "unknown stored rendition".into(),
                })?;
                snapshots.insert(version.as_str(), capture.resolve(key, checksum));
            }
            let snapshot = |version: &str| {
                snapshots.get(version)
                    .ok_or_else(|| StateError::Invariant {
                        operation: "refresh_downloaded_asset_metadata",
                        detail: "missing metadata snapshot for stored rendition".into(),
                    })
            };
            // Validate the entire family, including additional paths, before any
            // write. Each prepared publication belongs to its rendition's hash and
            // to the capture timestamp it was prepared against.
            let prepared = conn.prepare(&format!(
                "SELECT version_size, capture_repair_metadata_hash, created_at IS ?3 FROM ({source}) \
                 WHERE library = ?1 AND id = ?2 AND status = 'downloaded' AND is_deleted = 0 \
                   AND capture_repair_metadata_hash IS NOT NULL AND capture_repair_metadata_hash <> '' \
                   AND capture_repair_output_checksum IS NOT NULL AND capture_repair_output_checksum <> '' \
                   AND capture_repair_output_size IS NOT NULL AND capture_repair_output_size >= 0",
            )).and_then(|mut stmt| {
                stmt.query_map(rusqlite::params![&library, &asset_id, created_at], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, bool>(2)?))
                })?.collect::<Result<Vec<_>, _>>()
            }).map_err(|e| StateError::query("refresh_downloaded_asset_metadata::capture_repair_guard", e))?;
            for (version, hash, capture_date_matches) in prepared {
                if !capture_date_matches
                    || snapshot(&version)?.metadata_hash.as_deref() != Some(hash.as_str())
                {
                    return Ok(0);
                }
            }
            // Provider replacement can return every row to pending after its
            // metadata was stored. Count that fallback only when all rows match
            // their own intended snapshots; never promote a partially stale family.
            if !has_downloaded {
                ensure_asset_has_no_prepared_capture_repair(
                    conn,
                    &library,
                    &asset_id,
                    "refresh_downloaded_asset_metadata",
                )?;
                for (version, _, hash, _, dates_match) in &rows {
                    if hash.is_none() || *hash != snapshot(version)?.metadata_hash || !*dates_match {
                        return Ok(0);
                    }
                }
            }
            let mut durable = 0;
            for (version, _, _, _, _) in &rows {
                let metadata = snapshot(version)?;
                let updated = conn
                .execute(
                    r"
                    UPDATE assets SET
                        source = COALESCE(?1, source),
                        is_favorite = ?2,
                        rating = ?3,
                        latitude = ?4,
                        longitude = ?5,
                        altitude = ?6,
                        orientation = ?7,
                        duration_secs = ?8,
                        timezone_offset = ?9,
                        width = ?10,
                        height = ?11,
                        title = ?12,
                        keywords = ?13,
                        description = ?14,
                        media_subtype = ?15,
                        burst_id = ?16,
                        is_hidden = ?17,
                        is_archived = ?18,
                        modified_at = ?19,
                        is_deleted = ?20,
                        deleted_at = ?21,
                        provider_data = ?22,
                        metadata_hash = ?23,
                        created_at = ?30,
                        added_at = ?31,
                        capture_repair_output_checksum =
                            CASE
                                WHEN capture_repair_metadata_hash <> ''
                                     AND capture_repair_output_checksum <> ''
                                     AND capture_repair_output_size >= 0
                                    THEN capture_repair_output_checksum
                                WHEN capture_repair_metadata_hash IS NULL
                                     AND capture_repair_output_checksum IS NULL
                                     AND capture_repair_output_size IS NULL
                                    THEN NULL
                                WHEN capture_repair_metadata_hash <> ''
                                     AND capture_repair_output_checksum IS NULL
                                     AND capture_repair_output_size IS NULL
                                    THEN NULL
                                ELSE capture_repair_output_checksum
                            END,
                        capture_repair_output_size =
                            CASE
                                WHEN capture_repair_metadata_hash <> ''
                                     AND capture_repair_output_checksum <> ''
                                     AND capture_repair_output_size >= 0
                                    THEN capture_repair_output_size
                                WHEN capture_repair_metadata_hash IS NULL
                                     AND capture_repair_output_checksum IS NULL
                                     AND capture_repair_output_size IS NULL
                                    THEN NULL
                                WHEN capture_repair_metadata_hash <> ''
                                     AND capture_repair_output_checksum IS NULL
                                     AND capture_repair_output_size IS NULL
                                    THEN NULL
                                ELSE capture_repair_output_size
                            END,
                        capture_repair_metadata_hash =
                            CASE
                                WHEN capture_repair_metadata_hash <> ''
                                     AND capture_repair_output_checksum <> ''
                                     AND capture_repair_output_size >= 0
                                    THEN capture_repair_metadata_hash
                                WHEN capture_repair_metadata_hash <> ''
                                     AND capture_repair_output_checksum IS NULL
                                     AND capture_repair_output_size IS NULL
                                    THEN ?23
                                WHEN capture_repair_metadata_hash IS NULL
                                     AND capture_repair_output_checksum IS NULL
                                     AND capture_repair_output_size IS NULL
                                    THEN CASE WHEN ?25 = 1 THEN ?23 ELSE NULL END
                                ELSE capture_repair_metadata_hash
                            END,
                        metadata_write_failed_at =
                            CASE WHEN ?24 = 1 THEN ?26 ELSE metadata_write_failed_at END
                    WHERE library = ?27 AND id = ?28 AND version_size = ?29
                      AND status = 'downloaded' AND is_deleted = 0
                    ",
                    rusqlite::params![
                        metadata.source.as_deref(),
                        i64::from(metadata.is_favorite),
                        metadata.rating.map(i64::from),
                        metadata.latitude,
                        metadata.longitude,
                        metadata.altitude,
                        metadata.orientation.map(i64::from),
                        metadata.duration_secs,
                        metadata.timezone_offset.map(i64::from),
                        metadata.width.map(i64::from),
                        metadata.height.map(i64::from),
                        metadata.title.as_deref(),
                        metadata.keywords.as_deref(),
                        metadata.description.as_deref(),
                        metadata.media_subtype.as_deref(),
                        metadata.burst_id.as_deref(),
                        i64::from(metadata.is_hidden),
                        i64::from(metadata.is_archived),
                        metadata.modified_at.map(|dt| dt.timestamp()),
                        i64::from(metadata.is_deleted),
                        metadata.deleted_at.map(|dt| dt.timestamp()),
                        metadata.provider_data.as_deref(),
                        metadata.metadata_hash.as_deref(),
                        i64::from(mark_for_rewrite),
                        i64::from(mark_capture_repair),
                        rewrite_at,
                        library,
                        asset_id,
                        version,
                        created_at,
                        added_at,
                    ],
                )
                .map_err(|e| StateError::query("refresh_downloaded_asset_metadata", e))?;
                durable += if has_downloaded { updated } else { 1 };
                conn.execute(
                    "UPDATE asset_metadata_paths SET \
                        metadata_write_failed_at = CASE WHEN ?3 = 1 THEN ?5 ELSE metadata_write_failed_at END, \
                        capture_repair_metadata_hash = CASE WHEN ( \
                            (?4 = 1 AND capture_repair_metadata_hash IS NULL AND capture_repair_output_checksum IS NULL AND capture_repair_output_size IS NULL) \
                            OR (capture_repair_metadata_hash <> '' AND capture_repair_output_checksum IS NULL AND capture_repair_output_size IS NULL)) \
                            THEN ?6 ELSE capture_repair_metadata_hash END \
                     WHERE library = ?1 AND id = ?2 AND version_size = ?7 AND EXISTS ( \
                         SELECT 1 FROM assets a WHERE a.library = ?1 AND a.id = ?2 \
                           AND a.version_size = asset_metadata_paths.version_size \
                           AND a.checksum = asset_metadata_paths.provider_checksum \
                           AND a.status = 'downloaded' AND a.is_deleted = 0)",
                    rusqlite::params![&library, &asset_id, i64::from(mark_for_rewrite), i64::from(mark_capture_repair), rewrite_at, metadata.metadata_hash.as_deref(), version],
                ).map_err(|e| StateError::query("refresh_downloaded_asset_metadata::paths", e))?;
            }
            if durable > 0 {
                record_metadata_capture_revision(
                    conn,
                    &library,
                    &asset_id,
                    capture_revision,
                    rewrite_at,
                )?;
            }
            tx.commit().map_err(|e| StateError::query("refresh_downloaded_asset_metadata::commit", e))?;
            Ok(durable)
        })
        .await
    }

    #[cfg(test)]
    pub(crate) async fn clear_metadata_write_failure(
        &self,
        library: &str,
        asset_id: &str,
        version_size: &str,
    ) -> Result<(), StateError> {
        let library = library.to_owned();
        let asset_id = asset_id.to_owned();
        let version_size = version_size.to_owned();
        self.with_conn("clear_metadata_write_failure", move |conn| {
            conn.execute(
                "UPDATE assets SET metadata_write_failed_at = NULL \
                 WHERE library = ?1 AND id = ?2 AND version_size = ?3",
                rusqlite::params![library, asset_id, version_size],
            )
            .map_err(|e| StateError::query("clear_metadata_write_failure", e))?;
            Ok(())
        })
        .await
    }

    pub(crate) async fn get_downloaded_metadata_hashes(
        &self,
    ) -> Result<HashMap<(String, String, String), String>, StateError> {
        self.with_conn("get_downloaded_metadata_hashes", move |conn| {
            let mut stmt = conn
                .prepare_cached(
                    "SELECT library, id, version_size, metadata_hash FROM assets \
                     WHERE status = 'downloaded' AND metadata_hash IS NOT NULL",
                )
                .map_err(|e| StateError::query("get_downloaded_metadata_hashes", e))?;
            let mut hashes: HashMap<(String, String, String), String> = HashMap::new();
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
                .map_err(|e| StateError::query("get_downloaded_metadata_hashes", e))?;
            for row in rows {
                let (key, val) =
                    row.map_err(|e| StateError::query("get_downloaded_metadata_hashes", e))?;
                hashes.insert(key, val);
            }
            Ok(hashes)
        })
        .await
    }

    #[cfg(test)]
    pub(crate) async fn get_pending_metadata_rewrites(
        &self,
        limit: usize,
    ) -> Result<Vec<AssetRecord>, StateError> {
        self.get_pending_metadata_rewrites_page(None, 0, limit)
            .await
    }

    pub(crate) async fn get_pending_metadata_rewrites_page(
        &self,
        library_scope: Option<&[&str]>,
        offset: usize,
        limit: usize,
    ) -> Result<Vec<AssetRecord>, StateError> {
        Ok(self
            .get_pending_metadata_rewrites_page_for_queue(
                MetadataRewriteQueue::Ordinary,
                library_scope,
                offset,
                limit,
            )
            .await?
            .into_iter()
            .map(|pending| pending.asset)
            .collect())
    }

    pub(crate) async fn get_pending_metadata_rewrites_page_for_queue(
        &self,
        queue: MetadataRewriteQueue,
        library_scope: Option<&[&str]>,
        offset: usize,
        limit: usize,
    ) -> Result<Vec<PendingMetadataRewrite>, StateError> {
        let libraries = library_scope.map(unique_sorted_strings);
        if libraries.as_ref().is_some_and(Vec::is_empty) {
            return Ok(Vec::new());
        }
        self.with_conn("get_pending_metadata_rewrites_for_queue", move |conn| {
            query_pending_metadata_rewrites(conn, queue, libraries.as_deref(), offset, limit)
        })
        .await
    }

    pub(crate) async fn finish_metadata_rewrite(
        &self,
        pending: &PendingMetadataRewrite,
        selected_queue: MetadataRewriteQueue,
        local_checksum: Option<&str>,
        pre_rewrite_checksum: Option<&str>,
        completion: MetadataRewriteCompletion,
    ) -> Result<bool, StateError> {
        let metadata_hash = pending
            .asset
            .metadata
            .metadata_hash
            .clone()
            .ok_or_else(|| StateError::Invariant {
                operation: "finish_metadata_rewrite",
                detail: "the selected row has no metadata hash".into(),
            })?;
        if pre_rewrite_checksum.is_some()
            && pre_rewrite_checksum != pending.asset.local_checksum.as_deref()
        {
            return Err(StateError::Invariant {
                operation: "finish_metadata_rewrite",
                detail: "pre-rewrite checksum does not match the selected input".into(),
            });
        }
        let capture_receipt = pending.capture_repair_receipt.as_ref();
        if (selected_queue == MetadataRewriteQueue::CaptureRepair
            || completion.clears(MetadataRewriteQueue::CaptureRepair))
            && capture_receipt.is_none()
        {
            return Err(StateError::Invariant {
                operation: "finish_metadata_rewrite",
                detail: "capture-repair completion requires a typed receipt".into(),
            });
        }
        if let Some(CaptureRepairReceipt::Pending { .. }) = capture_receipt
            && completion.clears(MetadataRewriteQueue::CaptureRepair)
            && local_checksum != pending.asset.local_checksum.as_deref()
        {
            return Err(StateError::Invariant {
                operation: "finish_metadata_rewrite",
                detail: "capture-repair bytes changed without a prepared receipt".into(),
            });
        }
        if let Some(CaptureRepairReceipt::Prepared {
            output_checksum, ..
        }) = capture_receipt
            && completion.clears(MetadataRewriteQueue::CaptureRepair)
            && local_checksum != Some(output_checksum.as_str())
            && local_checksum != pending.asset.local_checksum.as_deref()
        {
            return Err(StateError::Invariant {
                operation: "finish_metadata_rewrite",
                detail: "capture-repair output does not match its prepared receipt".into(),
            });
        }

        let library = pending.asset.library.to_string();
        let asset_id = pending.asset.id.to_string();
        let version_size = pending.asset.version_size.as_str().to_owned();
        let input_checksum = pending.asset.local_checksum.clone();
        let local_checksum = local_checksum.map(str::to_owned);
        let pre_rewrite_checksum = pre_rewrite_checksum.map(str::to_owned);
        let capture_metadata_hash = capture_receipt.map(CaptureRepairReceipt::metadata_hash);
        let capture_output_checksum = capture_receipt.and_then(|receipt| match receipt {
            CaptureRepairReceipt::Pending { .. } => None,
            CaptureRepairReceipt::Prepared {
                output_checksum, ..
            } => Some(output_checksum.as_str()),
        });
        let capture_output_size = match capture_receipt {
            None | Some(CaptureRepairReceipt::Pending { .. }) => None,
            Some(CaptureRepairReceipt::Prepared { output_size, .. }) => Some(
                i64::try_from(*output_size).map_err(|_error| StateError::Invariant {
                    operation: "finish_metadata_rewrite",
                    detail: "capture-repair output size exceeds SQLite INTEGER".into(),
                })?,
            ),
        };
        let capture_metadata_hash = capture_metadata_hash.map(str::to_owned);
        let capture_output_checksum = capture_output_checksum.map(str::to_owned);
        let selected_capture = i64::from(selected_queue == MetadataRewriteQueue::CaptureRepair);
        let clear_ordinary = i64::from(completion.clears(MetadataRewriteQueue::Ordinary));
        let clear_capture = i64::from(completion.clears(MetadataRewriteQueue::CaptureRepair));
        let Some(path) = pending.asset.local_path.clone() else {
            return Ok(false);
        };
        self.with_conn_mut("finish_metadata_rewrite", move |conn| {
            let tx = conn
                .transaction()
                .map_err(|e| StateError::query("finish_metadata_rewrite::begin", e))?;
            let target = metadata_rewrite_target(&tx, &library, &asset_id, &version_size, &path)?;
            let (table, evidence) = target.sql();

            let updated = tx
                .execute(
                    &format!(
                        "UPDATE {table} SET \
                        local_checksum = ?6, \
                        download_checksum = COALESCE(download_checksum, ?7), \
                        metadata_write_failed_at = CASE WHEN ?8 = 1 \
                            THEN NULL ELSE metadata_write_failed_at END, \
                        capture_repair_metadata_hash = CASE \
                            WHEN ?9 = 1 THEN NULL \
                            ELSE capture_repair_metadata_hash END, \
                        capture_repair_output_checksum = CASE \
                            WHEN ?9 = 1 OR local_checksum IS NOT ?6 \
                                THEN NULL ELSE capture_repair_output_checksum END, \
                        capture_repair_output_size = CASE \
                            WHEN ?9 = 1 OR local_checksum IS NOT ?6 \
                                THEN NULL ELSE capture_repair_output_size END \
                     WHERE library = ?1 AND id = ?2 AND version_size = ?3 \
                       AND {evidence} AND local_checksum IS ?5 \
                       AND ((?10 = 0 AND metadata_write_failed_at IS NOT NULL) \
                         OR (?10 = 1 \
                           AND capture_repair_metadata_hash IS ?11 \
                           AND capture_repair_output_checksum IS ?12 \
                           AND capture_repair_output_size IS ?13)) \
                       AND (?9 = 0 OR ( \
                           capture_repair_metadata_hash IS ?11 \
                           AND capture_repair_output_checksum IS ?12 \
                           AND capture_repair_output_size IS ?13)) AND local_path IS ?14"
                    ),
                    rusqlite::params![
                        &library,
                        &asset_id,
                        &version_size,
                        metadata_hash,
                        input_checksum,
                        local_checksum,
                        pre_rewrite_checksum,
                        clear_ordinary,
                        clear_capture,
                        selected_capture,
                        capture_metadata_hash,
                        capture_output_checksum,
                        capture_output_size,
                        path.to_string_lossy()
                    ],
                )
                .map_err(|e| StateError::query("finish_metadata_rewrite", e))?;
            if updated > 0 && target == MetadataRewriteTarget::Catalogue {
                record_metadata_path(
                    &tx,
                    &library,
                    &asset_id,
                    &version_size,
                    MetadataPathWrite::Rewritten,
                )?;
            }
            tx.commit()
                .map_err(|e| StateError::query("finish_metadata_rewrite::commit", e))?;
            Ok(updated > 0 && completion.clears(selected_queue))
        })
        .await
    }

    pub(crate) async fn get_metadata_retry_markers(
        &self,
    ) -> Result<HashSet<(String, String, String)>, StateError> {
        self.with_conn("get_metadata_retry_markers", move |conn| {
            let source = metadata_rewrite_source_sql();
            let mut stmt = conn
                .prepare(&format!("SELECT library, id, version_size FROM ({source}) WHERE metadata_write_failed_at IS NOT NULL"))
                .map_err(|e| StateError::query("get_metadata_retry_markers", e))?;
            let mut markers: HashSet<(String, String, String)> = HashSet::new();
            let rows = stmt
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                })
                .map_err(|e| StateError::query("get_metadata_retry_markers", e))?;
            for row in rows {
                let key = row.map_err(|e| StateError::query("get_metadata_retry_markers", e))?;
                markers.insert(key);
            }
            Ok(markers)
        })
        .await
    }

    pub(crate) async fn has_downloaded_without_metadata_hash(&self) -> Result<bool, StateError> {
        self.with_conn("has_downloaded_without_metadata_hash", move |conn| {
            let exists: i64 = conn
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM assets WHERE status = 'downloaded' \
                     AND is_deleted = 0 AND metadata_hash IS NULL)",
                    [],
                    |row| row.get(0),
                )
                .map_err(|e| StateError::query("has_downloaded_without_metadata_hash", e))?;
            Ok(exists != 0)
        })
        .await
    }

    pub(crate) async fn begin_metadata_capture_revision(
        &self,
        library: &str,
        target_revision: i64,
    ) -> Result<MetadataCaptureStatus, StateError> {
        let library = library.to_owned();
        self.with_conn_mut("begin_metadata_capture_revision", move |conn| {
            let tx = conn.transaction().map_err(|e| {
                StateError::query("begin_metadata_capture_revision::transaction", e)
            })?;
            let remaining = metadata_capture_remaining(&tx, &library, target_revision)?;
            let existing = tx
                .query_row(
                    "SELECT active_revision, pending_revision, processed_assets, \
                            failed_assets, last_error \
                     FROM metadata_capture_state WHERE library = ?1",
                    [&library],
                    |row| {
                        Ok((
                            row.get::<_, i64>(0)?,
                            row.get::<_, Option<i64>>(1)?,
                            row.get::<_, i64>(2)?,
                            row.get::<_, i64>(3)?,
                            row.get::<_, Option<String>>(4)?,
                        ))
                    },
                )
                .optional()
                .map_err(|e| StateError::query("begin_metadata_capture_revision::read", e))?;
            let (mut active, previous_pending, mut processed, mut failed, mut last_error) =
                existing.unwrap_or((0, None, 0, 0, None));
            let pending = if remaining == 0 {
                active = active.max(target_revision);
                None
            } else {
                if previous_pending != Some(target_revision) {
                    processed = 0;
                    failed = 0;
                    last_error = None;
                }
                Some(target_revision)
            };
            let now = Utc::now().timestamp();
            tx.execute(
                "INSERT INTO metadata_capture_state \
                    (library, active_revision, pending_revision, processed_assets, \
                     failed_assets, last_error, updated_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7) \
                 ON CONFLICT(library) DO UPDATE SET \
                    active_revision = excluded.active_revision, \
                    pending_revision = excluded.pending_revision, \
                    processed_assets = excluded.processed_assets, \
                    failed_assets = excluded.failed_assets, \
                    last_error = excluded.last_error, updated_at = excluded.updated_at",
                rusqlite::params![library, active, pending, processed, failed, last_error, now],
            )
            .map_err(|e| StateError::query("begin_metadata_capture_revision::write", e))?;
            tx.commit()
                .map_err(|e| StateError::query("begin_metadata_capture_revision::commit", e))?;
            Ok(MetadataCaptureStatus {
                library,
                active_revision: active,
                pending_revision: pending,
                processed_assets: u64::try_from(processed).unwrap_or(0),
                failed_assets: u64::try_from(failed).unwrap_or(0),
                remaining_assets: remaining,
                last_error,
            })
        })
        .await
    }

    pub(crate) async fn get_metadata_capture_candidates(
        &self,
        library: &str,
        target_revision: i64,
        limit: usize,
    ) -> Result<Vec<MetadataCaptureCandidate>, StateError> {
        let library = library.to_owned();
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        self.with_conn("get_metadata_capture_candidates", move |conn| {
            let mut stmt = conn
                .prepare_cached(
                    r"
                    WITH stale_assets AS (
                        SELECT assets.library, assets.id
                        FROM assets
                        LEFT JOIN asset_metadata_capture_revisions AS revisions
                          ON revisions.library = assets.library
                         AND revisions.asset_id = assets.id
                        WHERE assets.library = ?1
                          AND assets.status = 'downloaded'
                          AND assets.is_deleted = 0
                          AND (revisions.revision IS NULL OR revisions.revision < ?2)
                        GROUP BY assets.library, assets.id
                        ORDER BY assets.id
                        LIMIT ?3
                    )
                    SELECT stale_assets.library, stale_assets.id,
                           COALESCE(mapping.master_record_name,
                                    owner.master_record_name,
                                    stale_assets.id),
                           COALESCE(mapping.asset_record_name,
                                    owner.asset_record_name),
                           assets.version_size, assets.checksum, assets.size_bytes
                    FROM stale_assets
                    JOIN assets
                      ON assets.library = stale_assets.library
                     AND assets.id = stale_assets.id
                     AND assets.status = 'downloaded'
                     AND assets.is_deleted = 0
                    LEFT JOIN asset_master_mappings AS mapping
                      ON mapping.library = stale_assets.library
                     AND mapping.asset_record_name = stale_assets.id
                    LEFT JOIN legacy_master_state_owners AS owner
                      ON owner.library = stale_assets.library
                     AND owner.master_record_name = stale_assets.id
                    ORDER BY stale_assets.id, assets.version_size
                    ",
                )
                .map_err(|e| StateError::query("get_metadata_capture_candidates::prepare", e))?;
            let rows = stmt
                .query_map(rusqlite::params![library, target_revision, limit], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, Option<String>>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, String>(5)?,
                        row.get::<_, i64>(6)?,
                    ))
                })
                .map_err(|e| StateError::query("get_metadata_capture_candidates::query", e))?;
            let mut candidates = Vec::<MetadataCaptureCandidate>::new();
            for row in rows {
                let (library, asset_id, master, asset, version, checksum, size) =
                    row.map_err(|e| StateError::query("get_metadata_capture_candidates::row", e))?;
                let version_size =
                    VersionSizeKey::from_str(&version).ok_or_else(|| StateError::Invariant {
                        operation: "get_metadata_capture_candidates",
                        detail: format!("unknown durable version_size {version:?}"),
                    })?;
                let evidence = MetadataCaptureVersionEvidence {
                    version_size,
                    checksum,
                    size_bytes: u64::try_from(size).unwrap_or(0),
                };
                if let Some(candidate) = candidates.last_mut()
                    && candidate.asset_id == asset_id
                {
                    candidate.versions.push(evidence);
                } else {
                    candidates.push(MetadataCaptureCandidate {
                        library,
                        asset_id,
                        master_record_name: master,
                        asset_record_name: asset,
                        versions: vec![evidence],
                    });
                }
            }
            Ok(candidates)
        })
        .await
    }

    pub(crate) async fn record_metadata_capture_failure(
        &self,
        library: &str,
        target_revision: i64,
        error: &str,
    ) -> Result<(), StateError> {
        let library = library.to_owned();
        let error = error.to_owned();
        self.with_conn("record_metadata_capture_failure", move |conn| {
            conn.execute(
                "UPDATE metadata_capture_state SET failed_assets = failed_assets + 1, \
                    last_error = ?1, updated_at = ?2 \
                 WHERE library = ?3 AND pending_revision = ?4",
                rusqlite::params![error, Utc::now().timestamp(), library, target_revision],
            )
            .map_err(|e| StateError::query("record_metadata_capture_failure", e))?;
            Ok(())
        })
        .await
    }

    pub(crate) async fn complete_metadata_capture_revision(
        &self,
        library: &str,
        target_revision: i64,
    ) -> Result<MetadataCaptureStatus, StateError> {
        let library = library.to_owned();
        self.with_conn_mut("complete_metadata_capture_revision", move |conn| {
            let tx = conn.transaction().map_err(|e| {
                StateError::query("complete_metadata_capture_revision::transaction", e)
            })?;
            let remaining = metadata_capture_remaining(&tx, &library, target_revision)?;
            if remaining == 0 {
                tx.execute(
                    "UPDATE metadata_capture_state SET active_revision = MAX(active_revision, ?1), \
                        pending_revision = NULL, failed_assets = 0, last_error = NULL, \
                        updated_at = ?2 \
                     WHERE library = ?3",
                    rusqlite::params![target_revision, Utc::now().timestamp(), library],
                )
                .map_err(|e| StateError::query("complete_metadata_capture_revision::write", e))?;
            }
            let status = metadata_capture_status(&tx, &library, target_revision, remaining)?;
            tx.commit()
                .map_err(|e| StateError::query("complete_metadata_capture_revision::commit", e))?;
            Ok(status)
        })
        .await
    }

    pub(crate) async fn has_metadata_capture_work(
        &self,
        libraries: &[&str],
        target_revision: i64,
    ) -> Result<bool, StateError> {
        if libraries.is_empty() {
            return Ok(false);
        }
        let libraries = unique_sorted_strings(libraries);
        self.with_conn("has_metadata_capture_work", move |conn| {
            for library in libraries {
                if metadata_capture_remaining(conn, &library, target_revision)? > 0 {
                    return Ok(true);
                }
                let pending: bool = conn
                    .query_row(
                        "SELECT EXISTS(SELECT 1 FROM metadata_capture_state \
                         WHERE library = ?1 AND pending_revision = ?2)",
                        rusqlite::params![library, target_revision],
                        |row| row.get::<_, i64>(0),
                    )
                    .map(|value| value != 0)
                    .map_err(|e| StateError::query("has_metadata_capture_work", e))?;
                if pending {
                    return Ok(true);
                }
            }
            Ok(false)
        })
        .await
    }
}

#[cfg(test)]
impl SqliteStateDb {
    pub(crate) fn fail_provider_metadata_refresh_for_test(&self) {
        let conn = self
            .acquire_lock("test_fail_provider_metadata_refresh")
            .unwrap();
        conn.execute_batch(
            "CREATE TEMP TRIGGER fail_provider_metadata_refresh \
             BEFORE UPDATE OF metadata_hash ON assets \
             BEGIN SELECT RAISE(FAIL, 'simulated provider metadata refresh failure'); END;",
        )
        .unwrap();
    }

    #[cfg(feature = "xmp")]
    pub(crate) fn fail_metadata_marker_clear_for_test(&self) {
        let conn = self
            .acquire_lock("test_fail_metadata_marker_clear")
            .unwrap();
        conn.execute_batch(
            "CREATE TEMP TRIGGER fail_metadata_marker_clear \
             BEFORE UPDATE OF metadata_write_failed_at ON assets \
             WHEN NEW.metadata_write_failed_at IS NULL \
             BEGIN SELECT RAISE(FAIL, 'simulated metadata marker clear failure'); END;",
        )
        .unwrap();
    }

    #[cfg(feature = "xmp")]
    pub(crate) fn fail_metadata_checksum_write_for_test(&self) {
        let conn = self
            .acquire_lock("test_fail_metadata_checksum_write")
            .unwrap();
        conn.execute_batch(
            "CREATE TEMP TRIGGER fail_metadata_checksum_write \
             BEFORE UPDATE OF local_checksum ON assets \
             WHEN NEW.local_checksum IS NOT NULL \
             BEGIN SELECT RAISE(FAIL, 'simulated rewritten checksum write failure'); END;",
        )
        .unwrap();
    }

    #[cfg(feature = "xmp")]
    pub(crate) fn clear_local_checksum_for_test(
        &self,
        library: &str,
        asset_id: &str,
        version_size: &str,
    ) {
        let conn = self.acquire_lock("test_clear_local_checksum").unwrap();
        conn.execute(
            "UPDATE assets SET local_checksum = NULL, \
                capture_repair_metadata_hash = NULL, \
                capture_repair_output_checksum = NULL, \
                capture_repair_output_size = NULL \
             WHERE library = ?1 AND id = ?2 AND version_size = ?3",
            rusqlite::params![library, asset_id, version_size],
        )
        .unwrap();
    }

    pub(crate) fn clear_metadata_hash_for_test(
        &self,
        library: &str,
        asset_id: &str,
        version_size: &str,
    ) {
        let conn = self.acquire_lock("test_clear_metadata_hash").unwrap();
        conn.execute(
            "UPDATE assets SET metadata_hash = NULL \
             WHERE library = ?1 AND id = ?2 AND version_size = ?3",
            rusqlite::params![library, asset_id, version_size],
        )
        .unwrap();
    }

    pub(crate) fn set_metadata_capture_revision_for_test(
        &self,
        library: &str,
        asset_id: &str,
        revision: i64,
    ) {
        let conn = self
            .acquire_lock("test_set_metadata_capture_revision")
            .unwrap();
        conn.execute(
            "INSERT INTO asset_metadata_capture_revisions \
                (library, asset_id, revision, updated_at) VALUES (?1, ?2, ?3, 0) \
             ON CONFLICT(library, asset_id) DO UPDATE SET revision = excluded.revision",
            rusqlite::params![library, asset_id, revision],
        )
        .unwrap();
    }
}

#[async_trait]
impl MetadataRewriteStore for SqliteStateDb {
    async fn record_metadata_write_failure(
        &self,
        library: &str,
        asset_id: &str,
        version_size: &str,
    ) -> Result<(), StateError> {
        SqliteStateDb::record_metadata_write_failure(self, library, asset_id, version_size).await
    }

    async fn refresh_downloaded_asset_metadata(
        &self,
        library: &str,
        asset_id: &str,
        capture: (&MetadataCapture, DateTime<Utc>, Option<DateTime<Utc>>),
        mark_for_rewrite: bool,
        mark_capture_repair: bool,
        capture_revision: i64,
    ) -> Result<usize, StateError> {
        SqliteStateDb::refresh_downloaded_asset_metadata(
            self,
            library,
            asset_id,
            capture,
            mark_for_rewrite,
            mark_capture_repair,
            capture_revision,
        )
        .await
    }

    async fn get_downloaded_metadata_hashes(
        &self,
    ) -> Result<HashMap<(String, String, String), String>, StateError> {
        SqliteStateDb::get_downloaded_metadata_hashes(self).await
    }

    async fn get_metadata_retry_markers(
        &self,
    ) -> Result<HashSet<(String, String, String)>, StateError> {
        SqliteStateDb::get_metadata_retry_markers(self).await
    }

    async fn get_pending_metadata_rewrites_page(
        &self,
        library_scope: Option<&[&str]>,
        offset: usize,
        limit: usize,
    ) -> Result<Vec<AssetRecord>, StateError> {
        SqliteStateDb::get_pending_metadata_rewrites_page(self, library_scope, offset, limit).await
    }

    async fn get_pending_metadata_rewrites_page_for_queue(
        &self,
        queue: MetadataRewriteQueue,
        library_scope: Option<&[&str]>,
        offset: usize,
        limit: usize,
    ) -> Result<Vec<PendingMetadataRewrite>, StateError> {
        SqliteStateDb::get_pending_metadata_rewrites_page_for_queue(
            self,
            queue,
            library_scope,
            offset,
            limit,
        )
        .await
    }

    async fn record_capture_repair_prepared(
        &self,
        pending: &PendingMetadataRewrite,
        output_checksum: &str,
        output_size: u64,
    ) -> Result<Option<CaptureRepairReceipt>, StateError> {
        SqliteStateDb::record_capture_repair_prepared(self, pending, output_checksum, output_size)
            .await
    }

    async fn finish_metadata_rewrite(
        &self,
        pending: &PendingMetadataRewrite,
        selected_queue: MetadataRewriteQueue,
        local_checksum: Option<&str>,
        pre_rewrite_checksum: Option<&str>,
        completion: MetadataRewriteCompletion,
    ) -> Result<bool, StateError> {
        SqliteStateDb::finish_metadata_rewrite(
            self,
            pending,
            selected_queue,
            local_checksum,
            pre_rewrite_checksum,
            completion,
        )
        .await
    }

    async fn has_downloaded_without_metadata_hash(&self) -> Result<bool, StateError> {
        SqliteStateDb::has_downloaded_without_metadata_hash(self).await
    }

    async fn begin_metadata_capture_revision(
        &self,
        library: &str,
        target_revision: i64,
    ) -> Result<MetadataCaptureStatus, StateError> {
        SqliteStateDb::begin_metadata_capture_revision(self, library, target_revision).await
    }

    async fn get_metadata_capture_candidates(
        &self,
        library: &str,
        target_revision: i64,
        limit: usize,
    ) -> Result<Vec<MetadataCaptureCandidate>, StateError> {
        SqliteStateDb::get_metadata_capture_candidates(self, library, target_revision, limit).await
    }

    async fn record_metadata_capture_failure(
        &self,
        library: &str,
        target_revision: i64,
        error: &str,
    ) -> Result<(), StateError> {
        SqliteStateDb::record_metadata_capture_failure(self, library, target_revision, error).await
    }

    async fn complete_metadata_capture_revision(
        &self,
        library: &str,
        target_revision: i64,
    ) -> Result<MetadataCaptureStatus, StateError> {
        SqliteStateDb::complete_metadata_capture_revision(self, library, target_revision).await
    }

    async fn has_metadata_capture_work(
        &self,
        libraries: &[&str],
        target_revision: i64,
    ) -> Result<bool, StateError> {
        SqliteStateDb::has_metadata_capture_work(self, libraries, target_revision).await
    }
}

#[cfg(test)]
mod tests;
