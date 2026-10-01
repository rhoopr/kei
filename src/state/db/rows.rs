//! Shared asset projections, date codecs, row decoding, and bounded query inputs.

use std::path::PathBuf;
use std::sync::Arc;

use chrono::{DateTime, TimeZone, Utc};

use crate::state::types::{AssetMetadata, AssetRecord, AssetStatus, MediaType, VersionSizeKey};

/// Drain a rusqlite row iterator into `Vec<T>`, dropping parse failures but
/// logging each at `debug!` and summarising the drop count at `warn!` so a
/// corrupted row never silently disappears from a bulk loader.
pub(super) fn collect_rows_with_warn<T, I>(rows: I, label: &'static str) -> Vec<T>
where
    I: Iterator<Item = rusqlite::Result<T>>,
{
    let mut out = Vec::new();
    let mut dropped = 0usize;
    for r in rows {
        match r {
            Ok(v) => out.push(v),
            Err(e) => {
                dropped += 1;
                tracing::debug!(error = %e, "{label}: row parse error");
            }
        }
    }
    if dropped > 0 {
        tracing::warn!(dropped, "{label}: dropped rows with parse errors");
    }
    out
}

pub(super) fn unique_sorted_strings(values: &[&str]) -> Vec<String> {
    let mut values: Vec<String> = values.iter().map(|value| (*value).to_owned()).collect();
    values.sort();
    values.dedup();
    values
}

pub(super) fn sqlite_placeholders(len: usize) -> String {
    std::iter::repeat_n("?", len).collect::<Vec<_>>().join(", ")
}

/// Column list for every `SELECT ... FROM assets` that feeds `row_to_asset_record`.
/// Keep this in sync with the indices read in `row_to_asset_record` and the
/// VALUES placeholder count in `upsert_seen`.
pub(super) const ASSET_COLUMNS: &str = "id, version_size, checksum, filename, created_at, \
     added_at, size_bytes, media_type, status, downloaded_at, local_path, \
     last_seen_at, download_attempts, last_error, local_checksum, \
     source, is_favorite, rating, latitude, longitude, altitude, orientation, \
     duration_secs, timezone_offset, width, height, title, keywords, description, \
     media_subtype, burst_id, is_hidden, is_archived, modified_at, is_deleted, \
     deleted_at, provider_data, metadata_hash, library, download_checksum";

/// Total number of columns in `ASSET_COLUMNS`. Validated by a unit test that
/// asserts `row_to_asset_record` reads exactly this many indices (0..N).
pub(super) const ASSET_COLUMN_COUNT: usize = 40;

pub(super) fn ts_to_utc(ts: i64) -> DateTime<Utc> {
    Utc.timestamp_opt(ts, 0)
        .single()
        .unwrap_or(DateTime::UNIX_EPOCH)
}

pub(super) fn optional_ts_to_utc(ts: Option<i64>) -> Option<DateTime<Utc>> {
    ts.and_then(|ts| Utc.timestamp_opt(ts, 0).single())
}

// Chrono's whole seconds fit exactly in f64; splitting before scaling preserves
// every millisecond even at the supported date limits (unlike timestamp_millis).
#[allow(
    clippy::cast_precision_loss,
    reason = "chrono seconds are within f64's exact integer range"
)]
pub(super) fn encode_asset_date(date: DateTime<Utc>) -> f64 {
    date.timestamp() as f64 + f64::from(date.timestamp_subsec_millis()) / 1000.0
}

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    reason = "finite chrono-range seconds and rounded 0..=1000 milliseconds are checked before casting"
)]
pub(super) fn decode_asset_date(value: f64, column: usize) -> rusqlite::Result<DateTime<Utc>> {
    let seconds = value.floor();
    if value.is_finite()
        && seconds >= DateTime::<Utc>::MIN_UTC.timestamp() as f64
        && seconds <= DateTime::<Utc>::MAX_UTC.timestamp() as f64
    {
        let millis = ((value - seconds) * 1000.0).round() as u32;
        if let Some(date) = DateTime::from_timestamp(
            seconds as i64 + i64::from(millis / 1000),
            (millis % 1000) * 1_000_000,
        ) {
            return Ok(date);
        }
    }
    Err(rusqlite::Error::FromSqlConversionFailure(
        column,
        rusqlite::types::Type::Real,
        std::io::Error::new(std::io::ErrorKind::InvalidData, "invalid asset date").into(),
    ))
}

