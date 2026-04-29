# Deprecated

Flags, TOML keys, and subcommands scheduled for removal. Each deprecated item still works and logs a warning until its target version, then it's gone.

See [CHANGELOG.md](CHANGELOG.md) for the release where each item was first marked deprecated.

## v0.20.0

- `--cookie-directory` CLI flag and `[auth] cookie_directory` TOML key. Use `--data-dir` and top-level `data_dir` instead.
- `--exclude-album NAME` CLI flag and `KEI_EXCLUDE_ALBUM` env var. Use `--album '!NAME'` (the new inline-exclusion grammar; `!Foo` operates on the category default).
- `{album}` token in `--folder-structure` (CLI, TOML `[download] folder_structure`, `KEI_FOLDER_STRUCTURE` env). Use `--folder-structure-albums "{album}/..."` and keep `--folder-structure` for the unfiled pass. kei auto-migrates at startup with a paste-ready suggestion.
- `[filters] album` (single-string) TOML key. Use `[filters] albums = ["name"]` (array form).
- `[filters] exclude_albums` TOML key. Merge into `[filters] albums` as `"!name"` entries.
- `[filters] library` (single-string) TOML key. Use `[filters] libraries = ["name"]` (array form).
- Implicit `--album all` promotion from `{album}` in the folder-structure template. v0.13's new `--album all` default makes this redundant; explicit `--album all` (now the default) replaces it.
- `--directory` CLI flag and `KEI_DIRECTORY` env var. Use `--download-dir` and `KEI_DOWNLOAD_DIR` instead. The TOML key `[download] directory` is unchanged. `-d` short flag still works.
- `--skip-live-photos` CLI flag, `KEI_SKIP_LIVE_PHOTOS` env var, and `[filters] skip_live_photos` TOML key. Use `--live-photo-mode skip` (and `[photos] live_photo_mode = "skip"`) instead.
- `--threads-num` CLI flag, `KEI_THREADS_NUM` env var, and `[download] threads_num` TOML key. Use `--threads`, `KEI_THREADS`, and `[download] threads` instead.
- `--retry-delay` CLI flag, `KEI_RETRY_DELAY` env var, and `[download.retry] delay` TOML key. The initial retry delay is now derived from `--max-retries` via a smart table (higher max means more patient delay). Remove explicit settings to pick up the smart default.
- `--no-incremental` CLI flag. Use `kei reset sync-token` before the sync for the same effect (both trigger a full enumeration and store a fresh token for subsequent incremental runs).
- `KEI_METRICS_PORT` env var and `[metrics] port` TOML section. Use `KEI_HTTP_PORT` / `[server] port` instead. Both were renamed in 0.11.0 but didn't have a removal deadline; fixing that now.
- Hidden compat flags on `kei sync`, superseded by dedicated subcommands:
  - `--auth-only` — use `kei login`.
  - `--list-albums` (and its short form `-l`) — use `kei list albums`.
  - `--list-libraries` — use `kei list libraries`.
  - `--reset-sync-token` — use `kei reset sync-token`.

  All four have existed as hidden compat flags since the subcommand refactor. They were missing an explicit removal target; v0.20.0 aligns them with the rest of the cleanup. Each now emits a deprecation warning at runtime naming v0.20.0.

## v0.20.0 — `sync_report.json` schema v2

Bump `SyncReport.version` from `"1"` to `"2"` alongside the flag removals above and apply these `RunOptions` field renames at the same time (one breaking schema change instead of three):

- `options.directory` → `options.download_dir` (matches `--download-dir`)
- `options.threads_num` → `options.threads` (matches `--threads`)
- `options.no_incremental` → removed (`--no-incremental` is gone)

Until v0.20.0 the schema stays at `"1"` and the old field names are retained in the report even though the CLI flags they came from are deprecated.
