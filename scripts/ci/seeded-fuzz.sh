#!/usr/bin/env bash
set -euo pipefail

toolchain=+nightly-2026-05-15
cargo "$toolchain" fuzz build
targets="$(cargo "$toolchain" fuzz list)"
test -n "$targets"
while IFS= read -r target; do
    mkdir -p "fuzz/corpus/$target"
    seeds=()
    if [[ -d "fuzz/seeds/$target" ]]; then seeds+=("fuzz/seeds/$target"); fi
    cargo "$toolchain" fuzz run "$target" "fuzz/corpus/$target" "${seeds[@]}" -- -max_total_time=10 -timeout=10
done <<<"$targets"
