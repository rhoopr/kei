#!/bin/bash
# Sync-position and planning invariants against live iCloud.
#
# Covers the state the sync core keeps between runs: the library's saved
# position, the planning fingerprint, and the saved copies. Each scenario
# reads or changes rows in the state DB between kei invocations, which is
# awkward from Rust tests but natural from shell.
#
# Runs in its own data dir seeded with the listed catalog, so the shared
# session dir never holds state. Two scenarios list the whole library again
# (about six minutes each on a 40k-record account).
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

KEI="$(kei_release_bin)"
kei_check_init

kei_sync() {
    local download_dir="${1:?download dir required}"
    shift
    local config
    local log_level="${KEI_SYNC_LOG_LEVEL:-info}"
    config="$(kei_write_sync_config "$DATA" "$download_dir")"
    # Shared single-pass primary-library selection bounds provider work.
    KEI_DATA_DIR="$DATA" "$KEI" sync \
        --config "$config" \
        --no-progress-bar \
        --log-level "$log_level" \
        "$@" 2>&1
}

kei_reconcile() {
    local config
    config="$(kei_write_sync_config "$DATA" "$DIR")"
    KEI_DATA_DIR="$DATA" "$KEI" reconcile --config "$config" "$@" 2>&1
}

get_position() { kei_db_query "SELECT position FROM zones WHERE zone = 'PrimarySync'"; }
get_plan_hash() { kei_db_query "SELECT value FROM kv WHERE key = 'plan_config'"; }
saved_count() { kei_db_query "SELECT COUNT(*) FROM files WHERE state = 'done' AND wanted = 1"; }
disk_count() { find "$DIR" -type f | wc -l | tr -d ' '; }

# The sync reported downloading N files.
downloaded_is() {
    [ "$(kei_synced "$OUTPUT" downloaded)" = "$1" ]
}
check_no_relisting() {
    if grep -q "Listing the whole library" <<<"$OUTPUT"; then
        kei_check "$1" 1
    else
        kei_check "$1" 0
    fi
}
check_position() {
    [ -n "$(get_position)" ]
    kei_check "listed library keeps its position"
}

kei_suite_banner "STATE-MACHINE VALIDATION"

echo ""
echo "--- Pre-flight ---"
kei_preflight_session

DATA=$(kei_scratch_dir state-data)
kei_copy_session_without_state "$DATA"
kei_seed_listed_state "$DATA" || exit 1
export ICLOUD_TEST_COOKIE_DIR="$DATA"
DIR=$(kei_scratch_dir state)

# ── 1. First sync from the listed catalog ────────────────────────────────
echo ""
echo "=== 1. First sync ==="
OUTPUT=$(kei_sync "$DIR")
kei_check "sync exit status" "$?"
echo "$OUTPUT" | grep -E "Library synced|Saved copies|Backup status"
check_no_relisting "seeded state reads only the changes since its listing"
check_position
[ -n "$(get_plan_hash)" ]
kei_check "planning fingerprint stored"
[ "$(disk_count)" -ge 1 ]
kei_check "files downloaded"
[ "$(disk_count)" -eq "$(saved_count)" ]
kei_check "every file on disk is a saved copy"
BASELINE_HASH=$(get_plan_hash)
BASELINE_FILES=$(disk_count)
DRIFT_PATH=$(kei_db_query "SELECT path FROM files WHERE state = 'done' AND wanted = 1 ORDER BY path LIMIT 1")
echo "  plan_hash=$BASELINE_HASH files=$BASELINE_FILES"

# ── 2. Repeat sync: nothing changed, nothing downloaded ──────────────────
echo ""
echo "=== 2. Repeat sync (no changes) ==="
OUTPUT=$(kei_sync "$DIR")
kei_check "sync exit status" "$?"
echo "$OUTPUT" | grep -E "Library synced"
downloaded_is 0
kei_check "0 new downloads"
check_no_relisting "repeat sync reads only the changes since"
check_position

# ── 3. A config change plans every photo again without listing ──────────
echo ""
echo "=== 3. Config change plans again ==="
OUTPUT=$(KEI_SYNC_FILTERS_TOML=$'exclude = ["live-photo-videos"]\n' kei_sync "$DIR")
kei_check "component-exclusion sync exit status" "$?"
echo "$OUTPUT" | grep -E "Library synced"
[ "$(get_plan_hash)" != "$BASELINE_HASH" ]
kei_check "planning fingerprint changed"
check_no_relisting "a config change keeps the saved position"
downloaded_is 0
kei_check "excluding a component downloads nothing"
[ "$(disk_count)" -eq "$BASELINE_FILES" ]
kei_check "copies that stop being selected keep their files"

# ── 4. Restore the original config ──────────────────────────────────────
echo ""
echo "=== 4. Restore original config ==="
OUTPUT=$(kei_sync "$DIR")
kei_check "sync exit status" "$?"
[ "$(get_plan_hash)" = "$BASELINE_HASH" ]
kei_check "planning fingerprint reverted"
downloaded_is 0
kei_check "selecting the kept files again downloads nothing"
[ "$(disk_count)" -eq "$(saved_count)" ]
kei_check "every file is a saved copy again"

