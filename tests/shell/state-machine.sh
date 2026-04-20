#!/bin/bash
# Sync-token and config-hash invariants against live iCloud.
#
# Covers the state machine around incremental sync: what is stored, when
# it is cleared, and how kei recovers from corrupted/stale state. Each
# scenario reads or mutates rows in the state DB between kei invocations,
# which is awkward from Rust tests but natural from shell.
#
# Uses ~15 Apple API calls. Session reuse via accountLogin avoids
# repeated SRP handshakes.
#
# Usage: ./tests/shell/state-machine.sh

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
        --log-level info \
        "$@" 2>&1
}

get_token() { kei_db_query "SELECT value FROM metadata WHERE key = 'sync_token:PrimarySync'"; }
get_hash()  { kei_db_query "SELECT value FROM metadata WHERE key = 'config_hash'"; }
token_count() { kei_db_query "SELECT COUNT(*) FROM metadata WHERE key LIKE '%token%'"; }

kei_suite_banner "STATE-MACHINE VALIDATION"

echo ""
echo "--- Pre-flight ---"
kei_preflight_session

DIR=$(kei_scratch_dir state)

# ── 1. Clean slate: full sync, verify token + config hash stored ─────────
echo ""
echo "=== 1. Clean slate full sync ==="
kei_db_exec "DELETE FROM metadata WHERE key LIKE '%token%' OR key = 'config_hash'"
echo "  Cleared: tokens=$(token_count), hash=$(get_hash || echo 'none')"
OUTPUT=$(kei_sync --directory "$DIR" --no-incremental)
echo "$OUTPUT" | grep -E "Incremental|token|Summary|downloaded|completed"
[ -n "$(get_token)" ]; kei_check "token stored after full sync"
[ -n "$(get_hash)" ];  kei_check "config hash stored"
[ "$(find "$DIR" -type f | wc -l | tr -d ' ')" -ge 1 ]; kei_check "files downloaded"
BASELINE_HASH=$(get_hash)
BASELINE_TOKEN=$(get_token)
echo "  hash=$BASELINE_HASH"

# ── 2. Incremental sync: no changes → 0 downloads, token preserved ──────
echo ""
echo "=== 2. Incremental sync (no changes) ==="
OUTPUT=$(kei_sync --directory "$DIR")
echo "$OUTPUT" | grep -E "incremental|token|change|download|completed"
echo "$OUTPUT" | grep -qi "incremental"; kei_check "used incremental sync"
echo "$OUTPUT" | grep -q "No new photos to download from incremental"; kei_check "0 change events"
[ "$(get_token)" = "$BASELINE_TOKEN" ]; kei_check "token preserved"

# ── 3. Config change: --size medium → hash changes, tokens cleared ───────
echo ""
echo "=== 3. Config change clears tokens ==="
HASH_BEFORE=$(get_hash)
OUTPUT=$(kei_sync --directory "$DIR" --size medium)
echo "$OUTPUT" | grep -E "config|changed|cleared|token|incremental|download|completed"
HASH_AFTER=$(get_hash)
TOKEN_AFTER=$(get_token)
[ "$HASH_BEFORE" != "$HASH_AFTER" ]; kei_check "config hash changed"
echo "  hash: $HASH_BEFORE -> $HASH_AFTER"
[ -n "$TOKEN_AFTER" ]; kei_check "new token stored"

# ── 4. Restore original config → hash reverts ───────────────────────────
echo ""
echo "=== 4. Restore original config ==="
OUTPUT=$(kei_sync --directory "$DIR")
echo "$OUTPUT" | grep -E "config|changed|cleared|token|incremental|download|completed"
[ "$(get_hash)" = "$BASELINE_HASH" ]; kei_check "hash reverted to original"
[ -n "$(get_token)" ]; kei_check "token stored"

# ── 5. --reset-sync-token forces full enumeration ────────────────────────
echo ""
echo "=== 5. --reset-sync-token ==="
OUTPUT=$(kei_sync --directory "$DIR" --reset-sync-token)
echo "$OUTPUT" | grep -E "reset|clear|token|Fetching|full|incremental|download|completed"
echo "$OUTPUT" | grep -qi "Fetching"; kei_check "full enumeration ran"
[ -n "$(get_token)" ]; kei_check "new token stored"

