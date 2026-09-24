use super::{
    CaptureTimestampRepair, MetadataFlags, plan_metadata_write, plan_metadata_write_with_repair,
};
use crate::download::filter::MetadataPayload;
use crate::download::metadata_rewrite::test_support::now_local;
#[cfg(feature = "xmp")]
use crate::download::metadata_rewrite::test_support::rich_payload;

#[cfg(feature = "xmp")]
#[test]
fn plan_metadata_write_gates_xmp_fields_on_embed_xmp() {
    let payload = rich_payload();
    let flags_no_embed = MetadataFlags::default();
    let w = plan_metadata_write(
        flags_no_embed,
        &payload,
        &now_local(),
        &crate::download::metadata::ExifProbe::default(),
    );
    assert!(
        w.title.is_none(),
        "title must not write when embed_xmp is off"
    );
    assert!(w.keywords.is_empty());
    assert!(w.people.is_empty());
    assert!(!w.is_hidden);
    assert!(w.offset_time_original.is_none());

    let flags_embed = MetadataFlags::DATETIME | MetadataFlags::EMBED_XMP;
    let w = plan_metadata_write(
        flags_embed,
        &payload,
        &now_local(),
        &crate::download::metadata::ExifProbe::default(),
    );
    assert_eq!(w.title.as_deref(), Some("T"));
    assert_eq!(w.keywords, vec!["vacation", "beach"]);
    assert_eq!(w.people, vec!["Alice"]);
    assert!(w.is_hidden);
    assert!(w.is_archived);
    assert_eq!(w.media_subtype.as_deref(), Some("portrait"));
    assert_eq!(w.burst_id.as_deref(), Some("b1"));
    assert_eq!(w.offset_time_original.as_deref(), Some("+11:00"));
}

#[test]
fn plan_metadata_write_respects_probe_skip_for_datetime_and_gps() {
    let payload = MetadataPayload {
        timezone_offset: Some(39_600),
        latitude: Some(37.7),
        longitude: Some(-122.4),
        ..MetadataPayload::default()
    };
    let flags = MetadataFlags::DATETIME | MetadataFlags::GPS;
    let created_local = now_local();
    let capture_local = created_local.format("%Y:%m:%d %H:%M:%S").to_string();

    let matching_clock = crate::download::metadata::ExifProbe {
        datetime_original: Some(capture_local),
        offset_time_original: None,
        has_other_datetime_offset: false,
        has_gps: true,
        ..crate::download::metadata::ExifProbe::default()
    };
    let write = plan_metadata_write(flags, &payload, &created_local, &matching_clock);
    assert!(
        write.datetime.is_none(),
        "must skip datetime when file already has one"
    );
    assert_eq!(
        write.offset_time_original.as_deref(),
        Some("+11:00"),
        "an offset may join a timestamp already rendering capture-local time"
    );
    assert!(
        write.gps.is_none(),
        "must skip gps when file already has one"
    );

    let existing_offset = crate::download::metadata::ExifProbe {
        offset_time_original: Some("+10:00".into()),
        ..matching_clock
    };
    let write = plan_metadata_write(flags, &payload, &created_local, &existing_offset);
    assert!(write.datetime.is_none());
    assert!(write.offset_time_original.is_none());
    assert!(write.gps.is_none());
}

#[test]
fn plan_metadata_write_replaces_offsets_orphaned_from_datetime_original() {
    let payload = MetadataPayload {
        timezone_offset: Some(39_600),
        ..MetadataPayload::default()
    };
    let flags = MetadataFlags::DATETIME;
    let created_local = now_local();

    for probe in [
        crate::download::metadata::ExifProbe {
            offset_time_original: Some("+10:00".into()),
            ..crate::download::metadata::ExifProbe::default()
        },
        crate::download::metadata::ExifProbe {
            has_other_datetime_offset: true,
            ..crate::download::metadata::ExifProbe::default()
        },
    ] {
        let write = plan_metadata_write(flags, &payload, &created_local, &probe);
        assert!(write.datetime.is_some());
        assert!(write.clear_datetime_offsets);
        assert_eq!(write.offset_time_original.as_deref(), Some("+11:00"));
    }

    let write = plan_metadata_write(
        flags,
        &MetadataPayload::default(),
        &created_local,
        &crate::download::metadata::ExifProbe {
            offset_time_original: Some("+10:00".into()),
            ..crate::download::metadata::ExifProbe::default()
        },
    );
    assert!(write.datetime.is_some());
    assert!(write.clear_datetime_offsets);
    assert!(write.offset_time_original.is_none());
}