# ── 5. reset sync-token lists the whole library again ───────────────────
echo ""
echo "=== 5. reset sync-token ==="
KEI_DATA_DIR="$DATA" "$KEI" reset sync-token --yes >/dev/null
kei_check "reset sync-token exit status" "$?"
[ -z "$(get_position)" ]
kei_check "reset forgets the position"
OUTPUT=$(kei_sync "$DIR")
kei_check "sync after reset exit status" "$?"
grep -q "Listing the whole library" <<<"$OUTPUT"
kei_check "the library is listed from the start"
downloaded_is 0
kei_check "listing again downloads nothing already saved"
check_position

# ── 6. A position iCloud rejects is forgotten and listed again ──────────
echo ""
echo "=== 6. Rejected position recovery ==="
kei_db_exec "UPDATE zones SET position = 'CORRUPT_GARBAGE_TOKEN_XYZ' WHERE zone = 'PrimarySync'"
OUTPUT=$(kei_sync "$DIR")
kei_check "sync with a rejected position exit status" "$?"
echo "$OUTPUT" | grep -E "rejected|Listing the whole library|Library synced"
grep -q "iCloud rejected the saved sync position" <<<"$OUTPUT"
kei_check "the rejected position is reported"
[ "$(get_position)" != 'CORRUPT_GARBAGE_TOKEN_XYZ' ] && [ -n "$(get_position)" ]
kei_check "a valid position replaces it"
downloaded_is 0
kei_check "recovery downloads nothing already saved"

# ── 7. A deleted file comes back through reconcile ──────────────────────
echo ""
echo "=== 7. Missing file recovery ==="
if [ -z "$DRIFT_PATH" ] || [ ! -f "$DIR/$DRIFT_PATH" ]; then
    echo "ABORT: selected baseline drift fixture is missing" >&2
    exit 1
fi
DRIFT_SIZE=$(wc -c <"$DIR/$DRIFT_PATH" | tr -d ' ')
rm -f "$DIR/$DRIFT_PATH"
echo "  Deleted one saved file from disk"
OUTPUT=$(kei_reconcile --dry-run)
kei_check "reconcile --dry-run exit status" "$?"
grep -qF "MISSING: $DIR/$DRIFT_PATH" <<<"$OUTPUT"
kei_check "reconcile lists the missing file"
OUTPUT=$(kei_reconcile)
kei_check "reconcile exit status" "$?"
grep -q "1 missing files will be downloaded again" <<<"$OUTPUT"
kei_check "reconcile owes exactly that copy again"
OUTPUT=$(kei_sync "$DIR")
kei_check "sync after reconcile exit status" "$?"
downloaded_is 1
kei_check "the next sync downloads it"
[ -f "$DIR/$DRIFT_PATH" ] && [ "$(wc -c <"$DIR/$DRIFT_PATH" | tr -d ' ')" = "$DRIFT_SIZE" ]
kei_check "the file is back at its path with its size"

# ── 8. --dry-run changes nothing ────────────────────────────────────────
echo ""
echo "=== 8. Dry run changes nothing ==="
POSITION_BEFORE=$(get_position)
SAVED_BEFORE=$(saved_count)
OUTPUT=$(kei_sync "$DIR" --dry-run)
kei_check "dry-run exit status" "$?"
grep -q "Dry run: no files or state were changed" <<<"$OUTPUT"
kei_check "dry run says nothing changed"
[ "$(get_position)" = "$POSITION_BEFORE" ] && [ "$(saved_count)" = "$SAVED_BEFORE" ]
kei_check "position and saved copies unchanged after dry-run"

# ── 9. A media filter changes the planning fingerprint ──────────────────
echo ""
echo "=== 9. Media filter changes the planning fingerprint ==="
OUTPUT=$(KEI_SYNC_FILTERS_TOML=$'exclude = ["videos"]\n' kei_sync "$DIR")
kei_check "media-filter sync exit status" "$?"
[ "$(get_plan_hash)" != "$BASELINE_HASH" ]
kei_check "planning fingerprint changed with media filter"

# ── 10. Session reuse check ─────────────────────────────────────────────
echo ""
echo "=== 10. Session reuse check ==="
OUTPUT=$(KEI_SYNC_LOG_LEVEL=debug kei_sync "$DIR")
kei_check "session-reuse sync exit status" "$?"
if grep -q "Existing session token is valid" <<<"$OUTPUT"; then
    kei_check "session reuse (validate_token succeeded)" 0
elif grep -q "accountLogin succeeded" <<<"$OUTPUT"; then
    kei_check "session reuse (accountLogin succeeded)" 0
elif grep -q "Session validated recently, skipping /validate call" <<<"$OUTPUT"; then
    kei_check "session reuse (cached validation)" 0
elif grep -q "Authenticating\|SRP" <<<"$OUTPUT"; then
    echo "  INFO: session did full SRP auth"
    kei_check "session reuse" 1
else
    echo "  INFO: could not determine auth method"
    echo "$OUTPUT" | grep -i "session\|auth\|token\|valid" | head -5
    kei_check "session reuse" 1
fi

rm -rf "$DIR" "$DATA"

kei_check_summary "STATE-MACHINE RESULTS"
