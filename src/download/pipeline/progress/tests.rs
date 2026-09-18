use std::time::Duration;

use crate::download::pipeline::progress::{format_duration, maybe_warn_rate_limit_pressure};

// ── maybe_warn_rate_limit_pressure ─────────────────────────────────────
//
// The helper itself is side-effect-only (emits tracing::warn!); we assert
// the pure-math decision via the percentage threshold. A full log-capture
// test would need tracing-subscriber machinery that the rest of this
// module doesn't set up.

fn stats_with_rl(assets_seen: u64, rate_limited: usize) -> crate::download::SyncStats {
    crate::download::SyncStats {
        assets_seen,
        rate_limited,
        ..crate::download::SyncStats::default()
    }
}

#[test]
fn rate_limit_pressure_triggers_at_exactly_10_percent() {
    let stats = stats_with_rl(100, 10);
    assert!(stats.has_rate_limit_pressure());
}

#[test]
fn rate_limit_pressure_below_10_percent_does_not_trigger() {
    let stats = stats_with_rl(100, 9);
    assert!(!stats.has_rate_limit_pressure());
}

#[test]
fn rate_limit_pressure_zero_assets_seen_does_not_panic() {
    // With zero assets_seen, the helper must skip percentage math (which
    // would produce a misleading "300%" for 3 observations) and emit a
    // separate no-anchor warn path.
    let stats = stats_with_rl(0, 3);
    assert!(stats.has_rate_limit_pressure());
    maybe_warn_rate_limit_pressure(&stats);
}

#[test]
fn rate_limit_pressure_zero_observations_skips_quickly() {
    let stats = stats_with_rl(100, 0);
    assert!(!stats.has_rate_limit_pressure());
    maybe_warn_rate_limit_pressure(&stats); // no panic, no denom needed
}

#[test]
fn test_format_duration_seconds_only() {
    assert_eq!(format_duration(Duration::from_secs(0)), "0s");
    assert_eq!(format_duration(Duration::from_secs(1)), "1s");
    assert_eq!(format_duration(Duration::from_secs(42)), "42s");
    assert_eq!(format_duration(Duration::from_secs(59)), "59s");
}

#[test]
fn test_format_duration_minutes_and_seconds() {
    assert_eq!(format_duration(Duration::from_secs(60)), "1m 00s");
    assert_eq!(format_duration(Duration::from_secs(61)), "1m 01s");
    assert_eq!(format_duration(Duration::from_secs(754)), "12m 34s");
    assert_eq!(format_duration(Duration::from_secs(3599)), "59m 59s");
}

#[test]
fn test_format_duration_hours() {
    assert_eq!(format_duration(Duration::from_secs(3600)), "1h 00m 00s");
    assert_eq!(format_duration(Duration::from_secs(5025)), "1h 23m 45s");
    assert_eq!(format_duration(Duration::from_secs(86399)), "23h 59m 59s");
}

// ── format_duration additional edge cases ────────────────────────────

#[test]
fn test_format_duration_125_seconds() {
    assert_eq!(format_duration(Duration::from_secs(125)), "2m 05s");
}

#[test]
fn test_format_duration_3661_seconds() {
    assert_eq!(format_duration(Duration::from_secs(3661)), "1h 01m 01s");
}

#[test]
fn test_format_duration_ignores_sub_second() {
    // Duration with millis should only show whole seconds
    assert_eq!(format_duration(Duration::from_millis(1999)), "1s");
    assert_eq!(format_duration(Duration::from_millis(500)), "0s");
}

/// CG-2 (broadened from adversarial pass, 2026-05-03): `log_sync_summary`
/// had no test coverage at all. The mutation experiment showed that
/// dropping every `tracing::info!()` from the body would land green —
/// silently disabling sync-completion reporting in production logs.
/// This test is a baseline contract: at least one info event must fire
/// per call, the structured `title` field must be captured, and the
/// `downloaded` / `failed` counts must reach the captured output.
#[tracing_test::traced_test]
#[test]
fn log_sync_summary_emits_sync_counts_via_tracing() {
    let stats = crate::download::SyncStats {
        downloaded: 3,
        failed: 1,
        skipped: crate::download::SkipBreakdown {
            by_state: 2,
            ..Default::default()
        },
        ..Default::default()
    };

    crate::download::pipeline::progress::log_sync_summary("── Test Summary ──", &stats);

    // Title-line: structured `title` field must be present.
    // Note: tracing renders `title = %title` (Display) unquoted.
    assert!(
        logs_contain("title=── Test Summary ──"),
        "structured title field expected on first event"
    );
    // Title-line: message text must be present.
    assert!(
        logs_contain("Sync summary"),
        "title-line event message expected"
    );
    // Count line: every count must reach the captured stream.
    assert!(
        logs_contain("3 downloaded"),
        "downloaded count missing from summary line"
    );
    assert!(
        logs_contain("1 failed"),
        "failed count missing from summary line"
    );
    // Skipped breakdown line: when skipped > 0, a Skipped: line fires.
    assert!(
        logs_contain("Skipped:"),
        "skipped breakdown line expected when stats.skipped.total() > 0"
    );
}

/// When only `enumeration_errors` is non-zero, the line-2 conditional
/// must still fire. Otherwise an enumeration-error-driven
/// `PartialFailure` would produce an empty failure line and an
/// operator chasing exit code 2 has no count.
#[tracing_test::traced_test]
#[test]
fn log_sync_summary_emits_enumeration_errors_when_only_enum_errs() {
    let stats = crate::download::SyncStats {
        downloaded: 0,
        failed: 0,
        enumeration_errors: 4,
        ..Default::default()
    };

    crate::download::pipeline::progress::log_sync_summary("── Test Summary ──", &stats);

    assert!(
        logs_contain("4 enumeration error(s)"),
        "line 2 must surface enumeration_errors when nonzero"
    );
}

/// Inverse of the above: when every error counter is zero, the
/// line-2 conditional must not fire.
#[tracing_test::traced_test]
#[test]
fn log_sync_summary_no_error_line_when_all_counters_zero() {
    let stats = crate::download::SyncStats {
        downloaded: 5,
        ..Default::default()
    };

    crate::download::pipeline::progress::log_sync_summary("── Test Summary ──", &stats);

    assert!(
        !logs_contain("EXIF write failure"),
        "line 2 must not fire when exif/state/enum counters are all zero"
    );
    assert!(
        !logs_contain("enumeration error"),
        "line 2 must not fire when exif/state/enum counters are all zero"
    );
}
