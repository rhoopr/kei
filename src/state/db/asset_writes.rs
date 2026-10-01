//! Shared asset-row writes and publication evidence within caller-owned transactions.

use std::path::Path;

use rusqlite::Connection;

use super::rows::{ASSET_COLUMNS, encode_asset_date};
use crate::state::error::StateError;
use crate::state::types::AssetRecord;

/// Fallback source identifier when `AssetMetadata::source` is unset.
///
/// The `assets.source` column is NOT NULL (v5 migration defaults pre-existing
/// rows to "icloud"). Test fixtures and legacy call sites that don't populate
/// metadata get the same value written here so that inserts always succeed.
/// CloudKit parsing sets `source` explicitly; this fallback is a safety net,
/// not the intended write path.
const DEFAULT_SOURCE: &str = "icloud";

/// Execute the asset UPSERT on `conn` (works against either a `Connection`
/// or a `Transaction`, since `Transaction: Deref<Target = Connection>`).
/// Shared by `upsert_seen` and `import_adopt` so both write the same column
/// set and conflict resolution.
pub(super) fn upsert_asset_row(
    conn: &Connection,
    record: &AssetRecord,
    last_seen_at: i64,
) -> Result<(), StateError> {
    let meta = &record.metadata;
    // Lazily compute metadata_hash if caller supplied metadata without one.
    // Storing the hash alongside the metadata is what lets feature 5 detect
    // metadata-only changes in O(1) during incremental sync. Computed only
    // when missing (rare — extract() normally pre-populates it).
    let computed_hash: Option<String> = if meta.metadata_hash.is_none() {
        Some(meta.compute_hash())
    } else {
        None
    };
    let metadata_hash: Option<&str> = meta.metadata_hash.as_deref().or(computed_hash.as_deref());
    let source = metadata_rewrite_source_sql();
    let blocked_capture_repair: i64 = conn
        .query_row(
            &format!(
                "SELECT EXISTS( \
                SELECT 1 FROM ({source}) \
                WHERE library = ?1 AND id = ?2 \
                  AND status = 'downloaded' AND is_deleted = 0 \
                  AND capture_repair_metadata_hash IS NOT NULL \
                  AND capture_repair_metadata_hash <> '' \
                  AND capture_repair_output_checksum IS NOT NULL \
                  AND capture_repair_output_checksum <> '' \
                  AND capture_repair_output_size IS NOT NULL \
                  AND capture_repair_output_size >= 0 \
                  AND (created_at IS NOT ?6 \
                       OR (version_size = ?4 AND capture_repair_metadata_hash IS NOT ?3)) \
                  AND (version_size <> ?4 OR checksum = ?5) \
            )"
            ),
            rusqlite::params![
                &record.library,
                &record.id,
                metadata_hash,
                record.version_size.as_str(),
                &record.checksum,
                encode_asset_date(record.created_at)
            ],
            |row| row.get(0),
        )
        .map_err(|e| StateError::query("upsert_seen::capture_repair_guard", e))?;
    if blocked_capture_repair != 0 {
        return Err(StateError::Invariant {
            operation: "upsert_seen",
            detail:
                "provider metadata changed while capture-repair publication awaits finalization"
                    .into(),
        });
    }

    let mut stmt = conn
        .prepare_cached(
            r"
                INSERT INTO assets (
                    library, id, version_size, checksum, filename, created_at, added_at,
                    size_bytes, media_type, status, last_seen_at,
                    source, is_favorite, rating, latitude, longitude, altitude,
                    orientation, duration_secs, timezone_offset, width, height,
                    title, keywords, description, media_subtype, burst_id,
                    is_hidden, is_archived, modified_at, is_deleted, deleted_at,
                    provider_data, metadata_hash
                )
                VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 'pending', ?10,
                        ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21,
                        ?22, ?23, ?24, ?25, ?26, ?27, ?28, ?29, ?30, ?31, ?32, ?33)
                ON CONFLICT(library, id, version_size) DO UPDATE SET
                    status = CASE
                        WHEN assets.status = 'policy_excluded'
                          OR assets.checksum <> excluded.checksum THEN 'pending'
                        ELSE assets.status
                    END,
                    downloaded_at = CASE
                        WHEN assets.checksum <> excluded.checksum THEN NULL
                        ELSE assets.downloaded_at
                    END,
                    checksum = excluded.checksum,
                    filename = excluded.filename,
                    created_at = excluded.created_at,
                    added_at = excluded.added_at,
                    size_bytes = excluded.size_bytes,
                    media_type = excluded.media_type,
                    last_seen_at = excluded.last_seen_at,
                    source = COALESCE(excluded.source, assets.source),
                    is_favorite = excluded.is_favorite,
                    rating = excluded.rating,
                    latitude = excluded.latitude,
                    longitude = excluded.longitude,
                    altitude = excluded.altitude,
                    orientation = excluded.orientation,
                    duration_secs = excluded.duration_secs,
                    timezone_offset = excluded.timezone_offset,
                    width = excluded.width,
                    height = excluded.height,
                    title = excluded.title,
                    keywords = excluded.keywords,
                    description = excluded.description,
                    media_subtype = excluded.media_subtype,
                    burst_id = excluded.burst_id,
                    is_hidden = excluded.is_hidden,
                    is_archived = excluded.is_archived,
                    modified_at = excluded.modified_at,
                    is_deleted = excluded.is_deleted,
                    deleted_at = excluded.deleted_at,
                    provider_data = excluded.provider_data,
                    metadata_hash = excluded.metadata_hash,
                    capture_repair_metadata_hash =
                        CASE
                            WHEN assets.checksum <> excluded.checksum THEN NULL
                            WHEN assets.capture_repair_metadata_hash IS NULL
                                 AND assets.capture_repair_output_checksum IS NULL
                                 AND assets.capture_repair_output_size IS NULL
                                THEN NULL
                            WHEN assets.capture_repair_metadata_hash <> ''
                                 AND assets.capture_repair_output_checksum IS NULL
                                 AND assets.capture_repair_output_size IS NULL
                                THEN excluded.metadata_hash
                            WHEN assets.capture_repair_metadata_hash <> ''
                                 AND assets.capture_repair_output_checksum <> ''
                                 AND assets.capture_repair_output_size >= 0
                                THEN excluded.metadata_hash
                            ELSE assets.capture_repair_metadata_hash
                        END,
                    capture_repair_output_checksum =
                        CASE
                            WHEN assets.checksum <> excluded.checksum THEN NULL
                            WHEN assets.capture_repair_metadata_hash IS NULL
                                 AND assets.capture_repair_output_checksum IS NULL
                                 AND assets.capture_repair_output_size IS NULL
                                THEN NULL
                            WHEN assets.capture_repair_metadata_hash <> ''
                                 AND assets.capture_repair_output_checksum IS NULL
                                 AND assets.capture_repair_output_size IS NULL
                                THEN NULL
                            WHEN assets.capture_repair_metadata_hash <> ''
                                 AND assets.capture_repair_output_checksum <> ''
                                 AND assets.capture_repair_output_size >= 0
                                THEN CASE
                                    WHEN assets.capture_repair_metadata_hash IS excluded.metadata_hash
                                        THEN assets.capture_repair_output_checksum
                                    ELSE NULL
                                END
                            ELSE assets.capture_repair_output_checksum
                        END,
                    capture_repair_output_size =
                        CASE
                            WHEN assets.checksum <> excluded.checksum THEN NULL
                            WHEN assets.capture_repair_metadata_hash IS NULL
                                 AND assets.capture_repair_output_checksum IS NULL
                                 AND assets.capture_repair_output_size IS NULL
                                THEN NULL
                            WHEN assets.capture_repair_metadata_hash <> ''
                                 AND assets.capture_repair_output_checksum IS NULL
                                 AND assets.capture_repair_output_size IS NULL
                                THEN NULL
                            WHEN assets.capture_repair_metadata_hash <> ''
                                 AND assets.capture_repair_output_checksum <> ''
                                 AND assets.capture_repair_output_size >= 0
                                THEN CASE
                                    WHEN assets.capture_repair_metadata_hash IS excluded.metadata_hash
                                        THEN assets.capture_repair_output_size
                                    ELSE NULL
                                END
                            ELSE assets.capture_repair_output_size
                        END
                WHERE NOT (
                    assets.capture_repair_metadata_hash IS NOT NULL
                    AND assets.capture_repair_metadata_hash <> ''
                    AND assets.capture_repair_output_checksum IS NOT NULL
                    AND assets.capture_repair_output_checksum <> ''
                    AND assets.capture_repair_output_size IS NOT NULL
                    AND assets.capture_repair_output_size >= 0
                    AND assets.checksum = excluded.checksum
                    AND (assets.created_at IS NOT excluded.created_at
                         OR assets.capture_repair_metadata_hash IS NOT excluded.metadata_hash)
                )
                ",
        )
        .map_err(|e| StateError::query("upsert_seen::prepare", e))?;
    let changed = stmt
        .execute(rusqlite::params![
            &record.library,
            &record.id,
            record.version_size.as_str(),
            &record.checksum,
            &record.filename,
            encode_asset_date(record.created_at),
            record.added_at.map(encode_asset_date),
            i64::try_from(record.size_bytes).unwrap_or(i64::MAX),
            record.media_type.as_str(),
            last_seen_at,
            meta.source.as_deref().unwrap_or(DEFAULT_SOURCE),
            i64::from(meta.is_favorite),
            meta.rating.map(i64::from),
            meta.latitude,
            meta.longitude,
            meta.altitude,
            meta.orientation.map(i64::from),
            meta.duration_secs,
            meta.timezone_offset.map(i64::from),
            meta.width.map(i64::from),
            meta.height.map(i64::from),
            meta.title.as_deref(),
            meta.keywords.as_deref(),
            meta.description.as_deref(),
            meta.media_subtype.as_deref(),
            meta.burst_id.as_deref(),
            i64::from(meta.is_hidden),
            i64::from(meta.is_archived),
            meta.modified_at.map(|dt| dt.timestamp()),
            i64::from(meta.is_deleted),
            meta.deleted_at.map(|dt| dt.timestamp()),
            meta.provider_data.as_deref(),
            metadata_hash,
        ])
        .map_err(|e| StateError::query("upsert_seen", e))?;
    if changed == 0 {
        return Err(StateError::Invariant {
            operation: "upsert_seen",
            detail:
                "provider metadata changed while capture-repair publication awaits finalization"
                    .into(),
        });
    }

    Ok(())
}