/// A file written before capture-local resolution holds a wall clock in the
/// backup host's timezone. Apple's offset does not describe that clock, so
/// pairing the two would publish an instant the asset never had.
#[test]
fn plan_metadata_write_withholds_offset_from_an_unverified_timestamp() {
    let payload = MetadataPayload {
        timezone_offset: Some(39_600),
        ..MetadataPayload::default()
    };
    let flags = MetadataFlags::DATETIME;
    let created_local = now_local() + chrono::Duration::milliseconds(629);
    let host_local = (created_local - chrono::Duration::hours(11))
        .format("%Y:%m:%d %H:%M:%S")
        .to_string();

    for existing in [
        host_local,
        "not a timestamp".to_string(),
        String::new(),
        "2024-06-15".to_string(),
        "2024-06-15T10:00:00.628+11:00".to_string(),
        "2024-06-15T10:00:00.628".to_string(),
        "2024-06-15T10:00:01+11:00".to_string(),
        // Capture-local wall clock, but claiming a different zone.
        "2024-06-15T10:00:00.629+10:00".to_string(),
        created_local.format("%Y-%m-%dT%H:%M:%S+05:00").to_string(),
    ] {
        let probe = crate::download::metadata::ExifProbe {
            datetime_original: Some(existing.clone()),
            #[cfg(feature = "xmp")]
            native_heif_capture_time: crate::download::metadata::HeifNativeCaptureTime::Present {
                datetime_original: Some(existing.clone()),
                offset_time_original: Some("+11:00".into()),
            },
            ..crate::download::metadata::ExifProbe::default()
        };
        let write = plan_metadata_write(flags, &payload, &created_local, &probe);
        assert!(write.datetime.is_none());
        assert!(
            write.offset_time_original.is_none(),
            "offset must not join the unverified timestamp {existing:?}"
        );
        #[cfg(feature = "xmp")]
        assert_eq!(
            probe.native_heif_capture_time_repair_required(&created_local, "+11:00"),
            Ok(Some(true)),
            "{existing}"
        );
    }
}

#[test]
fn capture_timestamp_repair_requires_an_offset_and_replaces_the_pair() {
    let created_local = now_local() + chrono::Duration::milliseconds(629);
    let probe = crate::download::metadata::ExifProbe {
        datetime_original: Some("2024:06:14 23:00:00".into()),
        offset_time_original: Some("+05:00".into()),
        has_other_datetime_offset: true,
        has_gps: false,
        ..crate::download::metadata::ExifProbe::default()
    };
    let write = plan_metadata_write_with_repair(
        MetadataFlags::DATETIME,
        &MetadataPayload {
            timezone_offset: Some(39_600),
            ..MetadataPayload::default()
        },
        &created_local,
        CaptureTimestampRepair::ReplaceWithCaptureLocal,
        &probe,
    );
    assert_eq!(write.datetime.as_deref(), Some("2024:06:15 10:00:00"));
    assert_eq!(write.offset_time_original.as_deref(), Some("+11:00"));
    assert!(write.clear_datetime_offsets);

    let correct_probe = crate::download::metadata::ExifProbe {
        datetime_original: write.datetime.clone(),
        offset_time_original: write.offset_time_original.clone(),
        has_other_datetime_offset: false,
        has_gps: false,
        ..crate::download::metadata::ExifProbe::default()
    };
    let already_repaired = plan_metadata_write_with_repair(
        MetadataFlags::DATETIME,
        &MetadataPayload {
            timezone_offset: Some(39_600),
            ..MetadataPayload::default()
        },
        &created_local,
        CaptureTimestampRepair::ReplaceWithCaptureLocal,
        &correct_probe,
    );
    assert!(already_repaired.is_empty());

    let write_without_offset = plan_metadata_write_with_repair(
        MetadataFlags::DATETIME,
        &MetadataPayload::default(),
        &created_local,
        CaptureTimestampRepair::ReplaceWithCaptureLocal,
        &probe,
    );
    assert!(write_without_offset.datetime.is_none());
    assert!(write_without_offset.offset_time_original.is_none());
    assert!(!write_without_offset.clear_datetime_offsets);
}

