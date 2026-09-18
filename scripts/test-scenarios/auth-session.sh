#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd "$(dirname "$0")" && pwd)"
source "$script_dir/lib.sh"

run_scenario_test lib session_error_reauth_tries_persisted_session_before_stripping
run_scenario_test lib clear_validation_cache_for_reauth_preserves_routing_state
run_scenario_test lib live_validate_success_uses_existing_session_even_with_hsa_flags
run_scenario_test lib send_2fa_push_treats_fresh_validation_cache_as_authenticated
run_scenario_test lib send_2fa_push_treats_live_validate_success_as_authenticated
run_scenario_test lib get_code
run_scenario_test lib reauth
run_scenario_test lib provider_session
