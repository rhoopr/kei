#!/usr/bin/env bash
set -euo pipefail

# --ignored and --exact alone still succeed when a rename or cfg selects zero.
# Check both the ignored catalog and the execution summary.
test_name=download::orchestration::queue_projection::tests::private_300_current_writers_and_600_aliases_finish_through_reseeds_without_repeated_writes
cargo_bin="${CARGO:-cargo}"
if [[ $# -gt 1 || ($# -eq 1 && "$1" != --check) ]]; then
    echo "usage: $0 [--check]" >&2
    exit 2
fi
work=$(mktemp -d)
log="$work/test.log"
trap 'rm -rf "$work"' EXIT
# Compile once, then use that exact harness for discovery and execution.
"$cargo_bin" test --locked --all-features --lib --no-run --message-format=json >"$work/build.jsonl"
binary=$(
    python3 - "$work/build.jsonl" <<'PYTHON'
import json, sys
from pathlib import Path
artifacts = [json.loads(line) for line in Path(sys.argv[1]).read_text().splitlines()]
binaries = [a["executable"] for a in artifacts if a.get("reason") == "compiler-artifact"
            and a.get("profile", {}).get("test") and "lib" in a.get("target", {}).get("kind", [])
            and a.get("executable")]
if len(binaries) != 1:
    raise SystemExit("copies stress: expected exactly one compiled library harness")
print(binaries[0])
PYTHON
)
"$binary" "$test_name" --ignored --exact --list >"$log"
if ! grep -Fxq "$test_name: test" "$log" || [[ $(grep -c ': test$' "$log") != 1 ]]; then
    echo "copies stress: expected exactly one ignored test: $test_name" >&2
    cat "$log" >&2
    exit 1
fi
[[ $# -eq 0 ]] || exit 0
"$binary" "$test_name" --ignored --exact --test-threads=1 | tee "$log"
grep -q '^test result: ok\. 1 passed; 0 failed; 0 ignored;' "$log"
