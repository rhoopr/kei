#!/usr/bin/env python3
"""Compute patch coverage and per-file file-level deltas.

Outputs JSON to stdout:
  {
    "total_covered": int,        # patch lines hit by tests
    "total_eligible": int,       # patch lines that have coverage data
    "percent": float | null,     # patch coverage %
    "files": [
      {
        "file": str,
        "covered": int,          # patch lines covered in this file
        "total": int,            # patch lines eligible in this file
        "percent": float,        # patch coverage % for this file
        "file_percent_head": float | null,  # full-file line coverage on head
        "file_percent_base": float | null,  # full-file line coverage on base
        "file_delta": float | null,         # head - base
      },
      ...
    ]
  }

A line is "eligible" if it has a DA: entry in the LCOV report (i.e. the
compiler emitted coverage instrumentation for it). Lines that are pure
whitespace, comments, or non-executable declarations have no DA: entry and
are excluded from the denominator -- they're not testable.
"""

import argparse
import json
import re
import subprocess
import sys
from collections import defaultdict
from pathlib import Path

HUNK_RE = re.compile(r"^@@ -\d+(?:,\d+)? \+(\d+)(?:,(\d+))? @@")


def parse_lcov(path: Path) -> dict[str, dict[int, int]]:
    """Parse LCOV into {file: {line: hit_count}}."""
    coverage: dict[str, dict[int, int]] = defaultdict(dict)
    current: str | None = None
    for raw in path.read_text().splitlines():
        if raw.startswith("SF:"):
            current = raw[3:]
        elif raw == "end_of_record":
            current = None
        elif raw.startswith("DA:") and current is not None:
            line_str, hits_str = raw[3:].split(",", 1)
            coverage[current][int(line_str)] = int(hits_str)
    return dict(coverage)


def parse_diff_added_lines(base: str) -> dict[str, set[int]]:
    """Return {relative_file_path: set(line_numbers)} for added/modified .rs lines."""
    diff = subprocess.check_output(
        ["git", "diff", "--unified=0", f"{base}...HEAD", "--", "*.rs"],
        text=True,
    )
    added: dict[str, set[int]] = defaultdict(set)
    current: str | None = None
    for line in diff.splitlines():
        if line.startswith("+++ b/"):
            current = line[6:]
        elif line.startswith("+++"):
            current = None
        elif current and (m := HUNK_RE.match(line)):
            start = int(m.group(1))
            count = int(m.group(2)) if m.group(2) else 1
            for i in range(count):
                added[current].add(start + i)
    return dict(added)


def normalize_paths(coverage: dict[str, dict[int, int]], workspace: Path) -> dict[str, dict[int, int]]:
    """Map absolute LCOV paths to repo-relative paths."""
    out: dict[str, dict[int, int]] = {}
    for path, lines in coverage.items():
        try:
            rel = str(Path(path).resolve().relative_to(workspace))
        except ValueError:
            rel = path
        out[rel] = lines
    return out


def file_pct(line_hits: dict[int, int]) -> float | None:
    """Whole-file line coverage % from a {line: hits} map."""
    if not line_hits:
        return None
    covered = sum(1 for hits in line_hits.values() if hits > 0)
    return covered / len(line_hits) * 100


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--base", required=True, help="base ref or SHA to diff against")
    ap.add_argument("--lcov", required=True, type=Path, help="path to head LCOV report")
    ap.add_argument("--base-lcov", type=Path, default=None, help="optional base LCOV for delta")
    ap.add_argument("--workspace", default=".", type=Path, help="repo root for path normalization")
    args = ap.parse_args()

    workspace = args.workspace.resolve()
    head_cov = normalize_paths(parse_lcov(args.lcov), workspace)
    base_cov: dict[str, dict[int, int]] = {}
    if args.base_lcov is not None and args.base_lcov.exists():
        base_cov = normalize_paths(parse_lcov(args.base_lcov), workspace)

    diff = parse_diff_added_lines(args.base)

    files = []
    total_covered = 0
    total_eligible = 0
    for path in sorted(diff):
        added_lines = diff[path]
        head_file = head_cov.get(path, {})
        eligible = sorted(line for line in added_lines if line in head_file)
        if not eligible:
            continue
        covered = sum(1 for line in eligible if head_file[line] > 0)

        head_pct = file_pct(head_file)
        base_pct = file_pct(base_cov.get(path, {})) if path in base_cov else None
        delta = (head_pct - base_pct) if (head_pct is not None and base_pct is not None) else None

        files.append({
            "file": path,
            "covered": covered,
            "total": len(eligible),
            "percent": covered / len(eligible) * 100,
            "file_percent_head": head_pct,
            "file_percent_base": base_pct,
            "file_delta": delta,
        })
        total_covered += covered
        total_eligible += len(eligible)

    result = {
        "total_covered": total_covered,
        "total_eligible": total_eligible,
        "percent": (total_covered / total_eligible * 100) if total_eligible else None,
        "files": files,
    }
    json.dump(result, sys.stdout, indent=2)
    sys.stdout.write("\n")


if __name__ == "__main__":
    main()
