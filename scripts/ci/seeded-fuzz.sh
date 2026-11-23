#!/usr/bin/env bash
set -euo pipefail

fuzz_max_total_time="${FUZZ_MAX_TOTAL_TIME:-120}"
fuzz_timeout="${FUZZ_TIMEOUT:-10}"

toolchain=+nightly-2026-05-15
cargo "$toolchain" fuzz build
targets="$(cargo "$toolchain" fuzz list)"
test -n "$targets"
while IFS= read -r target; do
    mkdir -p "fuzz/corpus/$target"
    seeds=()
    if [[ -d "fuzz/seeds/$target" ]]; then seeds+=("fuzz/seeds/$target"); fi
    cargo "$toolchain" fuzz run "$target" "fuzz/corpus/$target" "${seeds[@]}" -- \
        -max_total_time="$fuzz_max_total_time" -timeout="$fuzz_timeout"
done <<<"$targets"
