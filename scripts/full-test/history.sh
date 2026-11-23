#!/usr/bin/env bash
# Print a compact history table of the last N /full-test runs.
#
# Usage: history.sh [N]   (default 10)
#
# Emits one row per run, newest first. Columns: timestamp, branch, HEAD,
# total wall, total tests, then run-level metrics. Useful for spotting
# slow-creep regressions that diff_runs.sh (only N vs N-1) misses.

set -euo pipefail

runs_dir="${KEI_FULL_TEST_RUNS_DIR:-/tmp/codex/kei/full-test/test-runs}"
n="${1:-10}"

mapfile -t recs < <(find "$runs_dir" -maxdepth 1 -name '*.json' -type f -printf '%f\n' | sort -r | head -n "$n")
if [[ ${#recs[@]} -eq 0 ]]; then
    echo "(no run records found)" >&2
    exit 1
fi

python3 - "$runs_dir" "${recs[@]}" <<'PY'
import json, sys
from pathlib import Path

runs_dir = Path(sys.argv[1])
files = sys.argv[2:]

# (label, json_key_in_metrics_or_top_level, source, width)
COLS = [
    ("Date",       "_date",                 "derived", 10),
    ("Time",       "_time",                 "derived",  5),
    ("Branch",     "branch",                "top",     20),
    ("HEAD",       "head",                  "top",      8),
    ("Wall",       "_total_wall",           "derived",  6),
    ("Tests",      "_total_tests",          "derived",  5),
    ("Bin(MB)",    "binary_mb",             "metric",   7),
    ("Img(MB)",    "docker_image_mb",       "metric",   7),
    ("Deps",       "deps_count",            "metric",   4),
    ("Unw",        "src_unwrap_expect",     "metric",   3),
    ("All",        "src_allow_attrs",       "metric",   3),
    ("Fuz",        "fuzz_target_count",     "metric",   3),
    ("MiB tx",     "_bytes_transferred",    "derived",  7),
]

def derive(rec, key):
    if key == "_date":
        return (rec.get("started_at") or "")[:10]
    if key == "_time":
        return (rec.get("started_at") or "")[11:16]
    if key == "_total_wall":
        s = sum(p.get("wall_s", 0) for p in rec.get("phases", {}).values())
        return f"{s:.0f}s" if s else "-"
    if key == "_total_tests":
        s = sum(p.get("tests", 0) for p in rec.get("phases", {}).values() if isinstance(p.get("tests"), int))
        return str(s) if s else "-"
    if key == "_bytes_transferred":
        s = sum(p.get("bytes_transferred_mib", 0) for p in rec.get("phases", {}).values())
        return f"{s:.1f}" if s else "-"
    return "-"

def cell(rec, col):
    label, key, source, width = col
    if source == "derived":
        v = derive(rec, key)
    elif source == "top":
        v = rec.get(key) or "-"
    else:  # metric
        m = (rec.get("metrics") or {})
        v = m.get(key)
        v = "-" if v is None else str(v)
    s = str(v)
    if len(s) > width:
        s = s[:width-1] + "…"
    return s.ljust(width)

# Header
print("  ".join(c[0].ljust(c[3]) for c in COLS))
print("  ".join("-" * c[3]               for c in COLS))

# Rows (newest first; we already sorted -r and took head N)
for f in files:
    with open(runs_dir / f) as fh:
        rec = json.load(fh)
    print("  ".join(cell(rec, c) for c in COLS))

print()
print(f"({len(files)} runs shown)")
PY
