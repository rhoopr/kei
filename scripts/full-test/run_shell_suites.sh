#!/usr/bin/env bash
# Phase 4 -- enumerate tests/shell/*.sh and run each as a --live phase.
# A new script dropped in tests/shell/ is picked up automatically; phase
# name is test_shell_<basename-with-dashes-as-underscores>.
#
# Required env (set by the orchestrator):
#   KEI_DOCKER_IMAGE   default kei:dev (must match Phase 2 build tag)
#
# Each shell suite hits Apple via the live binary, so they run as --live
# phases. The rate-limit + prereq-skip flags are honored automatically.

set -euo pipefail

repo_root=$(git rev-parse --show-toplevel 2>/dev/null || pwd)
script_dir="$(cd "$(dirname "$0")" && pwd)"
time_phase="$script_dir/time_phase.sh"
shell_dir="$repo_root/tests/shell"

if [[ ! -d "$shell_dir" ]]; then
    echo "run_shell_suites: $shell_dir missing" >&2
    exit 1
fi

image="${KEI_DOCKER_IMAGE:-kei:dev}"
shell_scratch="${KEI_TEST_SCRATCH_DIR:-/tmp/codex/kei/full-test/shell}"
mkdir -p "$shell_scratch"

for sh in "$shell_dir"/*.sh; do
    [[ -f "$sh" ]] || continue
    base=$(basename "$sh" .sh)
    [[ "$base" == "lib" ]] && continue
    phase="test_shell_${base//-/_}"
    "$time_phase" --live "$phase" -- \
        env \
        KEI_DOCKER_IMAGE="$image" \
        KEI_TEST_SCRATCH_DIR="$shell_scratch" \
        "$sh"
done