pub(super) fn record_metadata_capture_revision(
    conn: &Connection,
    library: &str,
    asset_id: &str,
    revision: i64,
    updated_at: i64,
) -> Result<(), StateError> {
    let changed = conn
        .execute(
            "INSERT INTO asset_metadata_capture_revisions \
                (library, asset_id, revision, updated_at) \
             VALUES (?1, ?2, ?3, ?4) \
             ON CONFLICT(library, asset_id) DO UPDATE SET \
                revision = excluded.revision, updated_at = excluded.updated_at \
             WHERE asset_metadata_capture_revisions.revision < excluded.revision",
            rusqlite::params![library, asset_id, revision, updated_at],
        )
        .map_err(|e| StateError::query("record_metadata_capture_revision", e))?;
    if changed > 0 {
        conn.execute(
            "UPDATE metadata_capture_state SET \
                processed_assets = processed_assets + 1, updated_at = ?1 \
             WHERE library = ?2 AND pending_revision = ?3",
            rusqlite::params![updated_at, library, revision],
        )
        .map_err(|e| StateError::query("record_metadata_capture_revision::progress", e))?;
    }
    Ok(())
}

#[derive(Clone, Copy)]
pub(super) enum MetadataPathWrite {
    Published,
    Rewritten,
}

/// Checksum roles supplied by one file-finalization route.
pub(super) struct DownloadChecksums<'a> {
    pub(super) local: &'a str,
    pub(super) downloaded: Option<&'a str>,
    pub(super) source: Option<&'a str>,
}

