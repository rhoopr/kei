#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd "$(dirname "$0")" && pwd)"
source "$script_dir/lib.sh"

run_scenario_test lib contract_file_publish_no_overwrite_destination_already_exists
run_scenario_test lib contract_file_publish_different_destination_returns_typed_collision
run_scenario_test lib sync_core::tests::icloudpd_layout::
run_scenario_test lib sync_core::tests::an_existing_file_with_the_same_size_is_adopted
