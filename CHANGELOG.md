# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

---

## [Unreleased]

Selection and folder-structure flag redesign. `--exclude-album` becomes `--album '!NAME'`. New `--smart-folder` and `--unfiled` per-category flags. `--library` now repeatable. Per-category `--folder-structure-{albums,smart-folders}` templates with their own defaults. Multi-library scope with `{library}` in any template. State DB schema migrates to v8 (per-zone primary key) on first sync. Old shapes keep working with deprecation warnings until v0.20. Full migration guide: [docs/v0.13-migration.md](docs/v0.13-migration.md). ([#215])

### Added

- **`--smart-folder NAME` flag for selecting Apple smart folders.** Repeatable. Accepts a literal name (`Favorites`, `Hidden`), bare sentinels (`all`, `all-with-sensitive`, `none`), or `!literal` exclusions. Default is `none` so existing setups don't gain extra passes. `all` excludes Hidden and Recently Deleted (mirroring iOS Photos UX); `all-with-sensitive` includes them. The TOML key is `[filters].smart_folders`. ([#215])
- **`--unfiled` boolean flag for the unfiled-pass toggle.** `--unfiled` (or `--unfiled true`) opts in; `--unfiled false` disables. Default is `true`, so unfiled photos land at `--folder-structure` even when album passes also run. To restrict to album passes only, pass `--unfiled false` explicitly. The TOML key is `[filters].unfiled`. The `{album}`-in-template-implies-unfiled smart default is gone; layout no longer drives selection. ([#215])
- **`--library NAME` is now repeatable and accepts new sentinels.** Pass multiple `--library` flags; bare sentinels `primary` (PrimarySync), `shared` (every SharedSync zone), and `all` (every library) compose with named zones and `!name` exclusions in one grammar. The TOML key `[filters].libraries` takes the array form. The truncated 8-char form (`SharedSync-A1B2C3D4`) round-trips between paths and the flag. Every combination executes end-to-end against a real account: e.g. `--library primary --library SharedSync-AB12CD34`, `--library all '!SharedSync-Photos'`, or `--library shared` (every shared zone, primary excluded). A `--library` value that doesn't match any live zone bails at startup with the available zone list. ([#215])
- **`--folder-structure-albums` and `--folder-structure-smart-folders` per-category templates.** Album passes default to `{album}` (flat), smart-folder passes to `{smart-folder}` (flat). The base `--folder-structure` (default `%Y/%m/%d`) now drives only the unfiled pass. Add date hierarchy inside album folders explicitly with `--folder-structure-albums '{album}/%Y/%m/%d'`. The auto-migration of legacy `{album}` in `--folder-structure` produces exactly that shape when the legacy template carried a date suffix. Both new flags map to TOML keys `[download].folder_structure_albums` and `[download].folder_structure_smart_folders`. ([#215])
- **`{library}` token in folder-structure templates.** Renders as `PrimarySync` or a truncated raw zone name (`SharedSync-A1B2C3D4`) so users running multi-library syncs can keep paths separate. Allowed in any template; must be the first path segment; single occurrence. Combined with `{album}` or `{smart-folder}`, the order is always `{library}/{album}/...`. The truncated form is what the flag accepts back, so a path segment can be copied directly into `--library`. ([#215])
- **Per-zone primary key in the state DB (schema v8).** The `assets` table now keys on `(library, id, version_size)` so an asset present in both PrimarySync and a shared library tracks two distinct rows instead of silently sharing one. Existing rows backfill to `library = "PrimarySync"` on first sync after upgrade -- accounts that were already syncing only the primary library see no behavior change. Users who had shared libraries pre-v8 may have a small set of misattributed rows from the backfill; the next sync writes correct per-zone rows alongside, and the misattributed leftovers are harmless (they don't trigger re-downloads since the live asset is keyed correctly). ([#215])
- **Multi-library commingle warning at startup.** When more than one library is in scope and none of the active templates includes `{library}`, kei warns that paths share a namespace and the state DB will dedup cross-library matches by ID. Informational; the run continues. Users who want intentional merge see the warning and ignore it; users who wanted separation get pointed at `{library}`. ([#215])
- **Fuzz harnesses for 10 parser entry points.** New `fuzz/` directory with cargo-fuzz targets covering CloudKit JSON deserializers, auth responses (SRP init, account login, 2FA challenge), TOML config, the `*Enc` field decoders, path sanitization, HEIF atom walking, the HEIF XMP probe pipeline, Adobe XMP Toolkit (run via FFI so ASan can see C++ memory bugs), `PhotoAsset::from_records`, and the state enum `from_str` parsers. Checked-in seeds under `fuzz/seeds/` include two OOM regression repros for an upstream `mp4-atom` bug that kei calls synchronously during the EXIF probe. `just fuzz run TARGET` replays them on every invocation. ([#287])

### Changed

- **`kei sync` with no flags now enumerates each user album and runs an unfiled pass.** Previously it ran one library-wide stream. The internal change is real (more API calls, per-album knowledge); the visible change is the on-disk tree shape only when `--folder-structure-albums` is non-default -- the default `{album}` template gives flat per-album folders, while unfiled photos still land at `%Y/%m/%d/`. Aligns the no-flags experience with woutervanwijk's #215 ask without requiring opt-in configuration. ([#215])
- **`--folder-structure` now drives only the unfiled pass.** Album passes consume `--folder-structure-albums`; smart-folder passes consume `--folder-structure-smart-folders`. The legacy `{album}`-in-base auto-migrates with a warning (split across the two new templates) so existing configs keep working through v0.19. Removed in v0.20. ([#215])
- **`--library` now matches album names across every library in scope.** `--album Family` resolves to an album named "Family" in every selected library. To get distinct paths per library, add `{library}` to the relevant template; otherwise paths commingle and the state DB dedups by asset ID (same rule the multi-library warning surfaces at startup). ([#215])
- **First sync after upgrade triggers a full enumeration.** The sync-token invalidation hash now folds in the new library selector shape (and the per-zone state DB key), so existing sync tokens hash differently and kei refetches the full asset list once before resuming incremental syncs. Subsequent runs return to incremental performance. No data loss; idempotent. ([#215])
- **kei now compiles as a library plus a thin binary shim.** `src/main.rs` is a 7-line entry point that calls `kei::main_inner()`; the module tree moved verbatim into `src/lib.rs`. Behavior is identical: `cargo install kei`, the docker image, and `cargo run` all build and run the same binary. The crate has a lib target now, which lets the in-tree fuzz harnesses depend on it directly instead of re-inlining source files via `#[path]`. A new `__fuzz_internals` Cargo feature gates `pub mod __fuzz` containing wrappers around `pub(crate)` parser entry points; default builds don't include it. ([#287])
- **Live recovery tests now print kei's stderr on failure.** `sync_recovers_deleted_file` and `sync_truncated_file_does_not_cause_data_loss` previously panicked with just "deleted file should be re-downloaded" or "correctly-sized photo must be on disk". kei had exited 0, so `assert().success()` discarded stderr before the disk-state check ran, leaving nothing actionable when the tests failed intermittently. Both now run with `RUST_LOG=kei=debug` and include the captured stderr plus a post-sync walkdir in the panic message. ([#287])

### Deprecated

- **`--exclude-album NAME` CLI flag and `KEI_EXCLUDE_ALBUM` env var.** Use `--album '!NAME'` (the new inline-exclusion grammar). Bare `!Foo` operates on the category default (`all` for albums), so `--album '!Family'` means "every album except Family". Both forms still work and log a deprecation warning; removed in v0.20.0. ([#215])
- **`{album}` in `--folder-structure` (CLI / TOML / `KEI_FOLDER_STRUCTURE` env).** Use `--folder-structure-albums "{album}/..."` and keep `--folder-structure` for the unfiled pass. kei auto-migrates at startup: `{album}/%Y/%m/%d` lifts into `folder_structure_albums` and the base template strips the prefix. A one-line warning prints with the equivalent new-flag pair so the suggestion is paste-ready. Removed in v0.20.0. ([#215])
- **`[filters].album` (singular string) TOML key.** Use `[filters].albums = ["name"]` (array). Round-trips through the same selector grammar so `--album` and the array form are interchangeable. Removed in v0.20.0. ([#215])
- **`[filters].exclude_albums` TOML key.** Merge into `[filters].albums` as `"!name"` entries. The new selector grammar validates contradictions (`"Foo"` and `"!Foo"` together) in one place. Removed in v0.20.0. ([#215])
- **`[filters].library` (single-string) TOML key.** Use `[filters].libraries = ["name"]` (array). Removed in v0.20.0. ([#215])
- **Implicit `--album all` from `{album}` token in template.** v0.12 promoted the legacy template-driven shape to "every album" when `--album` was unset; v0.13's new `--album all` default makes the promotion redundant. The compat path still fires but logs a deprecation warning. Removed in v0.20.0. ([#215])

---

## [0.12.0] - 2026-04-26

### Added

- **Shared-library discovery notice on first sync.** Users on the default `--library PrimarySync` who also have iCloud shared libraries on their account now see a one-time warning at sync time naming how many shared libraries exist and how to opt in (`[filters] library = "all"` or `--library all`). The notice fires at most once per account via a marker in the state DB's `metadata` table. Accounts without shared libraries re-probe on every sync so the notice still fires if a library is added later. Users who've set `--library` explicitly see nothing. The notice never auto-changes sync behavior.
- **`--bandwidth-limit` accepts decimal values.** `1.5M`, `.5K`, and `2.5Gi` all parse now. Previously kei rejected decimals with a confusing error about the unit suffix (the parser was splitting `1.5M` at the first non-digit, leaving `.5m` as the unit). Values that round to less than 1 byte/sec (e.g. `0.0001K`) are rejected with a clear "rounds to zero" message; negative values and NaN/infinity stay rejected. Existing integer forms (`10M`, `500K`, `1Mi`) are unchanged.
- **`--recent` accepts a days-based form.** `--recent 100` still means "the 100 most-recent assets" (unchanged). `--recent 30d` now means "assets created in the last 30 days" and translates internally to `--skip-created-before 30d`. TOML `[filters] recent` accepts both `100` (integer) and `"30d"` (string). Passing both `--recent Nd` and `--skip-created-before` on the same invocation errors with a "pick one" message since they're equivalent controls. `import-existing` only accepts the count form (it scans files on disk, not iCloud creation dates) and errors on the days form.
- **`--notification-script` fires a new `sync_started` event.** The script is now invoked with `KEI_EVENT=sync_started` immediately before each sync cycle begins (after the watch-mode skip check, so skipped cycles don't fire). Useful for sending "sync running" pings to Slack or push notifications. Payload env vars match the other non-terminal events (`KEI_MESSAGE`, `KEI_ICLOUD_USERNAME`); per-cycle stats aren't available yet at start, so the extended `KEI_DOWNLOADED`/`KEI_FAILED`/etc. vars are omitted - they're set on `sync_complete` and `sync_failed` as before.
- **`--report-json` gains a TOML key.** Users can now set `[report] json = "/path/to/run.json"` in the config file instead of carrying `KEI_REPORT_JSON` separately. Resolution order is CLI > TOML > unset; `kei config show` round-trips the setting through the `[report]` section.
- **`sync_report.json` has a new `"interrupted"` status.** A cycle cut short by SIGINT/SIGTERM/SIGHUP now reports `status = "interrupted"` instead of `"success"` (when no failures) or `"partial_failure"` (when there were mid-flight recorded failures). Priority: `session_expired` > `interrupted` > `partial_failure` > `success`. Operators who alert on `status != "success"` should add `"interrupted"` to their benign-status allowlist unless they want to page on every graceful shutdown.
- **`--http-bind` / `KEI_HTTP_BIND` / `[server] bind`.** New flag to control which interface the `/healthz` + `/metrics` server listens on. Default stays `0.0.0.0` so Docker's `-p 9090:9090` keeps working out of the box, but desktop users who don't want kei's stats reachable from the local network can set `127.0.0.1` to restrict to loopback. Accepts any IPv4 or IPv6 literal; invalid values fail at startup.
- **`kei import-existing --dry-run`.** Scans the tree, runs filename + size matching, and reports the match/unmatched counts without writing to the state DB and without the SHA256 compute a real import would do. Useful before committing: a user with 50k existing photos can verify that `--folder-structure` and `--keep-unicode-in-filenames` match the tree layout without locking in bad results, and the preview finishes in seconds rather than hours. The completion banner reads "Import complete (DRY RUN - no changes written to state DB)" so operators can't accidentally miss that nothing was persisted.
- **`kei reconcile` caps its per-issue listing at 200 lines.** Same treatment as `kei verify` and `kei status` — a library with thousands of missing files previously printed one `MISSING: ...` line per asset with no cap. The `Marked failed:` count in the summary still reflects every re-queued row; only the per-line listing is capped. A tail line reads `... and N more issue(s) not listed (listing capped at 200)`.
- **`kei verify` caps its per-issue listing at 200 lines.** Previously the command printed one `MISSING: ...` / `CORRUPTED: ...` line per affected file with no cap — a library with thousands of missing files produced a wall of output. The summary section still shows the true totals; listings beyond the cap print a tail line (`... and N more issue(s) not listed (listing capped at 200)`). Same cap used by `kei status` and `sync_report.json` so operators see consistent detail levels across the three surfaces. Follow-ups (`--fix`, `--sample`, `--format json`, `--since <DATE>`) tracked in a scratch design note.
- **`kei status --failed / --pending / --downloaded` now caps the per-section listing at 200 rows.** Previously the three listings printed every matching row from the state DB — a library with 100k+ downloaded assets or thousands of failures produced a wall of output. Listings beyond the cap print a tail line (`... and N more (listing capped at 200)`) with the omitted count. Summary counts (at the top of the output) are unchanged and always show the full totals. The cap matches the `failed_assets` cap used by `sync_report.json` so operators see consistent detail levels across the two surfaces. A `--limit N` override is tracked in a follow-up design note.
- **`--save-password` now explains itself instead of silently no-op'ing.** The flag saves the password to the credential store after successful auth, but only for ephemeral sources (`--password` / `ICLOUD_PASSWORD`). Previously, pairing `--save-password` with `--password-file`, `--password-command`, an already-saved credential, or a fresh interactive prompt would quietly do nothing - the user had no signal the flag was ineffective. kei now emits a targeted warning for each case naming the actual persistent source, pointing at `kei password set` for interactive bootstrap, or calling out that a rotating secret-manager password shouldn't be snapshotted into the store.

### Changed

- **`--password-command` / `[auth] password_command` now has a 30-second timeout per invocation.** If the command hasn't exited by then, kei kills the child and fails the auth attempt with `password command timed out after 30s: <cmd>`. This matters most in watch mode, where a hung secret manager (network-stalled Vault, sleepy gpg-agent) would otherwise freeze the sync indefinitely. Commands that finish in milliseconds are unaffected.
- **`--password-command` / `[auth] password_command` is now rejected on Windows at startup.** kei runs the command via `sh -c`, which isn't on a stock Windows PATH, so previously the flag failed at auth time with a cryptic "No such file or directory". The new error fires at config load and points at `--password-file` or WSL. Unix behavior is unchanged.
- **`--notification-script` is now a no-op with a one-time warning on Windows.** kei invokes scripts via `/bin/sh`, which isn't available on a stock Windows PATH, so the flag previously failed silently at spawn time on every event. kei now logs a single warning at startup when the flag is set on Windows and skips script invocation for the rest of the run. Unix behavior is unchanged.
- **`--notify-systemd` auto-detects under systemd.** When neither the CLI flag, env var, nor TOML key is set, kei now enables sd_notify messages automatically if `NOTIFY_SOCKET` is present (which systemd publishes for `Type=notify` units). Users who set up a `kei.service` no longer have to remember the flag. An explicit `--notify-systemd=false` still suppresses even under systemd. Zero false positives: only systemd's `Type=notify` sets `NOTIFY_SOCKET`.
- **Initial retry delay now derives from `--max-retries`.** kei picks the initial exponential-backoff delay based on how patient the user asked to be: max 1-2 gets 2s, max 3 stays at 5s (today's default), max 4-6 gets 10s, max 7+ gets 30s. Higher max means "give the failing service time to recover", not "hammer faster" - Apple's 429 and 5xx failures both benefit from longer breathing room, and fast retry-storms make rate limiting worse. Explicit `--retry-delay` still wins during the deprecation window.
- **`--threads-num` CLI flag renamed to `--threads`.** `KEI_THREADS_NUM` becomes `KEI_THREADS`; the TOML key `[download] threads_num` becomes `[download] threads`. The old spellings still parse and log a deprecation warning; they'll be removed in v0.20.0. Passing `--threads` and `--threads-num` together (or both TOML keys) errors with "pick one". No short flag was added; `--threads-num` never had one.
- **`--skip-videos` + `--skip-photos` with a live-photo-mode that drops everything now errors at startup.** Previously a soft `tracing::warn!` would fire when `--skip-videos`, `--skip-photos`, and `--live-photo-mode skip` were all set and then the sync would silently complete with zero downloads. The validation now lives in `Config::build` and rejects the combination with a clear error explaining the outcome. The obscure legitimate case - `--live-photo-mode video-only` to download only Live Photo MOV companions - is still allowed.
- **`--directory` CLI flag renamed to `--download-dir`.** The short form `-d` keeps working, and `KEI_DIRECTORY` becomes `KEI_DOWNLOAD_DIR`. The TOML key `[download] directory` is unchanged. The old flag was generic sitting next to `--config`, `--data-dir`, and `--folder-structure`; the new name echoes the TOML section and makes Docker and systemd samples self-documenting.
- **Minimum supported Rust version (MSRV) is now 1.91.** `src/download/paths.rs` calls `str::floor_char_boundary`, which was stabilized in 1.91, so older toolchains never compiled cleanly anyway — they failed with a cryptic `E0658`. `Cargo.toml` now sets `rust-version = "1.91"`, so users on `< 1.91` get Cargo's `package requires rustc 1.91 or newer` error at the start of the build. The `clippy::incompatible_msrv` lint is enabled in CI to catch future MSRV regressions at PR time. Users on stable Rust ≥ 1.91 see no change. ([#263])

### Fixed

- **`kei import-existing` no longer fails on a fresh install.** On a brand-new machine (no prior `kei sync` or `kei login` run), the command bailed out with `Failed to open database at ...: unable to open database file: Error code 14` because the `~/.config/kei/cookies/` parent directory didn't exist yet, and SQLite won't `mkdir -p` for you. `SqliteStateDb::open` now creates the parent directory before opening the DB, so `import-existing` works without the previously-required `kei login` workaround. ([#264])
- **iOS 17+ HEICs with Apple `uri ` item references now embed XMP cleanly.** From iOS 17, Apple-shot HEICs carry an extra `infe` entry with `item_type = "uri "` pointing at `tag:apple.com,2023:photos/<id>`. The `mp4-atom` 0.10.1 release didn't read the trailing `item_uri_type` cstr and rejected every such file with `under decode: infe`, so kei's metadata-embed pass failed on every iOS 17+ HEIC and the file landed without XMP. kei now pins `mp4-atom` to a post-fix git rev (PR kixelated/mp4-atom#123, awaiting upstream v0.11.0 release) and adds a regression test that round-trips a synthetic `uri `-bearing HEIC. The HEIF writer surfaces a typed `HeifError` enum (`Decode { offset, total }`, `UnparsableTail`, `MissingMeta`, `Encode`, `Io`) so future failures land with byte-offset context instead of opaque anyhow strings. ([#274])
- **First-pass HEIC metadata writes no longer fail with a spurious "Opening … for XMP update" warning.** When `--set-exif-datetime` (or any other embed flag) was set, every HEIC asset logged a `Failed to write metadata` warning during its first download cycle, set a retry marker, and got rewritten correctly on the next sync. The metadata writer dispatched between the in-tree HEIF writer and Adobe XMP Toolkit by file-extension, but the embed step runs on the `<base32>.kei-tmp` part file before the atomic rename — so HEIC bytes routed to XMP Toolkit, which has no HEIF handler. Dispatch now sniffs the first 12 bytes for an ISO-BMFF `ftyp` HEIF brand, so part files dispatch correctly on the first attempt. No on-disk format change; existing pending markers from before the fix get cleared by the next sync as before. ([#269])
- **HEIC metadata reads no longer redundantly rewrite already-correct fields.** The read-side mirror of [#269]: `probe_exif` always routed through Adobe's XMP Toolkit, which has no HEIF handler, so every HEIC silently came back as `ExifProbe::default()`. The "field already present, skip" gate in `plan_metadata_write` had nothing to compare against, so kei rewrote `DateTimeOriginal` / GPS / rating / description on every iPhone HEIC even when the file already carried the same value. No data corruption (the HEIF writer reads existing XMP before mutating), but every embed-flag run produced unnecessary writes. `probe_exif` now content-sniffs the first 12 bytes and parses HEIC files via the in-tree HEIF reader, matching the dispatch fix in #271. ([#272])
- **Default Docker image now starts cleanly out of the box.** The v0.11.0 default `CMD` passed `--watch 24h`, but no such flag exists — running the published image with no overrides errored at startup with `unexpected argument '--watch' found`. Regression from the always-on-server work in #237; CI does not exercise the default `CMD`, so it slipped past review. The image's `CMD` now uses `--watch-with-interval 86400` (and `--download-dir` per the rename below), so `docker run ghcr.io/rhoopr/kei` works without a custom `CMD`.
- **`--file-match-policy name-id7` no longer risks producing a path separator inside a filename.** The 7-char asset-ID suffix baked into every filename was generated via standard base64, whose alphabet includes `/` and `+`. A CloudKit asset ID containing certain bytes (e.g. a `?` early in the ID) would produce `/` as the 4th base64 character, causing kei to drop that asset into a surprise subdirectory instead of the expected filename. kei now uses URL-safe base64 (`-` / `_`) for the suffix. Files that previously got the bug land at a new path on next sync and may re-download once. Added a fuzz test that walks every single-byte input and asserts the suffix only contains `[A-Za-z0-9_-]`.

### Security

- **Permissive-mode warning for `--password-file`.** kei now logs a warning at startup when `--password-file` or `[auth] password_file` points to a file that's readable by group or other users (any of the `0o077` permission bits set). The warning names the path, reports the current mode, and suggests `chmod 600 <path>`. Container-secret mount points (`/run/secrets/` and `/var/run/secrets/`) are exempted since Docker and Kubernetes publish those files as world-readable by design and enforce isolation at the mount layer. Unix only - no-op on Windows. Warns once per path per process, so watch mode doesn't spam.

### Removed

- **BREAKING: `[auth] password` TOML key.** Plaintext passwords in config files are a standing security risk, and the credential store plus file/command sources already cover the use case. Config files that still set it now error on startup with a migration message. Move to `kei password set` (OS keyring or encrypted file), `[auth] password_file`, or `[auth] password_command`.

### Deprecated

- **`--cookie-directory` CLI flag and `[auth] cookie_directory` TOML key.** Use `--data-dir` and top-level `data_dir` instead. The old spellings still work and log a deprecation warning; they'll be removed in v0.20.0.
- **`--directory` CLI flag and `KEI_DIRECTORY` env var.** Use `--download-dir` and `KEI_DOWNLOAD_DIR` instead. Both still parse and populate the same field but log a deprecation warning on use; they'll be removed in v0.20.0. Passing both forms together (CLI or env var) errors with "pick one" instead of silently picking a winner - docker-compose users upgrading from a pre-0.12 file should drop `KEI_DIRECTORY` before rolling out.
- **`--skip-live-photos` CLI flag, `KEI_SKIP_LIVE_PHOTOS` env var, and `[filters] skip_live_photos` TOML key.** Use `--live-photo-mode skip` and `[photos] live_photo_mode = "skip"` instead. All three still work and map to the same behavior, but now log a deprecation warning on use (previously the TOML path mapped silently); they'll be removed in v0.20.0.
- **`--no-incremental` CLI flag.** Use `kei reset sync-token` before the sync for the same effect. Both trigger a full enumeration and store a fresh token for subsequent incremental runs. The flag still parses and logs a deprecation warning; it'll be removed in v0.20.0. A narrow failure-path difference (a failed `--no-incremental` run preserved the prior token; a failed `reset + sync` did not) wasn't worth the flag's ongoing cost.
- **`--retry-delay` CLI flag, `KEI_RETRY_DELAY` env var, and `[download.retry] delay` TOML key.** The initial retry delay is now derived from `--max-retries` via a smart table; the explicit setting became redundant for almost every use case. All three still work with a deprecation warning; they'll be removed in v0.20.0. Remove the explicit setting to pick up the smart default.
- **`KEI_METRICS_PORT` env var and `[metrics] port` TOML section now name v0.20.0 as the removal target.** Both were deprecated in 0.11.0 in favor of `KEI_HTTP_PORT` / `[server] port`, but the deprecation messages said "a future release" without a concrete deadline. They now say v0.20.0.
- **Hidden compat flags on `kei sync` now name v0.20.0 as the removal target.** `--auth-only`, `--list-albums` / `-l`, `--list-libraries`, and `--reset-sync-token` have been hidden compat aliases since the subcommand refactor; each now emits a deprecation warning naming v0.20.0 and pointing at the replacement (`kei login`, `kei list albums`, `kei list libraries`, `kei reset sync-token`). `--reset-sync-token` previously emitted no warning at all; that's fixed. The shared `deprecation_warning()` helper used across the CLI was updated to always include the v0.20.0 target, so every flag-rename warning in the codebase is now consistent.

[#263]: https://github.com/rhoopr/kei/pull/263
[#264]: https://github.com/rhoopr/kei/issues/264
[#269]: https://github.com/rhoopr/kei/issues/269
[#272]: https://github.com/rhoopr/kei/issues/272
[#274]: https://github.com/rhoopr/kei/issues/274
[#287]: https://github.com/rhoopr/kei/pull/287

---

## [0.11.0] - 2026-04-22

### Added

- **Always-on `/healthz` and `/metrics` HTTP server in watch mode.** Running with `--watch` now starts an HTTP server on port 9090 (default) exposing both `/healthz` (JSON health status) and `/metrics` (Prometheus text format) with no extra flag. One-shot syncs skip the server since the process exits before anything could scrape it. The Docker image's `HEALTHCHECK` now uses `curl -f http://localhost:9090/healthz` instead of parsing `health.json` with `jq`, and `jq` has been removed from the runtime image. The default `CMD` gains `--watch 24h` so the healthcheck has something to probe out of the box; override `CMD` to run a one-shot. ([#237], thanks @billimek)

### Changed

- **`--metrics-port` renamed to `--http-port`.** The flag now sets the server port rather than gating whether the server runs. Default is 9090. ([#237], thanks @billimek)

### Deprecated

- **`KEI_METRICS_PORT` env var and `[metrics] port` TOML section.** Use `KEI_HTTP_PORT` and `[server] port` instead. The old spellings are still accepted and log a deprecation warning; they will be removed in a future release. ([#237], thanks @billimek)

### Added

- **`xmp` Cargo feature (default-on) to support builds without Adobe's XMP Toolkit.** `xmp_toolkit` vendors Adobe's C++ XMP Toolkit, which doesn't build on FreeBSD. Building with `cargo build --no-default-features` now compiles cleanly: it drops the `--embed-xmp`, `--xmp-sidecar`, `--set-exif-datetime`, `--set-exif-rating`, `--set-exif-gps`, and `--set-exif-description` flags (and the corresponding `[download]` TOML keys), and disables HEIC metadata writes. Download, auth, state tracking, magic-byte validation, and sidecar reads from other tools are unaffected. Default feature set is unchanged for every existing user. Also adds a FreeBSD target block for the `keyring` crate so the build doesn't fall through the OS-specific dependency matrix. ([#256], thanks @jan666)

[#256]: https://github.com/rhoopr/kei/issues/256

### Fixed

- **Spurious `File header does not match expected format` warning on live-photo MOVs.** The magic-byte check only accepted the ISO-BMFF `ftyp` box at offset 4 and warned on everything else. Apple serves live-photo and HEVC videos as classic QuickTime, which commonly begins with a `wide` padding atom (`00 00 00 08 77 69 64 65`) or an `mdat` data atom instead. The `.mov` branch now accepts `ftyp`, `wide`, `mdat`, `moov`, `free`, `skip`, and `pnot` - all valid QuickTime top-level atoms. `.heic`, `.heif`, `.mp4`, and `.m4v` remain strict ISO-BMFF (`ftyp` only). `.dng` now runs the TIFF magic check. Files were already being saved correctly; only the warning is gone. ([#247], thanks @woutervanwijk)

[#237]: https://github.com/rhoopr/kei/pull/237
[#247]: https://github.com/rhoopr/kei/issues/247

---

## [0.10.1] - 2026-04-21

### Fixed

- **Startup panic when `--metrics-port` is set.** Launching kei with `--metrics-port` (or `KEI_METRICS_PORT` / `[metrics] port` in TOML) crashed at startup with a `blocking_lock()` panic. Regression from 0.10.0: the `/healthz` staleness threshold was applied to the mutex-wrapped metrics handle after construction, and `blocking_lock()` panics unconditionally when called from inside a tokio runtime. The threshold is now passed into `MetricsHandle::new()` before the mutex wraps the inner state, so no locking is needed. ([#248], [#249], thanks @billimek)

[#248]: https://github.com/rhoopr/kei/issues/248
[#249]: https://github.com/rhoopr/kei/pull/249

---

## [0.10.0] - 2026-04-20

### Added

- **Provider-agnostic metadata capture.** Every asset now stores a full `AssetMetadata` record alongside the binary: favorite, rating, GPS (latitude/longitude/altitude), orientation, keywords, title, description, duration, dimensions, timezone offset, is_hidden, is_deleted, modified_at, plus a `provider_data` JSON column for fields without a neutral slot. iCloud fields decode through `src/icloud/photos/metadata.rs`, which handles Apple's plist-encoded location, keywords, and caption fields. Stored via a v5 schema migration with a `metadata_hash` column for drift detection. CloudKit `SoftDeleted` / `HardDeleted` events from `changes/zone` flip `is_deleted` and stamp `deleted_at`; local files are left alone. ([#239], [#19], [#83])

- **EXIF and XMP write-through (opt-in, default false).** New flags embed captured metadata after the file lands on disk:
  - `--set-exif-rating` maps iCloud's favorite flag to rating=5, otherwise writes the explicit rating.
  - `--set-exif-gps` embeds latitude / longitude / altitude.
  - `--set-exif-description` writes `dc:description`.
  - `--embed-xmp` writes a full XMP packet. JPEG / PNG / TIFF / MP4 / MOV go through Adobe's XMP Toolkit (vendored C++). HEIC / HEIF / AVIF go through a pure-Rust `mp4-atom` writer that inserts the packet as a MIME item inside the `meta` box without touching the encoded image bytes.
  - `--xmp-sidecar` writes a `.xmp` sidecar next to each media file. Existing sidecars from Darktable, digiKam, or Lightroom are parsed and kei's fields are layered on top rather than overwriting.

  Metadata-only drift on the server (rating change, keyword edit) is detected via `metadata_hash` comparison and queues a rewrite pass that patches EXIF/XMP on the already-downloaded file without re-fetching bytes. ([#239], [#84], [#85])

- **`kei reconcile` subcommand.** Scans the state DB for `downloaded` rows whose `local_path` no longer exists and marks them failed so the next sync re-downloads. `--dry-run` previews without writing. Never deletes files or DB rows. ([#239], [#230])

- **`--bandwidth-limit` flag to cap total download throughput.** Accepts human-readable values (`10M`, `500K`, `2Mi`, bare integer = bytes/sec). The cap is global across all concurrent downloads, so total throughput stays within budget regardless of `--threads-num`. Also configurable via `[download] bandwidth_limit` in the TOML config and `KEI_BANDWIDTH_LIMIT` env var. When set without an explicit `--threads-num`, concurrency defaults to 1 so the capped budget isn't fragmented across many starved connections. ([#53])

- **`-a all` to sync every user-created album in one run.** Case-insensitive, works as a CLI flag, `KEI_ALBUM` env var, or `filters.albums = ["all"]` in TOML. Apple's smart folders (Favorites, Screenshots, Videos, Hidden, Recently Deleted, etc.) are skipped - list them explicitly with `-a Favorites` if you want them. Combining `-a all` with specific names is rejected with "cannot combine 'all' with specific album names". ([#215])

- **Smart `{album}` auto-expansion in `--folder-structure`.** When the template contains `{album}` and no `-a` flag is passed, kei implicitly runs `-a all`. In either mode (explicit `-a all` or implicit via template), using `{album}` in the template adds a library-wide pass for photos that aren't in any user-created album - `{album}` collapses to empty for those, so `{album}/%Y/%m/%d` puts unfiled photos at `%Y/%m/%d/`. Without `{album}` in the template, `-a all` skips unfiled photos entirely. Photos that belong to multiple albums are copied into each album folder. ([#215])

- **`{album}` placement validation.** `{album}` must be the first path segment in `--folder-structure` and may only appear once. `{album}/%Y/%m` is fine; `Photos/{album}/%Y`, `%Y/{album}/%m`, and `{album}/%Y/{album}` are rejected at startup with a quoted error message. The restriction keeps unfiled-photo paths stable - without it, collapsing `{album}` shifts segments around and the unfiled tree no longer matches the album tree. ([#215])

- **DB-backed asset gauges on `/metrics`.** Three new Prometheus gauges updated once per real sync cycle: `kei_db_assets_total{status="downloaded|pending|failed"}`, `kei_db_assets_size_bytes{status="downloaded"}`, and `kei_db_last_sync_assets_seen`. The gap between `last_sync_assets_seen` and the `downloaded` count is the most actionable derived metric for "how far behind is the local copy?". No-op when `--metrics-port` is not set. ([#234])

### Changed

- **CLI and TOML bounds validation.** `--threads-num` clamped to 1..=64, `--watch-with-interval` to 60..=86400, `--max-retries` to 0..=100, `--retry-delay` to 1..=3600. Validation runs on both CLI and TOML paths so hand-written configs can't bypass the bounds. ([#239])

- **`/healthz` flips to 503 when sync is stale.** Returns 503 when `last_success_at` exceeds `watch-interval * 2`. Orchestrators and Docker healthchecks can now catch stuck syncs. ([#239])

- **Structured `failed_assets[]` in `sync_report.json`.** Each entry has `id`, `version_size`, and `error_message`. Capped at 200 entries with `failed_assets_truncated` carrying the tail count. ([#239])

### Fixed

- **CloudKit pagination EOF now requires two consecutive empty pages.** A single empty `/records/query` page no longer cuts enumeration short. ([#239])

- **`.part` resume window tightened to 1h with server-byte reconciliation on 206 responses.** Splicing bytes from pre- and post-rotation versions of the same asset is no longer possible. `.part` creation uses `OpenOptions::create_new` to reject concurrent writers. ([#239])

- **CDN allowlist on download URLs.** CloudKit-returned URLs are validated against a narrow allowlist (`.icloud-content.com[.cn]`, `.cdn-apple.com`) before any byte hits disk. ([#239])

- **JSON error envelopes no longer written as images.** Known CloudKit error envelope shapes are rejected during content validation, so an error page can't be saved out as `IMG_001.JPG`. ([#239])

- **Disk-space forecast before enqueueing a batch.** Batch-size estimate against free disk space warns at 90% and bails at 100%. ([#239])

- **State DB unwritable at startup bails the sync.** Previously accumulated progress that could not be saved. ([#239])

- **PID file liveness check before overwrite.** A second kei no longer clobbers a live process's PID file. ([#239])

- **Atomic EXIF/XMP/sidecar writes.** EXIF/XMP embeds write into a `.part` copy, patch in place, atomic rename. A RAII guard removes the `.meta-tmp` on any exit path including FFI panic from xmp_toolkit. Sidecar and sync-report writers share one `fs_util::atomic_install` helper that prefers rename and falls back to copy-to-sibling-then-rename under EXDEV. ([#239])

### Dependencies

- Added: `xmp_toolkit` (Adobe XMP Toolkit, vendored C++), `mp4-atom` (pure-Rust ISO-BMFF atom editing), `plist` (binary plist decoding for Apple's `Enc` fields), `async-speed-limit` (bandwidth throttle).
- Removed: `kamadak-exif`, `little_exif`.
- Bumped: `dialoguer` 0.11 -> 0.12.

### Known limitations

- The state DB tracks a single download path per asset. A photo copied to multiple album folders under `{album}/...` has all copies on disk, but the DB records only the most recently written path. Re-running sync stays idempotent because kei's filesystem-exists check is path-aware - it won't re-download files it already put on disk, regardless of which path the DB currently holds.

[#19]: https://github.com/rhoopr/kei/issues/19
[#53]: https://github.com/rhoopr/kei/issues/53
[#83]: https://github.com/rhoopr/kei/issues/83
[#84]: https://github.com/rhoopr/kei/issues/84
[#85]: https://github.com/rhoopr/kei/issues/85
[#215]: https://github.com/rhoopr/kei/issues/215
[#230]: https://github.com/rhoopr/kei/issues/230
[#234]: https://github.com/rhoopr/kei/pull/234
[#239]: https://github.com/rhoopr/kei/pull/239

---

## [0.9.2] - 2026-04-18

### Fixed

- **Ghost asset loop on filter / album-scope changes.** Assets the DB knew about but the current sync didn't re-enumerate (filter excluded, album scope changed, upstream record deleted) were promoted to `failed` at sync end. `prepare_for_retry` then reset them to `pending` on the next sync, and the cycle repeated. `kei status --failed` filled with `Not resolved during sync` entries that weren't failures. `promote_pending_to_failed` now promotes only pending assets the producer dispatched this sync (`last_seen_at >= sync_started_at`) that the consumer never finalized. Filtered or out-of-scope assets keep their stale `last_seen_at` and are left alone. ([#211])

### Added

- **`kei status --pending` and `--downloaded`.** Parallel to the existing `--failed` flag, these list assets in the requested state with filename, ID, and last-seen time (pending) or local path (downloaded). ([#211])

[#211]: https://github.com/rhoopr/kei/issues/211

---

## [0.9.1] - 2026-04-18

### Fixed

- **Apple IDs with FIDO/WebAuthn security keys fail fast with a clear error.** Accounts with a YubiKey or other hardware security key registered could sign in through SRP + 2FA, then hit an opaque CloudKit `HTTP 401 AUTHENTICATION_FAILED "no auth method found"` on the first API call and loop through re-auth until `AUTH_ERROR_THRESHOLD` stopped the sync. SRP now inspects the `/signin/complete` 409 body for `fsaChallenge` / `keyNames` and bails with `AuthError::FidoNotSupported { key_names }` before prompting for a 2FA code. The message names the registered keys and points to Settings > Apple ID & iCloud > Sign-In & Security > Security Keys. As a defense-in-depth, `HttpStatusError` now preserves a truncated copy of the CloudKit error body, and a 401 carrying "no auth method found" logs a WARN with the security-key hint and a link to the tracking issue. ([#221])

[#221]: https://github.com/rhoopr/kei/issues/221

---

## [0.9.0] - 2026-04-18

### Added

- **Prometheus metrics and `/healthz` HTTP endpoint.** New `--metrics-port <PORT>` flag (also `KEI_METRICS_PORT` env var and `[metrics] port` TOML key) opts in to an HTTP server that exposes `/metrics` in Prometheus text format and `/healthz` as a JSON health snapshot matching `health.json`. Metrics include cumulative counters (assets seen, downloaded, failed, per-reason skipped, bytes downloaded, bytes written, EXIF/state-write/enumeration failures, session expirations, interrupted cycles) and gauges (cycle duration, consecutive failures, last-success timestamp). When the flag is unset, no server starts and behavior is unchanged. ([#225])

[#225]: https://github.com/rhoopr/kei/pull/225

---

## [0.8.2] - 2026-04-17

### Fixed

- **Apple rate limits and auth-CDN flakes no longer surface as "wrong password".** SRP `/signin/complete` had a catch-all 4xx branch that mapped every non-409/412 status to `FailedLogin("Invalid email/password combination")`. Transient 429 rate limits and 403s from rotated routing cookies looked identical to a credential failure. Complete now distinguishes 401 (the real M1 rejection, the one code that actually means wrong password), 429 (`ApiError` with "Apple is rate limiting authentication"), 400 (`ApiError` with a kei-bug-or-protocol-change message), and other 4xx (`ApiError` with the raw status). `/signin/init` 401 is also no longer `FailedLogin` - init runs before the M1 proof is sent, so a 401 there is a stale auth context (cookies/scnt), not a bad password. ([#226])
- **Bare CloudKit 403 re-authenticates instead of falsely claiming ADP.** HTTP 403 at library init unconditionally mapped to `ServiceNotActivated` with Advanced Data Protection guidance. Real ADP is already caught upstream by `i_cdp_enabled` in the auth response and by CloudKit body errors (ZONE_NOT_FOUND, ACCESS_DENIED, AUTHENTICATION_FAILED); bare 403 has other causes (rate limits, rotated routing cookies, stale sessions). It now maps to `ICloudError::SessionExpired { status: 403 }` so the sync loop re-auths once, and `AUTH_ERROR_THRESHOLD = 3` in the download pipeline stops the loop if the 403 really is persistent ADP. `SessionExpired` now carries the actual HTTP status, so its Display renders "HTTP 403 from CloudKit" instead of always hardcoding "HTTP 401". ([#226])
- **Transient 5xx/429 on SRP and 2FA endpoints retry instead of failing hard.** `/signin/init`, `/signin/complete`, `/repair/complete`, `trigger_push_notification`, and `submit_2fa_code` all retry transient responses up to a shared `AUTH_RETRY_CONFIG` (2 retries, 2s base, 30s max) and honor `Retry-After` when Apple supplies one. Persistent 5xx after the budget is exhausted surfaces as `ApiError` with the actual status, not `FailedLogin`. ([#226])
- **SRP now detects `X-Apple-I-Rscd` session rejections.** Apple sometimes returns HTTP 200 with this header set to 401 or 403 to signal a hidden session rejection. The check was already wired into `/validate` and `/accountLogin`, but SRP init and complete used to treat the 200 as a valid handshake. Both now surface `AuthError::ServiceError` with code `rscd_401` / `rscd_403`. ([#226])
- **CloudKit retry loop honors `Retry-After`.** `HttpStatusError` captures the header (delta-seconds only, capped at 120s), `classify_api_error` returns a new `RetryAfter(Duration)` action on 429/5xx, and `retry_with_backoff` uses the server-provided delay in place of the exponential schedule for that attempt. ([#226])

### Changed

- **`AuthError::is_rate_limited` renamed to `is_transient_apple_failure`** and broadened from HTTP 503 only to 429 + all 5xx. After retries exhaust, the auth layer surfaces "Apple's auth service is returning transient errors (HTTP 429/5xx). Wait a few minutes and retry" context; previously only 503 got the back-off hint. Internal to the binary, no external API impact. ([#226])

[#226]: https://github.com/rhoopr/kei/pull/226

---

## [0.8.1] - 2026-04-17

### Fixed

- **CloudKit 401 / persistent 421 no longer loops under Docker restart.** Removed the 0.8.0 cache-fallback that accepted cached session data when `/validate` and `/accountLogin` both returned 421 — stale cached tokens would then produce a CloudKit 401, the process exited, and Docker restarted with the same cache forever. Auth now falls through to SRP in every stale-session case (trust_token is preserved, so 2FA is skipped in the common path). CloudKit 401 maps to `ICloudError::SessionExpired` and CloudKit 421 (after one pool reset) maps to `ICloudError::MisdirectedRequest`; the sync loop handles both by invalidating the validation cache, forcing SRP, and retrying init. A second failure surfaces cleanly. ([#217], [#199])
- **Final error output carries a timestamp.** The top-level `anyhow::Error` is now routed through `tracing::error!` in addition to stderr, so crash messages in `docker logs` / `journalctl` carry the same `YYYY-MM-DDTHH:MM:SS INFO kei::...` prefix as the rest of the output. Makes the log timeline correlate cleanly instead of jumping from "kei::sync_loop: ..." lines to an unprefixed "Error: ..." line.
- **Duplicate `--album` names no longer error** - `sync --album X --album X` previously failed with "Album 'X' not found" because the album map was drained on first match. Duplicate names are now deduplicated before resolution. ([#219])

### Added

- **Live download-integrity tests** - end-to-end coverage for data-sacred invariants: deleted files get re-downloaded on next sync, and truncated files don't cause data loss. ([#220])
- **Live gap tests** - `sync_watch_runs_multiple_cycles` verifies watch mode completes 2+ cycles, `sync_report_json_writes_valid_schema` parses `--report-json` output against the schema, and new password subcommand edge cases cover clear-without-stored-credential and backend-with-empty-data-dir. ([#219])

### Changed

- **Simplified 421 recovery.** `init_photos_service` no longer ladders through pool reset → 10/30/60s backoff → full re-auth (~150 lines). It resets the HTTP pool once and, on a second 421, surfaces the typed error through the same SRP re-auth path that handles 401. Per-endpoint pool-reset retries in `validate_token` / `authenticate_with_token` were hoisted to a single reset at `auth::authenticate` level, removing duplication.
- **Typed `ICloudError::MisdirectedRequest`.** Replaces the `Connection(String).contains("421")` stringly-typed check in `is_misdirected_request`. `check_apple_rscd` no longer treats `X-Apple-I-Rscd: 421` as an auth failure (never observed in the wild; only fed the removed cache-fallback).
- **CloudKit 421 response body logged at WARN.** Issue #199's breakthrough was seeing `Missing X-APPLE-WEBAUTH-USER cookie` in the 421 body — the single most useful signal for distinguishing ADP-class 421 from session-class 421. Surfaced by default so the next reporter doesn't need `RUST_LOG=debug`.
- **Live tests portable across accounts** - test setup reads `ICLOUD_USERNAME`, `ICLOUD_PASSWORD`, and `KEI_TEST_ALBUM` from env with sane defaults, no code edits needed. Shared bash helpers in `tests/lib.sh`; `run-gap-tests.sh`, `run-deep-validation.sh`, and `run-docker-live.sh` are now committed. ([#222])
- **Consolidated 9 deprecation tests** behind an `assert_deprecated(args, should_succeed, hint)` helper. Test names preserved so CI output still pinpoints the failing case. ([#219])

[#199]: https://github.com/rhoopr/kei/issues/199
[#217]: https://github.com/rhoopr/kei/issues/217
[#219]: https://github.com/rhoopr/kei/pull/219
[#220]: https://github.com/rhoopr/kei/pull/220
[#222]: https://github.com/rhoopr/kei/pull/222

---

## [0.8.0] - 2026-04-16

### Added

- **`--report-json <path>`** - writes a JSON summary after each sync cycle with schema version, CLI options, stats, and per-reason skip breakdown. In watch mode, each cycle overwrites the file. ([#195])
- **Bandwidth and disk usage tracking** - bytes downloaded and bytes written to disk are tracked through the download pipeline and shown in the sync summary. ([#196], [#197])
- **Per-reason skip breakdown** - sync summary now shows why assets were skipped: already downloaded, on disk, filtered by media type / date range / live photo / filename, excluded by album, live photo variants, retries exhausted. ([#198])
- **Extended notification env vars** - 21 new `KEI_*` variables passed to notification scripts: core counts (`KEI_DOWNLOADED`, `KEI_FAILED`, `KEI_SKIPPED`, ...), transfer stats (`KEI_BYTES_DOWNLOADED`, `KEI_DISK_BYTES`), error details, and the full skip breakdown. Absent for non-sync events, so existing scripts don't break. ([#212])

### Changed

- **Default log level restored to `info`** - was changed to `warn` in v0.7.11. Internal implementation messages (service init, token management, library resolution, schema migration) moved to `debug` so the default output is clean. ([#216])
- **Sync summary in plain English** - replaces the old `downloaded=N skipped=N failed=N` structured format with readable output like `50 downloaded, 350 skipped, 0 failed (400 total)`. ([#216])
- **`FilterReason` enum** - `is_asset_filtered` returns `Option<FilterReason>` instead of `bool`, enabling per-reason skip accounting in both full and incremental sync paths. ([#216])

### Fixed

- **Cookie domain loss on restart** - `reqwest::Jar::cookies()` strips `Domain=` attributes when persisting. On reload, cookies were host-scoped (e.g. `setup.icloud.com`) instead of broad domain (`.icloud.com`), so they weren't sent to `ckdatabasews.icloud.com` - causing 401 errors after container restarts. ([#217])
- **421 recovery triggered unnecessary 2FA** - the recovery order was pool reset -> re-auth -> backoff. A transient routing issue would nuke a valid session and force SRP + 2FA. Reordered to pool reset -> backoff with fresh pools (10s/30s/60s) -> re-auth as last resort. ([#217])
- **Auth 421 cache fallback** - when both `/validate` and `/accountLogin` return 421, cached auth data is now used instead of falling through to SRP. Prevents unnecessary 2FA prompts when Apple's auth CDN has a routing issue. ([#217])
- **CloudKit `HttpStatusError` not retried** - HTTP 5xx and 429 responses wrapped in `HttpStatusError` fell through to `Abort` instead of `Retry` in `classify_api_error`. ([#217])
- **Stale validation cache after 2FA** - the cache file is now deleted before retrying auth after a 2FA code submission, preventing `authenticate()` from returning pre-2FA cached data. ([#216])

[#195]: https://github.com/rhoopr/kei/issues/195
[#196]: https://github.com/rhoopr/kei/issues/196
[#197]: https://github.com/rhoopr/kei/issues/197
[#198]: https://github.com/rhoopr/kei/issues/198
[#212]: https://github.com/rhoopr/kei/issues/212
[#216]: https://github.com/rhoopr/kei/pull/216
[#217]: https://github.com/rhoopr/kei/issues/217

## [0.7.12] - 2026-04-15

### Fixed

- **On-disk assets falsely promoted to failed** - assets found on disk during sync were skipped without updating state, so `promote_pending_to_failed` marked them as failed at sync end. Fixed by tracking `last_seen_at` for on-disk assets. ([#206])
- **Skip counters inflated by per-version counting** - reported skip numbers were per-version instead of per-asset, overstating actual skips. Now counted per-asset. ([#206])
- **503 from Apple auth cascaded through all strategies** - a rate-limited response cascaded through validate, accountLogin, and SRP, each getting 503'd and extending the rate-limit window. Now bails on first 503. ([#206])
- **421 Misdirected Request in auth endpoints** - now triggers connection pool reset before escalating to full re-auth. ([#206])
- **Watch-mode reauth ignored CloudKit partition changes** - if the `ckdatabasews` URL changed between auth calls, kei continued with the stale endpoint. Now detects the change and forces full service re-init. ([#206])
- **Duplicate error log in Phase 2 download pass** - collapsed to a single message. ([#206])
- **rustls-webpki security update** - 0.103.10 to 0.103.12 (RUSTSEC-2026-0098, RUSTSEC-2026-0099). ([#206])

### Changed

- **Session reuse via accountLogin** - kei now tries `accountLogin` before falling through to SRP, cutting SRP handshakes from N-per-run to 1. Avoids Apple's SRP rate limit in watch mode and test suites. ([#206])
- **Validation cache** - skips the `/validate` call when the session was validated recently. ([#206])
- **AssetDisposition enum replaces boolean skip flags** - explicit state tracking (OnDisk, AmpmVariant, Filtered, RetryExhausted, Forwarded) with an invariant check at sync end: total = downloaded + failed + skipped. ([#206])
- **Codebase restructured** - extracted `src/commands/`, `src/sync_loop.rs`, `download/pipeline.rs`, `download/filter.rs` from monolithic `main.rs` and `download/mod.rs`. ([#206])
- **Typed auth errors** - string-based HTTP status detection replaced with `AuthError` variants (`is_rate_limited()`, `is_misdirected_request()`). ([#206])
- **Config hashing stabilized** - deterministic cross-platform hashing. ([#206])

### Added

- 39 new tests covering sync decisions, API error handling, and state edge cases. ([#206])
- SECURITY.md, CONTRIBUTING.md, PR template, and credential storage docs. ([#206])

[#206]: https://github.com/rhoopr/kei/pull/206

## [0.7.11] - 2026-04-14

### Fixed

- **Pending assets promoted to failed at end of sync** - assets still pending when a sync run finishes (not re-enumerated by the API or silently failed) are now promoted to `failed` with a descriptive error. Makes stuck assets visible in `kei status --failed` and unblocks incremental sync from being forced into full enumeration indefinitely. ([#208])

### Changed

- **Default log level changed from `info` to `warn`** - reduces output noise for typical usage. Set `log_level = "info"` in config or pass `--log-level info` for verbose output. ([#208])

[#208]: https://github.com/rhoopr/kei/pull/208

## [0.7.10] - 2026-04-14

### Fixed

- **Assets stuck in pending after exceeding retry limit** - assets that hit `max_download_attempts` were silently skipped in the producer loop, stuck as "pending" forever. They didn't appear in `kei status --failed` and `kei reset failed` couldn't touch them. Now marked as `failed` with a descriptive error so they're visible and recoverable. ([#207])

### Changed

- **Failed assets auto-retry on sync start** - all previously failed assets are reset to pending and have their attempt counts cleared before each sync via `prepare_for_retry()`. No need to manually `--retry-failed`. ([#207])
- **Incremental sync falls back to full when pending assets exist** - the `changes/zone` API only returns new modifications and can't re-enumerate pending assets from prior syncs. kei now detects leftover pending assets and falls back to full enumeration until everything is downloaded, then resumes incremental. ([#207])

[#207]: https://github.com/rhoopr/kei/pull/207

## [0.7.9] - 2026-04-13

### Fixed

- **421 re-auth no longer triggers 2FA** - strategy 2 was deleting the entire session file, which nuked `trust_token`. Apple treated every re-auth as a new device and demanded 2FA. Now rewrites the session file keeping only `trust_token` and `client_id`, clearing routing state (`session_token`, `session_id`, `scnt`). SRP still runs fresh but sends the preserved token in `trustTokens` so Apple can skip 2FA for recognised devices. Session file writes are now atomic. ([#204])

[#204]: https://github.com/rhoopr/kei/pull/204

## [0.7.8] - 2026-04-13

### Fixed

- **HTTP 403 on CloudKit now shows ADP guidance** - a 403 Forbidden after successful authentication was reported as a generic "Connection error" with no hint about ADP. Now detected and surfaced with the same actionable message as other service-not-activated errors. ([#199])

### Changed

- **ADP detection restored to hard stop** - v0.7.7 softened the `iCDPEnabled` check to a warning, but testing confirms ADP must be off for web API access to work. Restored to an immediate bail with a clear message. ([#199])
- **ADP messaging requires both settings** - all error messages now tell users to disable ADP *and* enable "Access iCloud Data on the Web". Previous wording implied either alone was enough. ([#199])

[#199]: https://github.com/rhoopr/kei/issues/199

## [0.7.7] - 2026-04-13

### Changed

- **ADP detection softened to warning** - `iCDPEnabled` only indicates ADP is active on the account, not whether web access is blocked. Users with ADP enabled and "Access iCloud Data on the Web" turned on have a valid config. The hard bail is now a warning log, allowing the sync to proceed. ([#99])

[#99]: https://github.com/rhoopr/kei/issues/99

## [0.7.6] - 2026-04-13

### Added

- **ADP detection** - kei now detects Advanced Data Protection (ADP) on the account at login and bails early with an actionable error message, instead of failing later with a cryptic CloudKit error. ([#203], [#99])
- ADP incompatibility documented in README and bug report template. ([#202])
- Bug report issues automatically labeled `user-reported`. ([#202])

[#99]: https://github.com/rhoopr/kei/issues/99
[#202]: https://github.com/rhoopr/kei/issues/202
[#203]: https://github.com/rhoopr/kei/issues/203

## [0.7.5] - 2026-04-13

### Fixed

- **421 recovery retries with exponential backoff** - After both recovery strategies fail (connection pool reset and full re-auth), kei now retries 3 more times with 10s/30s/60s backoff and fresh connection pools on each attempt. Handles transient Apple-side partition routing issues that resolve on their own. ([#199])
- CloudKit error responses (including 421) now have their response body captured and logged at DEBUG level for diagnostics, instead of being silently discarded.

## [0.7.4] - 2026-04-13

### Fixed

- **421 recovery still returned stale partition URL** - Strategy 2 ("full re-auth") called `authenticate()`, which found the persisted session token, validated it (token was fine), and returned the same `ckdatabasews` URL without ever performing SRP. Now deletes the session file before re-authenticating so a fresh SRP login is forced, giving Apple's `/accountLogin` a clean session context to return an updated partition URL. ([#199])

## [0.7.3] - 2026-04-13

### Fixed

- **421 Misdirected Request recovery** - v0.7.2's fix forced full SRP re-auth on every 421, then bailed if Apple returned the same partition URL. Now tries two strategies in order: reset the HTTP connection pool first (no password or 2FA prompt needed), then fall back to full re-auth only if the 421 persists. ([#199], [#201])

### Changed

- Move `AssetItemType`, `AssetVersionSize`, `ChangeReason` from `icloud/photos/types.rs` to `types.rs` (provider-agnostic).
- Extract `build_clients()` to deduplicate HTTP client construction between `Session::build()` and `reset_http_clients()`.
- `acquire_lock()` helper replacing repeated lock patterns in state DB.
- Pre-allocate bulk DB loads with `COUNT(*)` + `with_capacity`.
- `rename_part_to_final()` for handling concurrent download rename races.
- Increase state-write retry from 3 to 6 attempts with exponential backoff.
- `Box<str>` for CloudKit error fields, deferred clone in record filtering.
- Tighten `pub` to `pub(crate)` on internal types.
- Add `// SAFETY:` comments on all unsafe blocks.
- Const assertion guarding shift overflow in retry backoff.

[#201]: https://github.com/rhoopr/kei/pull/201

## [0.7.2] - 2026-04-13

### Fixed

- **421 Misdirected Request recovery** - The recovery path reused the existing session (same HTTP/2 connection pool, cookie jar, and session headers), so Apple returned the same stale partition URL on re-auth. Now tears down the session completely, clears persisted session/cookie files, and creates a fresh session via `auth::authenticate()`. Also adds a same-URL guard to bail early if Apple returns the same partition after clean re-auth. ([#199], [#200])

[#199]: https://github.com/rhoopr/kei/issues/199
[#200]: https://github.com/rhoopr/kei/pull/200

## [0.7.1] - 2026-04-12

### Fixed

- **Session lock held during watch idle sleep** - The exclusive file lock was held for the entire watch cycle including idle sleep, preventing `login get-code` / `login submit-code` from acquiring the lock for 2FA. Lock is now released before sleep and reacquired after. ([#191], [#192])
- **`--library` ignored by `import-existing` and `list albums`** - Users with shared libraries couldn't import or list albums for those libraries. ([#190])
- **Error message referenced deprecated `kei credential set`** - Updated to `kei password set`. ([#190])
- Replace blocking `std::fs::metadata` with `tokio::fs::metadata` in 2FA polling loop.
- Log `touch_last_seen` database errors instead of silently discarding them.

### Added

- `Session::reacquire_lock()` for re-locking after release in watch mode. ([#192])
- `AuthError::LockContention` typed variant, replacing fragile string matching. ([#192])
- `retry_on_lock_contention()` so `login` subcommands wait briefly instead of failing when sync is mid-auth. ([#192])

### Changed

- Return `Option<&str>` from `Session::client_id` instead of `Option<&String>`.
- Remove unnecessary `async` from `run_password` and `run_config_show`.
- Narrow 12 `pub` items to `pub(crate)` for tighter internal visibility.
- Add `const fn` on 10 predicate/constructor functions.
- Extract `TWO_FA_POLL_SECS` and `STALE_PART_FILE_SECS` named constants.
- Apply clippy pedantic/nursery fixes across 30 files (redundant closures, `Self` usage, idiomatic bindings, structured tracing, thiserror on `PartialSyncError`, `let-else`, lazy `or_else`).

[#190]: https://github.com/rhoopr/kei/pull/190
[#191]: https://github.com/rhoopr/kei/issues/191
[#192]: https://github.com/rhoopr/kei/pull/192

## [0.7.0] - 2026-04-11

### Added

- **Subcommand hierarchy** - `login` (get-code, submit-code), `list` (albums, libraries), `password` (set, clear, backend), `reset` (state, sync-token), `config` (show, setup). Cleaner `--help` with grouped commands. ([#170])
- **`config show`** - Dump resolved configuration as TOML with password redacted. ([#117])
- **`reset sync-token`** - Clear stored sync tokens so the next sync does a full enumeration. ([#168])
- **`KEI_*` environment variables** - Every CLI flag has an env var (`KEI_DIRECTORY`, `KEI_DATA_DIR`, `KEI_SIZE`, etc.). Useful for Docker. ([#118])
- **`--data-dir`** - Global flag replacing `--cookie-directory` for session/state/credential storage.
- **`sync --retry-failed`** - Flag on sync replacing the `retry-failed` subcommand.
- **`--live-photo-mode`** - Control live photo handling: `both` (default), `image-only`, `video-only`, `skip`. Replaces `--skip-live-photos`.
- **`--exclude-album`** - Exclude specific albums from sync. Multi-value, comma-separated. [env: `KEI_EXCLUDE_ALBUM`]
- **`--filename-exclude`** - Exclude files matching glob patterns (e.g., `*.AAE`, `Screenshot*`). Case-insensitive, multi-value. [env: `KEI_FILENAME_EXCLUDE`]
- **`{album}` token in `--folder-structure`** - Organize downloads by album name (e.g., `{album}/%Y/%m`).
- **Full strftime support in `--folder-structure`** - All standard strftime specifiers now work (`%B`, `%A`, `%j`, etc.), not just the six previously supported.
- **Auto-config on first run** - When no config file exists, kei creates a minimal `~/.config/kei/config.toml` from CLI arguments. Opt out with `KEI_NO_AUTO_CONFIG=1`.

### Fixed

- **421 Misdirected Request recovery** - When Apple migrates an account to a different CloudKit partition, the cached service URL stops working (HTTP 421). kei now performs a full SRP re-authentication to obtain fresh service URLs, recovering automatically. Previously, recovery attempted token validation which returned the same stale URLs. ([#175], [#177])

### Changed

- **`--username`, `--domain` are now global** - Accepted on all subcommands, not just sync.
- **Docker CMD** - Uses `--data-dir` instead of `--cookie-directory`.
- **`password` replaces `credential`** - `kei password set|clear|backend`. Old `credential` subcommand still works as hidden alias.
- **Folder structure token expansion** uses `chrono::strftime` instead of manual parsing. Behavior is unchanged for existing templates.
- **Filename exclude patterns** are compiled once at config build time for performance.

### Deprecated

- `--cookie-directory` (use `--data-dir`)
- `--auth-only` (use `kei login`)
- `--list-albums` / `--list-libraries` (use `kei list albums` / `kei list libraries`)
- `--reset-sync-token` flag on sync (use `kei reset sync-token`)
- `--skip-live-photos` (use `--live-photo-mode skip`)
- Top-level `get-code`, `submit-code`, `credential`, `retry-failed`, `reset-state`, `reset-sync-token`, `setup` subcommands (use new grouped equivalents)

All deprecated syntax continues to work and prints a one-line warning to stderr.

[#117]: https://github.com/rhoopr/kei/issues/117
[#118]: https://github.com/rhoopr/kei/issues/118
[#168]: https://github.com/rhoopr/kei/issues/168
[#170]: https://github.com/rhoopr/kei/pull/170
[#175]: https://github.com/rhoopr/kei/issues/175
[#177]: https://github.com/rhoopr/kei/pull/177

## [0.6.2] - 2026-04-08

### Fixed

- **Live photo MOV download failures** - Content validation no longer rejects live photo MOV files served in classic QuickTime format (without `ftyp` box). Magic byte mismatches are now logged as warnings instead of errors. HTML error pages from Apple's CDN remain a hard error. ([#166])

### Changed

- **Credential key file renamed** - The encrypted credential key file is renamed from `.credential-key` to `.session-state`. Existing files are migrated silently.

[#166]: https://github.com/rhoopr/kei/issues/166

## [0.6.1] - 2026-04-08

### Fixed

- **Apple iOS 26.4 2FA push change** - Apple changed the 2FA push mechanism around iOS 26.4. The old `bridge/step/0` endpoint no longer reliably delivers codes to trusted devices. Switched to PUT `/verify/trusteddevice/securitycode`, which works on both old and new iOS versions. ([#164])
- **Docker first-run crash loop** - When no password was configured and stdin wasn't a terminal (Docker, cron), kei exited with the cryptic "Password provider returned no data". Now shows an actionable error listing all password options. ([#163])

### Added

- **2FA retry loop** - Interactive 2FA prompt now allows up to 3 wrong code attempts instead of exiting on the first failure. Press Enter without a code to request a new push notification.
- **2FA code normalization** - Codes with spaces or dashes ("123 456", "123-456") are accepted in both the interactive prompt and `submit-code` CLI arg.

[#163]: https://github.com/rhoopr/kei/issues/163
[#164]: https://github.com/rhoopr/kei/issues/164

## [0.6.0] - 2026-04-06

### Added

- **Credential management** - New `credential` subcommand with `set`, `clear`, and `backend` actions. Passwords are stored in the OS keyring (macOS Keychain, Linux Secret Service, Windows Credential Manager) when available, with an AES-256-GCM encrypted file fallback for headless environments like Docker.
- **`--password-file`** - Read password from a file on each auth attempt. Supports Docker secrets (`/run/secrets/icloud_password`). Trailing newline is stripped. Conflicts with `--password`.
- **`--password-command`** - Execute a shell command to obtain the password on each auth attempt. Supports external secret managers like 1Password, Vault, and pass. Example: `--password-command "op read 'op://vault/icloud/password'"`. Conflicts with `--password` and `--password-file`.
- **`--save-password`** - After successful auth, persist the password to the credential store. On subsequent runs (including watch mode re-auth), the stored password is used automatically.
- **Adjusted video downloads** - `--size adjusted` and `--live-photo-size adjusted` download Apple's edited versions of videos and live photo MOVs. Falls back to original when no adjusted version exists (unless `--force-size` is set). ([#93])
- **Docker HEALTHCHECK** - The container now writes a `health.json` file to `/config` with `last_sync_at`, `last_success_at`, `consecutive_failures`, and `last_error`. The Dockerfile includes a HEALTHCHECK that marks the container unhealthy after 5 consecutive failures or 2 hours without a sync.
- **Hard shutdown timeout** - 30 seconds after the first shutdown signal (Ctrl+C, SIGTERM, SIGHUP), in-flight downloads are cancelled and the process exits. A second signal still force-exits immediately.
- **Low disk space warning** - Logs a warning before downloads if the target filesystem has less than 1 GiB free.
- **Structured exit codes** - `0` success, `1` failure, `2` partial sync (some downloads failed), `3` auth failure. Useful for scripting and monitoring.
- **HTTP status validation on CloudKit API responses** - Catches non-2xx responses that were previously ignored.
- **Config hash includes filter fields** - Changing `--skip-videos`, `--skip-photos`, `--recent`, date ranges, or album filters now automatically clears stored sync tokens, forcing a full re-scan so the filter change takes effect.
- **Password security** - Passwords use `SecretString` (auto-zeroized on drop, redacted from `Debug`/`Display`). The `ICLOUD_PASSWORD` environment variable is cleared from the process after reading.

### Changed

- **`--watch-with-interval` minimum raised to 60 seconds.** Values below 60 are rejected. Previously accepted down to 1 second. ([#125])
- **`--max-retries` capped at 100.** Previously unbounded.
- **`--retry-delay` range restricted to 1-3600 seconds.** Previously unbounded.
- **Mutually exclusive flags enforced** - `--auth-only` now conflicts with `--watch-with-interval`, `--list-albums`, and `--list-libraries`. `--list-albums` and `--list-libraries` conflict with `--watch-with-interval`.
- **Empty `--username` and `--password` rejected at parse time.** Passing `--password ""` is now an error instead of silently proceeding with an empty string.
- **`submit-code` validates 6-digit format.** Non-numeric or wrong-length codes are rejected before any network call.
- **Invalid filename characters replaced with `_`** instead of silently removed. `photo:/name` becomes `photo__name` rather than `photoname`. Matches Python icloudpd behavior. ([#139])
- **Cookie and session files written atomically** (write to temp, then rename). Prevents corruption if the process is killed mid-write.
- **State flushed to SQLite after each download** instead of at the end. Eliminates a crash window where completed downloads weren't recorded.
- **EXIF writes applied to `.kei-tmp` file** before renaming to the final path. EXIF failures no longer leave a file in the download directory with incorrect metadata.
- **Content validated before rename** - SHA256 checksum and size are verified while the file is still `.kei-tmp`. Failed verification doesn't pollute the download directory.
- **`verify` streams results** through paginated queries instead of loading all records into memory.
- **`docker-compose.yml` updated** with credential options (encrypted store, Docker secrets, password-command) and commented examples. Default `ICLOUD_PASSWORD` env var removed in favor of more secure alternatives.
- **Docker `stop_grace_period` set to 30 seconds** to allow in-flight downloads to finish before SIGKILL.
- Config files containing a password now warn if group/world-readable.
- Shared immutable data (`zone_id`, CloudKit params) uses `Arc<str>` / `Arc<Value>` to reduce cloning.
- Blocking filesystem I/O (stat calls, directory cache) moved off tokio worker threads.
- Schema migrations wrapped in SAVEPOINTs to prevent partial application on failure.
- Four separate COUNT queries in `get_summary` consolidated into a single table scan.
- Added `secrecy`, `keyring`, `aes-gcm`, `libc`, `bytes`, `http` dependencies. Added `wiremock` and `tracing-test` dev dependencies. Removed unused `uuid` v1 feature.

### Fixed

- **Pagination terminated on zero masters instead of zero records**, causing premature stop when a page contained only companion assets (like MOV files without their parent photo). ([#140])
- **Apple auth endpoints returning HTTP 200 with error payloads went undetected.** Auth errors embedded in successful HTTP responses are now parsed and surfaced. ([#140])
- **`--folder-structure` accepted path traversal sequences** like `../../etc`. Traversal components are now stripped. ([#126])
- **Non-existent `--cookie-directory` silently ignored until deep in auth setup.** The directory is now validated (and created if possible) at config build time. ([#126])
- **Long usernames caused OS "file name too long" errors** when creating lock/session/DB files. Usernames longer than 64 characters are now truncated with an FNV hash suffix. ([#126])
- **Filenames exceeding 255-byte filesystem limit** caused write failures. Long filenames are now truncated while preserving the extension.
- **Filename truncation with oversized extensions** dropped the extension entirely instead of truncating the stem.
- **Multi-byte UTF-8 in auth response body preview** could panic when truncated mid-character.
- **SRP PBKDF2 iteration count unbounded** - a malicious server response could request billions of iterations, hanging the client. Now capped.
- **Sync token not preserved on `changes_stream` error**, forcing a full library re-enumeration on the next run instead of resuming from the last good token.
- **`DeltaRecordBuffer` not flushed on `changes_stream` error**, losing already-buffered records.
- **Failed state DB writes silently dropped.** Now retried up to 3 times before reporting failure.
- **Spawned-task panics silently dropped.** Panics in download and enumeration tasks are now propagated to the parent.
- **Legacy cookie parser didn't recover from corruption.** Corrupt cookie files are now detected and the session re-authenticates.
- **Retry attempt counter could overflow** on pathological retry counts. Uses saturating arithmetic.
- **`--set-exif-datetime` silently failed on every JPEG** because `little_exif` determines file type from the extension, and the `.kei-tmp` temp file extension was unrecognized. Switched to in-memory EXIF writing with explicit JPEG type.
- **Post-download checksum verification warned on 100% of files.** Apple's `fileChecksum` is an MMCS compound signature, not a content hash - the comparison could never succeed. Removed in favor of size and content-type validation.
- **Config hash changed on every run with relative date intervals** (e.g., `--skip-created-before 20d`). The resolved timestamp included seconds, producing different hashes seconds apart. Now truncated to day precision.
- **Changing `--recent` forced full library re-enumeration.** The incremental path already applies the recent cap post-fetch, so sync token invalidation was unnecessary. `--recent` is now excluded from the enumeration config hash.
- **`--dry-run` and `--only-print-filenames` could be combined with `--watch-with-interval`**, creating an infinite no-op loop. Now rejected as conflicts.
- **`--recent 0` accepted on CLI and in TOML**, producing a no-op sync. Now requires >= 1.
- **TOML config allowed `password` + `password_file` simultaneously** without error. CLI enforced mutual exclusivity but TOML did not. Now validated at config build time.
- **Apple HTTP 503 errors dumped raw HTML** into error messages. Server errors now show a clean status message; client errors (4xx) are distinguished with different guidance.
- **Docker HEALTHCHECK failed on fresh containers** because `date -d "null"` is invalid when `last_sync_at` is null before the first sync. Staleness check is now skipped when no sync has occurred.
- **Docker HEALTHCHECK `start-period` increased from 10 to 15 minutes** to accommodate first-sync enumeration of large libraries.
- **`import-existing --no-progress-bar` suppressed the final summary**, leaving Docker users with zero output. Summary now always prints.

[#93]: https://github.com/rhoopr/kei/issues/93
[#125]: https://github.com/rhoopr/kei/issues/125
[#126]: https://github.com/rhoopr/kei/issues/126
[#139]: https://github.com/rhoopr/kei/issues/139
[#140]: https://github.com/rhoopr/kei/issues/140

---

## [0.5.3] - 2026-04-03

### Added

- **`get-code` subcommand** - Triggers Apple to send a 2FA code to your trusted devices. In Docker, run `docker exec kei kei get-code` when you're ready to receive a code, then `docker exec kei kei submit-code <CODE>` to submit it.

### Fixed

- **Docker 2FA flow reworked** - v0.5.2 never triggered the push notification in headless mode, so users were told to submit a code that was never sent. The container now detects 2FA, logs what to do, and waits. `get-code` and `submit-code` are separate manual steps - no surprise notifications from unattended restarts. `submit-code` no longer fires a new push notification, which was invalidating the code being submitted. ([#153])
- **False wakeups during 2FA wait** - `get-code` writes to the session file during SRP auth, which woke the waiting container before the session was actually trusted. The wait loop now retries on `TwoFactorRequired` instead of exiting.
- **Lock contention with `submit-code`** - If `submit-code` was still running when the container woke up, the lock error crashed the process. The retry now backs off and retries up to 3 times.
- **Push notification errors swallowed** - `get-code` now reports when Apple's bridge endpoint rejects the push request instead of telling you a code was sent.

[#153]: https://github.com/rhoopr/kei/pull/153

---

## [0.5.2] - 2026-04-02

### Fixed

- **Docker restart loop during 2FA** - v0.5.1's push notification bridge call fired before checking whether a code could be collected, causing repeated Apple API hits in a non-TTY restart loop until `securityCodeLocked`. kei now bails before the bridge call in headless mode and stays running while waiting for `submit-code` instead of exiting. ([#152])

[#152]: https://github.com/rhoopr/kei/pull/152

---

## [0.5.1] - 2026-04-02

### Added

- **Push notification to trusted devices during 2FA** — Apple requires a POST to `/auth/bridge/step/0` to initiate push notifications for 2FA codes. Without this, some accounts only receive a "website login" email instead of a code on their trusted devices. ([#151])

[#151]: https://github.com/rhoopr/kei/pull/151

---

## [0.5.0] - 2026-04-01

### Changed
- **Renamed project from icloudpd-rs to kei.** Binary, crate, Docker image, Homebrew formula, and default paths have all changed. See migration guide.
- Default cookie directory: `~/.icloudpd-rs` → `~/.config/kei/cookies`
- Default config path: `~/.config/icloudpd-rs/config.toml` → `~/.config/kei/config.toml`
- Default temp suffix: `.icloudpd-tmp` → `.kei-tmp`
- Notification env vars: `ICLOUDPD_EVENT` → `KEI_EVENT`, `ICLOUDPD_MESSAGE` → `KEI_MESSAGE`, `ICLOUDPD_USERNAME` → `KEI_ICLOUD_USERNAME`
- Docker image: `ghcr.io/rhoopr/icloudpd-rs` → `ghcr.io/rhoopr/kei`
- Auto-migration: existing `~/.icloudpd-rs/` and `~/.config/icloudpd-rs/` data is automatically copied to new paths on first run.

---

## [0.4.2] - 2026-03-30

### Fixed

- **"Photo library not finished indexing" blocking all operations** - The `CheckIndexingState` gate has been downgraded from a fatal error to a warning. The iCloud API serves photos normally regardless of this field, but stale or freshly-created sessions often return a non-`FINISHED` state, permanently blocking downloads, album listings, and all other photo operations. Users now see a log warning and proceed as normal ([#144])

[#144]: https://github.com/rhoopr/icloudpd-rs/issues/144

---

## [0.4.1] - 2026-03-28

### Added

- **`--only-print-filenames` flag** - Prints the paths of files that would be downloaded, one per line to stdout, without actually downloading them. Progress bar is suppressed. Respects state DB filtering so only undownloaded files are listed. Doesn't advance the sync token - safe to run before a real sync ([#17])
- **`--version` flag** - Standard `-V` / `--version` support ([#127])
- **`--no-progress-bar` for `import-existing`** - Suppresses all progress output (header, periodic counter, summary), matching the behavior of `sync` and `retry-failed` ([#127])

### Fixed

- **Progress bar overshoot with companion files** - Live photos produce two download tasks (image + MOV), but the progress bar total is the photo count. The bar now increments once per photo in the producer instead of once per task in the consumer, so it no longer shows "53/50" ([#47])
- **Confusing cookiejar warning on first run** - Changed the cookiejar existence check from `.exists()` to `.is_file()`, preventing the misleading "Failed to read cookiejar: Is a directory" warning when the cookie path isn't a regular file ([#127])

### Changed

- Updated `aws-lc-sys` to 0.39.1 and `rustls-webpki` to 0.103.10 (RUSTSEC-2026-0044, RUSTSEC-2026-0048, RUSTSEC-2026-0049)
- Bumped `rand` 0.9→0.10, `rusqlite` 0.38→0.39, `sd-notify` 0.4→0.5, `toml` 0.8→1.0, `clap` 4.5→4.6
- Narrowed `tokio` features from `"full"` to minimal set; removed unused direct `time` dependency

[#17]: https://github.com/rhoopr/icloudpd-rs/issues/17
[#47]: https://github.com/rhoopr/icloudpd-rs/issues/47
[#127]: https://github.com/rhoopr/icloudpd-rs/issues/127

---

## [0.4.0] - 2026-03-11

### Added

- **Incremental sync via CloudKit syncToken** - After the first full sync, subsequent runs use Apple's `changes/database` and `changes/zone` APIs to fetch only new/changed/deleted photos instead of re-enumerating the entire library. A no-change cycle completes in 1-2 API calls (~75 fewer than a full scan). Tokens are persisted per-zone in the state DB's metadata table and chained across paginated responses for crash-safe resume. Falls back to full enumeration automatically if a token expires or the server rejects it ([#131])
- **`--library all`** - Downloads from all available libraries (personal + shared) in a single run instead of requiring separate `--library` invocations per zone. Each library syncs with its own per-zone sync token. `--list-albums --library all` shows albums grouped by library ([#98])
- **`--no-incremental` flag** - Forces a full library enumeration even when a stored sync token exists. Available on `sync` and `retry-failed` ([#131])
- **`--reset-sync-token` flag** - Clears stored sync tokens before syncing. Useful for recovery if incremental sync gets into a bad state ([#131])
- **Early state DB skip** - During re-syncs, assets already confirmed in the state DB skip path resolution and filesystem checks entirely. Uses a config hash to detect when download settings change (invalidating trust). Eliminates ~16k path resolutions per cycle for a 16k-photo library with only a handful of new photos. Adds metadata table (schema v2 migration) ([#129])

### Fixed

- **SharedSync zone queried for unsupported album types** - Smart folder and user album queries were sent to SharedSync zones, which don't support them. These queries are now skipped for shared libraries ([#98])
- **`retry-failed` downloading entire library** - `retry-failed` now only retries assets already known to the state DB, skipping new iCloud assets that appear between runs ([#129])
- **SHA-1 checksum support** - Apple's 20-byte (raw SHA-1) and 21-byte (0x01 prefix + SHA-1) checksum formats are now handled in both downloads and verify ([#129])
- **Session cookie persistence** - All cookies from the jar (including those set by redirect responses) are now persisted, so sessions survive process restarts ([#129])
- **Docker lock contention UX** - Improved error message when the lock file is held by another instance, with Docker-specific troubleshooting guidance ([#129])
- **Large async futures** - Heap-allocate 256 KiB resume buffer and `Box::pin` deep async chains to prevent ~263 KiB stack futures per concurrent download ([#129])
- **Write lock timeout** - 30s timeout on session validation prevents a hung HTTP request from starving download tasks ([#129])
- **Schema migration logic** - `migrate_to_version` now uses a proper `match` on version instead of always applying SCHEMA_V1, which would have broken on future migrations ([#129])

### Changed

- Boxed large error enum variants (`reqwest::Error`, `io::Error`) in `DownloadError`, `AuthError`, `ICloudError` to reduce stack size ~75% with compile-time size guards ([#131])
- Converted ~70 tracing calls across 13 files from string interpolation to structured fields ([#131])
- Fused incremental sync event filtering into a single pass, removing intermediate `Vec` and two redundant iterations ([#131])
- Replaced bare `as` numeric casts with `try_from().unwrap_or()` in SQLite layer to prevent silent overflow ([#131])
- Increased auth throttle to 8s to avoid Apple SRP rate limiting during rapid re-auth
- Updated quinn-proto to 0.11.14 (RUSTSEC-2026-0037 fix) ([#131])
- Inline format args across 10 files (~40 instances) ([#129])
- Narrowed `pub` to `pub(crate)` for 14 functions and 6 structs ([#129])
- Capped mpsc channel buffer at 500, removed intermediate `.collect()` before `select_all` ([#129])
- Removed needless raw string hashes in SQL literals ([#129])
- Merged identical match arms, used `let...else` and `is_some_and` where applicable ([#129])
- Derived `PartialEq` on `CookieEntry`, flattened nested `if let`, simplified match arms ([#129])
- Replaced redundant `.to_string().into_boxed_str()` with `.clone()` / `.into()` ([#129])

[#98]: https://github.com/rhoopr/icloudpd-rs/issues/98
[#131]: https://github.com/rhoopr/icloudpd-rs/pull/131
[#129]: https://github.com/rhoopr/icloudpd-rs/pull/129

---

## [0.3.0] - 2026-03-07

### Added

#### Configuration

- **TOML config file ([#51])** - Settings can now live in a `config.toml` file instead of (or alongside) CLI flags. Loads from `~/.config/icloudpd-rs/config.toml` by default, or a custom path via `--config`. Grouped into sections: `[auth]`, `[download]`, `[filters]`, `[photos]`, `[watch]`, `[notifications]`. Layered resolution: CLI flags override TOML values, which override built-in defaults. The config file is optional - CLI flags still work exactly as before.

[#51]: https://github.com/rhoopr/icloudpd-rs/issues/51

#### Distribution

- **Docker image ([#40])** - Multi-arch images (amd64/arm64) published to `ghcr.io/rhoopr/icloudpd-rs`. Multi-stage build with `debian:bookworm-slim` runtime (includes `bash` and `curl` for notification scripts). Uses `/config` and `/photos` volumes. Supports `ICLOUD_USERNAME`, `ICLOUD_PASSWORD`, and `TZ` environment variables. Includes `docker-compose.yml` example.

[#40]: https://github.com/rhoopr/icloudpd-rs/issues/40

#### Authentication

- **Headless MFA ([#36])** - New `submit-code` subcommand for non-interactive 2FA. Run `icloudpd-rs submit-code 123456` (or `docker exec icloudpd-rs icloudpd-rs submit-code 123456`) to submit a code from outside the running process. In headless mode (non-interactive stdin), the sync returns a `TwoFactorRequired` status and fires a notification instead of blocking on a prompt.

[#36]: https://github.com/rhoopr/icloudpd-rs/issues/36

#### Notifications

- **Notification scripts ([#32])** - `--notification-script <path>` (or `[notifications] script` in TOML) runs a script on sync events. The script receives `ICLOUDPD_EVENT`, `ICLOUDPD_MESSAGE`, and `ICLOUDPD_USERNAME` as environment variables. Events: `2fa_required`, `sync_complete`, `sync_failed`, `session_expired`. Fire-and-forget execution with a 30-second timeout.

[#32]: https://github.com/rhoopr/icloudpd-rs/issues/32

---

## [0.2.1] - 2026-02-23

### Fixed

- **Parallel photo enumeration** - Library enumeration now runs across multiple parallel API fetchers (2x `--threads-num`), reducing scan time from ~10 minutes to ~30 seconds for a 16k-item library. Previously, pages were fetched sequentially at ~3-4s each ([#114])

[#114]: https://github.com/rhoopr/icloudpd-rs/pull/114

---

## [0.2.0] - 2026-02-09

### Added

- **Watch mode album refresh** - Albums are now re-resolved each watch cycle, so newly created iCloud albums are discovered without restarting the daemon ([#23])
- **`--notify-systemd` flag** - Sends sd_notify messages (`READY`, `STOPPING`, `STATUS`, `WATCHDOG`) for systemd service integration. Linux-only; no-op on other platforms ([#23])
- **`--pid-file` flag** - Writes the process PID to a file on startup and removes it on exit, for service managers and monitoring ([#23])
- **Watch mode error tolerance** - `PartialFailure` outcomes in watch mode now log a warning and continue to the next cycle instead of exiting, since transient failures are expected in long-running sessions ([#23])

[#23]: https://github.com/rhoopr/icloudpd-rs/issues/23

### Fixed

- **Epoch date fallback warnings** - `asset_date()`, `added_date()`, and file mtime now log warnings when falling back to the Unix epoch or clamping negative timestamps, making silent data loss visible
- **EXIF failure tracking** - Download summary now reports EXIF stamping failures separately (e.g., `10 downloaded (2 EXIF failures), 0 failed`) instead of only logging per-file warnings
- **Path traversal protection** - Album names from iCloud are sanitized to prevent directory traversal (`../`), Windows reserved names (`CON`, `NUL`, etc.), and leading dot attacks
- **Unknown checksum format warning** - Checksums with unrecognized formats (not 32 or 33 bytes) now log a warning instead of silently passing verification
- **Resume restart logging** - When a server ignores an HTTP Range header and returns 200 instead of 206, the restart is now logged at info level
- **Password redaction in logs** - Passwords provided via `--password` or `ICLOUD_PASSWORD` are redacted from all tracing output, replacing occurrences with `********`
- **AM/PM filename matching** - Files with whitespace variants before AM/PM (regular space, narrow no-break space U+202F, or no space) are now recognized as the same file, preventing duplicate downloads of macOS screenshots across locale configurations
- **WEBP file type recognition** - WEBP images (`org.webmproject.webp`) are now correctly classified as images instead of defaulting to movie, preventing `--skip-videos` from incorrectly excluding WEBP photos ([#90])
- **Large video download integrity** - Downloads now verify content-length against bytes received before checksum comparison, catching CDN truncation (e.g. Apple silently cutting off videos at ~1 GB) earlier and triggering automatic retry ([#91])
- **CAS Op-Lock / TRY_AGAIN_LATER retry** - CloudKit server errors (`TRY_AGAIN_LATER`, `CAS_OP_LOCK`, `RETRY_LATER`, `THROTTLED`) embedded in JSON responses are now detected and automatically retried with exponential backoff, preventing silent page loss during photo enumeration ([#94])
- **Configurable temp file suffix** - Partial downloads now use `.icloudpd-tmp` by default instead of `.part`, avoiding conflicts with Nextcloud/WebDAV sync clients that reject `.part` files. Configurable via `--temp-suffix` ([#92])
- **Live photo dedup suffix consistency** - When two live photos share the same base filename and size-based deduplication adds a suffix to the HEIC, the MOV companion now derives from the deduped HEIC name, keeping the pair visually matched on disk ([#102])
- **ADP detection and error handling** - Users with Advanced Data Protection (ADP) enabled now receive a clear, actionable error message explaining the incompatibility and how to resolve it, instead of a generic API failure. Detects `ZONE_NOT_FOUND`, `AUTHENTICATION_FAILED`, `ACCESS_DENIED`, and "private db access disabled" responses from CloudKit ([#99])

[#90]: https://github.com/rhoopr/icloudpd-rs/issues/90
[#91]: https://github.com/rhoopr/icloudpd-rs/issues/91
[#92]: https://github.com/rhoopr/icloudpd-rs/issues/92
[#94]: https://github.com/rhoopr/icloudpd-rs/issues/94
[#99]: https://github.com/rhoopr/icloudpd-rs/issues/99
[#102]: https://github.com/rhoopr/icloudpd-rs/issues/102

---

## [0.1.0] - 2026-02-08

Initial release. A ground-up Rust rewrite of [icloud-photos-downloader](https://github.com/icloud-photos-downloader/icloud_photos_downloader) with full photo/video download capabilities and SQLite state tracking.

### Added

#### State Management (New in Rust)

- **SQLite state database** tracks every asset's status (`pending`, `downloaded`, `failed`) with checksums, paths, and error messages
- **Skip-by-database** - subsequent syncs skip already-downloaded assets without filesystem checks
- **Automatic re-download** - if database says downloaded but file is missing, re-downloads automatically
- **Sync run history** - records start time, completion, and statistics for each run

#### CLI Subcommands (New in Rust)

| Command | Purpose |
|---------|---------|
| `sync` | Download photos from iCloud (default) |
| `status` | Show sync status and database summary |
| `retry-failed` | Reset failed downloads to pending and re-sync |
| `reset-state` | Delete the state database and start fresh |
| `import-existing` | Import existing local files into the state database |
| `verify` | Verify downloaded files exist and optionally check checksums |

#### Authentication

- SRP-6a with Apple's custom protocol variants (automatic `s2k`/`s2k_fo` negotiation)
- Two-factor authentication via trusted device codes
- Session persistence with cookie management
- Interactive secure password prompt (or `ICLOUD_PASSWORD` environment variable)
- Automatic SRP repair flow on HTTP 412 responses
- Domain redirect detection for region-specific endpoints (`.cn`)

#### Downloads

- Streaming pipeline with configurable concurrency (`--threads-num`, default: 10)
- Resumable `.part` files via HTTP Range; existing bytes hashed on resume for full SHA256 verification
- Exponential backoff with jitter and transient/permanent error classification
- Progress bar with download tracking (auto-hidden in non-TTY)
- Live photo MOV collision detection with asset ID suffix fallback
- File collision deduplication via `--file-match-policy`
- Two-phase cleanup pass re-fetches expired CDN URLs before final retry
- Deterministic `.part` filenames derived from checksum (base32, filesystem-safe)

#### Content & Organization

- Photo, video, and live photo MOV downloads with size variants
- Shared and private library selection (`--library`) with zone discovery (`--list-libraries`)
- Force exact size variant or skip (`--force-size`)
- RAW file alignment (`--align-raw`: as-is, original, alternative)
- Live photo MOV filename policies (`--live-photo-mov-filename-policy`: suffix, original)
- Independent live photo video size (`--live-photo-size`)
- Content filtering by media type, date range, album, and recency
- Smart album support (favorites, bursts, time-lapse, slo-mo, videos)
- Date-based folder structures (`--folder-structure %Y/%m/%d`)
- EXIF date tag read/write (`--set-exif-datetime`)
- Filename sanitization with Unicode control (`--keep-unicode-in-filenames`)
- Both plain-text and base64-encoded CloudKit filenames supported
- Fingerprint-based fallback filenames when CloudKit filename is absent

#### Operations

- Dry-run mode (`--dry-run`)
- Auth-only mode (`--auth-only`)
- List albums (`--list-albums`) and libraries (`--list-libraries`)
- Watch mode with configurable intervals (`--watch-with-interval`)
- Mid-sync session recovery (up to 3 re-auth attempts)
- Graceful shutdown (first signal finishes in-flight, second force-exits)
- Library indexing readiness check before querying
- Log level control (`--log-level`: debug, info, warn, error)
- Domain selection (`--domain`: com, cn)
- Custom cookie/session directory (`--cookie-directory`)

### Changed (vs Python icloudpd)

These are intentional improvements over the Python implementation:

| Area | Python Behavior | Rust Behavior |
|------|-----------------|---------------|
| **Concurrency** | Sequential downloads (`--threads-num` deprecated) | True parallel downloads (default: 10) |
| **State** | No persistence; re-scans filesystem every run | SQLite tracks state; near-instant subsequent syncs |
| **Startup** | Queries album counts before downloading | Downloads begin as first API page returns |
| **Resumable** | Resumes `.part` files but no checksum verification | Resumes `.part` files with SHA256 verification of full file |
| **Retry control** | Hardcoded `MAX_RETRIES = 0` (no retries) | Configurable `--max-retries` and `--retry-delay` |
| **Session safety** | No file locks; concurrent instances can corrupt | Lock files prevent concurrent corruption |
| **Cookie security** | Default file permissions | Owner-only permissions (`0600`) on Unix |
| **Expired cookies** | Loads with `ignore_expires=True` | Prunes expired cookies on load |
| **CDN expiry** | Failed downloads stay failed | Cleanup pass re-fetches URLs before retry |
| **Mid-sync auth** | Re-authenticates but doesn't retry download | Re-authenticates and retries (up to 3 times) |
| **Recent filter** | Counts albums first, then `islice` to N | Stops fetching from API after N photos |
| **API errors** | Retry loop exists but `MAX_RETRIES = 0` | Automatic retry with jitter on 5xx/429 |
| **Album fetch** | Sequential (`for album in albums`) | Concurrent (bounded by `--threads-num`) |
| **Error handling** | No error classification | Classifies transient vs permanent errors |
| **Cookie format** | LWPCookieJar format | JSON format (not compatible with Python's LWP cookies - re-auth required) |
| **Folder syntax** | Python datetime format (`{:%Y/%m/%d}`) | Both `{:%Y}` and `%Y` strftime accepted |

### Not Implemented (Planned)

The following Python icloudpd features are not yet available. Links go to tracking issues:

#### Coming in v0.5

- [#28](https://github.com/rhoopr/icloudpd-rs/issues/28) - **Auto-delete** (detect iCloud deletions, optionally remove local copies)
- [#29](https://github.com/rhoopr/icloudpd-rs/issues/29) - **Delete after download** (`--delete-after-download`)

#### Authentication & Security
- [#21](https://github.com/rhoopr/icloudpd-rs/issues/21) - SMS-based 2FA (trusted device only currently)
- [#22](https://github.com/rhoopr/icloudpd-rs/issues/22) - OS keyring integration for password storage

#### Content & Downloads
- [#19](https://github.com/rhoopr/icloudpd-rs/issues/19) - XMP sidecar export (`--xmp-sidecar`)
- [#14](https://github.com/rhoopr/icloudpd-rs/issues/14) - Multiple size downloads (`--size` accepting multiple values)
- [#52](https://github.com/rhoopr/icloudpd-rs/issues/52) - HEIC to JPEG conversion (`--convert-heic`)

#### iCloud Lifecycle
- [#30](https://github.com/rhoopr/icloudpd-rs/issues/30) - Keep iCloud recent days (`--keep-icloud-recent-days`)

#### Notifications & Monitoring
- [#31](https://github.com/rhoopr/icloudpd-rs/issues/31) - Email/SMTP notifications on 2FA expiration
- [#55](https://github.com/rhoopr/icloudpd-rs/issues/55) - Prometheus metrics export

#### Configuration
- [#33](https://github.com/rhoopr/icloudpd-rs/issues/33) - Multi-account support

### Removed (vs Python icloudpd)

- `--until-found` - Replaced by SQLite state tracking; the database knows what's already downloaded
- `--smtp-*` flags - Email notifications not yet implemented ([#31](https://github.com/rhoopr/icloudpd-rs/issues/31))

### Known Issues

- [#69](https://github.com/rhoopr/icloudpd-rs/issues/69) - Schema migration logic needs improvement before v2

---

[Unreleased]: https://github.com/rhoopr/kei/compare/v0.6.1...HEAD
[0.6.1]: https://github.com/rhoopr/kei/compare/v0.6.0...v0.6.1
[0.6.0]: https://github.com/rhoopr/kei/compare/v0.5.3...v0.6.0
[0.5.3]: https://github.com/rhoopr/kei/compare/v0.5.2...v0.5.3
[0.5.2]: https://github.com/rhoopr/kei/compare/v0.5.1...v0.5.2
[0.5.1]: https://github.com/rhoopr/kei/compare/v0.5.0...v0.5.1
[0.5.0]: https://github.com/rhoopr/kei/compare/v0.4.2...v0.5.0
[0.4.2]: https://github.com/rhoopr/icloudpd-rs/compare/v0.4.1...v0.4.2
[0.4.1]: https://github.com/rhoopr/icloudpd-rs/compare/v0.4.0...v0.4.1
[0.4.0]: https://github.com/rhoopr/icloudpd-rs/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/rhoopr/icloudpd-rs/compare/v0.2.1...v0.3.0
[0.2.1]: https://github.com/rhoopr/icloudpd-rs/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/rhoopr/icloudpd-rs/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/rhoopr/icloudpd-rs/releases/tag/v0.1.0