/// Copy the catalogue path's exact publication evidence in the same transaction.
/// Other paths retain their own fingerprints and independent retry receipts.
pub(super) fn record_metadata_path(
    conn: &Connection,
    library: &str,
    id: &str,
    version_size: &str,
    write: MetadataPathWrite,
) -> Result<(), StateError> {
    let preserve_debt = i64::from(matches!(write, MetadataPathWrite::Published));
    conn.execute(
        "INSERT INTO asset_metadata_paths \
            (library, id, version_size, local_path, provider_checksum, local_checksum, \
             download_checksum, metadata_write_failed_at, capture_repair_metadata_hash, \
             capture_repair_output_checksum, capture_repair_output_size) \
         SELECT library, id, version_size, local_path, checksum, local_checksum, \
                download_checksum, metadata_write_failed_at, capture_repair_metadata_hash, \
                capture_repair_output_checksum, capture_repair_output_size \
         FROM assets WHERE library = ?1 AND id = ?2 AND version_size = ?3 \
             AND status = 'downloaded' AND local_path IS NOT NULL \
         ON CONFLICT(library, id, version_size, local_path) DO UPDATE SET \
             provider_checksum = excluded.provider_checksum, \
             local_checksum = excluded.local_checksum, \
             download_checksum = excluded.download_checksum, \
             source_checksum = CASE WHEN asset_metadata_paths.provider_checksum = excluded.provider_checksum \
                 THEN asset_metadata_paths.source_checksum ELSE NULL END, \
             metadata_write_failed_at = CASE WHEN ?4 = 1 THEN \
                 COALESCE(excluded.metadata_write_failed_at, asset_metadata_paths.metadata_write_failed_at) \
                 ELSE excluded.metadata_write_failed_at END, \
             capture_repair_metadata_hash = CASE WHEN ?4 = 1 \
                 AND asset_metadata_paths.provider_checksum = excluded.provider_checksum \
                 AND asset_metadata_paths.local_checksum IS excluded.local_checksum \
                 THEN COALESCE(excluded.capture_repair_metadata_hash, asset_metadata_paths.capture_repair_metadata_hash) \
                 ELSE excluded.capture_repair_metadata_hash END, \
             capture_repair_output_checksum = CASE WHEN ?4 = 1 \
                 AND asset_metadata_paths.provider_checksum = excluded.provider_checksum \
                 AND asset_metadata_paths.local_checksum IS excluded.local_checksum \
                 THEN COALESCE(excluded.capture_repair_output_checksum, asset_metadata_paths.capture_repair_output_checksum) \
                 ELSE excluded.capture_repair_output_checksum END, \
             capture_repair_output_size = CASE WHEN ?4 = 1 \
                 AND asset_metadata_paths.provider_checksum = excluded.provider_checksum \
                 AND asset_metadata_paths.local_checksum IS excluded.local_checksum \
                 THEN COALESCE(excluded.capture_repair_output_size, asset_metadata_paths.capture_repair_output_size) \
                 ELSE excluded.capture_repair_output_size END",
        rusqlite::params![library, id, version_size, preserve_debt],
    )
    .map_err(|e| StateError::query("record_metadata_path", e))?;
    if matches!(write, MetadataPathWrite::Published) {
        // A previously additional path can become the catalogue path again.
        // Keep its own preserved debt visible in the catalogue projection.
        conn.execute(
            "UPDATE assets SET (metadata_write_failed_at, capture_repair_metadata_hash, \
                 capture_repair_output_checksum, capture_repair_output_size) = ( \
                 SELECT p.metadata_write_failed_at, p.capture_repair_metadata_hash, \
                        p.capture_repair_output_checksum, p.capture_repair_output_size \
                 FROM asset_metadata_paths p WHERE p.library = assets.library AND p.id = assets.id \
                   AND p.version_size = assets.version_size AND p.local_path = assets.local_path) \
             WHERE library = ?1 AND id = ?2 AND version_size = ?3 AND EXISTS ( \
                 SELECT 1 FROM asset_metadata_paths p WHERE p.library = assets.library AND p.id = assets.id \
                   AND p.version_size = assets.version_size AND p.local_path = assets.local_path \
                   AND (p.metadata_write_failed_at IS NOT assets.metadata_write_failed_at \
                     OR p.capture_repair_metadata_hash IS NOT assets.capture_repair_metadata_hash \
                     OR p.capture_repair_output_checksum IS NOT assets.capture_repair_output_checksum \
                     OR p.capture_repair_output_size IS NOT assets.capture_repair_output_size))",
            rusqlite::params![library, id, version_size],
        ).map_err(|e| StateError::query("record_metadata_path::restore_debt", e))?;
    }
    Ok(())
}

