# Smart-selection transition coverage

The former smart_reconciliation module required existing smart-folder files to
be copied from an old template to a new template before configuration promotion.
That behavior was removed for v0.27: recorded paths, media and metadata remain
stable, while genuinely missing selected destinations are downloaded.
The nine old tests are retired for that contract, not counted as passing tests.

Current cases below run through the real sync cycle, owned state, current
provider scope, baseline boundary and new versioned tokens.

| Retired test suffix | Replacement or open disposition |
|---|---|
| promotes_after_fresh_query_and_stays_quiet | v027_hidden_template_change_preserves_recorded_paths_then_stays_quiet |
| favorites_mixed_selection_stays_quiet | v027_favorites_mixed_template_change_preserves_paths_and_current_membership |
| sidecars_survive_transition_and_restart | v027_smart_template_change_preserves_sidecar_bytes_and_timestamps |
| other_empty_library_failure_holds_promotion | v027_blocked_and_healthy_libraries_share_cycle_and_reopen_in_either_order; global promotion hold intentionally removed |
| existing_date_bound_smart_path_drift_keeps_source_hold | v027_bound_baseline_then_new_delta_asset_then_quiet_cycle_without_inventory plus template-preservation cases; old indefinite source hold removed |
| bounded_smart_reconciliation_converges_without_advancing_source | Bound baseline, v027_recent_removal_selects_unchanged_next_member_then_stays_quiet, v027_recent_short_window_without_token_cannot_authorize_baseline_bridge and v027_bound_baseline_unaccountable_inventory_keeps_cursor_unset |
| bounded_smart_reconciliation_retained_selection_debt_holds_promotion | v027_retained_retry_deferral_write_failure_keeps_cursor and v027_failed_current_transfer_does_not_starve_legacy_pending_retry; promotion coupling removed, exact deferred-selection variant not separately demonstrated |
| retains_incomplete_work_then_recovers | Partial replacement only. Current failed-transfer/reopen and independent retry cases prove durable pending work. Fault gaps below remain open. |
| bounded_smart_reconciliation_failures_retain_then_recover | Partial replacement only. Current short-window/unaccountable guards protect baseline progress. Fault gaps below remain open. |

The selected-albums case
v027_album_relation_only_changes_route_new_copies_and_unfiled_after_reopen
also proves additions/removals, retained original bytes/mtime, independently
required copies, and quiet incremental behavior.

## Open fault-specific evidence

Do not treat publication SIGKILL or retry-deferral failure as substitutes for:

- Current v27 checkpoint-write failure at its actual persistence boundary.
- Malformed later inventory page after earlier work was admitted.
- Early and late cooperative cancellation of current selected inventory.
- A stale selected plan.
- An incomplete current album membership snapshot.
- Downloaded-status write failure after actual transfer.
- Exact deferred-selection recovery under a changed smart selection.

These should be represented by small current-format fixtures when qualification
reaches those boundaries. They remain readiness gaps; removing the obsolete
promotion harness does not clear them. The two retired fault-matrix entries
and bounded_smart_reconciliation_retained_selection_debt_holds_promotion remain
open in the original failure ledger until their current safety coverage is
accounted for. Six entries can retire with replacement outcome evidence; these
three entries remain open despite removal of their obsolete harness.
