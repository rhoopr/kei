# Live-test migration coverage map

Issue #763 replaces account-content assumptions, not provider or safety proof.
The maintainer approved this migration after #793 and #815 completed. The
baseline is PR #849 commit `8802d17`. No production behavior changes are in scope.

## Content-dependent replacements

Names in the left column are in `tests/sync.rs`, unless qualified otherwise.
Replacement names are library tests under the applicable production owner.
Create and pass each replacement before removing its live counterpart.

| Live assertions to replace | Retained or new deterministic test | Concrete proof |
| --- | --- | --- |
| `sync_skip_videos_excludes_video_files`, `sync_skip_photos_excludes_image_files`, `sync_skip_live_photos_excludes_companions` | `bundled_media_filters_select_exact_assets` | Mixed JPEG, standalone MOV and authentic Live Photo; exact files, excluded requests, retained standalone video, restart and second sync |
| `sync_date_filters_exclude_by_creation_date` | `bundled_date_filters_select_exact_assets` | Controlled capture dates and both bounds; retain offline CLI interval parsing coverage |
| `sync_size_medium_produces_smaller_files`, `sync_force_resolution_succeeds_when_available` | `bundled_rendition_selection_and_fallback` | Exact medium bytes and URL, original fallback, forced missing rendition, RAW fallback |
| `sync_name_id7_appends_asset_id`, `sync_keep_unicode_preserves_special_chars`, `sync_custom_folder_structure` | `bundled_filename_and_folder_policies` | Exact identity suffix, Unicode and sanitized names, album and unfiled year templates, bytes and durable paths |
| `sync_raw_policy_controls_raw_naming` | `bundled_raw_and_companion_naming` | Real DNG/JPEG alternatives and all RAW naming policies |
| `sync_live_photo_mov_policy_controls_naming` | `bundled_raw_and_companion_naming`, `bundled_live_photo_modes_preserve_pair_and_companion_naming` | Both companion naming policies and all four Live Photo modes with a real pair |
| `sync_set_exif_datetime_embeds_date`, `sync_set_exif_rating_embeds_rating`, `sync_set_exif_gps_embeds_gps`, `sync_set_exif_description_embeds_description`, `sync_embed_xmp_writes_xmp_packet` | `bundled_metadata_roundtrips` | Exact source metadata and XMP values, missing metadata, preserved image payload, source/local checksum roles and steady state; native EXIF without XMP |
| `sync_xmp_sidecar_writes_sidecar_file` | `bundled_sidecar_roundtrip` | Parsed sidecar values, unchanged media, no repeat write or orphan temporary files |
| `sync_embed_xmp_on_heic_succeeds_and_keeps_the_file` | `bundled_heif_prepublication_xmp_preserves_media_and_checksum_roles` | Five independent HEIF/AVIF layouts, rating 5, non-XMP item preservation, independent pixel audit, distinct checksums and restart |
| `tests/import_existing_live.rs::roundtrip_skip_videos_sync_skips_imported_photos`, `roundtrip_name_id7_sync_skips_after_import` | `bundled_import_media_filter_roundtrip` | Real import into file-backed SQLite then sync of selected photos without HTTP downloads; excluded video absent; default and identity-suffixed paths |
| `tests/shell/concurrency.sh` fixed-date read-only-directory case | `bundled_partial_failure_recovers_after_restart` | Mixed success/failure, partial outcome, durable failed work, no overwrite, restart retry, unchanged third cycle |

`sync_album_downloads_all_asset_types` becomes a bounded, format-independent
live download test. Its exact-format assertions remain covered by
`bundled_media_download_finalize_reopen_and_second_sync` for all 13 media files.
The import scan-limit test keeps its bounded live case; its multi-pass cap is
covered by `bundled_import_recent_caps_each_pass` rather than enumerating all
account albums. `classify_exit_error_partial_sync_uses_exit_partial` retains
the exact exit-code-2 assertion for the removed shell partial-failure case.

## Bounded checkpoint coverage

A recent-limited pass cannot checkpoint an incomplete library inventory. Live
checks require either a stored token after proven EOF or the explicit
`recent_limited_full_enumeration` hold with no token. The second run must do no
new downloads in either mode. Config changes inspect pending reconciliation
as well as the committed hash. Positive incremental and corrupt-token live
checks run only when the bounded selection proves EOF; large libraries retain
the checkpoint-hold, reset, config-drift, and file-recovery checks instead.

| Adapted live assertion | Unconditional deterministic proof |
| --- | --- |
| `sync_incremental_second_run_skips_download` becomes `sync_bounded_second_run_preserves_checkpoint_and_skips_download`; shell token creation and second cycle | `run_cycle_recent_exact_inventory_stores_zone_token`, `watch_recent_exact_first_cycle_seeds_incremental_token`, `full_sync_recent_download_saves_token_when_cap_does_not_bind` |
| Shell config hash promotion after resolution/media changes | `run_cycle_enum_config_drift_atomically_promotes_bridged_checkpoint`, `enum_config_hash_drift_stages_reconciliation_and_preserves_tokens`, `enum_config_revert_clears_pending_reconciliation` |
| Shell corrupt-token fallback when a live token exists | `run_cycle_provider_session_failures_preserve_and_recover_checkpoints` (invalid-token full fallback, failed recovery, successful retry and steady state); `run_cycle_failed_token_repair_preserves_prior_sqlite_checkpoint` |
| Shell missing-file discovery after forced full enumeration | `bundled_invalid_download_retains_retry_evidence_then_recovers_after_restart`, retained live deleted/truncated-file recovery |

Do not fabricate a valid checkpoint or enumerate the full library to make a
bounded live test take the incremental path.

## Retained live responsibilities

Keep authentication/session reuse, album/library output shape, bounded
enumeration/download, dry-run, idempotency/incremental sync, state/checkpoints,
watch/report/notification/process behavior, deleted/truncated-file recovery,
and Docker/release/import/service smokes. File recovery must accept any media
format. Keep the generated nonexistent-selector cases. Keep the separate
opt-in cross-zone fixture unchanged.

All general live paths use `live-selection.toml`. Explicit negative selector
cases may override selectors; scan-limit tests may lower the shared bound.
Preflight reports the bounded selection and observed eligible filename count,
without exposing filenames or requiring specific formats. Empty selection is
a failure, not a passing skip. Test state and download directories stay isolated. Live import seeds use fresh
per-run trees, so previous cached media cannot change collision naming.

## Safety and validation

Fixture tests enter real production orchestration with controlled provider/HTTP
boundaries, checked-in bytes, `TempDir`, and file-backed SQLite. Assert initial
state, controlled input/failure, final files and rows, restart, and unchanged
repeat behavior where applicable. Keep all existing safety-contract tests.
Preserve CLI/config parsing coverage separately from content compatibility.

Run both feature modes, extracted-package checks, affected scenario slices,
the gate, all live tests single-threaded, and full-test. Compare baseline/head
test inventories. Prove the forbidden-album-consumer guard with a deliberate
violation. Record actual results in the PR; this map is not a claim of a pass.
