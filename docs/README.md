# icloudpd-rs Documentation

## Getting Started

```sh
cargo build --release
./target/release/icloudpd-rs --username my@email.address --directory /photos
```

Use `--dry-run` to preview what would be downloaded. Use `--auth-only` to verify credentials without starting a download.

## CLI Reference

### Authentication

| Flag | Description |
|------|-------------|
| [`-u, --username`](cli/username.md) | Apple ID email |
| [`-p, --password`](cli/password.md) | iCloud password |
| [`--auth-only`](cli/auth-only.md) | Authenticate without downloading |
| [`--domain`](cli/domain.md) | iCloud region (com or cn) |
| [`--cookie-directory`](cli/cookie-directory.md) | Session storage path |

### Content Selection

| Flag | Description |
|------|-------------|
| [`-a, --album`](cli/album.md) | Album(s) to download |
| [`-l, --list-albums`](cli/list-albums.md) | List available albums |
| [`--list-libraries`](cli/list-libraries.md) | List available libraries |
| [`--recent`](cli/recent.md) | Download only N most recent photos |
| [`--skip-videos`](cli/skip-videos.md) | Don't download videos |
| [`--skip-photos`](cli/skip-photos.md) | Don't download photos |
| [`--skip-live-photos`](cli/skip-live-photos.md) | Don't download live photos |
| [`--skip-created-before`](cli/skip-created-before.md) | Skip assets before a date |
| [`--skip-created-after`](cli/skip-created-after.md) | Skip assets after a date |

### Download Options

| Flag | Description |
|------|-------------|
| [`-d, --directory`](cli/directory.md) | Local download directory |
| [`--size`](cli/size.md) | Image size variant |
| [`--align-raw`](cli/align-raw.md) | RAW/JPEG alignment policy |
| [`--live-photo-size`](cli/live-photo-size.md) | Live photo MOV size variant |
| [`--live-photo-mov-filename-policy`](cli/live-photo-mov-filename-policy.md) | MOV filename style |
| [`--folder-structure`](cli/folder-structure.md) | Date-based folder template |
| [`--set-exif-datetime`](cli/set-exif-datetime.md) | Write EXIF tags |
| [`--threads-num`](cli/threads-num.md) | Concurrent downloads |
| [`--max-retries`](cli/max-retries.md) | Retry limit per download |
| [`--retry-delay`](cli/retry-delay.md) | Initial retry delay |
| [`--dry-run`](cli/dry-run.md) | Preview without writing files |

### Operational

| Flag | Description |
|------|-------------|
| [`--watch-with-interval`](cli/watch-with-interval.md) | Continuous sync mode |
| [`--log-level`](cli/log-level.md) | Log verbosity |

## Features

| Topic | Description |
|-------|-------------|
| [Authentication & 2FA](features/authentication.md) | SRP-6a, trusted device codes, session persistence |
| [Download Pipeline](features/download-pipeline.md) | Streaming, resumable, concurrent downloads |
| [Live Photos](features/live-photos.md) | MOV companion file handling |
| [Content Filtering](features/content-filtering.md) | Media type, date range, album filters |
| [Retry & Resilience](features/retry.md) | Exponential backoff, checksum verification |
| [Watch Mode](features/watch-mode.md) | Continuous sync with interval |
| [EXIF Handling](features/exif.md) | Date tag reading and writing |
| [Folder Structure](features/folder-structure.md) | Date-based directory organization |
