#!/bin/bash
# Run all tests. Auth tests require a pre-authenticated session in .test-cookies/
#
# Each test binary runs separately to avoid lock file contention on
# .test-cookies/ and to prevent flooding Apple's API with parallel requests.
#
# Order: no-auth tests first, then auth tests. Bad-credentials test runs
# last (sorts as zz_*) since it hits Apple's auth endpoint from scratch.
#
# Auth throttling: session reuse via accountLogin avoids repeated SRP
# handshakes (only 1 SRP per run). TEST_THROTTLE_SECS (default 2) spaces
# out test functions for API politeness. A short inter-suite delay
# separates cargo test binaries.
#
# Auth test suites fail-fast: if one suite fails (likely 503 rate limit),
# remaining auth suites are skipped to avoid piling on.
#
# Results are logged to tests/results.log

set -o pipefail

LOG="$(dirname "$0")/results.log"
: > "$LOG"

FAILED=0
STARTED=""

# Auth throttle: seconds between individual test functions (Rust-side).
# Override with TEST_THROTTLE_SECS env var. Default: 2.
export TEST_THROTTLE_SECS="${TEST_THROTTLE_SECS:-2}"

# Inter-suite delay: seconds to wait between auth test suites.
INTER_SUITE_DELAY="${INTER_SUITE_DELAY:-3}"

elapsed() {
    local start="$1"
    local now
    now=$(date +%s)
    local delta=$((now - start))
    printf '%dm %02ds' $((delta / 60)) $((delta % 60))
}

run() {
    local label="$1"
    shift
    local t0
    t0=$(date +%s)
    echo ""
    echo "==> $label"
    echo "" >> "$LOG"
    echo "==> $label" >> "$LOG"
    "$@" 2>&1 | tee -a "$LOG"
    local rc="${PIPESTATUS[0]}"
    if [ "$rc" -ne 0 ]; then
        FAILED=1
        echo "  FAILED ($(elapsed "$t0"))"
    else
        echo "  passed ($(elapsed "$t0"))"
    fi
}

# fail-fast variant: aborts the script on failure
run_or_stop() {
    local label="$1"
    shift
    local t0
    t0=$(date +%s)
    echo ""
    echo "==> $label"
    echo "" >> "$LOG"
    echo "==> $label" >> "$LOG"
    "$@" 2>&1 | tee -a "$LOG"
    local rc="${PIPESTATUS[0]}"
    if [ "$rc" -ne 0 ]; then
        echo ""
        echo "FAILED: $label ($(elapsed "$t0")) — skipping remaining auth suites (likely rate-limited)."
        echo "Wait 10-15 minutes before retrying."
        echo "See $LOG"
        exit 1
    fi
    echo "  passed ($(elapsed "$t0"))"
}

# Delay between auth test suites to avoid API rate limiting.
auth_delay() {
    if [ "$INTER_SUITE_DELAY" -gt 0 ]; then
        echo ""
        echo "--- Waiting ${INTER_SUITE_DELAY}s between auth suites (API rate-limit avoidance) ---"
        sleep "$INTER_SUITE_DELAY"
    fi
}

STARTED=$(date +%s)

# ── No-auth tests (always run all) ──────────────────────────────────────
run "Unit tests"              cargo test --bin kei
run "CLI integration tests"   cargo test --test cli
run "Behavioral tests"        cargo test --test behavioral

if [ "$FAILED" -ne 0 ]; then
    echo ""
    echo "No-auth tests failed. Fix these before running auth tests."
    echo "See $LOG"
    exit 1
fi

# ── Auth tests (fail-fast to avoid 503 cascade) ─────────────────────────
echo ""
echo "Auth tests: TEST_THROTTLE_SECS=${TEST_THROTTLE_SECS}, INTER_SUITE_DELAY=${INTER_SUITE_DELAY}"

run_or_stop "Sync tests"         cargo test --test sync -- --ignored --test-threads=1

auth_delay

run_or_stop "State tests (auth)" cargo test --test state_auth -- --ignored --test-threads=1

echo ""
if [ "$FAILED" -ne 0 ]; then
    echo "Some suites FAILED. ($(elapsed "$STARTED") total)"
    echo "See $LOG"
    exit 1
fi
echo "All suites passed. ($(elapsed "$STARTED") total)"
exit 0
