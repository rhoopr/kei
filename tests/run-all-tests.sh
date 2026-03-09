#!/bin/bash
# Run all tests. Auth tests require a pre-authenticated session in .test-cookies/
#
# Each test binary runs separately to avoid lock file contention on
# .test-cookies/ and to prevent flooding Apple's API with parallel requests.
#
# Order: no-auth tests first, then auth tests. Bad-credentials test runs
# last (sorts as zz_*) since it hits Apple's auth endpoint from scratch.
#
# Auth test suites fail-fast: if one suite fails (likely 503 rate limit),
# remaining auth suites are skipped to avoid piling on.
#
# Results are logged to tests/results.log

LOG="$(dirname "$0")/results.log"
: > "$LOG"

FAILED=0

run() {
    local label="$1"
    shift
    echo ""
    echo "==> $label"
    echo "" >> "$LOG"
    echo "==> $label" >> "$LOG"
    "$@" 2>&1 | tee -a "$LOG"
    if [ "${PIPESTATUS[0]}" -ne 0 ]; then
        FAILED=1
    fi
}

# fail-fast variant: aborts the script on failure
run_or_stop() {
    local label="$1"
    shift
    echo ""
    echo "==> $label"
    echo "" >> "$LOG"
    echo "==> $label" >> "$LOG"
    "$@" 2>&1 | tee -a "$LOG"
    if [ "${PIPESTATUS[0]}" -ne 0 ]; then
        echo ""
        echo "FAILED: $label — skipping remaining auth suites (likely rate-limited)."
        echo "Wait 10-15 minutes before retrying."
        echo "See $LOG"
        exit 1
    fi
}

# ── No-auth tests (always run all) ──────────────────────────────────────
run "Unit tests"              cargo test --bin icloudpd-rs
run "CLI integration tests"   cargo test --test cli
run "State tests (no-auth)"   cargo test --test state

if [ "$FAILED" -ne 0 ]; then
    echo ""
    echo "No-auth tests failed. Fix these before running auth tests."
    echo "See $LOG"
    exit 1
fi

# ── Auth tests (fail-fast to avoid 503 cascade) ─────────────────────────
run_or_stop "Sync tests"         cargo test --test sync -- --test-threads=1
run_or_stop "State tests (auth)" cargo test --test state_auth -- --test-threads=1

echo ""
echo "All suites passed."
exit 0
