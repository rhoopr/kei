# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

---

## [Unreleased]

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
- [#17](https://github.com/rhoopr/icloudpd-rs/issues/17) - Print filenames only (`--only-print-filenames`)
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

- [#47](https://github.com/rhoopr/icloudpd-rs/issues/47) - Progress bar position can overshoot when photos have companion files
- [#69](https://github.com/rhoopr/icloudpd-rs/issues/69) - Schema migration logic needs improvement before v2

---

[Unreleased]: https://github.com/rhoopr/icloudpd-rs/compare/v0.4.0...HEAD
[0.4.0]: https://github.com/rhoopr/icloudpd-rs/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/rhoopr/icloudpd-rs/compare/v0.2.1...v0.3.0
[0.2.1]: https://github.com/rhoopr/icloudpd-rs/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/rhoopr/icloudpd-rs/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/rhoopr/icloudpd-rs/releases/tag/v0.1.0

