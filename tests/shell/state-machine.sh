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
KEI="$(kei_release_bin)"
kei_check_init

kei_sync() {
    local download_dir="${1:?download dir required}"
    shift
    local config
    local log_level="${KEI_SYNC_LOG_LEVEL:-info}"
    config="$(kei_write_sync_config "$COOKIES" "$download_dir")"
    # Shared single-pass primary-library selection bounds provider work.
    KEI_DATA_DIR="$COOKIES" "$KEI" sync \
        --password "$ICLOUD_PASSWORD" \
        --config "$config" \
        --no-progress-bar \
        --log-level "$log_level" \
        "$@" 2>&1
}

get_token() { kei_db_query "SELECT value FROM metadata WHERE key = 'sync_token:PrimarySync'"; }
get_enum_hash() { kei_db_query "SELECT value FROM metadata WHERE key = 'enum_config_hash'"; }
get_effective_enum_hash() {
    kei_db_query "SELECT value FROM metadata WHERE key IN ('pending_enum_config_hash', 'enum_config_hash') ORDER BY key DESC LIMIT 1"
}
check_checkpoint() {
    if echo "$OUTPUT" | grep -q 'recent_limited_full_enumeration'; then
        [ -z "$(get_token)" ]
        kei_check "bounded incomplete inventory does not checkpoint"
    else
        [ -n "$(get_token)" ]
        kei_check "complete inventory stores checkpoint"
    fi
}
token_count() { kei_db_query "SELECT COUNT(*) FROM metadata WHERE key LIKE '%token%'"; }

kei_suite_banner "STATE-MACHINE VALIDATION"

echo ""
echo "--- Pre-flight ---"
kei_preflight_session

DIR=$(kei_scratch_dir state)

# ── 1. Clean slate: full sync, verify token + enum config hash stored ────
echo ""
echo "=== 1. Clean slate full sync ==="
# Wipe both the metadata table (tokens, config hashes) AND the assets table
# so the next sync starts from zero. Stale `assets` rows from prior runs
# leave dangling on-disk paths that the incremental trust-state sample
# treats as missing, forcing a fall-back to full enumeration in test 2.
kei_db_exec "DELETE FROM metadata WHERE key LIKE '%token%' OR key IN ('config_hash', 'enum_config_hash', 'pending_enum_config_hash')"
kei_db_exec "DELETE FROM assets"
echo "  Cleared: tokens=$(token_count), enum_hash=$(get_enum_hash || echo 'none')"
OUTPUT=$(kei_sync "$DIR")
kei_check "sync exit status" "$?"
echo "$OUTPUT" | grep -E "Incremental|token|Summary|downloaded|completed"
check_checkpoint
[ -n "$(get_enum_hash)" ]
kei_check "enum config hash stored"
[ "$(find "$DIR" -type f | wc -l | tr -d ' ')" -ge 1 ]
kei_check "files downloaded"
BASELINE_ENUM_HASH=$(get_enum_hash)
BASELINE_TOKEN=$(get_token)
echo "  enum_hash=$BASELINE_ENUM_HASH"

# ── 2. Incremental sync: no changes → 0 downloads, token preserved ──────
echo ""
echo "=== 2. Bounded repeat sync (no changes) ==="
OUTPUT=$(kei_sync "$DIR")
kei_check "sync exit status" "$?"
echo "$OUTPUT" | grep -E "incremental|token|change|download|[Cc]ompleted"
# The incremental path logs "No new photos to download from incremental
# sync" when the change feed is empty. If the trust-state sample detects
# missing files (e.g. stale rows from a previous run pointing at deleted
# scratch dirs) it falls back to a full enumeration that logs the
# shorter "No new photos to download" instead. Both indicate "nothing
# to do"; either is acceptable here.
DL_LINE=$(echo "$OUTPUT" | grep -E "No new photos to download|All incremental assets already downloaded|0 downloaded")
[ -n "$DL_LINE" ]
kei_check "sync reported no-op"
NEW_DOWNLOADS=$(echo "$OUTPUT" | grep -oE '[0-9]+ downloaded' | head -1 | grep -oE '^[0-9]+')
[ "${NEW_DOWNLOADS:-0}" -eq 0 ]
kei_check "0 new downloads"
[ "$(get_token)" = "$BASELINE_TOKEN" ]
kei_check "checkpoint preserved"
check_checkpoint

