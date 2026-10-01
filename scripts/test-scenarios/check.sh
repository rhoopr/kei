#!/usr/bin/env bash
# Validate focused filters without replaying tests covered by the offline suite.
# Catalogs use default features, just like the focused commands.
set -euo pipefail

script_dir="$(cd "$(dirname "$0")" && pwd)"
scenario_catalog_dir=$(mktemp -d)
trap 'rm -rf "$scenario_catalog_dir"' EXIT
scenarios=$("$script_dir/list.sh")
[[ -n "$scenarios" ]] || {
    echo "no scenarios found" >&2
    exit 2
}
while read -r scenario; do
    # Source runners so only this process can select catalog-only validation.
    # shellcheck disable=SC1090
    source "$script_dir/$scenario.sh"
done <<<"$scenarios"
echo "scenario catalogs validated (no test replay)"
