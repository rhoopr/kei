# icloudpd-rs

A ground-up Rust rewrite of [icloud-photos-downloader](https://github.com/icloud-photos-downloader/icloud_photos_downloader) (`icloudpd`).

## Status

[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE.md)
[![Rust](https://img.shields.io/badge/Rust-stable-orange.svg)](https://www.rust-lang.org/)
[![Status](https://img.shields.io/badge/Status-Early%20Development-blue.svg)]()

> [!IMPORTANT]
> Early development. Core authentication (SRP, 2FA) and photo download are functional, but several features are still in progress. Expect breaking changes.

## Project Roadmap

<details open>
<summary><strong>Implemented</strong></summary>

- SRP-6a authentication with 2FA and session persistence
- iCloud Photos API — albums, smart folders, shared libraries, pagination
- Streaming download pipeline with concurrent downloads and parallel API fetching
- Photo, video, and live photo MOV downloads with size variants
- Content filtering by media type, date range, album, recency
- Date-based folder structures, filename sanitization, and deduplication policies
- EXIF DateTimeOriginal read/write and file modification time sync
- Retry with exponential backoff and error classification
- Resumable downloads with SHA256 checksum verification
- Two-phase cleanup pass with fresh CDN URLs for failures
- Dry-run, auth-only, list albums/libraries, watch mode
- Strongly typed API layer and structured error handling
- Low memory streaming for large libraries (100k+ photos)
- Log level control (`--log-level`), `--skip-photos`, `--domain cn`, `--cookie-directory`
- Live photo size and MOV filename policy selection
- EXIF DateTimeOriginal write (`--set-exif-datetime`)
- RAW file alignment (`--align-raw` with `as-is`, `original`, `alternative` modes)

</details>

<details open>
<summary><strong>Now</strong></summary>

- Robust session persistence (mid-sync re-auth, token expiry tracking, lock files)
- Progress bar integration (`--no-progress-bar` to disable)
- Incremental sync with SQLite state tracking and CloudKit sync tokens
- Failed asset tracking with persistent state across runs
- Graceful shutdown with signal handling

</details>

<details>
<summary><strong>Next</strong></summary>

- Multiple size downloads (`--size` accepting multiple per run)
- `--force-size` (don't fall back to original when requested size is missing)
- `--file-match-policy` for existing-file matching strategies
- `--only-print-filenames` (filename-only dry-run output)
- Write all EXIF date tags (DateTime, DateTimeDigitized)
- XMP sidecar export (GPS, keywords, ratings, title/description)
- Shared library download integration
- SMS-based 2FA
- Password providers with priority ordering (parameter, keyring, console)
- Robust watch/daemon mode (session refresh, album re-enumeration, systemd/launchd)
- Relative day intervals for date range filters (e.g., `30` for last 30 days)

</details>

<details>
<summary><strong>Later</strong></summary>

- Auto-delete via "Recently Deleted" album scan (`--auto-delete`)
- Delete after download (`--delete-after-download`)
- Keep recent days in iCloud (`--keep-icloud-recent-days`)
- Email/SMTP notifications
- Notification scripts
- Multi-account support (multiple `-u`/`-p` blocks in single run)
- OS locale date formatting
- Fingerprint fallback filenames
- Docker and AUR builds with `docker exec` MFA submission for headless re-auth

</details>

## Documentation

See [docs/](docs/) for detailed CLI flag reference and feature guides.

## Build

```sh
cargo build --release
```

Binary: `target/release/icloudpd-rs`

## Usage

```sh
icloudpd-rs --username my@email.address --directory /photos
```

> [!TIP]
> Use `--dry-run` to preview what would be downloaded without writing any files. Use `--auth-only` to verify your credentials without starting a download.

## CLI Flags

| Flag | Purpose | Default |
|------|---------|---------|
| `-u, --username` | Apple ID email | |
| `-p, --password` | iCloud password (or `ICLOUD_PASSWORD` env) | prompt |
| `-d, --directory` | Local download directory | |
| `--auth-only` | Only authenticate, don't download | |
| `-l, --list-albums` | List available albums | |
| `--list-libraries` | List available libraries | |
| `-a, --album` | Album(s) to download (repeatable) | all |
| `--size` | Image size: original, medium, thumb, adjusted, alternative | `original` |
| `--align-raw` | RAW alignment: as-is, original, alternative | `as-is` |
| `--live-photo-size` | Live photo MOV size: original, medium, thumb | `original` |
| `--live-photo-mov-filename-policy` | MOV naming: suffix, original | `suffix` |
| `--recent N` | Download only the N most recent photos | |
| `--threads-num N` | Number of concurrent downloads | `1` |
| `--skip-videos` | Don't download videos | |
| `--skip-photos` | Don't download photos | |
| `--skip-live-photos` | Don't download live photos | |
| `--skip-created-before` | Skip assets before ISO date or interval (e.g., `2025-01-02` or `20d`) | |
| `--skip-created-after` | Skip assets after ISO date or interval | |
| `--folder-structure` | Folder template for organizing downloads | `%Y/%m/%d` |
| `--set-exif-datetime` | Write DateTimeOriginal EXIF tag if missing | |
| `--domain` | iCloud domain: com, cn | `com` |
| `--cookie-directory` | Session/cookie storage path | `~/.icloudpd-rs` |
| `--log-level` | Log verbosity: debug, info, error | `debug` |
| `--max-retries N` | Max retries per download (0 = no retries) | `2` |
| `--retry-delay N` | Initial retry delay in seconds | `5` |
| `--watch-with-interval N` | Run continuously, waiting N seconds between runs | |
| `--dry-run` | Preview without modifying files or iCloud | |

## License

MIT - see [LICENSE.md](LICENSE.md)
