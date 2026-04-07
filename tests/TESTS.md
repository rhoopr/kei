# Test Suite Reference

## Overview

| File | Tests | Auth Required | Network |
|------|------:|:---:|:---:|
| Unit tests (`src/`) | 974 | No | No |
| `cli.rs` | 79 | No | No |
| `state.rs` | 10 | No | No |
| `state_auth.rs` | 13 (ignored) | Yes | Yes |
| `sync.rs` | 27 (ignored) | Yes | Yes |
| `setup_auth.rs` | 1 (ignored) | Yes | Yes |
| **Total** | **1104** | | |

## Running Tests

```sh
# Pre-commit safe (no auth, no network)
cargo test --bin kei --test cli --test state

# Live iCloud tests (requires pre-auth session + icloudpd-test album)
cargo test --test sync --test state_auth -- --ignored --test-threads=1

# Full suite (requires pre-auth session + icloudpd-test album)
./tests/run-all-tests.sh

# Single test
cargo test --test sync sync_dry_run_downloads_nothing -- --ignored --test-threads=1
```

See `tests/README.md` for setup instructions.

---

## Unit Tests (`cargo test --bin kei`)

974 tests across 31 source modules. All offline, no credentials needed. Covers
CLI parsing, config, download pipeline, path resolution, EXIF, iCloud API
client, session management, SRP auth, state DB, retry logic, and shutdown.

---

## CLI Tests (`tests/cli.rs`)

79 tests. Pure argument parsing — no network, no credentials. Validates that
every subcommand, flag, and enum value is accepted or rejected correctly.

### Help Output (8 tests)

| Test | Confirms |
|------|----------|
| `help_flag_succeeds` | `--help` exits 0, prints "Download iCloud photos and videos" |
| `help_lists_all_subcommands` | Help output contains all 8 subcommands |
| `sync_help_succeeds` | `sync --help` shows `--directory` |
| `status_help_succeeds` | `status --help` shows `--failed` |
| `reset_state_help_succeeds` | `reset-state --help` shows `--yes` |
| `import_existing_help_succeeds` | `import-existing --help` shows `--directory` |
| `verify_help_succeeds` | `verify --help` shows `--checksums` |
| `get_code_help_succeeds` | `get-code --help` shows "2FA" |
| `submit_code_help_succeeds` | `submit-code --help` shows "2FA" |
| `retry_failed_help_succeeds` | `retry-failed --help` shows `--directory` |

### Invalid Input (4 tests)

| Test | Confirms |
|------|----------|
| `unknown_subcommand_fails` | `frobnicate` → exit 1, "error" in stderr |
| `unknown_flag_fails` | `--nonexistent-flag` → failure |
| `unknown_flag_on_subcommand_fails` | `sync --bogus-flag` → failure |
| `unknown_flag_on_status_fails` | `status --bogus-flag` → failure |
| `unknown_flag_on_all_subcommands_fails` | `--bogus-flag` rejected on verify, reset-state, import-existing, get-code, submit-code, retry-failed |

### Global Flags (3 tests)

| Test | Confirms |
|------|----------|
| `log_level_debug_accepted` | `--log-level debug` parses OK |
| `log_level_invalid_rejected` | `--log-level verbose` → failure |
| `config_flag_accepted` | `--config /path` parses OK |

### Short Flag Aliases (6 tests)

| Test | Confirms |
|------|----------|
| `short_u_flag_accepted` | `-u` works for `--username` |
| `short_p_flag_accepted` | `-p` works for `--password` |
| `short_d_flag_accepted` | `-d` works for `--directory` |
| `short_l_flag_accepted` | `-l` works for `--list-albums` |
| `short_a_flag_accepted` | `-a` works for `--album` |
| `short_y_flag_on_reset_state` | `-y` works for `--yes` |

### Enum Validation (8 tests)

