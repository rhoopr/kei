//! Tests moved from `state::db::tests`, with their original names and assertions.
use std::path::Path;
use std::sync::Arc;

use chrono::{DateTime, Utc};

use crate::state::db::SqliteStateDb;
use crate::state::db::rows::{
    ASSET_COLUMN_COUNT, ASSET_COLUMNS, decode_asset_date, encode_asset_date,
};
use crate::state::types::{AssetRecord, MediaType, VersionSizeKey};

#[test]
fn asset_date_codec_preserves_milliseconds_and_rejects_invalid_values() {
    for seconds in [
        DateTime::<Utc>::MIN_UTC.timestamp(),
        -1,
        0,
        1_700_000_000,
        DateTime::<Utc>::MAX_UTC.timestamp(),
    ] {
        for millis in 0..1000 {
            let date = DateTime::from_timestamp(seconds, millis * 1_000_000).unwrap();
            assert_eq!(decode_asset_date(encode_asset_date(date), 4).unwrap(), date);
        }
    }
    for (value, expected_millis) in [(-0.001, -1), (-1.999, -1999), (0.9999999, 1000)] {
        assert_eq!(
            decode_asset_date(value, 4).unwrap().timestamp_millis(),
            expected_millis
        );
    }
    for invalid in [
        f64::NAN,
        f64::INFINITY,
        f64::NEG_INFINITY,
        9.223_372_036_854_776e18,
        -9.223_372_036_854_776e18,
    ] {
        assert!(decode_asset_date(invalid, 4).is_err(), "{invalid}");
    }
}

#[tokio::test]
async fn row_to_asset_record_unknown_status_falls_back_to_pending() {
    // Arrange: manually insert a row with a status string that doesn't match any AssetStatus variant
    let db = SqliteStateDb::open_in_memory().unwrap();
    {
        let conn = db.conn.lock().unwrap();
        let now = Utc::now().timestamp();
        conn.execute(
            "INSERT INTO assets (library, id, version_size, checksum, filename, created_at, size_bytes, media_type, status, last_seen_at)
                 VALUES ('PrimarySync', 'ABx7kQ9nR2', 'original', 'b5bb9d8014a0f9b1d61e21e796d78dccdf1352f23cd32812f4850b878ae4944c', 'IMG_7892.HEIC', ?1, 6_291_456, 'photo', 'corrupted_junk', ?1)",
            rusqlite::params![now],
        ).unwrap();
    }

    // Act: retrieve via get_failed (won't match 'corrupted_junk'), and get_downloaded_page also won't match.
    // Instead, query via should_download which reads the row and parses status.
    // The unknown status falls back to Pending via AssetStatus::from_str -> unwrap_or(Pending).
    let needs_download = db
        .should_download(
            "PrimarySync",
            "ABx7kQ9nR2",
            "original",
            "b5bb9d8014a0f9b1d61e21e796d78dccdf1352f23cd32812f4850b878ae4944c",
            Path::new("/photos/2026/04/IMG_7892.HEIC"),
        )
        .await
        .unwrap();

    // Assert: unknown status treated as pending, which means should download
    assert!(needs_download);

    // Also verify via summary: the unknown status won't match 'downloaded', 'pending', or 'failed'
    // COUNT(CASE WHEN ...) so it counts as part of total but not any specific bucket
    let summary = db.get_summary().await.unwrap();
    assert_eq!(summary.total_assets, 1);
    assert_eq!(summary.downloaded, 0);
    assert_eq!(summary.pending, 0);
    assert_eq!(summary.failed, 0);
}

#[test]
fn asset_column_count_matches_projection() {
    let counted = ASSET_COLUMNS.split(',').count();
    assert_eq!(
        counted, ASSET_COLUMN_COUNT,
        "ASSET_COLUMN_COUNT out of sync with ASSET_COLUMNS"
    );
}

