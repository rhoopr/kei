#!/bin/bash
# Concurrency, resume, and partial-failure tests against live iCloud.
#
# Covers scenarios that are easier to exercise from shell than from Rust:
#   1. threads-num=5 against a real album, asserting DB/disk consistency
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
ALBUM="$(kei_album)"
KEI="$(kei_release_bin)"
kei_check_init

kei_sync() {
    "$KEI" sync \
        --username "$ICLOUD_USERNAME" \
        --password "$ICLOUD_PASSWORD" \
        --data-dir "$COOKIES" \
        --album "$ALBUM" \
        --no-progress-bar \
        --log-level info "$@" 2>&1
}

kei_suite_banner "CONCURRENCY / RESUME / PARTIAL-FAILURE"

echo ""
echo "--- Pre-flight ---"
kei_preflight_session

# ══════════════════════════════════════════════════════════════════════════
# 1. Concurrent downloads + state DB consistency
# ══════════════════════════════════════════════════════════════════════════
echo ""
echo "=== 1. Concurrent downloads (threads-num=5) ==="
DIR1=$(kei_scratch_dir concurrent)
kei_db_exec "DELETE FROM assets"

OUT=$(kei_sync --directory "$DIR1" --no-incremental --threads-num 5)
echo "$OUT" | grep -E "concurrency|downloaded|failed|completed"

FC=$(find "$DIR1" -type f | wc -l | tr -d ' ')
EMPTY=$(find "$DIR1" -type f -empty | wc -l | tr -d ' ')
DB_COUNT=$(kei_db_query "SELECT COUNT(DISTINCT id) FROM assets WHERE status='downloaded'")
DUPES=$(kei_db_query "SELECT COUNT(*) FROM (SELECT id, version_size, COUNT(*) c FROM assets GROUP BY id, version_size HAVING c > 1)")
echo "  Files=$FC Empty=$EMPTY DB_assets=$DB_COUNT Dupes=$DUPES"
[ "$FC" -ge 1 ];       kei_check "files downloaded"
[ "$EMPTY" -eq 0 ];    kei_check "no empty files"
[ "$DB_COUNT" -ge 1 ]; kei_check "DB tracks all files"
[ "$DUPES" -eq 0 ];    kei_check "no duplicate DB entries"

# Every file on disk must have a matching DB entry.
ORPHANS=0
while read -r f; do
    [ -z "$f" ] && continue
    basename=$(basename "$f")
    in_db=$(kei_db_query "SELECT COUNT(*) FROM assets WHERE filename='$basename' AND status='downloaded'")
    if [ "$in_db" -eq 0 ]; then
        echo "  ORPHAN: $basename not in state DB"
        ORPHANS=$((ORPHANS + 1))
    fi
done < <(find "$DIR1" -type f)
[ "$ORPHANS" -eq 0 ]; kei_check "no orphan files (all tracked in DB)"
rm -rf "$DIR1"

# ══════════════════════════════════════════════════════════════════════════
# 2. Partial download + resume (.part files)
# ══════════════════════════════════════════════════════════════════════════
echo ""
echo "=== 2. Interrupted download + resume ==="
DIR2=$(kei_scratch_dir resume)
kei_db_exec "DELETE FROM assets"

# Session reuse puts auth at ~3s; kill at 4s so we interrupt mid- or
# just-post-download. Even if all files complete before the kill the
# resume pass validates idempotency.
kei_sync --directory "$DIR2" --no-incremental --threads-num 1 &
SYNC_PID=$!
sleep 4
kill -9 $SYNC_PID 2>/dev/null
wait $SYNC_PID 2>/dev/null
kei_clear_stale_lock

PART_COUNT=$(find "$DIR2" -name "*.kei-tmp" | wc -l | tr -d ' ')
FILE_COUNT=$(find "$DIR2" -type f ! -name "*.kei-tmp" | wc -l | tr -d ' ')
echo "  After interrupt: $FILE_COUNT complete, $PART_COUNT .kei-tmp files"

OUT=$(kei_sync --directory "$DIR2" --no-incremental --threads-num 1)
echo "$OUT" | grep -E "downloaded|failed|completed|Skipping"

FINAL_FILES=$(find "$DIR2" -type f ! -name "*.kei-tmp" | wc -l | tr -d ' ')
FINAL_PARTS=$(find "$DIR2" -name "*.kei-tmp" | wc -l | tr -d ' ')
echo "  After resume: $FINAL_FILES complete, $FINAL_PARTS .kei-tmp files"
[ "$FINAL_FILES" -ge 1 ]; kei_check "all files complete after resume"
[ "$FINAL_PARTS" -eq 0 ]; kei_check "no .kei-tmp files remain"
rm -rf "$DIR2"

# ══════════════════════════════════════════════════════════════════════════
# 3. Exit code 2 (partial sync failure)
# ══════════════════════════════════════════════════════════════════════════
echo ""
echo "=== 3. Exit code 2 (partial failure) ==="
DIR3=$(kei_scratch_dir partial-fail)
kei_db_exec "DELETE FROM assets"

# Force one of the test album's files to land in a read-only directory.
# Album passes default to `{album}/` since the per-category template
# refactor (PR #288), so we explicitly request a date hierarchy and
# pre-create one date dir as read-only. GOPR0558.JPG in icloudpd-test
# is dated 2019-11-09; making that path 555 makes its write fail while
# the other dates succeed.
mkdir -p "$DIR3/2019/11/09"
chmod 555 "$DIR3/2019/11/09" "$DIR3/2019/11" "$DIR3/2019"

kei_sync --directory "$DIR3" --no-incremental --threads-num 1 \
    --folder-structure-albums "%Y/%m/%d"
EC=$?
echo "  Exit code: $EC"

DOWNLOADED=$(find "$DIR3" -type f 2>/dev/null | wc -l | tr -d ' ')
DB_FAILED=$(kei_db_query "SELECT COUNT(*) FROM assets WHERE status='failed'")
echo "  Files downloaded: $DOWNLOADED, DB failed: $DB_FAILED"

chmod -R 755 "$DIR3" 2>/dev/null
[ "$EC" -eq 2 ]; kei_check "exit code 2 (partial failure)"
rm -rf "$DIR3"

kei_check_summary "CONCURRENCY RESULTS"
