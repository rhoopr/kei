#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd "$(dirname "$0")" && pwd)"
source "$script_dir/lib.sh"

run_scenario_test lib hard_delete
run_scenario_test lib sync_core::tests::a_new_photo_needs_only_its_own_work
run_scenario_test lib sync_core::tests::contract_sync_never_deletes_album_paths_are_stable_and_departed_copies_stay
run_scenario_test lib sync_core::tests::contract_unknown_provider_identity_remains_pending_until_its_master_arrives
run_scenario_test lib sync_core::tests::shared_library_photos_follow_their_personal_library_albums
run_scenario_test lib sync_core::tests::smart_folders_take_their_photos_out_of_unfiled
run_scenario_test lib sync_core::state::tests::memberships_and_masters_mark_assets_for_planning
