#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd "$(dirname "$0")" && pwd)"
source "$script_dir/lib.sh"

# Consumer matrices: malformed/epoch dates, mixed visibility, restart and recovery.
run_scenario_test lib hidden_invalid_capture_date
# Historical ownership and failed durable retry writes.
run_scenario_test lib run_cycle_legacy_owner_guard_preserves_dates_and_checkpoint
run_scenario_test lib bounded_full_sync_hydrates_live_legacy_pending_master
# Deferral, authoritative changed evidence, completion and unchanged follow-up.
run_scenario_test lib run_cycle_metadata_capture_retry_preserves_durable_checkpoint_until_repaired
run_scenario_test lib ambiguous_capture_repair_keeps_identity_and_checkpoint_without_full_backfill
run_scenario_test lib metadata_capture_retry_changed_evidence_is_due_and_old_attempt_cannot_delay_it
# Full cycle checkpoint isolation and both streaming/collecting recovery routes.
run_scenario_test lib unresolved_identity_survives_restart_and_other_zone_success_then_recovers
run_scenario_test lib sparse_retry_omitted_source_preserves_failed_work_then_recovers_media
# Schema-28 preservation keeps unattributed history separate from current children.
run_scenario_test lib run_cycle_ambiguous_children_preserved_independently
run_scenario_test lib run_cycle_single_survivor_preserved