/// XMP stores capture times as ISO 8601, and kei's own writer appends the
/// offset. Such a timestamp is already capture-local, so it still accepts
/// the offset tag.
#[test]
fn plan_metadata_write_accepts_iso_timestamps_from_the_xmp_probe() {
    let payload = MetadataPayload {
        timezone_offset: Some(39_600),
        ..MetadataPayload::default()
    };
    let created_local = now_local() + chrono::Duration::milliseconds(629);

    for existing in [
        created_local.format("%Y-%m-%dT%H:%M:%S").to_string(),
        created_local.format("%Y-%m-%dT%H:%M:%S+11:00").to_string(),
        "2024-06-15T10:00:00.629+11:00".to_string(),
        "2024-06-15T10:00:00.000+11:00".to_string(),
        "2024-06-15T10:00:00.629".to_string(),
        "2024-06-15T10:00:00.000".to_string(),
        format!("  {}\0", created_local.format("%Y:%m:%d %H:%M:%S")),
    ] {
        let probe = crate::download::metadata::ExifProbe {
            datetime_original: Some(existing.clone()),
            #[cfg(feature = "xmp")]
            native_heif_capture_time: crate::download::metadata::HeifNativeCaptureTime::Present {
                datetime_original: Some(existing.clone()),
                offset_time_original: Some("+11:00".into()),
            },
            ..crate::download::metadata::ExifProbe::default()
        };
        let write = plan_metadata_write(MetadataFlags::DATETIME, &payload, &created_local, &probe);
        assert!(write.datetime.is_none());
        assert_eq!(
            write.offset_time_original.as_deref(),
            Some("+11:00"),
            "capture-local timestamp {existing:?} must accept its offset"
        );
        #[cfg(feature = "xmp")]
        assert_eq!(
            probe.native_heif_capture_time_repair_required(&created_local, "+11:00"),
            Ok(Some(false)),
            "{existing}"
        );
        #[cfg(feature = "xmp")]
        if existing == "2024-06-15T10:00:00.629+11:00" {
            assert_eq!(
                probe.native_heif_capture_time_repair_required(&created_local, "+10:00"),
                Ok(Some(true)),
                "the native offset must still match"
            );
        }
    }
}

#[cfg(feature = "xmp")]
#[test]
fn plan_metadata_write_skips_datetime_for_heif_native_exif() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/sample.heic");
    let probe = crate::download::metadata::probe_exif(&path).expect("sample HEIC probe");
    let write = plan_metadata_write(
        MetadataFlags::DATETIME,
        &MetadataPayload::default(),
        &now_local(),
        &probe,
    );

    assert!(
        write.datetime.is_none(),
        "native HEIF DateTimeOriginal must suppress the derived datetime write"
    );
}

#[test]
fn metadata_flags_any_embed_captures_embed_only() {
    let mut flags = MetadataFlags::default();
    assert!(!flags.any_embed());
    flags.insert(MetadataFlags::XMP_SIDECAR);
    assert!(
        !flags.any_embed(),
        "sidecar-only must not trigger the .part-edit flow"
    );
    flags.remove(MetadataFlags::XMP_SIDECAR);
    flags.insert(MetadataFlags::EMBED_XMP);
    assert!(flags.any_embed());
}
