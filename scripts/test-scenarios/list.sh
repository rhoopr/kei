#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd "$(dirname "$0")" && pwd)"
for script in "$script_dir"/*.sh; do
    name=$(basename "$script" .sh)
    [[ "$name" == "list" || "$name" == "lib" || "$name" == "check" ]] && continue
    printf '%s\n' "$name"
done | sort
