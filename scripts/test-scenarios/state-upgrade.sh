#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd "$(dirname "$0")" && pwd)"
source "$script_dir/lib.sh"

run_scenario_test lib sync_core::tests::contract_upgrade_cleanup_requires_validated_state_reuses_copies_then_removes_old_state
run_scenario_test lib sync_core::tests::an_upgrade_refuses_another_accounts_state
run_scenario_test lib sync_core::tests::an_upgrade_reads_the_released_v0240_schema_after_ownership_is_bound
run_scenario_test lib sync_core::access::tests::