| Test | Confirms |
|------|----------|
| `size_accepts_all_valid_variants` | original, medium, thumb, adjusted, alternative accepted |
| `size_rejects_invalid_variant` | `--size huge` → failure |
| `domain_accepts_com_and_cn` | com, cn accepted |
| `domain_rejects_invalid` | `--domain uk` → failure |
| `live_photo_size_accepts_valid` | original, medium, thumb accepted |
| `live_photo_size_rejects_invalid` | `--live-photo-size xlarge` → failure |
| `live_photo_mov_filename_policy_accepts_valid` | suffix, original accepted |
| `live_photo_mov_filename_policy_rejects_invalid` | `custom` → failure |
| `align_raw_accepts_valid` | as-is, original, alternative accepted |
| `align_raw_rejects_invalid` | `bogus` → failure |
| `file_match_policy_accepts_valid` | name-size-dedup-with-suffix, name-id7 accepted |
| `file_match_policy_rejects_invalid` | `random` → failure |

### Numeric Validation (5 tests)

| Test | Confirms |
|------|----------|
| `threads_num_rejects_zero` | `--threads-num 0` → failure |
| `threads_num_rejects_negative` | `--threads-num -1` → failure |
| `threads_num_accepts_positive` | `--threads-num 5` parses OK |
| `threads_num_rejects_non_numeric` | `--threads-num abc` → failure |
| `recent_rejects_non_numeric` | `--recent abc` → failure |
| `watch_with_interval_rejects_non_numeric` | `--watch-with-interval abc` → failure |
| `max_retries_rejects_non_numeric` | `--max-retries abc` → failure |
| `retry_delay_rejects_non_numeric` | `--retry-delay abc` → failure |

### Flag Acceptance (9 tests)

| Test | Confirms |
|------|----------|
| `boolean_sync_flags_accepted` | 12 boolean flags parse OK |
| `value_sync_flags_accepted` | 14 value flags parse OK |
| `album_flag_accepts_multiple` | Multiple `--album` flags accepted |
| `bare_invocation_with_username_and_directory_parses` | No subcommand defaults to sync |
| `config_global_flag_works_with_all_subcommands` | `--config` works on all 8 subcommands |
| `log_level_global_flag_works_with_all_subcommands` | `--log-level` works on all 8 subcommands |
| `auth_flags_accepted_on_all_subcommands` | --username, --password, --domain, --cookie-directory on all |
| `log_level_all_variants_accepted` | debug, info, warn, error all accepted |
| `log_level_before_subcommand` | `--log-level error sync` works (global before subcommand) |

### Subcommand-Specific (7 tests)

| Test | Confirms |
|------|----------|
| `submit_code_requires_code_argument` | `submit-code` without CODE arg → failure |
| `submit_code_accepts_code_argument` | `submit-code --help` parses OK |
| `import_existing_requires_directory` | `import-existing` without `--directory` → failure |
| `import_existing_folder_structure_flag` | `--folder-structure` works on import-existing |
| `import_existing_recent_flag` | `--recent` works on import-existing |
| `import_existing_short_d_flag` | `-d` works on import-existing |
| `retry_failed_accepts_sync_flags` | Sync flags (--directory, --recent, etc.) work on retry-failed |

### Cross-Subcommand Flags (3 tests)

| Test | Confirms |
|------|----------|
| `domain_flag_works_on_status` | `--domain cn` works on status |
| `cookie_directory_flag_works_on_verify` | `--cookie-directory` works on verify |
| `password_flag_works_on_submit_code` | `-p` works on submit-code |

### Exit Codes (6 tests)

| Test | Confirms |
|------|----------|
| `exit_code_0_on_help` | `--help` → exit 0 |
| `exit_code_0_on_version` | `--version` → exit 0 |
| `exit_code_1_on_missing_username` | Missing `--username` → exit 1, stderr contains "--username is required" |
| `exit_code_3_on_empty_password_file` | Empty `--password-file` → exit 3 (auth failure) |
| `exit_code_3_on_newline_only_password_file` | Newline-only `--password-file` → exit 3 (auth failure) |
| `exit_code_2_on_invalid_argument` | Empty `--username ""` → exit 2 (clap validation) |

### Misc (5 tests)

| Test | Confirms |
|------|----------|
| `username_from_env_var` | `ICLOUD_USERNAME` env var accepted |
| `password_from_env_var` | `ICLOUD_PASSWORD` env var accepted |
| `config_explicit_nonexistent_path_fails_at_runtime` | Explicit `--config /nonexistent` → runtime failure |
| `only_print_filenames_hidden_flag_accepted` | Hidden `--only-print-filenames` parses OK |
| `submit_code_fails_without_username` | submit-code without username → failure with stderr |

---

## State Tests — No Auth (`tests/state.rs`)

