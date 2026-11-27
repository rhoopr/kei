#!/bin/bash
# Concurrency, resume, and partial-failure tests against live iCloud.
#
# Covers scenarios that are easier to exercise from shell than from Rust:
#   1. threads=5 against a real album, asserting DB/disk consistency
#   2. Interrupt mid-download (SIGKILL) and resume on re-run
#   3. chmod-555 a target dir to force a per-asset failure, assert exit
#      code 2 (partial failure)
#
# Usage: ./tests/shell/concurrency.sh

set -o pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/../.." && pwd)"
# shellcheck disable=SC1091
source "$SCRIPT_DIR/lib.sh"

kei_require_env
kei_require_release_binary
kei_install_scratch_cleanup

SESSION_SOURCE="$(kei_cookie_dir)"
KEI="$(kei_release_bin)"
kei_check_init

kei_sync() {
    local download_dir="${1:?download dir required}"
    shift
    local config
    config="$(kei_write_sync_config "$COOKIES" "$download_dir")"
    # Shared single-pass primary-library selection bounds provider work.
    KEI_DATA_DIR="$COOKIES" "$KEI" sync \
        --config "$config" \
        --no-progress-bar \
        --log-level info "$@" 2>&1
}

# Each download root gets its own copy of the listed catalog. Only login
# state is shared with the operator or the other scenario.
fresh_session() {
    COOKIES=$(kei_scratch_dir "$1")
    ICLOUD_TEST_COOKIE_DIR="$SESSION_SOURCE" kei_copy_session_without_state "$COOKIES"
    ICLOUD_TEST_COOKIE_DIR="$SESSION_SOURCE" kei_seed_listed_state "$COOKIES" || exit 1
    export ICLOUD_TEST_COOKIE_DIR="$COOKIES"
}

kei_suite_banner "CONCURRENCY / RESUME"

echo ""
echo "--- Pre-flight ---"
kei_preflight_session

# ══════════════════════════════════════════════════════════════════════════
# 1. Concurrent downloads + state DB consistency
# ══════════════════════════════════════════════════════════════════════════
echo ""
echo "=== 1. Concurrent downloads (threads=5) ==="
DIR1=$(kei_scratch_dir concurrent)
fresh_session concurrent-session

OUT=$(KEI_SYNC_DOWNLOAD_TOML=$'threads = 5\n' kei_sync "$DIR1")
echo "$OUT" | grep -E "concurrency|Library synced|Saved copies"

FC=$(find "$DIR1" -type f | wc -l | tr -d ' ')
EMPTY=$(find "$DIR1" -type f -empty | wc -l | tr -d ' ')
DB_COUNT=$(kei_db_query "SELECT COUNT(*) FROM files WHERE state = 'done' AND wanted = 1")
DUPES=$(kei_db_query "SELECT COUNT(*) FROM (SELECT path, COUNT(*) c FROM files GROUP BY path HAVING c > 1)")
echo "  Files=$FC Empty=$EMPTY DB_copies=$DB_COUNT Dupes=$DUPES"
[ "$FC" -ge 1 ]
kei_check "files downloaded"
[ "$EMPTY" -eq 0 ]
kei_check "no empty files"
[ "$DB_COUNT" -eq "$FC" ]
kei_check "DB tracks all files"
[ "$DUPES" -eq 0 ]
kei_check "no duplicate DB entries"

# Every file on disk must have a matching DB entry.
ORPHANS=0
while IFS= read -r -d '' f; do
    [ -z "$f" ] && continue
    basename=$(basename "$f")
    in_db=$(kei_db_query "SELECT COUNT(*) FROM files WHERE path = $(kei_sql_string "${f#"$DIR1/"}") AND state = 'done'")
    if [ "$in_db" -eq 0 ]; then
        echo "  ORPHAN: $basename not in state DB"
        ORPHANS=$((ORPHANS + 1))
    fi
done < <(find "$DIR1" -type f -print0)
[ "$ORPHANS" -eq 0 ]
kei_check "no orphan files (all tracked in DB)"
rm -rf "$DIR1"

# ══════════════════════════════════════════════════════════════════════════
# 2. Partial download + resume (.part files)
# ══════════════════════════════════════════════════════════════════════════
echo ""
echo "=== 2. Interrupted download + resume ==="
DIR2=$(kei_scratch_dir resume)
fresh_session resume-session

# Kill as soon as downloads start, so a copy is likely mid-transfer. Even
# if every file completes before the kill, the resume pass validates
# idempotency.
SYNC_LOG="$DIR2.log"
KEI_SYNC_DOWNLOAD_TOML=$'threads = 1\n' kei_sync "$DIR2" >"$SYNC_LOG" 2>&1 &
SYNC_PID=$!
for _ in $(seq 1 1200); do
    grep -q "Saving owed copies" "$SYNC_LOG" && break
    kill -0 "$SYNC_PID" 2>/dev/null || break
    sleep 0.5
done
grep -q "Saving owed copies" "$SYNC_LOG"
kei_check "downloads started before the interrupt"
sleep 1
kill -9 $SYNC_PID 2>/dev/null
wait $SYNC_PID 2>/dev/null
rm -f "$SYNC_LOG"
kei_clear_stale_lock

PART_COUNT=$(find "$DIR2" -name "*.kei-tmp" | wc -l | tr -d ' ')
FILE_COUNT=$(find "$DIR2" -type f ! -name "*.kei-tmp" | wc -l | tr -d ' ')
echo "  After interrupt: $FILE_COUNT complete, $PART_COUNT .kei-tmp files"

OUT=$(KEI_SYNC_DOWNLOAD_TOML=$'threads = 1\n' kei_sync "$DIR2")
kei_check "resume sync exit status" "$?"
echo "$OUT" | grep -E "Library synced|Saved copies"

FINAL_FILES=$(find "$DIR2" -type f ! -name "*.kei-tmp" | wc -l | tr -d ' ')
FINAL_PARTS=$(find "$DIR2" -name "*.kei-tmp" | wc -l | tr -d ' ')
echo "  After resume: $FINAL_FILES complete, $FINAL_PARTS .kei-tmp files"
[ "$FINAL_FILES" -ge 1 ] && [ "$FINAL_FILES" -eq "$(kei_db_query "SELECT COUNT(*) FROM files WHERE state = 'done' AND wanted = 1")" ]
kei_check "all files complete after resume"
[ "$FINAL_PARTS" -eq 0 ]
kei_check "no .kei-tmp files remain"
rm -rf "$DIR2"

# Partial failure and exit-code mapping are deterministic fixture/library tests:
# bundled_partial_failure_recovers_after_restart and
# classify_exit_error_partial_sync_uses_exit_partial.

kei_check_summary "CONCURRENCY RESULTS"
