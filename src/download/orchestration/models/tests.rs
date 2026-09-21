use crate::commands::PassKind;
use crate::download::recap;
use crate::icloud::photos::ProviderRecordId;

use super::{
    DownloadOutcome, FullEnumerationReason, PassKey, SkipBreakdown, SyncResult, SyncStats,
    sync_token_blocked_explanation,
};

// ── determine_media_type tests ──────────────────────────────────────

// ── NameId7 filter tests ────────────────────────────────────────────

// ── keep_unicode_in_filenames tests ─────────────────────────────────

// ── Medium/Thumb size suffix tests ──────────────────────────────────

// ── NormalizedPath direct tests ─────────────────────────────────────

// ---------- SyncMode / SyncResult tests ----------

#[test]
fn test_sync_result_partial_failure() {
    let result = SyncResult {
        outcome: DownloadOutcome::PartialFailure { failed_count: 3 },
        sync_token: Some("tok".to_string()),
        stats: SyncStats::default(),
        full_enumeration_ran: false,
    };
    match result.outcome {
        DownloadOutcome::PartialFailure { failed_count } => {
            assert_eq!(failed_count, 3);
        }
        _ => panic!("Expected PartialFailure"),
    }
}

#[test]
fn test_sync_result_session_expired() {
    let result = SyncResult {
        outcome: DownloadOutcome::SessionExpired {
            auth_error_count: 5,
        },
        sync_token: None,
        stats: SyncStats::default(),
        full_enumeration_ran: false,
    };
    match result.outcome {
        DownloadOutcome::SessionExpired { auth_error_count } => {
            assert_eq!(auth_error_count, 5);
        }
        _ => panic!("Expected SessionExpired"),
    }
}

