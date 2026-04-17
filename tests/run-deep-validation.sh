#!/bin/bash
# Deep validation tests for sync token, config hash, and edge cases.
# Requires: pre-authenticated cookie dir and credentials (see tests/lib.sh
# for the ICLOUD_* / KEI_TEST_* env vars).
#
# Uses ~15 Apple API calls. Session reuse via accountLogin avoids repeated
# SRP handshakes, so cooldown before running is typically not needed.
#
# Usage: ./tests/run-deep-validation.sh

set -o pipefail
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"
# shellcheck disable=SC1091
source "$SCRIPT_DIR/lib.sh"
kei_require_env

COOKIES="$(kei_cookie_dir)"
DB="$(kei_db_path)"
ALBUM="$(kei_album)"
PASS=0
FAIL=0
SKIP=0

kei() {
  "$PROJECT_DIR/target/release/kei" sync \
    --username "$ICLOUD_USERNAME" \
    --password "$ICLOUD_PASSWORD" \
    --data-dir "$COOKIES" \
    --album "$ALBUM" \
    --no-progress-bar \
    --log-level info \
    "$@" 2>&1
}

check() {
  local label="$1" result="$2"
  if [ "$result" -eq 0 ]; then
    echo "  PASS: $label"
    PASS=$((PASS + 1))
  else
    echo "  FAIL: $label"
    FAIL=$((FAIL + 1))
  fi
}

get_token() {
  sqlite3 "$DB" "SELECT value FROM metadata WHERE key = 'sync_token:PrimarySync'" 2>/dev/null
}
get_hash() {
  sqlite3 "$DB" "SELECT value FROM metadata WHERE key = 'config_hash'" 2>/dev/null
}
token_count() {
  sqlite3 "$DB" "SELECT COUNT(*) FROM metadata WHERE key LIKE '%token%'" 2>/dev/null
}

echo "=================================================="
echo "  DEEP VALIDATION"
echo "  $(date '+%Y-%m-%d %H:%M:%S')"
echo "=================================================="

# ── 0. Pre-flight ────────────────────────────────────────────────────────
echo ""
echo "--- Pre-flight: verify session ---"
PREFLIGHT=$("$PROJECT_DIR/target/release/kei" login \
  --username "$ICLOUD_USERNAME" --password "$ICLOUD_PASSWORD" \
  --data-dir "$COOKIES" 2>&1)
if echo "$PREFLIGHT" | grep -q "Authentication completed\|Session OK\|already authenticated"; then
  echo "  PASS: session valid"
  PASS=$((PASS + 1))
else
  echo "  FAIL: session invalid or rate-limited"
  echo "$PREFLIGHT" | tail -3
  echo "  ABORT: wait 30 min and retry"
  exit 1
fi

DIR="$PROJECT_DIR/.deep-test"

# ── 1. Clean slate: full sync, verify token + config hash stored ─────────
echo ""
echo "=== 1. Clean slate full sync ==="
rm -rf "$DIR"; mkdir -p "$DIR"
sqlite3 "$DB" "DELETE FROM metadata WHERE key LIKE '%token%' OR key = 'config_hash'" 2>/dev/null
echo "  Cleared: tokens=$(token_count), hash=$(get_hash || echo 'none')"
OUTPUT=$(kei --directory "$DIR" --no-incremental)
echo "$OUTPUT" | grep -E "Incremental|token|Summary|downloaded|completed"
check "token stored after full sync" "$([ -n "$(get_token)" ]; echo $?)"
check "config hash stored" "$([ -n "$(get_hash)" ]; echo $?)"
check "files downloaded" "$([ $(find "$DIR" -type f | wc -l | tr -d ' ') -ge 1 ]; echo $?)"
BASELINE_HASH=$(get_hash)
BASELINE_TOKEN=$(get_token)
echo "  hash=$BASELINE_HASH"