10 tests. No credentials or network needed. Tests state subcommands and
metadata operations against absent/fresh databases.

| Test | What It Does | Confirms |
|------|-------------|----------|
| `status_no_db_prints_informational_message` | Runs `status` with no DB | Prints "No state database found" |
| `status_failed_flag_accepted` | Runs `status --failed` with no DB | `--failed` flag works, still shows "No state database found" |
| `reset_state_no_db_prints_message` | Runs `reset-state --yes` with no DB | Prints "No state database found" |
| `verify_no_db_prints_informational_message` | Runs `verify` with no DB | Prints "No state database found" |

---

## State Tests — Auth Required (`tests/state_auth.rs`)

13 tests, all `#[ignore]`. Require pre-authenticated session. Run with:

```sh
cargo test --test state_auth -- --ignored --test-threads=1
```

### Status (1 test)

| Test | What It Does | Confirms |
|------|-------------|----------|
| `status_after_sync_shows_counts` | Syncs 2 files, then runs `status` | Output contains State Database, Assets, Total, Downloaded, Pending, Failed, Last sync started |

### Reset-State (2 tests)

| Test | What It Does | Confirms |
|------|-------------|----------|
| `reset_state_deletes_db_after_sync` | Syncs 1 file, runs `reset-state --yes` | .db file exists after sync, gone after reset; prints "State database deleted" |
| `reset_state_without_yes_does_not_delete` | Syncs 1 file, runs `reset-state` (no --yes, no stdin) | Prints "Cancelled"; DB file count unchanged |

### Verify (4 tests)

| Test | What It Does | Confirms |
|------|-------------|----------|
| `verify_after_sync_reports_results` | Syncs 2 files, runs `verify` | Output contains "Verifying", "Results:", "Verified:" |
| `verify_checksums_after_sync` | Syncs 1 file, runs `verify --checksums` | Output contains "Verified:" (checksums match) |
| `verify_detects_missing_files` | Syncs 1 file, deletes files, runs `verify` | Exit code 1; output contains "MISSING", "Missing:", "Results:" |
| `verify_checksums_detects_corruption` | Syncs 1 file, overwrites with "CORRUPTED DATA", runs `verify --checksums` | Exit code 1; output contains "CORRUPTED", "Corrupted:" |

### Import-Existing (4 tests)

| Test | What It Does | Confirms |
|------|-------------|----------|
| `import_existing_with_nonexistent_directory_fails` | Runs import on `/nonexistent/path` | Exit code 1; stderr contains "does not exist" |
| `import_existing_matches_synced_files` | Syncs 2 files, resets state, runs import, runs status | Import prints "Import complete:"; status shows "Downloaded:" |
| `import_existing_empty_directory_reports_zero_matches` | Runs import on empty tempdir | Prints "Import complete:", "Total assets scanned:", "Files matched:", "Unmatched versions:" |
| `import_existing_custom_folder_structure` | Syncs with `--folder-structure %Y`, resets, imports with same structure | Prints "Import complete:" |

### Retry-Failed (2 tests)

| Test | What It Does | Confirms |
|------|-------------|----------|
| `retry_failed_after_successful_sync_is_noop` | Syncs 1 file, runs `retry-failed` | Exits 0 (nothing to retry) |
| `retry_failed_with_no_db_succeeds` | Runs `retry-failed` with no prior sync | Exits 0 |

---

## Sync Tests (`tests/sync.rs`)

27 tests, all `#[ignore]`. Uses the `icloudpd-test` album for deterministic behavioral assertions.
Require pre-authenticated session. Run with:

```sh
cargo test --test sync -- --ignored --test-threads=1
```

### Test Album (`icloudpd-test`)

| Asset | Filename | Type |
|-------|----------|------|
| Regular JPEG | `GOPR0558.JPG` | `public.jpeg` |
| Video | `IMG_0962.MOV` | `com.apple.quicktime-movie` |
| Live Photo | `IMG_1127.HEIC` + MOV companion | `public.heic` |
| Apple ProRAW | `IMG_0199.DNG` + JPEG derivative | `com.adobe.raw-image` |
| Unicode filename | `Café_🧠godzill.jpg` | `public.jpeg` |

### Metadata (2 tests, no downloads)