/// Convert a database row to an `AssetRecord`.
///
/// Returns `rusqlite::Error` on column extraction failures instead of silently
/// falling back to defaults, so schema mismatches or corruption are surfaced.
pub(super) fn row_to_asset_record(row: &rusqlite::Row<'_>) -> rusqlite::Result<AssetRecord> {
    let id: String = row.get(0)?;
    let version_size_str: String = row.get(1)?;
    let checksum: String = row.get(2)?;
    let filename: String = row.get(3)?;
    let created_at = decode_asset_date(row.get(4)?, 4)?;
    let added_at = row
        .get::<_, Option<f64>>(5)?
        .map(|value| decode_asset_date(value, 5))
        .transpose()?;
    let size_bytes: i64 = row.get(6)?;
    let media_type_str: String = row.get(7)?;
    let status_str: String = row.get(8)?;
    let downloaded_at_ts: Option<i64> = row.get(9)?;
    let local_path_str: Option<String> = row.get(10)?;
    let last_seen_at_ts: i64 = row.get(11)?;
    let download_attempts: i64 = row.get(12)?;
    let last_error: Option<String> = row.get(13)?;
    let local_checksum: Option<String> = row.get(14)?;

    let source_str: Option<String> = row.get(15)?;
    let source = source_str.map(Arc::<str>::from);
    let is_favorite: i64 = row.get(16)?;
    let rating: Option<i64> = row.get(17)?;
    let latitude: Option<f64> = row.get(18)?;
    let longitude: Option<f64> = row.get(19)?;
    let altitude: Option<f64> = row.get(20)?;
    let orientation: Option<i64> = row.get(21)?;
    let duration_secs: Option<f64> = row.get(22)?;
    let timezone_offset: Option<i64> = row.get(23)?;
    let width: Option<i64> = row.get(24)?;
    let height: Option<i64> = row.get(25)?;
    let title: Option<String> = row.get(26)?;
    let keywords: Option<String> = row.get(27)?;
    let description: Option<String> = row.get(28)?;
    let media_subtype: Option<String> = row.get(29)?;
    let burst_id: Option<String> = row.get(30)?;
    let is_hidden: i64 = row.get(31)?;
    let is_archived: i64 = row.get(32)?;
    let modified_at_ts: Option<i64> = row.get(33)?;
    let is_deleted: i64 = row.get(34)?;
    let deleted_at_ts: Option<i64> = row.get(35)?;
    let provider_data: Option<String> = row.get(36)?;
    let metadata_hash: Option<String> = row.get(37)?;
    let library: String = row.get(38)?;
    let download_checksum: Option<String> = row.get(39)?;

    let metadata = AssetMetadata {
        source,
        is_favorite: is_favorite != 0,
        rating: rating.and_then(|v| u8::try_from(v).ok()),
        latitude,
        longitude,
        altitude,
        orientation: orientation.and_then(|v| u8::try_from(v).ok()),
        duration_secs,
        timezone_offset: timezone_offset.and_then(|v| i32::try_from(v).ok()),
        width: width.and_then(|v| u32::try_from(v).ok()),
        height: height.and_then(|v| u32::try_from(v).ok()),
        title,
        keywords,
        description,
        media_subtype,
        burst_id,
        is_hidden: is_hidden != 0,
        is_archived: is_archived != 0,
        modified_at: modified_at_ts.and_then(|ts| Utc.timestamp_opt(ts, 0).single()),
        is_deleted: is_deleted != 0,
        deleted_at: deleted_at_ts.and_then(|ts| Utc.timestamp_opt(ts, 0).single()),
        provider_data,
        metadata_hash,
    };

    Ok(AssetRecord {
        library: Arc::from(library),
        id: id.into_boxed_str(),
        checksum: checksum.into_boxed_str(),
        filename: filename.into_boxed_str(),
        local_path: local_path_str.map(PathBuf::from),
        last_error,
        local_checksum,
        download_checksum,
        size_bytes: u64::try_from(size_bytes).unwrap_or(0),
        created_at,
        added_at,
        downloaded_at: optional_ts_to_utc(downloaded_at_ts),
        last_seen_at: ts_to_utc(last_seen_at_ts),
        download_attempts: u32::try_from(download_attempts).unwrap_or(u32::MAX),
        version_size: VersionSizeKey::from_str(&version_size_str)
            .unwrap_or(VersionSizeKey::Original),
        media_type: MediaType::from_str(&media_type_str).unwrap_or(MediaType::Photo),
        status: AssetStatus::from_str(&status_str).unwrap_or(AssetStatus::Pending),
        metadata: Arc::new(metadata),
    })
}

#[cfg(test)]
mod tests;
