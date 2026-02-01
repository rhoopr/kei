# icloudpd-rs

A ground-up Rust rewrite of [icloud-photos-downloader](https://github.com/icloud-photos-downloader/icloud_photos_downloader) (`icloudpd`).

## Status

[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE.md)
[![Rust](https://img.shields.io/badge/Rust-stable-orange.svg)](https://www.rust-lang.org/)
[![Status](https://img.shields.io/badge/Status-Early%20Development-blue.svg)]()

> [!IMPORTANT]
> Early development. Core authentication (SRP, 2FA) and photo download are functional, but several features are still in progress. Expect breaking changes.

## Project Roadmap

See [CHANGELOG.md](CHANGELOG.md) for what's already implemented.

**Now** — Incremental sync (skip already-downloaded assets across runs), graceful shutdown, and mid-sync session recovery.

**Next** — XMP sidecar export, shared library downloads, OS keyring integration, robust daemon mode with systemd/launchd support, and additional download controls.

**Later** — iCloud lifecycle management (auto-delete, delete-after-download), notifications, headless MFA for Docker, and multi-account support.

## Documentation

See the [Wiki](https://github.com/rhoopr/icloudpd-rs/wiki) for detailed CLI flag reference and feature guides.

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
| `--no-progress-bar` | Disable the progress bar | |
| `--log-level` | Log verbosity: debug, info, error | `error` |
| `--max-retries N` | Max retries per download (0 = no retries) | `2` |
| `--retry-delay N` | Initial retry delay in seconds | `5` |
| `--watch-with-interval N` | Run continuously, waiting N seconds between runs | |
| `--dry-run` | Preview without modifying files or iCloud | |

## License

MIT - see [LICENSE.md](LICENSE.md)