| Test | Confirms |
|------|----------|
| `list_albums_prints_album_names` | Output contains "Albums:" |
| `list_libraries_prints_output` | Output contains "libraries:" |

### Core Download (3 tests)

| Test | Confirms |
|------|----------|
| `sync_album_downloads_all_asset_types` | ≥5 files downloaded; all non-empty; JPEG, MOV, HEIC, DNG extensions present |
| `sync_dry_run_downloads_nothing` | Zero files written to disk |
| `sync_idempotent_second_run_noop` | Second sync: same file count, same modification times |

### Filter Flags (5 tests)

| Test | Flag | Confirms |
|------|------|----------|
| `sync_skip_videos_excludes_video_files` | `--skip-videos` | No .mov/.mp4 files; image files still present |
| `sync_skip_photos_excludes_image_files` | `--skip-photos` | No .jpg/.heic/.dng files; video files still present |
| `sync_skip_live_photos_excludes_companions` | `--skip-live-photos` | Standalone video (0962) present; Live Photo MOV (1127) absent |
| `sync_skip_all_media_downloads_nothing` | `--skip-videos --skip-photos --skip-live-photos` | Zero files |
| `sync_date_filters_exclude_by_creation_date` | `--skip-created-before/after` | Future date → empty; past date → empty; interval "1d" parses OK |

### Size and Naming (5 tests)

| Test | Flag | Confirms |
|------|------|----------|
| `sync_size_medium_produces_smaller_files` | `--size medium` | All photo files under 2MB (originals are 3-15MB) |
| `sync_force_size_succeeds_when_available` | `--size medium --force-size` | Downloads files when requested size exists |
| `sync_name_id7_appends_asset_id` | `--file-match-policy name-id7` | Every filename stem ends with separator + 7 alphanumeric chars |
| `sync_custom_folder_structure` | `--folder-structure %Y` | Files in `YYYY/filename` paths; year dir is 4 digits |
| `sync_keep_unicode_preserves_special_chars` | `--keep-unicode-in-filenames` | At least one filename contains non-ASCII chars (é, 🧠) |

### EXIF (1 test)

| Test | Flag | Confirms |
|------|------|----------|
| `sync_set_exif_datetime_embeds_date` | `--set-exif-datetime` | Downloaded JPEG has `DateTimeOriginal` EXIF tag present |

### RAW Alignment (1 test)

| Test | Flag | Confirms |
|------|------|----------|
| `sync_align_raw_controls_raw_naming` | `--align-raw as-is/original/alternative` | DNG present in all variants; flag accepted without error (no RAW+JPEG pairs in library) |

### Live Photo MOV Policy (1 test)

| Test | Flag | Confirms |
|------|------|----------|
| `sync_live_photo_mov_policy_controls_naming` | `--live-photo-mov-filename-policy suffix/original` | Live Photo MOV filenames differ between policies |

### Misc Flags (4 tests)

| Test | Flag | Confirms |
|------|------|----------|
| `sync_temp_suffix_leaves_no_remnants` | `--temp-suffix .downloading` | No `.downloading` files remain after sync |
| `sync_threads_num_reflected_in_log` | `--threads-num 1` | Stderr contains `concurrency=1` |
| `sync_notification_script_fires_event` | `--notification-script` | Marker file created; contains `KEI_EVENT` value |
| `sync_pid_file_cleaned_up_after_sync` | `--pid-file` | PID file does not exist after completion |

### Bare Invocation (1 test)

| Test | Confirms |
|------|----------|
| `sync_bare_invocation_works_like_sync` | Omitting "sync" subcommand downloads files from test album |

### Error Paths (4 tests)

| Test | Confirms |
|------|----------|
| `sync_without_directory_fails` | Exit 1; stderr mentions `--directory` |
| `sync_nonexistent_album_fails` | Exit 1; stderr contains "not found" |
| `sync_nonexistent_library_fails` | Exit 1; non-empty stderr |
| `zz_bad_credentials_fails` | Exit 1; non-empty stderr (runs last — hits auth from scratch) |

---

## Setup Auth (`tests/setup_auth.rs`)

1 test, `#[ignore]` by default. Verifies a pre-auth session is still valid.

| Test | What It Does | Confirms |
|------|-------------|----------|
| `verify_preauth_session` | `sync --auth-only` with pre-auth cookies | Exits 0 without 2FA prompt |
