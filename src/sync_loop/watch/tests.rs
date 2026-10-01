use crate::sync_loop::watch::{
    METADATA_CAPTURE_FOLLOW_UP_INTERVAL_SECS, metadata_capture_watch_interval,
    refresh_metadata_forces_one_shot, repair_truncated_forces_one_shot,
    validate_capture_timestamp_repair,
};
use crate::sync_loop::{SERVICE_MODE_DEFAULT_WATCH_INTERVAL, service_mode_default_interval};
use crate::{config, download};

#[test]
fn service_mode_default_locked_at_one_day() {
    // The roadmap promises the kei daemon polls every 24h out of the
    // box. Regression-guard the constant so a casual rename doesn't
    // silently shorten the interval and 10x the API call rate.
    assert_eq!(SERVICE_MODE_DEFAULT_WATCH_INTERVAL, 86_400);
}

#[test]
fn service_mode_default_applied_when_no_other_source_set_interval() {
    assert_eq!(
        service_mode_default_interval(None, true),
        Some(SERVICE_MODE_DEFAULT_WATCH_INTERVAL)
    );
}

#[test]
fn service_mode_default_skipped_when_cli_or_toml_already_set_interval() {
    // A user-provided interval (CLI / TOML / env) must always win;
    // service mode never silently overrides an explicit choice.
    assert_eq!(service_mode_default_interval(Some(60), true), None);
    assert_eq!(service_mode_default_interval(Some(3600), true), None);
}

#[test]
fn service_mode_default_skipped_outside_service_mode() {
    // Plain `kei sync` must remain single-shot when no interval is
    // configured. Only the service entry point applies the fallback.
    assert_eq!(service_mode_default_interval(None, false), None);
}

#[test]
fn progressing_metadata_capture_shortens_the_watch_interval() {
    let default_interval = Some(SERVICE_MODE_DEFAULT_WATCH_INTERVAL);
    let stats = download::SyncStats {
        metadata_capture_refreshed: 500,
        metadata_capture_remaining: 701,
        metadata_capture_progressed: true,
        ..download::SyncStats::default()
    };

    assert_eq!(
        metadata_capture_watch_interval(default_interval, &stats, 0),
        Some(METADATA_CAPTURE_FOLLOW_UP_INTERVAL_SECS)
    );
    assert_eq!(
        metadata_capture_watch_interval(Some(60), &stats, 0),
        Some(60)
    );
    assert_eq!(metadata_capture_watch_interval(None, &stats, 0), None);

    let absorbed_rate_limit = download::SyncStats {
        assets_seen: 100,
        rate_limited: 9,
        ..stats
    };
    assert_eq!(
        metadata_capture_watch_interval(default_interval, &absorbed_rate_limit, 0),
        Some(METADATA_CAPTURE_FOLLOW_UP_INTERVAL_SECS)
    );

    let deleted_batch = download::SyncStats {
        metadata_capture_remaining: 1,
        metadata_capture_progressed: true,
        ..download::SyncStats::default()
    };
    assert_eq!(
        metadata_capture_watch_interval(default_interval, &deleted_batch, 0),
        Some(METADATA_CAPTURE_FOLLOW_UP_INTERVAL_SECS)
    );
}

#[test]
fn stalled_or_failed_metadata_capture_keeps_the_configured_interval() {
    for (stats, cycle_failed_count) in [
        (
            download::SyncStats {
                metadata_capture_remaining: 701,
                ..download::SyncStats::default()
            },
            0,
        ),
        (
            download::SyncStats {
                metadata_capture_refreshed: 499,
                metadata_capture_failures: 1,
                metadata_capture_remaining: 701,
                metadata_capture_progressed: true,
                ..download::SyncStats::default()
            },
            1,
        ),
        (
            download::SyncStats {
                metadata_capture_refreshed: 500,
                metadata_capture_progressed: true,
                ..download::SyncStats::default()
            },
            0,
        ),
    ] {
        assert_eq!(
            metadata_capture_watch_interval(
                Some(SERVICE_MODE_DEFAULT_WATCH_INTERVAL),
                &stats,
                cycle_failed_count,
            ),
            Some(SERVICE_MODE_DEFAULT_WATCH_INTERVAL)
        );
    }
}

#[test]
fn rate_limited_metadata_capture_keeps_the_configured_interval() {
    for stats in [
        download::SyncStats {
            rate_limited: 1,
            metadata_capture_remaining: 701,
            metadata_capture_progressed: true,
            ..download::SyncStats::default()
        },
        download::SyncStats {
            assets_seen: 100,
            rate_limited: 10,
            metadata_capture_remaining: 701,
            metadata_capture_progressed: true,
            ..download::SyncStats::default()
        },
    ] {
        assert_eq!(
            metadata_capture_watch_interval(Some(SERVICE_MODE_DEFAULT_WATCH_INTERVAL), &stats, 0,),
            Some(SERVICE_MODE_DEFAULT_WATCH_INTERVAL)
        );
    }
}

#[test]
fn refresh_metadata_off_never_forces_one_shot() {
    // Not requested: no gating even under service mode or with filters.
    assert!(!refresh_metadata_forces_one_shot(false, true, true).unwrap());
}

#[test]
fn refresh_metadata_forces_one_shot_for_plain_sync() {
    assert!(refresh_metadata_forces_one_shot(true, false, false).unwrap());
}

#[test]
fn refresh_metadata_rejected_under_service_run() {
    let err = refresh_metadata_forces_one_shot(true, true, false).unwrap_err();
    assert!(err.to_string().contains("service run"), "{err}");
}

#[test]
fn refresh_metadata_rejected_with_narrowing_filter() {
    let err = refresh_metadata_forces_one_shot(true, false, true).unwrap_err();
    assert!(err.to_string().contains("filters"), "{err}");
}

#[test]
fn capture_timestamp_repair_requires_embedded_datetime_output() {
    let repair = download::CaptureTimestampRepair::ReplaceWithCaptureLocal;
    let err =
        validate_capture_timestamp_repair(repair, &config::MetadataConfig::default()).unwrap_err();
    assert!(err.to_string().contains("set_exif_datetime"), "{err}");

    let metadata = config::MetadataConfig {
        set_exif_datetime: true,
        ..config::MetadataConfig::default()
    };
    assert_eq!(
        validate_capture_timestamp_repair(repair, &metadata).unwrap(),
        repair
    );
}

#[test]
fn repair_truncated_off_never_forces_one_shot() {
    assert!(!repair_truncated_forces_one_shot(false, true).unwrap());
}

#[test]
fn repair_truncated_forces_one_shot_for_plain_sync() {
    assert!(repair_truncated_forces_one_shot(true, false).unwrap());
}

#[test]
fn repair_truncated_rejected_under_service_run() {
    let err = repair_truncated_forces_one_shot(true, true).unwrap_err();
    assert!(err.to_string().contains("service run"), "{err}");
}
