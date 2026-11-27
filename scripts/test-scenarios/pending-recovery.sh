#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd "$(dirname "$0")" && pwd)"
source "$script_dir/lib.sh"

run_scenario_test lib sync_core::tests::an_interrupted_listing_resumes_from_its_last_saved_page
run_scenario_test lib sync_core::tests::a_rejected_position_relists_without_downloading_again
run_scenario_test lib sync_core::state::tests::contract_position_commits_with_observations
run_scenario_test lib sync_core::tests::confirmed_damage_is_replaced_by_the_next_sync_only_while_unchanged
run_scenario_test lib sync_core::tests::a_replacement_published_before_a_crash_is_not_downloaded_again
run_scenario_test lib sync_core::tests::metadata_pass::kei_bytes_published_before_a_crash_are_not_taken_for_an_edit
