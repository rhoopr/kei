#!/usr/bin/env bash
# Convert the full-test staging JSONL into a finalized run record with
# branch/head/rustc metadata
# and run-level metrics from collect_metrics.py.
#
# Print the path of the finalized record on stdout.

set -euo pipefail

runs_dir="${KEI_FULL_TEST_RUNS_DIR:-/tmp/codex/kei/full-test/test-runs}"
current="$runs_dir/.current.jsonl"
start_file="$runs_dir/.run-started-at"
start_head_file="$runs_dir/.run-start-head"
start_worktree_file="$runs_dir/.run-start-worktree-clean"
script_dir="$(cd "$(dirname "$0")" && pwd)"

if [[ ! -s "$current" ]]; then
    echo "no phases recorded in $current" >&2
    exit 1
fi

ts=$(date +%Y%m%dT%H%M%S)
out="$runs_dir/$ts.json"

branch=$(git branch --show-current 2>/dev/null || echo "(detached)")
if [[ ! -s "$start_head_file" ]]; then
    echo "missing full-test start head: $start_head_file" >&2
    exit 1
fi
head=$(head -n 1 "$start_head_file")
end_head=$(git rev-parse HEAD 2>/dev/null || echo "(no rev)")
if [[ ! -s "$start_worktree_file" ]]; then
    echo "missing full-test start worktree state: $start_worktree_file" >&2
    exit 1
fi
start_worktree_clean=$(head -n 1 "$start_worktree_file")
if [[ -z "$(git status --porcelain=v1 --untracked-files=all)" ]]; then
    end_worktree_clean=true
else
    end_worktree_clean=false
fi
rustc=$(rustc -V 2>/dev/null || echo "(no rustc)")
if [[ -s "$start_file" ]]; then
    started_at=$(head -n 1 "$start_file")
else
    started_at=$(date +%Y-%m-%dT%H:%M:%S)
fi

# Run-level metrics. Failures here don't fail the finalize step -- prefer
# a partial record over a missing one.
metrics_json=$("$script_dir/collect_metrics.py" 2>/dev/null || echo "{}")

python3 - "$current" "$out" "$branch" "$head" "$end_head" "$start_worktree_clean" "$end_worktree_clean" "$rustc" "$started_at" "$metrics_json" <<'PY'
import json, sys
src, dst, branch, head, end_head, start_clean, end_clean, rustc, started_at, metrics_json = sys.argv[1:11]
phases = {}
with open(src) as f:
    for line in f:
        line = line.strip()
        if not line:
            continue
        rec = json.loads(line)
        phase = rec.pop("phase")
        phases[phase] = rec
# Never publish incomplete required validation as a successful run. Live and
# platform-dependent phases can skip explicitly; offline proof cannot.
required = {
    "static_checks", "offline_core", "nightly_tools", "package", "docker_full",
    "live_provider", "live_import_rehearsal", "service",
}
may_skip = {"live_provider", "live_import_rehearsal", "service"}
for phase in sorted(required):
    result = phases.get(phase, {})
    status = result.get("status")
    allowed = {"pass", "skipped", "rate_limited"} if phase in may_skip else {"pass"}
    if status not in allowed or result.get("exit", 0) != 0:
        raise SystemExit(f"required phase {phase} did not complete: {status or 'missing'}")
if any(result.get("status") == "fail" or result.get("exit", 0) != 0 for result in phases.values()):
    raise SystemExit("cannot finalize a failed phase as a successful run")
record = {
    "started_at": started_at,
    "branch": branch,
    "head": head,
    "end_head": end_head,
    "start_worktree_clean": start_clean == "true",
    "end_worktree_clean": end_clean == "true",
    "rustc": rustc,
    "phases": phases,
    "metrics": json.loads(metrics_json or "{}"),
}
with open(dst, "w") as f:
    json.dump(record, f, indent=2, sort_keys=True)
PY

# Clear staging + run marker (lets the next /full-test start cleanly).
rm -f "$current" "$runs_dir/.run-marker" "$start_file" "$start_head_file" "$start_worktree_file"
echo "$out"