/// Execute the `mark_downloaded` UPDATE on `conn`. Returns rows affected;
/// callers decide what zero rows means in their context.
#[expect(
    clippy::too_many_arguments,
    reason = "the state owner receives the exact downloaded-file fields and explicit repair intent for one atomic SQL update"
)]
pub(super) fn update_status_to_downloaded(
    conn: &Connection,
    library: &str,
    id: &str,
    version_size: &str,
    local_path: &Path,
    local_checksum: &str,
    download_checksum: Option<&str>,
    mark_capture_repair: bool,
    downloaded_at: i64,
) -> Result<usize, StateError> {
    let mut stmt = conn
        .prepare_cached(
            "UPDATE assets SET status = 'downloaded', downloaded_at = ?1, local_path = ?2, \
             local_checksum = ?3, download_checksum = COALESCE(?4, download_checksum), last_error = NULL, \
             capture_repair_metadata_hash = CASE \
                 WHEN ?5 = 1 THEN metadata_hash \
                 WHEN local_checksum IS ?3 AND local_path IS ?2 THEN capture_repair_metadata_hash ELSE NULL END, \
             capture_repair_output_checksum = CASE \
                 WHEN ?5 = 1 THEN NULL \
                 WHEN local_checksum IS ?3 AND local_path IS ?2 THEN capture_repair_output_checksum ELSE NULL END, \
             capture_repair_output_size = CASE \
                 WHEN ?5 = 1 THEN NULL \
                 WHEN local_checksum IS ?3 AND local_path IS ?2 THEN capture_repair_output_size ELSE NULL END \
             WHERE library = ?6 AND id = ?7 AND version_size = ?8",
        )
        .map_err(|e| StateError::query("mark_downloaded::prepare", e))?;
    let updated = stmt
        .execute(rusqlite::params![
            downloaded_at,
            local_path.to_string_lossy(),
            local_checksum,
            download_checksum,
            i64::from(mark_capture_repair),
            library,
            id,
            version_size
        ])
        .map_err(|e| StateError::query("mark_downloaded", e))?;
    record_metadata_path(
        conn,
        library,
        id,
        version_size,
        MetadataPathWrite::Published,
    )?;
    Ok(updated)
}

