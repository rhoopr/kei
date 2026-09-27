#!/usr/bin/env bash
# Phase 5 -- live binary smokes against the production release binary.
# Each smoke wraps a single CLI subcommand through time_phase.sh --live so
# the rate-limit / prereq-skip flags are honored automatically.
#
# Required env when live phases are not being skipped:
#   ICLOUD_USERNAME  iCloud account (sourced from .env by the orchestrator)
#
# Optional env:
#   KEI_TEST_DATA_DIR  cookie / db dir (default .test-cookies under repo)
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
DD="${KEI_TEST_DATA_DIR:-$repo_root/.test-cookies}"
DOWNLOAD_DIR="${KEI_TEST_DOWNLOAD_DIR:-${TMPDIR:-/tmp/codex/kei/full-test/tmp}/photos-test}"
mkdir -p "$DOWNLOAD_DIR"

sync_config="$(kei_write_sync_config "$DD" "$DOWNLOAD_DIR")"

run() {
    local phase="$1"
    shift
    "$time_phase" --live "$phase" -- "$@"
}

# Wrappers for commands that need shell composition (rc check, etc.).
verify_wrapper() {
    set +e
    env ICLOUD_USERNAME="$USR" KEI_DATA_DIR="$DD" "$binary" verify
    rc=$?
    set -e
    # rc=2 is clap parse error; everything else (including non-zero data
    # mismatches) means the command at least reached the binary correctly.
    [[ $rc -ne 2 ]]
}
export -f verify_wrapper
export USR DD binary

run live_status env ICLOUD_USERNAME="$USR" KEI_DATA_DIR="$DD" "$binary" status
run live_libraries env ICLOUD_USERNAME="$USR" KEI_DATA_DIR="$DD" "$binary" list libraries
run live_albums env ICLOUD_USERNAME="$USR" KEI_DATA_DIR="$DD" "$binary" list albums
run live_dryrun env ICLOUD_USERNAME="$USR" KEI_DATA_DIR="$DD" "$binary" sync --dry-run --recent 5 --config "$sync_config"
run live_config_show env ICLOUD_USERNAME="$USR" KEI_DATA_DIR="$DD" "$binary" config show
run live_verify bash -c verify_wrapper
run live_reconcile_dryrun env ICLOUD_USERNAME="$USR" KEI_DATA_DIR="$DD" "$binary" reconcile --dry-run
run live_password_backend env ICLOUD_USERNAME="$USR" KEI_DATA_DIR="$DD" "$binary" password backend
run live_import_dryrun env ICLOUD_USERNAME="$USR" KEI_DATA_DIR="$DD" "$binary" import-existing --dry-run --recent 5 --config "$sync_config"