# ── 3. Config change: medium resolution -> enum hash changes ─────────────
echo ""
echo "=== 3. Config change stages reconciliation ==="
ENUM_HASH_BEFORE=$(get_effective_enum_hash)
OUTPUT=$(KEI_SYNC_PHOTOS_TOML=$'resolution = "medium"\n' kei_sync "$DIR")
kei_check "resolution-change sync exit status" "$?"
echo "$OUTPUT" | grep -E "config|changed|cleared|token|incremental|download|completed"
ENUM_HASH_AFTER=$(get_effective_enum_hash)
[ "$ENUM_HASH_BEFORE" != "$ENUM_HASH_AFTER" ]
kei_check "enum config hash changed"
echo "  enum_hash: $ENUM_HASH_BEFORE -> $ENUM_HASH_AFTER"
check_checkpoint

# ── 4. Restore original config → hash reverts ───────────────────────────
echo ""
echo "=== 4. Restore original config ==="
OUTPUT=$(kei_sync "$DIR")
kei_check "sync exit status" "$?"
echo "$OUTPUT" | grep -E "config|changed|cleared|token|incremental|download|completed"
[ "$(get_effective_enum_hash)" = "$BASELINE_ENUM_HASH" ]
kei_check "enum hash reverted to original"
check_checkpoint

# ── 5. reset sync-token forces full enumeration ─────────────────────
echo ""
echo "=== 5. reset sync-token ==="
KEI_DATA_DIR="$COOKIES" "$KEI" reset sync-token --yes >/dev/null
OUTPUT=$(KEI_SYNC_LOG_LEVEL=debug kei_sync "$DIR")
kei_check "reset sync exit status" "$?"
echo "$OUTPUT" | grep -E "reset|clear|token|Fetching|full|incremental|download|completed"
echo "$OUTPUT" | grep -qi "Fetching\|full enumeration"
kei_check "full enumeration ran"
check_checkpoint

# ── 6. Corrupt token → fallback to full enumeration ──────────────────────
echo ""
echo "=== 6. Corrupt token recovery ==="
if [ -n "$(get_token)" ]; then
    kei_db_exec "UPDATE metadata SET value = 'CORRUPT_GARBAGE_TOKEN_XYZ' WHERE key = 'sync_token:PrimarySync'"
    OUTPUT=$(KEI_SYNC_LOG_LEVEL=debug kei_sync "$DIR")
    SYNC_RC=$?
    kei_check "corrupt-token sync succeeds" "$SYNC_RC"
    echo "$OUTPUT" | grep -qi "fallback\|full enumeration\|Fetching"
    kei_check "fell back to full enumeration"
    [ "$(get_token)" != 'CORRUPT_GARBAGE_TOKEN_XYZ' ]
    kei_check "corrupt token replaced"
    check_checkpoint
else
    # No valid token exists after a bounded partial inventory. Do not bypass
    # the checkpoint guard to force incremental mode against live account data.
    echo "  Partial inventory: positive incremental fallback covered offline"
    check_checkpoint
fi

