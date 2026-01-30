# icloudpd-rs

A Rust rewrite of [icloud-photos-downloader](https://github.com/icloud-photos-downloader/icloud_photos_downloader) (`icloudpd`).

## Status

[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE.md)
[![Rust](https://img.shields.io/badge/Rust-stable-orange.svg)](https://www.rust-lang.org/)
[![Status](https://img.shields.io/badge/Status-Early%20Development-blue.svg)]()

Early development. Core authentication (SRP, 2FA) and photo download are functional.

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

| Flag | Purpose | Status |
|------|---------|--------|
| `-u, --username` | Apple ID email | ✅ |
| `-p, --password` | iCloud password (or `ICLOUD_PASSWORD` env) | ✅ |
| `-d, --directory` | Local download directory | ✅ |
| `--auth-only` | Only authenticate, don't download | ✅ |
| `-l, --list-albums` | List available albums | ✅ |
| `--list-libraries` | List available libraries | ✅ |
| `-a, --album` | Album(s) to download | ✅ |
| `--library` | Library to download (default: PrimarySync) | ❌ |
| `--size` | Image size: original, medium, thumb | ✅ |
| `--live-photo-size` | Live photo video size | ❌ |
| `--recent` | Download only N most recent photos | ❌ |
| `--until-found` | Stop after N consecutive existing photos | ✅ |
| `--skip-videos` | Don't download videos | ✅ |
| `--skip-photos` | Don't download photos | ✅ |
| `--skip-live-photos` | Don't download live photos | ❌ |
| `--force-size` | Only download requested size, no fallback | ❌ |
| `--auto-delete` | Delete local files removed from iCloud | ❌ |
| `--folder-structure` | Folder template (default: `%Y/%m/%d`) | ✅ |
| `--set-exif-datetime` | Write EXIF DateTimeOriginal if missing | ✅ |
| `--dry-run` | Preview without modifying files or iCloud | ✅ |
| `--domain` | iCloud domain: com or cn | ✅ |
| `--watch-with-interval` | Run continuously every N seconds | ✅ |
| `--log-level` | Log verbosity | ✅ |
| `--no-progress-bar` | Disable progress bar | ❌ |
| `--cookie-directory` | Session/cookie storage (default: `~/.icloudpd-rs`) | ✅ |
| `--keep-unicode-in-filenames` | Preserve Unicode in filenames | ❌ |
| `--live-photo-mov-filename-policy` | MOV naming: suffix, original | ❌ |
| `--align-raw` | RAW treatment: as-is, alternative | ❌ |
| `--file-match-policy` | Dedup policy | ❌ |
| `--skip-created-before` | Skip assets before date/interval | ✅ |
| `--skip-created-after` | Skip assets after date/interval | ✅ |
| `--delete-after-download` | Delete from iCloud after download | ❌ |
| `--keep-icloud-recent-days` | Keep N recent days in iCloud | ❌ |
| `--only-print-filenames` | Print filenames without downloading | ❌ |

## License

MIT - see [LICENSE.md](LICENSE.md)
