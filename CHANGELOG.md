# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

---

## [0.1.0] - 2026-02-08

Initial release. A ground-up Rust rewrite of [icloud-photos-downloader](https://github.com/icloud-photos-downloader/icloud_photos_downloader) with full photo/video download capabilities and SQLite state tracking.

### Added

#### State Management (New in Rust)

- **SQLite state database** tracks every asset's status (`pending`, `downloaded`, `failed`) with checksums, paths, and error messages
- **Skip-by-database** — subsequent syncs skip already-downloaded assets without filesystem checks
- **Automatic re-download** — if database says downloaded but file is missing, re-downloads automatically
- **Sync run history** — records start time, completion, and statistics for each run

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
| **Cookie format** | LWPCookieJar format | JSON format + legacy LWP import support |
| **Folder syntax** | Python datetime format (`{:%Y/%m/%d}`) | Both `{:%Y}` and `%Y` strftime accepted |

### Not Implemented (Planned)

The following Python icloudpd features are not yet available. Links go to tracking issues:

#### Authentication & Security
- [#21](https://github.com/rhoopr/icloudpd-rs/issues/21) — SMS-based 2FA (trusted device only currently)
- [#38](https://github.com/rhoopr/icloudpd-rs/issues/38) — Legacy two-step authentication (2SA)
- [#22](https://github.com/rhoopr/icloudpd-rs/issues/22) — OS keyring integration for password storage
- [#36](https://github.com/rhoopr/icloudpd-rs/issues/36) — Headless MFA via `--submit-code` for Docker
- [#37](https://github.com/rhoopr/icloudpd-rs/issues/37) — Python LWPCookieJar session import

#### Content & Downloads
- [#19](https://github.com/rhoopr/icloudpd-rs/issues/19) — XMP sidecar export (`--xmp-sidecar`)
- [#14](https://github.com/rhoopr/icloudpd-rs/issues/14) — Multiple size downloads (`--size` accepting multiple values)
- [#17](https://github.com/rhoopr/icloudpd-rs/issues/17) — Print filenames only (`--only-print-filenames`)
- [#52](https://github.com/rhoopr/icloudpd-rs/issues/52) — HEIC to JPEG conversion (`--convert-heic`)

#### iCloud Lifecycle
- [#28](https://github.com/rhoopr/icloudpd-rs/issues/28) — Auto-delete (Recently Deleted album scan)
- [#29](https://github.com/rhoopr/icloudpd-rs/issues/29) — Delete after download (`--delete-after-download`)
- [#30](https://github.com/rhoopr/icloudpd-rs/issues/30) — Keep iCloud recent days (`--keep-icloud-recent-days`)

#### Notifications & Monitoring
- [#31](https://github.com/rhoopr/icloudpd-rs/issues/31) — Email/SMTP notifications on 2FA expiration
- [#32](https://github.com/rhoopr/icloudpd-rs/issues/32) — Notification scripts (`--notification-script`)
- [#55](https://github.com/rhoopr/icloudpd-rs/issues/55) — Prometheus metrics export

#### Distribution & Configuration
- [#40](https://github.com/rhoopr/icloudpd-rs/issues/40) — Docker images and AUR builds
- [#51](https://github.com/rhoopr/icloudpd-rs/issues/51) — Config file support (TOML)
- [#33](https://github.com/rhoopr/icloudpd-rs/issues/33) — Multi-account support
- [#34](https://github.com/rhoopr/icloudpd-rs/issues/34) — OS locale date formatting (`--use-os-locale`)

### Removed (vs Python icloudpd)

- `--until-found` — Replaced by SQLite state tracking; the database knows what's already downloaded
- `--smtp-*` flags — Email notifications not yet implemented ([#31](https://github.com/rhoopr/icloudpd-rs/issues/31))
- `--notification-*` flags — Script notifications not yet implemented ([#32](https://github.com/rhoopr/icloudpd-rs/issues/32))

### Known Issues

- [#47](https://github.com/rhoopr/icloudpd-rs/issues/47) — Progress bar position can overshoot when photos have companion files
- [#69](https://github.com/rhoopr/icloudpd-rs/issues/69) — Schema migration logic needs improvement before v2

---

[0.1.0]: https://github.com/rhoopr/icloudpd-rs/releases/tag/v0.1.0

