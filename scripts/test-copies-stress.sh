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
log=$(mktemp)
trap 'rm -f "$log"' EXIT
"$cargo_bin" test --locked --all-features --lib "$test_name" -- --ignored --exact --list >"$log"
if ! grep -Fxq "$test_name: test" "$log" || [[ $(grep -c ': test$' "$log") != 1 ]]; then
    echo "copies stress: expected exactly one ignored test: $test_name" >&2
    cat "$log" >&2
    exit 1
fi
[[ $# -eq 0 ]] || exit 0
"$cargo_bin" test --locked --all-features --lib "$test_name" -- --ignored --exact --test-threads=1 | tee "$log"
grep -q '^test result: ok\. 1 passed; 0 failed; 0 ignored;' "$log"
