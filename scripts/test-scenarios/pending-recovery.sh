#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd "$(dirname "$0")" && pwd)"
source "$script_dir/lib.sh"

run_scenario_test lib issue_770_recovery
run_scenario_test lib sync_core::tests::an_interrupted_listing_resumes_from_its_last_saved_page
run_scenario_test lib sync_core::tests::a_rejected_position_relists_without_downloading_again
run_scenario_test lib sync_core::state::tests::contract_position_commits_with_observations
run_scenario_test lib sync_core::tests::confirmed_damage_is_replaced_by_the_next_sync_only_while_unchanged
