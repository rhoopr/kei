#!/usr/bin/env bash
# Master orchestrator for `just full-test`. Runs every phase in deterministic
# order with prereq checks, rate-limit handling, per-phase logs, run records,
# and a final diff_runs.sh report.
#
# Phases run in this order:
#   begin_run    -- staging + concurrency guard, clears flag files
#   prereqs      -- check_prereqs.sh; sets .live-skipped if .env / cookies bad
#   static_checks    fmt, clippy, docs, audit, workflow/script lint, contracts, typos
#   offline_core     all-feature/no-default tests + scenario catalog validation
#   nightly_tools    fuzz build when available + cargo-udeps
#   package          release build + archive smoke
#   docker_full      Docker build, PUID smoke, multiarch, CLI/default-command smokes
#   live_provider    live cargo (just test live)               --live
#   live_shell       auto-discovered shell suites              --live
#   live_binary      release-binary live smokes/import checks  --live
#   service          service-smoke when supported
#   host_service     real service lifecycle when KEI_FULL_TEST_REAL_SERVICE=1 --live
#   finalize_run + diff_runs on success
#
# Live-tagged phases honor .live-skipped (prereq fail) and .rate-limited
# (Apple 503 detected) -- both clear at begin_run, both auto-skip downstream
# live phases without LLM intervention.
#
# Exit code:
#   0   no failed phases (skips + rate-limited are not failures)
#   1   first failed phase exits non-zero immediately
#   64  unsupported local userland (missing GNU-ish helper behavior)
#   65  another /full-test run is in progress (begin_run refused)

set -euo pipefail

repo_root=$(git rev-parse --show-toplevel 2>/dev/null) || {
    echo "run_all: not in a git repo" >&2
    exit 1
}
script_dir="$(cd "$(dirname "$0")" && pwd)"
cd "$repo_root"
runs_dir="${KEI_FULL_TEST_RUNS_DIR:-/tmp/codex/kei/full-test/test-runs}"
run_started=0
current_phase="setup"
summary_path="$runs_dir/.current.jsonl"

cleanup_failed_run() {
    rc=$?
    if [[ $rc -ne 0 && $run_started -eq 1 ]]; then
        "$script_dir/render_summary.py" "$summary_path" \
            --result fail \
            --fallback-failure "$current_phase" "run stopped before this phase completed" ||
            true
        rm -f "$runs_dir/.run-marker"
    fi
    exit "$rc"
}
trap cleanup_failed_run EXIT

# --- Userland --------------------------------------------------------------
"$script_dir/check_userland.sh"

# --- Begin -----------------------------------------------------------------
run_id=$("$script_dir/begin_run.sh") || exit $?
run_started=1
echo "begin: run_id=$run_id (repo: $repo_root)" >&2

# --- Source .env so live phases see ICLOUD_USERNAME etc. ------------------
if [[ -f .env ]]; then
    set -a
    # shellcheck disable=SC1091
    source .env
    set +a
fi

current_phase="prereqs"
"$script_dir/check_prereqs.sh"

# Live env exports (lib.sh + just both consult these)
export ICLOUD_TEST_COOKIE_DIR="$repo_root/.test-cookies"
export KEI_TEST_ALBUM="${KEI_TEST_ALBUM:-kei-test}"
export KEI_DOCKER_IMAGE="${KEI_DOCKER_IMAGE:-kei:dev}"

# Keep child tempdirs out of the repo checkout. These are Codex working files,
# not retained .scratch documents.
full_tmp_dir="${KEI_FULL_TEST_TMPDIR:-/tmp/codex/kei/full-test/tmp}"
mkdir -p "$full_tmp_dir"
export TMPDIR="$full_tmp_dir"
export TEMP="$full_tmp_dir"
export TMP="$full_tmp_dir"
export KEI_TEST_SCRATCH_DIR="${KEI_TEST_SCRATCH_DIR:-$full_tmp_dir/shell}"
mkdir -p "$KEI_TEST_SCRATCH_DIR"

tp() { "$script_dir/time_phase.sh" "$@"; }
run_phase() {
    current_phase="$1"
    tp "$@"
}
run_live_phase() {
    current_phase="$1"
    tp --live "$@"
}

# --- Static + offline behavior -------------------------------------------
run_phase static_checks -- just static-checks
run_phase offline_core -- just test offline
run_phase nightly_tools -- just test nightly-tools

# --- Build/package/container ----------------------------------------------
run_phase package -- just test packaging
run_phase docker_full -- just test docker-full

# --- Live provider + shell + release-binary smokes ------------------------
run_live_phase live_provider -- env ICLOUD_TEST_COOKIE_DIR="$ICLOUD_TEST_COOKIE_DIR" just test live

current_phase="live_shell"
"$script_dir/run_shell_suites.sh"

current_phase="live_binary"
"$script_dir/run_live_smokes.sh"
run_live_phase live_import_rehearsal -- "$script_dir/run_live_import_rehearsal.sh"
if [[ -n "${KEI_FULL_TEST_CROSS_ZONE_ALBUM:-}" ]]; then
    run_live_phase live_cross_zone_album -- "$script_dir/run_cross_zone_album_hydration.sh"
fi

# --- Service smoke ---------------------------------------------------------
if ! command -v systemd-analyze >/dev/null 2>&1 && ! command -v plutil >/dev/null 2>&1; then
    "$script_dir/record_skip.sh" service skipped "not Linux or macOS"
else
    run_phase service -- just test service
fi

if [[ "${KEI_FULL_TEST_REAL_SERVICE:-0}" == "1" ]]; then
    run_live_phase host_service -- just test host-service
fi

# --- Cleanup --------------------------------------------------------------
current_phase="cleanup"
rm -rf "$full_tmp_dir/photos-test" 2>/dev/null || true

# --- Finalize + report ----------------------------------------------------
current_phase="finalize"
record=$("$script_dir/finalize_run.sh")
summary_path="$record"
echo "finalized: $record" >&2
echo
"$script_dir/diff_runs.sh"
current_phase="summary"
"$script_dir/render_summary.py" "$record" --result pass
