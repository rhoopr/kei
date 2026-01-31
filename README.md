# icloudpd-rs

A ground-up Rust rewrite of [icloud-photos-downloader](https://github.com/icloud-photos-downloader/icloud_photos_downloader) (`icloudpd`).

## Status

[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE.md)
[![Rust](https://img.shields.io/badge/Rust-stable-orange.svg)](https://www.rust-lang.org/)
[![Status](https://img.shields.io/badge/Status-Early%20Development-blue.svg)]()

Early development. Core authentication (SRP, 2FA) and photo download are functional.

## Project Goals

### Ported Features

Full feature parity with the Python icloudpd, including:

- **Complete Apple authentication** — SRP-6a, two-factor authentication, session persistence with trust tokens
- **Full iCloud Photos API** — albums, smart folders, shared libraries, asset enumeration with pagination
- **Flexible downloads** — multiple size variants (original, medium, thumb, adjusted, alternative), live photos, RAW files
- **Content filtering** — by media type, date range, album, recency
- **File organization** — date-based folder structures, filename sanitization, deduplication policies
- **EXIF metadata** — read and write DateTimeOriginal, file modification time sync
- **XMP sidecar export** — GPS, keywords, ratings, title, description, orientation
- **iCloud management** — auto-delete synced removals, delete-after-download, keep-recent-days
- **Operational modes** — watch/daemon mode, dry-run, auth-only, list albums/libraries

### Performance and Optimizations

Taking advantage of what Rust offers over Python:

- **True parallel downloads** — concurrent file downloads without GIL constraints, saturating available bandwidth
- **Concurrent API pipeline** — parallel album fetching, page prefetching, and overlapped processing
- **Low memory footprint** — strongly typed compact structs instead of raw JSON blobs, streaming page-by-page processing for 100k+ photo libraries
- **Incremental sync** — SQLite-backed state tracking with CloudKit sync tokens to skip unchanged assets across runs, with migration support for existing Python icloudpd libraries
- **Efficient retry** — exponential backoff with jitter, error classification (transient vs permanent), configurable limits

### New Features

Going beyond the Python original:

- **Graceful shutdown** — signal handling (Ctrl+C/SIGTERM) that finishes the current download, flushes state, and cleans up temp files
- **Robust session management** — trust token expiry tracking, proactive session refresh during long syncs, concurrent-instance safety with lock files
- **Strongly typed API layer** — compile-time guarantees on API response shapes; malformed responses surface as errors, not silent corruption
- **Typed error handling** — structured error enums throughout, so retry logic can distinguish network timeouts from auth failures from disk errors
- **Failed asset tracking** — persistent record of what succeeded, failed, or was skipped, with summary reporting and retry-only-failures mode
- **Native daemon mode** — proper signal handling, session refresh between cycles, album re-enumeration, and optional systemd/launchd integration

## Build

```sh
cargo build --release
```

Binary: `target/release/icloudpd-rs`

## Usage

```sh
icloudpd-rs --username my@email.address --directory /photos
```

## CLI Flags

| Flag | Purpose |
|------|---------|
| `-u, --username` | Apple ID email |
| `-p, --password` | iCloud password (or `ICLOUD_PASSWORD` env) |
| `-d, --directory` | Local download directory |
| `--auth-only` | Only authenticate, don't download |
| `-l, --list-albums` | List available albums |
| `--list-libraries` | List available libraries |
| `--recent N` | Download only the N most recent photos |
| `--threads-num N` | Number of concurrent downloads (default: 1) |
| `--max-retries N` | Max retries per download (default: 2, 0 = no retries) |
| `--retry-delay N` | Initial retry delay in seconds (default: 5) |
| `--dry-run` | Preview without modifying files or iCloud |

## License

MIT - see [LICENSE.md](LICENSE.md)
