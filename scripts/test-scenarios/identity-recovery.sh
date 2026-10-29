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
# Historical capture ambiguity is preserved without blocking new selected backup.
run_scenario_test lib v027_historical_pending_identity_survives_baseline_and_quiet_progress
run_scenario_test lib v027_prepared_layout_recovery_finishes_without_current_pass_or_layout
run_scenario_test lib metadata_capture_retry_changed_evidence_is_due_and_old_attempt_cannot_delay_it
# Full cycle checkpoint isolation and both streaming/collecting recovery routes.
run_scenario_test lib unresolved_identity_survives_restart_and_other_zone_success_then_recovers
run_scenario_test lib sparse_retry_omitted_source_preserves_failed_work_then_recovers_media
# Current sync preserves historical shared claims and metadata debt without backfill.
run_scenario_test lib v027_normal_selected_sync_preserves_shared_historical_claims_and_debt
run_scenario_test lib v027_baseline_preserves_queued_existing_metadata_while_new_files_get_sidecars

# Fixed seeded orderings: deferred work, regressing evidence, interruption and a quiet tail.
run_scenario_test lib current_recovery_sequence_seed_

# Synthetic current-work histories inspired by public issues, and bounded generation with reduction.
run_scenario_test lib current_reconstructed_histories
run_scenario_test lib current_generated_histories
