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

COOKIES="$(kei_cookie_dir)"
KEI="$(kei_release_bin)"
kei_check_init

kei_sync() {
    local download_dir="${1:?download dir required}"
    shift
    local config
    config="$(kei_write_sync_config "$COOKIES" "$download_dir")"
    # Shared single-pass primary-library selection bounds provider work.
    KEI_DATA_DIR="$COOKIES" "$KEI" sync \
        --password "$ICLOUD_PASSWORD" \
        --config "$config" \
        --no-progress-bar \
        --log-level info "$@" 2>&1
}

# Each scenario uses a different download root. Clear provider checkpoints and
# path/config hashes with the asset rows so the next scenario starts with a
# full inventory instead of treating the new root as deferred reconciliation.
reset_sync_state() {
    kei_db_exec "DELETE FROM metadata WHERE key LIKE '%token%' OR key IN ('config_hash', 'enum_config_hash', 'pending_download_config_hash', 'pending_enum_config_hash')"
    kei_db_exec "DELETE FROM assets"
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
reset_sync_state

OUT=$(KEI_SYNC_DOWNLOAD_TOML=$'threads = 5\n' kei_sync "$DIR1")
echo "$OUT" | grep -E "concurrency|downloaded|failed|completed"

FC=$(find "$DIR1" -type f | wc -l | tr -d ' ')
EMPTY=$(find "$DIR1" -type f -empty | wc -l | tr -d ' ')
DB_COUNT=$(kei_db_query "SELECT COUNT(DISTINCT id) FROM assets WHERE status='downloaded'")
DUPES=$(kei_db_query "SELECT COUNT(*) FROM (SELECT id, version_size, COUNT(*) c FROM assets GROUP BY id, version_size HAVING c > 1)")
echo "  Files=$FC Empty=$EMPTY DB_assets=$DB_COUNT Dupes=$DUPES"
[ "$FC" -ge 1 ]
kei_check "files downloaded"
[ "$EMPTY" -eq 0 ]
kei_check "no empty files"
[ "$DB_COUNT" -ge 1 ]
kei_check "DB tracks all files"
[ "$DUPES" -eq 0 ]
kei_check "no duplicate DB entries"

# Every file on disk must have a matching DB entry.
ORPHANS=0
while IFS= read -r -d '' f; do
    [ -z "$f" ] && continue
    basename=$(basename "$f")
    in_db=$(kei_db_query "SELECT COUNT(*) FROM assets WHERE local_path=$(kei_sql_string "$f") AND status='downloaded'")
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
reset_sync_state

# Session reuse puts auth at ~3s; kill at 4s so we interrupt mid- or
# just-post-download. Even if all files complete before the kill the
# resume pass validates idempotency.
KEI_SYNC_DOWNLOAD_TOML=$'threads = 1\n' kei_sync "$DIR2" &
SYNC_PID=$!
sleep 4
kill -9 $SYNC_PID 2>/dev/null
wait $SYNC_PID 2>/dev/null
kei_clear_stale_lock

PART_COUNT=$(find "$DIR2" -name "*.kei-tmp" | wc -l | tr -d ' ')
FILE_COUNT=$(find "$DIR2" -type f ! -name "*.kei-tmp" | wc -l | tr -d ' ')
echo "  After interrupt: $FILE_COUNT complete, $PART_COUNT .kei-tmp files"

OUT=$(KEI_SYNC_DOWNLOAD_TOML=$'threads = 1\n' kei_sync "$DIR2")
echo "$OUT" | grep -E "downloaded|failed|completed|Skipping"

FINAL_FILES=$(find "$DIR2" -type f ! -name "*.kei-tmp" | wc -l | tr -d ' ')
FINAL_PARTS=$(find "$DIR2" -name "*.kei-tmp" | wc -l | tr -d ' ')
echo "  After resume: $FINAL_FILES complete, $FINAL_PARTS .kei-tmp files"
[ "$FINAL_FILES" -ge 1 ]
kei_check "all files complete after resume"
[ "$FINAL_PARTS" -eq 0 ]
kei_check "no .kei-tmp files remain"
rm -rf "$DIR2"

# Partial failure and exit-code mapping are deterministic fixture/library tests:
# bundled_partial_failure_recovers_after_restart and
# classify_exit_error_partial_sync_uses_exit_partial.

kei_check_summary "CONCURRENCY RESULTS"