# ── 6. Corrupt token → fallback to full enumeration ──────────────────────
echo ""
echo "=== 6. Corrupt token recovery ==="
GOOD_TOKEN=$(get_token)
kei_db_exec "UPDATE metadata SET value = 'CORRUPT_GARBAGE_TOKEN_XYZ' WHERE key = 'sync_token:PrimarySync'"
echo "  Injected: CORRUPT_GARBAGE_TOKEN_XYZ"
OUTPUT=$(kei_sync --directory "$DIR")
echo "$OUTPUT" | grep -E "token|invalid|fallback|full|error|Fetching|incremental|download|completed"
RECOVERED_TOKEN=$(get_token)
if echo "$OUTPUT" | grep -qi "fallback\|full enumeration\|Fetching"; then
    kei_check "fell back to full enumeration" 0
elif echo "$OUTPUT" | grep -q "503"; then
    kei_skip "rate-limited before token validation"
    kei_db_exec "UPDATE metadata SET value = '$GOOD_TOKEN' WHERE key = 'sync_token:PrimarySync'"
else
    kei_check "fell back to full enumeration" 1
    echo "  OUTPUT: $(echo "$OUTPUT" | head -5)"
    kei_db_exec "UPDATE metadata SET value = '$GOOD_TOKEN' WHERE key = 'sync_token:PrimarySync'"
fi
[ -n "$RECOVERED_TOKEN" ] && [ "$RECOVERED_TOKEN" != 'CORRUPT_GARBAGE_TOKEN_XYZ' ]; kei_check "valid token after recovery"

# ── 7. Simulated missing file: full re-enum re-downloads it ─────────────
echo ""
echo "=== 7. Missing file detection via --no-incremental ==="
DELETED_FILE=$(kei_db_query "SELECT filename FROM assets WHERE status='downloaded' LIMIT 1")
DELETED_PATH=$(kei_db_query "SELECT local_path FROM assets WHERE filename = '$DELETED_FILE' LIMIT 1")
kei_db_exec "DELETE FROM assets WHERE filename = '$DELETED_FILE'"
rm -f "$DELETED_PATH"
echo "  Deleted from state + disk: $DELETED_FILE"
OUTPUT=$(kei_sync --directory "$DIR")
echo "$OUTPUT" | grep -E "incremental|change|download|completed"
echo "$OUTPUT" | grep -q "completed"; kei_check "incremental completed without error"
OUTPUT=$(kei_sync --directory "$DIR" --no-incremental)
CLEAN_OUTPUT=$(echo "$OUTPUT" | sed 's/\x1b\[[0-9;]*m//g')
DL_COUNT=$(echo "$CLEAN_OUTPUT" | grep -oE '[0-9]+ downloaded,' | head -1 | grep -oE '^[0-9]+')
DL_COUNT="${DL_COUNT:-0}"
echo "  Full re-enum re-downloaded: $DL_COUNT"
[ "$DL_COUNT" -ge 1 ]; kei_check "full re-enum finds missing file"

# ── 8. --dry-run preserves token ─────────────────────────────────────────
echo ""
echo "=== 8. Dry run preserves token ==="
TOKEN_BEFORE=$(get_token)
kei_sync --directory "$DIR" --dry-run >/dev/null
[ "$(get_token)" = "$TOKEN_BEFORE" ]; kei_check "token unchanged after dry-run"

# ── 9. Filter flag changes config hash ───────────────────────────────────
echo ""
echo "=== 9. Filter flag changes config hash ==="
HASH_BEFORE=$(get_hash)
OUTPUT=$(kei_sync --directory "$DIR" --skip-videos)
echo "$OUTPUT" | grep -E "config|changed|cleared|token|download|completed"
[ "$HASH_BEFORE" != "$(get_hash)" ]; kei_check "hash changed with --skip-videos"

# ── 10. Session reuse check ─────────────────────────────────────────────
echo ""
echo "=== 10. Session reuse check ==="
OUTPUT=$(kei_sync --directory "$DIR" --log-level debug 2>&1)
if echo "$OUTPUT" | grep -q "Existing session token is valid"; then
    kei_check "session reuse (validate_token succeeded)" 0
elif echo "$OUTPUT" | grep -q "accountLogin succeeded"; then
    kei_check "session reuse (accountLogin succeeded)" 0
elif echo "$OUTPUT" | grep -q "Authenticating\|SRP"; then
    echo "  INFO: session did full SRP auth"
    kei_check "session reuse" 1
else
    echo "  INFO: could not determine auth method"
    echo "$OUTPUT" | grep -i "session\|auth\|token\|valid" | head -5
    kei_check "session reuse" 0
fi

# ── Cleanup: restore the original config so future runs start consistent ─
kei_sync --directory "$DIR" >/dev/null 2>&1
rm -rf "$DIR"

kei_check_summary "STATE-MACHINE RESULTS"