pub(super) fn ensure_asset_has_no_prepared_capture_repair(
    conn: &Connection,
    library: &str,
    asset_id: &str,
    operation: &'static str,
) -> Result<(), StateError> {
    let source = metadata_rewrite_source_sql();
    let prepared: i64 = conn
        .query_row(
            &format!(
                "SELECT EXISTS( \
                SELECT 1 FROM ({source}) \
                WHERE library = ?1 AND id = ?2 \
                  AND capture_repair_metadata_hash IS NOT NULL \
                  AND capture_repair_metadata_hash <> '' \
                  AND capture_repair_output_checksum IS NOT NULL \
                  AND capture_repair_output_checksum <> '' \
                  AND capture_repair_output_size IS NOT NULL \
                  AND capture_repair_output_size >= 0 \
            )"
            ),
            rusqlite::params![library, asset_id],
            |row| row.get(0),
        )
        .map_err(|error| StateError::query(operation, error))?;
    if prepared != 0 {
        return Err(StateError::Invariant {
            operation,
            detail: "source deletion cannot hide a prepared capture-repair receipt".into(),
        });
    }
    Ok(())
}

pub(super) fn ensure_master_family_has_no_prepared_capture_repair(
    conn: &Connection,
    library: &str,
    master_record_name: &str,
    operation: &'static str,
) -> Result<(), StateError> {
    let source = metadata_rewrite_source_sql();
    let prepared: i64 = conn
        .query_row(
            &format!(
                "SELECT EXISTS( \
                SELECT 1 FROM ({source}) \
                WHERE library = ?1 \
                  AND (id = ?2 OR id IN ( \
                      SELECT asset_record_name FROM asset_master_mappings \
                      WHERE library = ?1 AND master_record_name = ?2 \
                  )) \
                  AND capture_repair_metadata_hash IS NOT NULL \
                  AND capture_repair_metadata_hash <> '' \
                  AND capture_repair_output_checksum IS NOT NULL \
                  AND capture_repair_output_checksum <> '' \
                  AND capture_repair_output_size IS NOT NULL \
                  AND capture_repair_output_size >= 0 \
            )"
            ),
            rusqlite::params![library, master_record_name],
            |row| row.get(0),
        )
        .map_err(|error| StateError::query(operation, error))?;
    if prepared != 0 {
        return Err(StateError::Invariant {
            operation,
            detail: "source deletion cannot hide a prepared capture-repair receipt".into(),
        });
    }
    Ok(())
}

pub(super) fn metadata_rewrite_source_sql() -> String {
    let path_columns = ASSET_COLUMNS
        .split(',')
        .map(str::trim)
        .map(|column| {
            let owner = match column {
                "local_path" | "local_checksum" | "download_checksum" => "p",
                _ => "a",
            };
            format!("{owner}.{column}")
        })
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "SELECT {ASSET_COLUMNS}, metadata_write_failed_at, capture_repair_metadata_hash, \
             capture_repair_output_checksum, capture_repair_output_size, \
             (SELECT p.source_checksum FROM asset_metadata_paths p \
              WHERE p.library = assets.library AND p.id = assets.id \
                AND p.version_size = assets.version_size AND p.local_path = assets.local_path \
                AND p.provider_checksum = assets.checksum) AS source_checksum FROM assets \
         UNION ALL SELECT {path_columns}, p.metadata_write_failed_at, p.capture_repair_metadata_hash, \
             p.capture_repair_output_checksum, p.capture_repair_output_size, p.source_checksum \
         FROM asset_metadata_paths p JOIN assets a \
           ON a.library = p.library AND a.id = p.id AND a.version_size = p.version_size \
         WHERE p.local_path IS NOT a.local_path AND p.provider_checksum = a.checksum"
    )
}
