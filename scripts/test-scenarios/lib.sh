#!/usr/bin/env bash

run_scenario_test() {
    local target="$1"
    local filter="$2"
    local cargo_bin="${CARGO:-cargo}"
    local -a target_args
    if [[ -z "$filter" ]]; then
        echo "scenario runner: empty filter for target=$target" >&2
        return 2
    fi

    case "$target" in
        lib)
            target_args=(--lib)
            ;;
        test:*)
            target_args=(--test "${target#test:}")
            ;;
        *)
            echo "scenario runner: unsupported target '$target'" >&2
            return 2
            ;;
    esac

    local listed
    if [[ -n "${scenario_catalog_dir:-}" ]]; then
        # check.sh owns this private cache for one catalog-only validation run.
        local catalog="$scenario_catalog_dir/${target//:/_}"
        if [[ ! -f "$catalog" ]]; then
            if ! "$cargo_bin" test "${target_args[@]}" -- --list >"$catalog"; then
                echo "scenario runner: could not list target=$target" >&2
                return 1
            fi
        fi
        listed=$(awk -v filter="$filter" '/: test$/ { name=substr($0, 1, length($0)-6); if (index(name, filter)) print }' "$catalog")
    elif ! listed=$("$cargo_bin" test "${target_args[@]}" "$filter" -- --list); then
        echo "scenario runner: could not list target=$target filter=$filter" >&2
        return 1
    fi
    if ! grep -q ': test$' <<<"$listed"; then
        echo "scenario runner: no tests matched target=$target filter=$filter" >&2
        return 2
    fi

    [[ -z "${scenario_catalog_dir:-}" ]] || return 0
    "$cargo_bin" test "${target_args[@]}" "$filter"
}