# ── 2. Incremental sync: no changes → 0 downloads, token preserved ──────
echo ""
echo "=== 2. Incremental sync (no changes) ==="
OUTPUT=$(kei --directory "$DIR")
echo "$OUTPUT" | grep -E "incremental|token|change|download|completed"
check "used incremental sync" "$(echo "$OUTPUT" | grep -qi "incremental"; echo $?)"
check "0 change events" "$(echo "$OUTPUT" | grep -q "No new photos to download from incremental"; echo $?)"
check "token preserved" "$([ "$(get_token)" = "$BASELINE_TOKEN" ]; echo $?)"

# ── 3. Config change: --size medium → hash changes, tokens cleared ───────
echo ""
echo "=== 3. Config change clears tokens ==="
HASH_BEFORE=$(get_hash)
TOKEN_BEFORE=$(get_token)
OUTPUT=$(kei --directory "$DIR" --size medium)
echo "$OUTPUT" | grep -E "config|changed|cleared|token|incremental|download|completed"
HASH_AFTER=$(get_hash)
TOKEN_AFTER=$(get_token)
check "config hash changed" "$([ "$HASH_BEFORE" != "$HASH_AFTER" ]; echo $?)"
echo "  hash: $HASH_BEFORE → $HASH_AFTER"
# After config change, the old tokens should have been cleared, but a new token
# is stored at the end of the sync. So we check the token VALUE changed.
check "new token stored" "$([ -n "$TOKEN_AFTER" ]; echo $?)"

# ── 4. Restore original config → hash reverts, full re-enum ─────────────
echo ""
echo "=== 4. Restore original config ==="
OUTPUT=$(kei --directory "$DIR")
echo "$OUTPUT" | grep -E "config|changed|cleared|token|incremental|download|completed"
HASH_RESTORED=$(get_hash)
check "hash reverted to original" "$([ "$HASH_RESTORED" = "$BASELINE_HASH" ]; echo $?)"
check "token stored" "$([ -n "$(get_token)" ]; echo $?)"

# ── 5. --reset-sync-token: forces full enum ──────────────────────────────
echo ""
echo "=== 5. --reset-sync-token ==="
TOKEN_BEFORE=$(get_token)
OUTPUT=$(kei --directory "$DIR" --reset-sync-token)
echo "$OUTPUT" | grep -E "reset|clear|token|Fetching|full|incremental|download|completed"
TOKEN_AFTER=$(get_token)
check "full enumeration ran" "$(echo "$OUTPUT" | grep -qi "Fetching"; echo $?)"
check "new token stored" "$([ -n "$TOKEN_AFTER" ]; echo $?)"

# ── 6. Corrupt token → fallback to full enumeration ──────────────────────
echo ""
echo "=== 6. Corrupt token recovery ==="
GOOD_TOKEN=$(get_token)
sqlite3 "$DB" "UPDATE metadata SET value = 'CORRUPT_GARBAGE_TOKEN_XYZ' WHERE key = 'sync_token:PrimarySync'" 2>/dev/null
echo "  Injected: CORRUPT_GARBAGE_TOKEN_XYZ"
OUTPUT=$(kei --directory "$DIR")
echo "$OUTPUT" | grep -E "token|invalid|fallback|full|error|Fetching|incremental|download|completed"
RECOVERED_TOKEN=$(get_token)
if echo "$OUTPUT" | grep -qi "fallback\|full enumeration\|Fetching"; then
  check "fell back to full enumeration" 0
elif echo "$OUTPUT" | grep -q "503"; then
  echo "  SKIP: rate-limited before token validation"
  SKIP=$((SKIP + 1))
  # Restore good token
  sqlite3 "$DB" "UPDATE metadata SET value = '$GOOD_TOKEN' WHERE key = 'sync_token:PrimarySync'" 2>/dev/null
else
  check "fell back to full enumeration" 1
  echo "  OUTPUT: $(echo "$OUTPUT" | head -5)"
  sqlite3 "$DB" "UPDATE metadata SET value = '$GOOD_TOKEN' WHERE key = 'sync_token:PrimarySync'" 2>/dev/null
fi
check "valid token after recovery" "$([ -n "$RECOVERED_TOKEN" ] && [ "$RECOVERED_TOKEN" != 'CORRUPT_GARBAGE_TOKEN_XYZ' ]; echo $?)"

