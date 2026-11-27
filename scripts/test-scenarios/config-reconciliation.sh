#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd "$(dirname "$0")" && pwd)"
source "$script_dir/lib.sh"

run_scenario_test lib local_reconciliation_copy_preserves_source_and_refuses_conflict
run_scenario_test lib reconciliation_leaf
run_scenario_test lib confined_path_
run_scenario_test lib reconciliation_confined_
run_scenario_test lib issue_770_recovery

# Planning changes keep stored paths and owe only what the new settings select.
run_scenario_test lib sync_core::state::tests::plans_keep_paths_and_replace_only_through_pending
run_scenario_test lib sync_core::tests::the_date_range_limits_which_photos_are_saved
