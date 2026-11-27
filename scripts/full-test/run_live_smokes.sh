#!/usr/bin/env bash
# Phase 5 -- live binary smokes against the production release binary.
# Each smoke wraps a single CLI subcommand through time_phase.sh --live so
# the rate-limit / prereq-skip flags are honored automatically.
#
# Required env when live phases are not being skipped:
#   ICLOUD_USERNAME  iCloud account (sourced from .env by the orchestrator)
#
# Optional env:
#   KEI_TEST_DATA_DIR  data dir to run against as is (default: a scratch dir
#                      with the .test-cookies session and a freshly listed
#                      catalog, so the shared session dir never holds state)
#   KEI_TEST_DOWNLOAD_DIR  temp dir for sync/import dry-run (default under TMPDIR)
#
# Adding a new CLI subcommand: add a smoke line below. Don't add destructive
# (reset) or interactive (login) commands -- they're covered elsewhere.

set -euo pipefail

repo_root=$(git rev-parse --show-toplevel 2>/dev/null || pwd)
PROJECT_DIR="$repo_root"
script_dir="$(cd "$(dirname "$0")" && pwd)"
time_phase="$script_dir/time_phase.sh"
binary="$("$script_dir/cargo_target_dir.sh")/release/kei"
# shellcheck disable=SC1091
source "$repo_root/tests/shell/lib.sh"

if [[ ! -x "$binary" ]]; then
    echo "run_live_smokes: missing release binary at $binary (run Phase 1 first)" >&2
    exit 1
fi

USR="${ICLOUD_USERNAME:-missing@example.invalid}"
DOWNLOAD_DIR="${KEI_TEST_DOWNLOAD_DIR:-${TMPDIR:-/tmp/codex/kei/full-test/tmp}/photos-test}"
kei_live_fixed_date >/dev/null || exit 1
mkdir -p "$DOWNLOAD_DIR"
if [[ -n "${KEI_TEST_DATA_DIR:-}" ]]; then
    DD="$KEI_TEST_DATA_DIR"
    seed_data=0
else
    DD=$(mktemp -d "${TMPDIR:-/tmp/codex/kei/full-test/tmp}/kei-live-smokes-XXXXX")
    trap 'rm -rf "$DD"' EXIT
    seed_data=1
fi

sync_config="$(kei_write_sync_config "$DD" "$DOWNLOAD_DIR")"

run() {
    local phase="$1"
    shift
    "$time_phase" --live "$phase" -- "$@"
}

# Wrappers for commands that need shell composition (rc check, etc.).
verify_wrapper() {
    set +e
    env ICLOUD_USERNAME="$USR" KEI_DATA_DIR="$DD" "$binary" verify --config "$sync_config"
    rc=$?
    set -e
    # rc=2 is clap parse error; everything else (including non-zero data
    # mismatches) means the command at least reached the binary correctly.
    [[ $rc -ne 2 ]]
}
# List the library once into the scratch data dir (about six minutes on a
# 40k-record account), so the smokes below read a real catalog.
seed_wrapper() {
    # shellcheck disable=SC1091
    source "$PROJECT_DIR/tests/shell/lib.sh"
    kei_require_env
    kei_preflight_selection || return 1
    kei_copy_session_without_state "$DD"
    kei_seed_listed_state "$DD" || return 1
    rm -rf "$(kei_listed_state_dir)"
}
export -f verify_wrapper seed_wrapper
export USR DD binary PROJECT_DIR sync_config

if [[ "$seed_data" == 1 ]]; then
    run live_smoke_listing bash -c seed_wrapper
fi

run live_status env ICLOUD_USERNAME="$USR" KEI_DATA_DIR="$DD" "$binary" status
run live_libraries env ICLOUD_USERNAME="$USR" KEI_DATA_DIR="$DD" "$binary" list libraries
run live_albums env ICLOUD_USERNAME="$USR" KEI_DATA_DIR="$DD" "$binary" list albums
run live_dryrun env ICLOUD_USERNAME="$USR" KEI_DATA_DIR="$DD" "$binary" sync --dry-run --config "$sync_config"
run live_config_show env ICLOUD_USERNAME="$USR" KEI_DATA_DIR="$DD" "$binary" config show
run live_verify bash -c verify_wrapper
run live_reconcile_dryrun env ICLOUD_USERNAME="$USR" KEI_DATA_DIR="$DD" "$binary" reconcile --dry-run --config "$sync_config"
run live_password_backend env ICLOUD_USERNAME="$USR" KEI_DATA_DIR="$DD" "$binary" password backend
run live_import_dryrun env ICLOUD_USERNAME="$USR" KEI_DATA_DIR="$DD" "$binary" import-existing --dry-run --config "$sync_config"