/// End-to-end: PhotoAsset constructed from a realistic CloudKit JSON pair
/// flows through metadata extraction, `upsert_seen`, and round-trips out
/// of the DB with all fields intact. Guards against regressions in any
/// link of the pipeline.
#[tokio::test]
async fn photo_asset_metadata_roundtrips_through_upsert_and_read() {
    use crate::icloud::photos::PhotoAsset;
    use base64::Engine;
    use serde_json::json;

    fn b64(bytes: &[u8]) -> String {
        base64::engine::general_purpose::STANDARD.encode(bytes)
    }
    fn bplist(value: plist::Value) -> Vec<u8> {
        let mut out = Vec::new();
        plist::to_writer_binary(&mut out, &value).unwrap();
        out
    }

    let mut loc_dict = plist::Dictionary::new();
    loc_dict.insert("lat".into(), plist::Value::Real(37.7749));
    loc_dict.insert("lon".into(), plist::Value::Real(-122.4194));
    loc_dict.insert("alt".into(), plist::Value::Real(17.0));
    let loc_bp = bplist(plist::Value::Dictionary(loc_dict));

    let keywords_bp = bplist(plist::Value::Array(vec![
        plist::Value::String("vacation".into()),
        plist::Value::String("beach".into()),
    ]));

    let master = json!({
        "recordName": "RT_1",
        "fields": {
            "itemType": {"value": "public.jpeg"},
            "filenameEnc": {"value": "img.jpg", "type": "STRING"},
            "resOriginalRes": {"value": {"size": 10, "downloadURL": "https://p01.icloud-content.com/x", "fileChecksum": "ck"}},
            "resOriginalFileType": {"value": "public.jpeg"},
            "resOriginalWidth": {"value": 4032},
            "resOriginalHeight": {"value": 3024},
        },
    });
    let asset_json = json!({
        "fields": {
            "assetDate": {"value": 1736899200123_f64},
            "addedDate": {"value": 1736985600789_f64},
            "isFavorite": {"value": 1},
            "orientation": {"value": 6},
            "duration": {"value": 12.5},
            "timeZoneOffset": {"value": -28800},
            "captionEnc": {"value": "Beach day", "type": "STRING"},
            "extendedDescEnc": {"value": "Long description", "type": "STRING"},
            "keywordsEnc": {"value": b64(&keywords_bp), "type": "ENCRYPTED_BYTES"},
            "locationEnc": {"value": b64(&loc_bp), "type": "ENCRYPTED_BYTES"},
            "assetSubtypeV2": {"value": 16},
            "burstId": {"value": "burst_x"},
            "recordChangeTag": {"value": "tag42"},
        },
    });
    let photo = PhotoAsset::new(master, asset_json);

    let db = SqliteStateDb::open_in_memory().unwrap();
    let record = AssetRecord::new_pending(
        Arc::from("PrimarySync"),
        photo.id().to_string(),
        VersionSizeKey::Original,
        "ck".to_string(),
        photo.filename().unwrap_or("").to_string(),
        photo.created(),
        Some(photo.added_date()),
        10,
        MediaType::Photo,
    )
    .with_metadata_arc(photo.metadata_arc(VersionSizeKey::Original));
    db.upsert_seen(&record).await.unwrap();

    let pending = db.get_pending().await.unwrap();
    assert_eq!(pending.len(), 1);
    let got = &pending[0];
    let created = DateTime::from_timestamp_millis(1_736_899_200_123).unwrap();
    let added = Some(DateTime::from_timestamp_millis(1_736_985_600_789).unwrap());
    assert_eq!((got.created_at, got.added_at), (created, added));
    let manifest = db.get_manifest_assets().await.unwrap();
    assert_eq!(
        (manifest[0].created_at, manifest[0].added_at),
        (created, added)
    );
    let m = &got.metadata;
    assert_eq!(m.source.as_deref(), Some("icloud"));
    assert!(m.is_favorite);
    assert_eq!(m.rating, Some(5));
    assert_eq!(m.latitude, Some(37.7749));
    assert_eq!(m.longitude, Some(-122.4194));
    assert_eq!(m.altitude, Some(17.0));
    assert_eq!(m.orientation, Some(6));
    assert_eq!(m.duration_secs, Some(12.5));
    assert_eq!(m.timezone_offset, Some(-28800));
    assert_eq!(m.width, Some(4032));
    assert_eq!(m.height, Some(3024));
    assert_eq!(m.title.as_deref(), Some("Beach day"));
    assert_eq!(m.description.as_deref(), Some("Long description"));
    assert_eq!(m.keywords.as_deref(), Some(r#"["vacation","beach"]"#));
    assert_eq!(m.media_subtype.as_deref(), Some("portrait"));
    assert_eq!(m.burst_id.as_deref(), Some("burst_x"));
    assert!(m.metadata_hash.is_some());

    let provider = m
        .provider_data
        .as_deref()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok())
        .expect("provider_data should be valid JSON");
    assert_eq!(provider["recordChangeTag"], json!("tag42"));
    assert_eq!(provider["assetSubtypeV2"], json!(16));

    let legacy_created = DateTime::from_timestamp(1_700_000_000, 0).unwrap();
    for (literal, added) in [
        ("1700000001", DateTime::from_timestamp(1_700_000_001, 0)),
        ("NULL", None),
    ] {
        db.acquire_lock("legacy dates")
            .unwrap()
            .execute_batch(&format!(
                "UPDATE assets SET created_at = 1700000000, added_at = {literal}"
            ))
            .unwrap();
        let row = db.get_pending().await.unwrap().pop().unwrap();
        let manifest = db.get_manifest_assets().await.unwrap().pop().unwrap();
        for dates in [
            (row.created_at, row.added_at),
            (manifest.created_at, manifest.added_at),
        ] {
            assert_eq!(dates, (legacy_created, added));
        }
    }
    for column in ["created_at", "added_at"] {
        db.acquire_lock("invalid dates")
            .unwrap()
            .execute_batch(&format!(
                "UPDATE assets SET created_at = 0, added_at = NULL;
                 UPDATE assets SET {column} = 'invalid'"
            ))
            .unwrap();
        assert!(db.get_pending().await.is_err(), "{column}");
        assert!(db.get_manifest_assets().await.is_err(), "{column}");
    }
}