# ── 7. Simulated missing file: full re-enum re-downloads it ─────────────
#
# A checkpointed account needs a forced full enumeration to find local
# state/disk drift. A bounded account without a token already enumerates.
echo ""
echo "=== 7. Missing file detection ==="
delete_one_downloaded_file() {
    local path
    path=$(kei_db_query "SELECT local_path FROM assets WHERE status='downloaded' ORDER BY local_path LIMIT 1")
    kei_db_exec "DELETE FROM assets WHERE local_path = $(kei_sql_string "$path")"
    rm -f "$path"
    echo "  Deleted one selected file from state + disk"
}
sync_and_count_downloads() {
    local label="$1"
    local out clean dl
    out=$(kei_sync "$DIR")
    kei_check "$label exit status" "$?"
    echo "$out" | grep -E "incremental|change|download|[Cc]ompleted"
    echo "$out" | grep -qE "No new photos|[Cc]ompleted"
    kei_check "$label completed without error"
    # Parameter expansion uses glob patterns and cannot express ANSI SGR sequences.
    # shellcheck disable=SC2001
    clean=$(echo "$out" | sed 's/\x1b\[[0-9;]*m//g')
    dl=$(echo "$clean" | grep -oE '[0-9]+ downloaded,' | head -1 | grep -oE '^[0-9]+')
    dl="${dl:-0}"
    echo "  $label downloads: $dl"
    DL_RESULT="$dl"
}
HAD_TOKEN=$(get_token)
delete_one_downloaded_file
sync_and_count_downloads "bounded repeat"
if [ -n "$HAD_TOKEN" ]; then
    [ "$DL_RESULT" -eq 0 ]
    kei_check "incremental leaves non-delta local drift for full reconcile"
else
    [ "$DL_RESULT" -ge 1 ]
    kei_check "bounded full enumeration finds missing file"
    delete_one_downloaded_file
fi
KEI_DATA_DIR="$COOKIES" "$KEI" reset sync-token --yes >/dev/null
[ -z "$(get_token)" ]
kei_check "reset removes checkpoint"
sync_and_count_downloads "full re-enum"
[ "$DL_RESULT" -ge 1 ]
kei_check "full re-enum finds missing file"

# ── 8. --dry-run preserves token ─────────────────────────────────────────
echo ""
echo "=== 8. Dry run preserves token ==="
TOKEN_BEFORE=$(get_token)
kei_sync "$DIR" --dry-run >/dev/null
kei_check "dry-run exit status" "$?"
[ "$(get_token)" = "$TOKEN_BEFORE" ]
kei_check "token unchanged after dry-run"

# ── 9. Filter flag changes enum config hash ──────────────────────────────
echo ""
echo "=== 9. Filter flag changes enum config hash ==="
ENUM_HASH_BEFORE=$(get_effective_enum_hash)
OUTPUT=$(KEI_SYNC_FILTERS_TOML=$'media = ["photos", "live-photos"]\n' kei_sync "$DIR")
kei_check "media-filter sync exit status" "$?"
echo "$OUTPUT" | grep -E "config|changed|cleared|token|download|completed"
[ "$ENUM_HASH_BEFORE" != "$(get_effective_enum_hash)" ]
kei_check "enum hash changed with media filter"

# ── 10. Session reuse check ─────────────────────────────────────────────
echo ""
echo "=== 10. Session reuse check ==="
OUTPUT=$(KEI_SYNC_LOG_LEVEL=debug kei_sync "$DIR")
kei_check "session-reuse sync exit status" "$?"
if echo "$OUTPUT" | grep -q "Existing session token is valid"; then
    kei_check "session reuse (validate_token succeeded)" 0
elif echo "$OUTPUT" | grep -q "accountLogin succeeded"; then
    kei_check "session reuse (accountLogin succeeded)" 0
elif echo "$OUTPUT" | grep -q "Session validated recently, skipping /validate call"; then
    kei_check "session reuse (cached validation)" 0
elif echo "$OUTPUT" | grep -q "Authenticating\|SRP"; then
    echo "  INFO: session did full SRP auth"
    kei_check "session reuse" 1
else
    echo "  INFO: could not determine auth method"
    echo "$OUTPUT" | grep -i "session\|auth\|token\|valid" | head -5
    kei_check "session reuse" 1
fi

# ── Cleanup: restore the original config so future runs start consistent ─
kei_sync "$DIR" >/dev/null 2>&1
rm -rf "$DIR"

kei_check_summary "STATE-MACHINE RESULTS"
