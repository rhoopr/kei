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
| bounded_smart_reconciliation_retained_selection_debt_holds_promotion | Current admitted-work quiet retry and unconfirmed-selection cursor replay in selected_coverage::interruptions; old global promotion coupling removed. Arbitrary historical configuration is not execution authority. |
| retains_incomplete_work_then_recovers | Current checkpoint-write, early/late cancellation, stale-plan, downloaded-status, missing confirmation and reopen controls below. |
| bounded_smart_reconciliation_failures_retain_then_recover | Current malformed-later-page and incomplete-album inventory controls below, plus existing bounded-window and unaccountable-inventory checkpoint guards. |

The selected-albums case
v027_album_relation_only_changes_route_new_copies_and_unfiled_after_reopen
also proves additions/removals, retained original bytes/mtime, independently
required copies, and quiet incremental behavior.

## Current interruption evidence

The interruption fixtures use normal owned run cycles and reopen the database:

- A trigger rejects the actual v27 checkpoint write after publication; the old
  cursor remains and replay does not transfer the file again.
- A later inventory response has an invalid records type after an earlier
  selected file has published; completed work survives recovery.
- Early cancellation fires during selected inventory, not library setup.
  Late cancellation occurs after publication. Both retain the source cursor.
- A stale selected plan cannot advance progress.
- A trigger rejects downloaded status for the new asset after file publication.
  Reopen finishes the retained work without another transfer.
- A failed admitted transfer retains its unfinished destination. With a quiet
  changes feed, reopen resolves that same owner without rank inventory.
- A current identity that cannot be confirmed is not given an invented
  download manifest. Its source cursor stays held; replay restores the input,
  then a later quiet cycle performs no inventory or transfer.
- Real selected albums exercise missing lookup confirmation and, separately,
  malformed selected-album inventory. The former discovered the missing
  hydration checkpoint veto. Both recover with unchanged existing content.

These are distinct guarantees. An admitted retry is not an unconfirmed
identity; an empty changes feed is not permission to omit observations after a
withheld cursor. Arbitrary historical configuration/pass identifiers are not
authorization to publish into a current destination. Existing historical
preservation tests remain required; this suite does not certify all old debt.

The old smart-folder promotion harness can retire as a whole once these focused
results and review are recorded. This does not retire broader metadata, layout,
raw-response or checkpoint suites: they also contain retained-operation,
ownership and no-loss assertions that still apply.
