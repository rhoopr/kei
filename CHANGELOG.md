# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

---

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

