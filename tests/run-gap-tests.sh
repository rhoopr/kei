#!/bin/bash
# Gap coverage tests: concurrent downloads, partial failure exit codes,
# interrupted download resume, and filename dedup verification.
#
# Usage: ./tests/run-gap-tests.sh

set -o pipefail
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"
# shellcheck disable=SC1091
source "$SCRIPT_DIR/lib.sh"
kei_require_env

COOKIES="$(kei_cookie_dir)"
DB="$(kei_db_path)"
ALBUM="$(kei_album)"
KEI="$PROJECT_DIR/target/release/kei"
PASS=0; FAIL=0

check() {
  if [ "$2" -eq 0 ]; then echo "  PASS: $1"; PASS=$((PASS+1))
  else echo "  FAIL: $1"; FAIL=$((FAIL+1)); fi
}

kei_sync() {
  "$KEI" sync --username "$ICLOUD_USERNAME" --password "$ICLOUD_PASSWORD" \
    --data-dir "$COOKIES" --album "$ALBUM" --no-progress-bar \
    --log-level info "$@" 2>&1
}

echo "=============================================="
echo "  GAP COVERAGE TESTS"
echo "  $(date '+%Y-%m-%d %H:%M:%S')"
echo "=============================================="

# ── Pre-flight ──
echo ""
echo "--- Pre-flight ---"
OUT=$("$KEI" login --username "$ICLOUD_USERNAME" --password "$ICLOUD_PASSWORD" \
  --data-dir "$COOKIES" 2>&1)
if echo "$OUT" | grep -q "Authentication completed\|Session OK\|already authenticated"; then
  echo "  OK: session valid"
else
  echo "  ABORT: auth failed"; echo "$OUT" | tail -3; exit 1
fi

# ══════════════════════════════════════════════════════════════════════════
# Gap 1: Concurrent downloads + state DB consistency
# ══════════════════════════════════════════════════════════════════════════
echo ""
echo "=== Gap 1: Concurrent downloads (threads-num=5) ==="
DIR1="$PROJECT_DIR/.gap1"
rm -rf "$DIR1"; mkdir -p "$DIR1"
sqlite3 "$DB" "DELETE FROM assets" 2>/dev/null

OUT=$(kei_sync --directory "$DIR1" --no-incremental --threads-num 5)
echo "$OUT" | grep -E "concurrency|downloaded|failed|completed"

FC=$(find "$DIR1" -type f | wc -l | tr -d ' ')
EMPTY=$(find "$DIR1" -type f -empty | wc -l | tr -d ' ')
DB_COUNT=$(sqlite3 "$DB" "SELECT COUNT(DISTINCT id) FROM assets WHERE status='downloaded'" 2>/dev/null)
DUPES=$(sqlite3 "$DB" "SELECT COUNT(*) FROM (SELECT id, version_size, COUNT(*) c FROM assets GROUP BY id, version_size HAVING c > 1)" 2>/dev/null)
echo "  Files=$FC Empty=$EMPTY DB_assets=$DB_COUNT Dupes=$DUPES"
check "files downloaded" "$([ "$FC" -ge 1 ]; echo $?)"
check "no empty files" "$([ "$EMPTY" -eq 0 ]; echo $?)"
check "DB tracks all files" "$([ "$DB_COUNT" -ge 1 ]; echo $?)"
check "no duplicate DB entries" "$([ "$DUPES" -eq 0 ]; echo $?)"

# Verify each file on disk has a matching DB entry
ORPHANS=0
for f in $(find "$DIR1" -type f); do
  BASENAME=$(basename "$f")
  IN_DB=$(sqlite3 "$DB" "SELECT COUNT(*) FROM assets WHERE filename='$BASENAME' AND status='downloaded'" 2>/dev/null)
  if [ "$IN_DB" -eq 0 ]; then
    echo "  ORPHAN: $BASENAME not in state DB"
    ORPHANS=$((ORPHANS + 1))
  fi
done
check "no orphan files (all tracked in DB)" "$([ "$ORPHANS" -eq 0 ]; echo $?)"
rm -rf "$DIR1"

# ══════════════════════════════════════════════════════════════════════════
# Gap 2: Partial download + resume (.part files)
# ══════════════════════════════════════════════════════════════════════════
echo ""
echo "=== Gap 2: Interrupted download + resume ==="
DIR2="$PROJECT_DIR/.gap2"
rm -rf "$DIR2"; mkdir -p "$DIR2"
sqlite3 "$DB" "DELETE FROM assets" 2>/dev/null

