#!/usr/bin/env bash
# Local-only release qualification. Hosted CI owns offline/platform validation.
set -euo pipefail
repo_root=$(git rev-parse --show-toplevel)
cd "$repo_root"
[[ "${KEI_FULL_TEST_REAL_SERVICE:-0}" == 1 ]] || {
    echo "local release checks require explicit KEI_FULL_TEST_REAL_SERVICE=1 authorization" >&2
    exit 64
}
[[ -z "$(git status --porcelain=v1 --untracked-files=all)" ]] || {
    echo "local release qualification requires a clean exact-commit checkout" >&2
    exit 1
}
export KEI_FULL_TEST_RUNS_DIR="${KEI_FULL_TEST_RUNS_DIR:-/tmp/codex/kei/local-release/test-runs}"
export KEI_FULLTEST_LOG_DIR="${KEI_FULLTEST_LOG_DIR:-/tmp/codex/kei/local-release/logs}"
export KEI_TEST_SCRATCH_DIR="${KEI_TEST_SCRATCH_DIR:-/tmp/codex/kei/local-release/shell}"
export TMPDIR="${KEI_FULL_TEST_TMPDIR:-/tmp/codex/kei/local-release/tmp}"
export TMP="$TMPDIR" TEMP="$TMPDIR"
mkdir -p "$TMPDIR" "$KEI_TEST_SCRATCH_DIR" "$KEI_FULLTEST_LOG_DIR"
scripts/full-test/begin_run.sh
trap 'rm -f "$KEI_FULL_TEST_RUNS_DIR/.run-marker"' EXIT
if [[ -f .env ]]; then
    set -a
    # shellcheck disable=SC1091
    source .env
    set +a
fi
scripts/full-test/check_prereqs.sh
[[ ! -e "$KEI_FULL_TEST_RUNS_DIR/.live-skipped" ]] || {
    echo "local release prerequisites are unavailable; no qualification recorded" >&2
    exit 1
}
export ICLOUD_TEST_COOKIE_DIR="$repo_root/.test-cookies"
export KEI_DOCKER_IMAGE="${KEI_DOCKER_IMAGE:-kei:local-release}"
phase() { scripts/full-test/time_phase.sh "$@"; }
# These builds supply executables for live checks; no offline suite is replayed.
phase build_release -- cargo build --locked --release
phase build_live_container -- docker build -t "$KEI_DOCKER_IMAGE" .
phase --live live_provider -- just test live
scripts/full-test/run_shell_suites.sh
scripts/full-test/run_live_smokes.sh
phase --live live_import_rehearsal -- scripts/full-test/run_live_import_rehearsal.sh
if [[ -n "${KEI_FULL_TEST_CROSS_ZONE_ALBUM:-}" ]]; then
    phase --live live_cross_zone_album -- scripts/full-test/run_cross_zone_album_hydration.sh
fi
phase --live host_service -- just test host-service
python3 scripts/ci/local_release.py finalize --repo rhoopr/kei --sha "$(git rev-parse HEAD)" --report "$KEI_FULL_TEST_RUNS_DIR/qualified.json"
