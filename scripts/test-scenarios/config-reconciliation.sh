#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd "$(dirname "$0")" && pwd)"
source "$script_dir/lib.sh"

run_scenario_test lib confined_path_

# Planning changes keep stored paths and owe only what the new settings select.
run_scenario_test lib sync_core::state::tests::plans_keep_paths_and_replace_only_through_pending
run_scenario_test lib sync_core::tests::the_date_range_limits_which_photos_are_saved