/// `SyncStats::accumulate` is the sole sum used to fold per-library
/// stats into a cycle-wide total. Pin every counter so a future refactor
/// (or a new field added without updating `accumulate`) cannot silently
/// drop one. Touches every numeric field plus `interrupted` plus the
/// nested `SkipBreakdown`.
///
/// The earlier inline accumulator in `sync_loop::run_cycle` missed
/// `rate_limited` -- this test pins that field too so the bug cannot
/// regress.
#[test]
fn sync_loop_run_cycle_aggregates_stats_across_libraries() {
    let lib_a = SyncStats {
        identity_incomplete: true,
        assets_seen: 10,
        api_total_at_start: Some(12),
        api_total_at_start_partial: false,
        downloaded: 4,
        failed: 1,
        skipped: SkipBreakdown {
            by_state: 2,
            on_disk: 3,
            by_media_type: 4,
            by_date_range: 5,
            by_live_photo: 6,
            by_filename: 7,
            by_excluded_album: 8,
            ampm_variant: 9,
            duplicates: 10,
            retry_exhausted: 11,
            retry_only: 12,
        },
        bytes_downloaded: 1_000,
        disk_bytes_written: 900,
        exif_failures: 1,
        metadata_capture_revision: Some(1),
        metadata_capture_refreshed: 2,
        metadata_capture_failures: 1,
        metadata_capture_remaining: 5,
        metadata_capture_progressed: false,
        state_write_failures: 2,
        enumeration_errors: 3,
        count_probe_failures: 4,
        stale_pending_pruned: 5,
        pagination_shortfall_warnings: 1,
        pagination_shortfall_assets: 9,
        tail_probes: 2,
        count_undercount_assets: 3,
        enumeration_incomplete: false,
        inventory_drop_warnings: 1,
        inventory_drop_assets: 5,
        inventory_drop_percent: Some(5.0),
        inventory_drop_previous_total: Some(100),
        inventory_drop_current_total: Some(95),
        inventory_drop_library: Some("PrimarySync".to_string()),
        sync_token_blocked: true,
        sync_token_blocked_reason: Some("icloud_blank_sync_token"),
        sync_token_blocked_source: Some("icloud"),
        sync_token_blocked_explanation: Some(sync_token_blocked_explanation(
            "icloud_blank_sync_token",
        )),
        sync_token_blocked_zone: Some("PrimarySync".to_string()),
        sync_token_expected_receivers: Some(3),
        sync_token_receivers_with_token: Some(2),
        sync_token_receivers_missing: Some(1),
        sync_token_receivers_blank: Some(0),
        sync_token_receivers_dropped: Some(0),
        sync_token_unique_values: Some(1),
        same_cycle_recovery_attempts: 1,
        same_cycle_recovery_successes: 1,
        checkpoint_retry_passes: vec![PassKey {
            index: 0,
            kind: PassKind::Album,
            label: "album".to_string(),
        }],
        checkpoint_revalidate_records: vec![ProviderRecordId::new("master-a")],
        full_enumeration_reason: Some(FullEnumerationReason::NoStoredToken),
        elapsed_secs: 1.5,
        interrupted: false,
        rate_limited: 7,
        photos_downloaded: 3,
        videos_downloaded: 1,
        recap: recap::RunRecap::default(),
    };

    let lib_b = SyncStats {
        identity_incomplete: false,
        assets_seen: 20,
        api_total_at_start: Some(22),
        api_total_at_start_partial: true,
        downloaded: 11,
        failed: 2,
        skipped: SkipBreakdown {
            by_state: 1,
            on_disk: 1,
            by_media_type: 1,
            by_date_range: 1,
            by_live_photo: 1,
            by_filename: 1,
            by_excluded_album: 1,
            ampm_variant: 1,
            duplicates: 1,
            retry_exhausted: 1,
            retry_only: 1,
        },
        bytes_downloaded: 2_500,
        disk_bytes_written: 2_400,
        exif_failures: 4,
        metadata_capture_revision: Some(1),
        metadata_capture_refreshed: 3,
        metadata_capture_failures: 2,
        metadata_capture_remaining: 7,
        metadata_capture_progressed: true,
        state_write_failures: 5,
        enumeration_errors: 6,
        count_probe_failures: 7,
        stale_pending_pruned: 8,
        pagination_shortfall_warnings: 2,
        pagination_shortfall_assets: 11,
        tail_probes: 4,
        count_undercount_assets: 5,
        enumeration_incomplete: true,
        inventory_drop_warnings: 2,
        inventory_drop_assets: 11,
        inventory_drop_percent: Some(10.0),
        inventory_drop_previous_total: Some(110),
        inventory_drop_current_total: Some(99),
        inventory_drop_library: Some("SharedSync-abc".to_string()),
        sync_token_blocked: false,
        sync_token_blocked_reason: None,
        sync_token_blocked_source: Some("kei"),
        sync_token_blocked_explanation: Some("should not overwrite first"),
        sync_token_blocked_zone: Some("SharedSync-abc".to_string()),
        sync_token_expected_receivers: Some(9),
        sync_token_receivers_with_token: Some(9),
        sync_token_receivers_missing: Some(0),
        sync_token_receivers_blank: Some(0),
        sync_token_receivers_dropped: Some(0),
        sync_token_unique_values: Some(1),
        same_cycle_recovery_attempts: 2,
        same_cycle_recovery_successes: 1,
        checkpoint_retry_passes: vec![PassKey {
            index: 1,
            kind: PassKind::Unfiled,
            label: "unfiled".to_string(),
        }],
        checkpoint_revalidate_records: vec![ProviderRecordId::new("master-b")],
        full_enumeration_reason: Some(FullEnumerationReason::MetadataBackfill),
        elapsed_secs: 0.75,
        interrupted: true,
        rate_limited: 3,
        photos_downloaded: 8,
        videos_downloaded: 3,
        recap: recap::RunRecap::default(),
    };

    let mut acc = SyncStats::default();
    acc.accumulate(&lib_a);
    acc.accumulate(&lib_b);
    assert!(acc.identity_incomplete);

    assert_eq!(acc.assets_seen, 30, "assets_seen must sum");
    assert_eq!(
        acc.api_total_at_start,
        Some(34),
        "api_total_at_start must sum known library totals"
    );
    assert!(
        acc.api_total_at_start_partial,
        "api_total_at_start_partial must OR"
    );
    assert_eq!(acc.downloaded, 15, "downloaded must sum");
    assert_eq!(acc.failed, 3, "failed must sum");
    assert_eq!(acc.bytes_downloaded, 3_500, "bytes_downloaded must sum");
    assert_eq!(acc.disk_bytes_written, 3_300, "disk_bytes_written must sum");
    assert_eq!(acc.exif_failures, 5, "exif_failures must sum");
    assert_eq!(acc.metadata_capture_revision, Some(1));
    assert_eq!(acc.metadata_capture_refreshed, 5);
    assert_eq!(acc.metadata_capture_failures, 3);
    assert_eq!(acc.metadata_capture_remaining, 12);
    assert!(acc.metadata_capture_progressed);
    assert_eq!(acc.state_write_failures, 7, "state_write_failures must sum");
    assert_eq!(acc.enumeration_errors, 9, "enumeration_errors must sum");
    assert_eq!(
        acc.count_probe_failures, 11,
        "count_probe_failures must sum"
    );
    assert_eq!(
        acc.stale_pending_pruned, 13,
        "stale_pending_pruned must sum"
    );
    assert_eq!(
        acc.pagination_shortfall_warnings, 3,
        "pagination shortfall warnings must sum"
    );
    assert_eq!(
        acc.pagination_shortfall_assets, 20,
        "pagination shortfall assets must sum"
    );
    assert_eq!(acc.tail_probes, 6, "tail probes must sum");
    assert_eq!(
        acc.count_undercount_assets, 8,
        "count undercount assets must sum"
    );
    assert!(acc.enumeration_incomplete, "enumeration_incomplete must OR");
    assert_eq!(
        acc.inventory_drop_warnings, 3,
        "inventory drop warnings must sum"
    );
    assert_eq!(
        acc.inventory_drop_assets, 11,
        "largest inventory drop must win"
    );
    assert_eq!(acc.inventory_drop_percent, Some(10.0));
    assert_eq!(acc.inventory_drop_previous_total, Some(110));
    assert_eq!(acc.inventory_drop_current_total, Some(99));
    assert_eq!(
        acc.inventory_drop_library,
        Some("SharedSync-abc".to_string())
    );
    assert!(acc.sync_token_blocked, "sync_token_blocked must OR");
    assert_eq!(
        acc.sync_token_blocked_reason,
        Some("icloud_blank_sync_token")
    );
    assert_eq!(acc.sync_token_blocked_source, Some("icloud"));
    assert_eq!(
        acc.sync_token_blocked_explanation,
        Some(sync_token_blocked_explanation("icloud_blank_sync_token"))
    );
    assert_eq!(acc.sync_token_blocked_zone.as_deref(), Some("PrimarySync"));
    assert_eq!(acc.sync_token_expected_receivers, Some(3));
    assert_eq!(acc.sync_token_receivers_with_token, Some(2));
    assert_eq!(acc.sync_token_receivers_missing, Some(1));
    assert_eq!(acc.sync_token_receivers_blank, Some(0));
    assert_eq!(acc.sync_token_receivers_dropped, Some(0));
    assert_eq!(acc.sync_token_unique_values, Some(1));
    assert_eq!(acc.same_cycle_recovery_attempts, 3);
    assert_eq!(acc.same_cycle_recovery_successes, 2);
    assert_eq!(acc.checkpoint_retry_passes.len(), 2);
    assert_eq!(acc.checkpoint_revalidate_records.len(), 2);
    assert_eq!(
        acc.full_enumeration_reason,
        Some(FullEnumerationReason::NoStoredToken)
    );
    assert!(
        (acc.elapsed_secs - 2.25).abs() < 1e-9,
        "elapsed_secs must sum (got {})",
        acc.elapsed_secs
    );
    assert!(
        acc.interrupted,
        "interrupted must OR -- any library interrupted -> cycle interrupted"
    );
    assert_eq!(
        acc.rate_limited, 10,
        "rate_limited must sum -- pre-fix the inline accumulator dropped this field"
    );

    assert_eq!(acc.skipped.by_state, 3);
    assert_eq!(acc.skipped.on_disk, 4);
    assert_eq!(acc.skipped.by_media_type, 5);
    assert_eq!(acc.skipped.by_date_range, 6);
    assert_eq!(acc.skipped.by_live_photo, 7);
    assert_eq!(acc.skipped.by_filename, 8);
    assert_eq!(acc.skipped.by_excluded_album, 9);
    assert_eq!(acc.skipped.ampm_variant, 10);
    assert_eq!(acc.skipped.duplicates, 11);
    assert_eq!(acc.skipped.retry_exhausted, 12);
    assert_eq!(acc.skipped.retry_only, 13);
    assert_eq!(
        acc.skipped.total(),
        3 + 4 + 5 + 6 + 7 + 8 + 9 + 10 + 11 + 12 + 13,
        "skip total must reflect summed breakdown"
    );
}

