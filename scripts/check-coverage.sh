#!/usr/bin/env bash
# Enforces coverage thresholds from coverage-thresholds.json against
# cargo-llvm-cov JSON output. Exits non-zero if any threshold is violated.
#
# Usage: scripts/check-coverage.sh coverage.json [coverage-thresholds.json]
set -euo pipefail

COVERAGE_JSON="${1:?Usage: check-coverage.sh <coverage.json> [thresholds.json]}"
THRESHOLDS="${2:-coverage-thresholds.json}"

if ! command -v jq &>/dev/null; then
  echo "error: jq is required" >&2
  exit 1
fi

failed=0

# --- Overall threshold ---
overall_threshold=$(jq '.overall' "$THRESHOLDS")
overall_coverage=$(jq '.data[0].totals.lines.percent' "$COVERAGE_JSON")

echo "=== Overall Coverage ==="
printf "  Coverage:  %s%%\n" "$overall_coverage"
printf "  Threshold: %s%%\n" "$overall_threshold"

if [ "$(echo "$overall_coverage < $overall_threshold" | bc -l)" -eq 1 ]; then
  echo "::error::Overall coverage ${overall_coverage}% is below ${overall_threshold}% threshold"
  failed=1
else
  echo "  PASS"
fi
echo ""

# --- Per-file thresholds ---
file_keys=$(jq -r '.files | keys[]' "$THRESHOLDS")

if [ -n "$file_keys" ]; then
  echo "=== Per-File Coverage ==="
  while IFS= read -r file; do
    threshold=$(jq -r --arg f "$file" '.files[$f]' "$THRESHOLDS")

    # Find the file in coverage data (strip leading src/ for matching)
    coverage=$(jq -r --arg f "$file" '
      .data[0].files[]
      | select(.filename | endswith($f))
      | .summary.lines.percent
    ' "$COVERAGE_JSON")

    if [ -z "$coverage" ]; then
      printf "  %-45s  SKIP (not in coverage data)\n" "$file"
      continue
    fi

    if [ "$(echo "$coverage < $threshold" | bc -l)" -eq 1 ]; then
      printf "  %-45s  %6s%% < %s%%  FAIL\n" "$file" "$coverage" "$threshold"
      echo "::error::${file} coverage ${coverage}% is below ${threshold}% threshold"
      failed=1
    else
      printf "  %-45s  %6s%% >= %s%%  PASS\n" "$file" "$coverage" "$threshold"
    fi
  done <<< "$file_keys"
fi

echo ""
if [ "$failed" -eq 1 ]; then
  echo "Coverage check FAILED"
  exit 1
else
  echo "Coverage check PASSED"
fi
