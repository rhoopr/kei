#!/usr/bin/env bash
# Phase 5.5 - live import-existing mini rehearsal.
#
# Seeds a tiny real photo tree with the release binary, then imports that tree
# into a fresh DB. This exercises the v0.20 TOML-first import path without
# relying on prior state in .test-cookies.

set -euo pipefail

repo_root=$(git rev-parse --show-toplevel 2>/dev/null) || {
    echo "run_live_import_rehearsal: not in a git repo" >&2
    exit 1
}
cd "$repo_root"

PROJECT_DIR="$repo_root"
# shellcheck disable=SC1091
source "$repo_root/tests/shell/lib.sh"

kei_require_env
kei_require_release_binary
kei_preflight_selection

binary=$(kei_release_bin)
recent=$(kei_live_recent)
work=$(mktemp -d "${TMPDIR:-/tmp/codex/kei/full-test/tmp}/kei-live-import-rehearsal-XXXXX")
trap 'rm -rf "$work"' EXIT

sync_data="$work/sync-data"
import_data="$work/import-data"
photos="$work/photos"
mkdir -p "$sync_data" "$import_data" "$photos"

kei_copy_session_without_state "$sync_data"
kei_copy_session_without_state "$import_data"

write_config() {
    local data_dir="$1"
    local path="$2"
    {
        printf 'data_dir = %s\n' "$(kei_toml_string "$data_dir")"
        echo
        echo "[auth]"
        printf 'username = %s\n' "$(kei_toml_string "$ICLOUD_USERNAME")"
        echo
        echo "[download]"
        printf 'directory = %s\n' "$(kei_toml_string "$photos")"
        echo
        kei_live_filters
    } >"$path"
}

sync_config="$work/sync.toml"
import_config="$work/import.toml"
write_config "$sync_data" "$sync_config"
write_config "$import_data" "$import_config"

run_and_show() {
    local name="$1"
    shift
    local out="$work/$name.out"
    local err="$work/$name.err"
    set +e
    "$@" >"$out" 2>"$err"
    local rc=$?
    set -e
    echo "--- $name stderr tail ---"
    tail -30 "$err"
    echo "--- $name stdout tail ---"
    tail -30 "$out"
    return "$rc"
}

echo "--- seed sync (bounded primary-library selection) ---"
run_and_show seed-sync \
    "$binary" sync --no-progress-bar --config "$sync_config"

file_count=$(find "$photos" -type f | wc -l | tr -d ' ')
echo "seed_files=$file_count"
if [[ "$file_count" -lt 1 ]]; then
    echo "run_live_import_rehearsal: seed sync wrote no files" >&2
    exit 1
fi

echo "--- import dry-run into fresh DB ---"
run_and_show import-dry-run \
    "$binary" import-existing --dry-run --force-empty --recent "$recent" --no-progress-bar --config "$import_config"

echo "--- import real into fresh DB ---"
run_and_show import-real \
    "$binary" import-existing --force-empty --recent "$recent" --no-progress-bar --config "$import_config"

echo "--- import repeat dry-run ---"
run_and_show import-repeat-dry-run \
    "$binary" import-existing --dry-run --force-empty --recent "$recent" --no-progress-bar --config "$import_config"

matched=$(awk -F: '/Files matched/ { gsub(/[[:space:]]/, "", $2); print $2 }' "$work/import-real.out" | tail -1)
unmatched=$(awk -F: '/Unmatched versions/ { gsub(/[[:space:]]/, "", $2); print $2 }' "$work/import-real.out" | tail -1)
hash_errors=$(awk -F: '/Hash errors/ { gsub(/[[:space:]]/, "", $2); print $2 }' "$work/import-real.out" | tail -1)

if [[ -z "$matched" || "$matched" -lt 1 ]]; then
    echo "run_live_import_rehearsal: import matched no files" >&2
    exit 1
fi
if [[ "${unmatched:-999}" != "0" ]]; then
    echo "run_live_import_rehearsal: import left unmatched versions: $unmatched" >&2
    exit 1
fi
if [[ "${hash_errors:-999}" != "0" ]]; then
    echo "run_live_import_rehearsal: import had hash errors: $hash_errors" >&2
    exit 1
fi

db=$(find "$import_data" -maxdepth 1 -name '*.db' | head -1)
if [[ -z "$db" ]]; then
    echo "run_live_import_rehearsal: import DB was not created" >&2
    exit 1
fi

downloaded=$(sqlite3 "$db" "SELECT COUNT(*) FROM assets WHERE status='downloaded'" 2>/dev/null || echo 0)
echo "import_matched=$matched"
echo "db_downloaded=$downloaded"
if [[ "$downloaded" -lt "$matched" ]]; then
    echo "run_live_import_rehearsal: DB downloaded count $downloaded is less than matched $matched" >&2
    exit 1
fi

echo "live import rehearsal passed"