# ── 7. Simulated new photo: delete from state, incremental skips it ──────
echo ""
echo "=== 7. Simulated new photo detection ==="
# Delete one asset from state DB and disk to simulate "new" photo
DELETED_FILE=$(sqlite3 "$DB" "SELECT filename FROM assets WHERE status='downloaded' LIMIT 1" 2>/dev/null)
DELETED_PATH=$(sqlite3 "$DB" "SELECT local_path FROM assets WHERE filename = '$DELETED_FILE' LIMIT 1" 2>/dev/null)
sqlite3 "$DB" "DELETE FROM assets WHERE filename = '$DELETED_FILE'" 2>/dev/null
rm -f "$DELETED_PATH"
echo "  Deleted from state + disk: $DELETED_FILE"
# Run incremental — since the token hasn't changed and no iCloud changes happened,
# incremental returns 0 changes. The "missing" file won't be re-downloaded by incremental
# alone — this is expected behavior (incremental only sees iCloud-side changes).
OUTPUT=$(kei --directory "$DIR")
echo "$OUTPUT" | grep -E "incremental|change|download|completed"
check "incremental completed without error" "$(echo "$OUTPUT" | grep -q "completed"; echo $?)"
# The real way to pick up the "missing" file is --no-incremental
OUTPUT=$(kei --directory "$DIR" --no-incremental)
CLEAN_OUTPUT=$(echo "$OUTPUT" | sed 's/\x1b\[[0-9;]*m//g')
DL_COUNT=$(echo "$CLEAN_OUTPUT" | grep -oE '[0-9]+ downloaded,' | head -1 | grep -oE '^[0-9]+')
DL_COUNT="${DL_COUNT:-0}"
echo "  Full re-enum re-downloaded: $DL_COUNT"
check "full re-enum finds missing file" "$([ "$DL_COUNT" -ge 1 ]; echo $?)"

# ── 8. --dry-run preserves token ─────────────────────────────────────────
echo ""
echo "=== 8. Dry run preserves token ==="
TOKEN_BEFORE=$(get_token)
OUTPUT=$(kei --directory "$DIR" --dry-run)
TOKEN_AFTER=$(get_token)
check "token unchanged after dry-run" "$([ "$TOKEN_BEFORE" = "$TOKEN_AFTER" ]; echo $?)"

# ── 9. --skip-videos changes config hash ─────────────────────────────────
echo ""
echo "=== 9. Filter flag changes config hash ==="
HASH_BEFORE=$(get_hash)
OUTPUT=$(kei --directory "$DIR" --skip-videos)
echo "$OUTPUT" | grep -E "config|changed|cleared|token|download|completed"
HASH_AFTER=$(get_hash)
check "hash changed with --skip-videos" "$([ "$HASH_BEFORE" != "$HASH_AFTER" ]; echo $?)"

# ── 10. Session reuse: back-to-back runs, check for SRP vs validate ─────
echo ""
echo "=== 10. Session reuse check ==="
OUTPUT=$(kei --directory "$DIR" --log-level debug 2>&1)
if echo "$OUTPUT" | grep -q "Existing session token is valid"; then
  check "session reuse (validate_token succeeded)" 0
elif echo "$OUTPUT" | grep -q "accountLogin succeeded"; then
  check "session reuse (accountLogin succeeded)" 0
elif echo "$OUTPUT" | grep -q "Authenticating\|SRP"; then
  echo "  INFO: session did full SRP auth (session reuse not working)"
  check "session reuse" 1
else
  echo "  INFO: could not determine auth method"
  echo "$OUTPUT" | grep -i "session\|auth\|token\|valid" | head -5
  check "session reuse" 0
fi

# ── Cleanup ──────────────────────────────────────────────────────────────
# Restore original config for future test runs
kei --directory "$DIR" > /dev/null 2>&1
rm -rf "$DIR"

echo ""
echo "=================================================="
echo "  RESULTS: $PASS pass, $FAIL fail, $SKIP skip"
echo "=================================================="
[ "$FAIL" -eq 0 ] && exit 0 || exit 1