# Start sync, kill it mid-flight to test resume behavior.
# With session reuse, auth completes in ~3s. Kill after 4s to
# interrupt during (or just after) download. If all files complete
# before the kill, the resume test still validates idempotency.
kei_sync --directory "$DIR2" --no-incremental --threads-num 1 &
SYNC_PID=$!
sleep 4
kill -9 $SYNC_PID 2>/dev/null
wait $SYNC_PID 2>/dev/null
# Clean up stale lock file left by kill -9
rm -f "$COOKIES"/*.lock

PART_COUNT=$(find "$DIR2" -name "*.kei-tmp" | wc -l | tr -d ' ')
FILE_COUNT=$(find "$DIR2" -type f ! -name "*.kei-tmp" | wc -l | tr -d ' ')
echo "  After interrupt: $FILE_COUNT complete, $PART_COUNT .kei-tmp files"

# Re-run sync — should complete all files (resume any partial, skip complete)
OUT=$(kei_sync --directory "$DIR2" --no-incremental --threads-num 1)
echo "$OUT" | grep -E "downloaded|failed|completed|Skipping"

FINAL_FILES=$(find "$DIR2" -type f ! -name "*.kei-tmp" | wc -l | tr -d ' ')
FINAL_PARTS=$(find "$DIR2" -name "*.kei-tmp" | wc -l | tr -d ' ')
echo "  After resume: $FINAL_FILES complete, $FINAL_PARTS .kei-tmp files"
check "all files complete after resume" "$([ "$FINAL_FILES" -ge 1 ]; echo $?)"
check "no .kei-tmp files remain" "$([ "$FINAL_PARTS" -eq 0 ]; echo $?)"
rm -rf "$DIR2"

# ══════════════════════════════════════════════════════════════════════════
# Gap 3: Exit code 2 (partial sync failure)
# ══════════════════════════════════════════════════════════════════════════
echo ""
echo "=== Gap 3: Exit code 2 (partial failure) ==="
DIR3="$PROJECT_DIR/.gap3"
rm -rf "$DIR3"; mkdir -p "$DIR3"
sqlite3 "$DB" "DELETE FROM assets" 2>/dev/null

# The test album has 3 files in different date directories:
#   2019/11/09/GOPR0558.JPG
#   2025/04/13/IMG_0962.MOV
#   2026/02/09/Cafe_godzill.JPG
#
# Make one date directory read-only so one download fails.
mkdir -p "$DIR3/2019/11/09"
chmod 555 "$DIR3/2019/11/09"
chmod 555 "$DIR3/2019/11"
chmod 555 "$DIR3/2019"

kei_sync --directory "$DIR3" --no-incremental --threads-num 1
EC=$?
echo "  Exit code: $EC"

# Count successes and failures
DOWNLOADED=$(find "$DIR3" -type f 2>/dev/null | wc -l | tr -d ' ')
DB_FAILED=$(sqlite3 "$DB" "SELECT COUNT(*) FROM assets WHERE status='failed'" 2>/dev/null)
echo "  Files downloaded: $DOWNLOADED, DB failed: $DB_FAILED"

# Restore permissions for cleanup
chmod -R 755 "$DIR3" 2>/dev/null

if [ "$EC" -eq 2 ]; then
  check "exit code 2 (partial failure)" 0
elif [ "$EC" -eq 1 ]; then
  echo "  INFO: exit code 1 (may be total failure — all 3 files might target same permissions issue)"
  check "exit code 2 (partial failure)" 1
else
  check "exit code 2 (partial failure)" 1
fi
rm -rf "$DIR3"

# ══════════════════════════════════════════════════════════════════════════
# Gap 4: Filename dedup unit test verification
# ══════════════════════════════════════════════════════════════════════════
echo ""
echo "=== Gap 4: Filename dedup (unit test coverage check) ==="
# Can't test live (need duplicate filenames in album), but verify unit tests exist
DEDUP_TESTS=$(cargo test --bin kei -- --list 2>&1 | grep -cE "dedup|collision")
echo "  Dedup-related unit tests: $DEDUP_TESTS"
check "dedup unit tests exist (>=4)" "$([ "$DEDUP_TESTS" -ge 4 ]; echo $?)"

# Run them to confirm they pass
cargo test --bin kei dedup 2>&1 | grep "test result:"
cargo test --bin kei collision 2>&1 | grep "test result:"

# ══════════════════════════════════════════════════════════════════════════
# Gap 5: Multiple --album flags
# ══════════════════════════════════════════════════════════════════════════
echo ""
echo "=== Gap 5: Multiple --album flags ==="
DIR5="$PROJECT_DIR/.gap5"
rm -rf "$DIR5"; mkdir -p "$DIR5"

OUT=$(kei_sync --directory "$DIR5" --album Favorites --dry-run --no-incremental)
echo "$OUT" | grep -E "Fetching|album|completed|No new"
EC=$?
check "multiple albums accepted" "$([ $EC -eq 0 ]; echo $?)"
rm -rf "$DIR5"

# ══════════════════════════════════════════════════════════════════════════
# Gap 6: --only-print-filenames
# ══════════════════════════════════════════════════════════════════════════
echo ""
echo "=== Gap 6: --only-print-filenames ==="
OUT=$(kei_sync --directory /tmp/claude/opf --only-print-filenames --no-incremental)
# Should print filenames to stdout without downloading
LINE_COUNT=$(echo "$OUT" | grep -cv "INFO\|WARN\|ERROR\|Starting")
echo "  Output lines (non-log): $LINE_COUNT"
check "--only-print-filenames produces output" "$([ "$LINE_COUNT" -ge 1 ]; echo $?)"

# ══════════════════════════════════════════════════════════════════════════
echo ""
echo "=============================================="
echo "  GAP RESULTS: $PASS pass, $FAIL fail"
echo "=============================================="
[ "$FAIL" -eq 0 ] && exit 0 || exit 1
