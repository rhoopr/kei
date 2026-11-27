#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd "$(dirname "$0")" && pwd)"
source "$script_dir/lib.sh"
run_scenario_test lib sync_core::tests::an_expired_url_is_refreshed_once_in_the_same_cycle
run_scenario_test lib sync_core::tests::full_listing_saves_unfiled_and_album_copies_then_stays_quiet