/// When multiple libraries block token advancement in one cycle, the
/// aggregated cycle stats preserve the first blocked diagnostic payload.
#[test]
fn sync_stats_accumulate_preserves_first_token_blocked_diagnostics() {
    let first = SyncStats {
        sync_token_blocked: true,
        sync_token_blocked_reason: Some("icloud_blank_sync_token"),
        sync_token_blocked_source: Some("icloud"),
        sync_token_blocked_explanation: Some(sync_token_blocked_explanation(
            "icloud_blank_sync_token",
        )),
        sync_token_blocked_zone: Some("PrimarySync".to_string()),
        sync_token_expected_receivers: Some(2),
        sync_token_receivers_with_token: Some(0),
        sync_token_receivers_missing: Some(0),
        sync_token_receivers_blank: Some(2),
        sync_token_receivers_dropped: Some(0),
        sync_token_unique_values: Some(0),
        ..SyncStats::default()
    };
    let second = SyncStats {
        sync_token_blocked: true,
        sync_token_blocked_reason: Some("icloud_sync_token_mismatch"),
        sync_token_blocked_source: Some("icloud"),
        sync_token_blocked_explanation: Some(sync_token_blocked_explanation(
            "icloud_sync_token_mismatch",
        )),
        sync_token_blocked_zone: Some("SharedSync-XYZ".to_string()),
        sync_token_expected_receivers: Some(3),
        sync_token_receivers_with_token: Some(3),
        sync_token_receivers_missing: Some(0),
        sync_token_receivers_blank: Some(0),
        sync_token_receivers_dropped: Some(0),
        sync_token_unique_values: Some(2),
        ..SyncStats::default()
    };

    let mut acc = SyncStats::default();
    acc.accumulate(&first);
    acc.accumulate(&second);

    assert!(acc.sync_token_blocked);
    assert_eq!(
        acc.sync_token_blocked_reason,
        first.sync_token_blocked_reason
    );
    assert_eq!(
        acc.sync_token_blocked_source,
        first.sync_token_blocked_source
    );
    assert_eq!(
        acc.sync_token_blocked_explanation,
        first.sync_token_blocked_explanation
    );
    assert_eq!(acc.sync_token_blocked_zone, first.sync_token_blocked_zone);
    assert_eq!(
        acc.sync_token_expected_receivers,
        first.sync_token_expected_receivers
    );
    assert_eq!(
        acc.sync_token_receivers_with_token,
        first.sync_token_receivers_with_token
    );
    assert_eq!(
        acc.sync_token_receivers_missing,
        first.sync_token_receivers_missing
    );
    assert_eq!(
        acc.sync_token_receivers_blank,
        first.sync_token_receivers_blank
    );
    assert_eq!(
        acc.sync_token_receivers_dropped,
        first.sync_token_receivers_dropped
    );
    assert_eq!(acc.sync_token_unique_values, first.sync_token_unique_values);
}

/// Companion: accumulating into an empty `SyncStats` is a faithful copy
/// (the operation is the additive identity for the empty case).
#[test]
fn sync_stats_accumulate_into_empty_is_copy() {
    let src = SyncStats {
        assets_seen: 5,
        downloaded: 2,
        failed: 1,
        skipped: SkipBreakdown {
            duplicates: 7,
            ..SkipBreakdown::default()
        },
        rate_limited: 4,
        interrupted: true,
        ..SyncStats::default()
    };
    let mut dst = SyncStats::default();
    dst.accumulate(&src);
    assert_eq!(dst.assets_seen, 5);
    assert_eq!(dst.downloaded, 2);
    assert_eq!(dst.failed, 1);
    assert_eq!(dst.skipped.duplicates, 7);
    assert_eq!(dst.rate_limited, 4);
    assert!(dst.interrupted);
}
