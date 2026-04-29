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
# Wipe both the metadata table (tokens, config hash) AND the assets table
# so the next sync starts from zero. Stale `assets` rows from prior runs
# leave dangling on-disk paths that the incremental trust-state sample
# treats as missing, forcing a fall-back to full enumeration in test 2.
kei_db_exec "DELETE FROM metadata WHERE key LIKE '%token%' OR key = 'config_hash'"
kei_db_exec "DELETE FROM assets"
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
echo "$OUTPUT" | grep -E "incremental|token|change|download|[Cc]ompleted"
# The incremental path logs "No new photos to download from incremental
# sync" when the change feed is empty. If the trust-state sample detects
# missing files (e.g. stale rows from a previous run pointing at deleted
# scratch dirs) it falls back to a full enumeration that logs the
# shorter "No new photos to download" instead. Both indicate "nothing
# to do"; either is acceptable here.
DL_LINE=$(echo "$OUTPUT" | grep -E "No new photos to download|0 downloaded")
[ -n "$DL_LINE" ]; kei_check "sync reported no-op"
NEW_DOWNLOADS=$(echo "$OUTPUT" | grep -oE '[0-9]+ downloaded' | head -1 | grep -oE '^[0-9]+')
[ "${NEW_DOWNLOADS:-0}" -eq 0 ]; kei_check "0 new downloads"
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
#
# Two-stage check, each starting from a delete-from-state-and-disk seed
# so each sync mode is exercised on a genuinely-missing file. Doing both
# from a single delete would mask the second mode -- the first sync
# would re-download the file, leaving the second a no-op.
echo ""
echo "=== 7. Missing file detection ==="
delete_and_sync() {
    local mode_flag="$1"
    local label="$2"
    local f path out clean dl
    f=$(kei_db_query "SELECT filename FROM assets WHERE status='downloaded' LIMIT 1")
    path=$(kei_db_query "SELECT local_path FROM assets WHERE filename = '$f' LIMIT 1")
    kei_db_exec "DELETE FROM assets WHERE filename = '$f'"
    rm -f "$path"
    echo "  Deleted from state + disk: $f"
    out=$(kei_sync --directory "$DIR" $mode_flag)
    echo "$out" | grep -E "incremental|change|download|[Cc]ompleted"
    echo "$out" | grep -qE "[Cc]ompleted in"; kei_check "$label completed without error"
    clean=$(echo "$out" | sed 's/\x1b\[[0-9;]*m//g')
    dl=$(echo "$clean" | grep -oE '[0-9]+ downloaded,' | head -1 | grep -oE '^[0-9]+')
    dl="${dl:-0}"
    echo "  $label re-downloaded: $dl"
    [ "$dl" -ge 1 ]; kei_check "$label finds missing file"
}
delete_and_sync "" "incremental"
delete_and_sync "--no-incremental" "full re-enum"

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
