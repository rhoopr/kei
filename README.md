# icloudpd-rs

[![Version](https://img.shields.io/github/v/release/rhoopr/icloudpd-rs?color=blue)](https://github.com/rhoopr/icloudpd-rs/releases)
[![Build](https://img.shields.io/github/actions/workflow/status/rhoopr/icloudpd-rs/ci.yml?label=build)](https://github.com/rhoopr/icloudpd-rs/actions)
[![License: MIT](https://img.shields.io/github/license/rhoopr/icloudpd-rs?color=8b959e)](LICENSE.md)
[![Downloads](https://img.shields.io/github/downloads/rhoopr/icloudpd-rs/total?color=green)](https://github.com/rhoopr/icloudpd-rs/releases)
[![Homebrew](https://img.shields.io/badge/homebrew-tap-FBB040?logo=homebrew)](https://github.com/rhoopr/homebrew-icloudpd-rs)
[![Docker](https://img.shields.io/badge/ghcr.io-icloudpd--rs-blue?logo=docker)](https://ghcr.io/rhoopr/icloudpd-rs)

Modern iCloud photo downloader. Enumerates large libraries in seconds, tracks state across runs, downloads in parallel, and runs unattended in Docker: all in a single binary.

A Rust rewrite of [icloud-photos-downloader](https://github.com/icloud-photos-downloader/icloud_photos_downloader) (icloudpd). 5x+ faster downloads, 15x faster library scanning, incremental sync that only fetches what changed, and resumable transfers with SHA256 verification.

> [!TIP]
> Coming from Python icloudpd? The [Migration Guide](docs/migration-from-python.md) maps every flag and shows how to pick up where you left off without re-downloading.

---

## Install

### Homebrew (macOS & Linux)

```sh
brew tap rhoopr/icloudpd-rs
brew install icloudpd-rs
```

### Docker

```sh
docker pull ghcr.io/rhoopr/icloudpd-rs:latest
```

See the [Docker wiki page](https://github.com/rhoopr/icloudpd-rs/wiki/Docker) for the full setup guide, including compose files and headless 2FA via `submit-code`.

### Pre-built binaries

[GitHub Releases](https://github.com/rhoopr/icloudpd-rs/releases) - macOS (Apple Silicon, Intel), Linux (ARM64, x86_64), Windows (x86_64).

### From source

```sh
git clone https://github.com/rhoopr/icloudpd-rs.git && cd icloudpd-rs
cargo build --release
```

---

## Quick start

The `setup` wizard walks you through config interactively:

```sh
icloudpd-rs setup
```

This generates a TOML config file at `~/.config/icloudpd-rs/config.toml`. Then run:

```sh
icloudpd-rs
```

Or skip the wizard and pass flags directly:

```sh
icloudpd-rs -u you@example.com -d ~/Photos/iCloud
```

You'll be prompted for your password (or set `ICLOUD_PASSWORD`), then asked to approve 2FA on a trusted device. Downloads start right after.

---

## Usage

```sh
# Specific albums, skip videos, last 100 photos only
icloudpd-rs -u you@example.com -d ~/Photos --album "Favorites" --recent 100 --skip-videos

# All libraries (personal + shared) in one run
icloudpd-rs -u you@example.com -d ~/Photos --library all

# Keep syncing every hour with notifications
icloudpd-rs -u you@example.com -d ~/Photos --watch-with-interval 3600 --notification-script ./notify.sh

# Preview what would be downloaded
icloudpd-rs -u you@example.com -d ~/Photos --only-print-filenames

# Dry run (no writes to disk or iCloud)
icloudpd-rs -u you@example.com -d ~/Photos --dry-run
```

Run `icloudpd-rs --help` for all flags. The [Wiki](https://github.com/rhoopr/icloudpd-rs/wiki) has detailed guides.

---

## Features

| Feature | |
|---|---|
| Parallel downloads | Configurable concurrency; downloads start as the first API page returns |
| Incremental sync | CloudKit syncToken delta sync - subsequent runs only fetch changes |
| State tracking | SQLite DB tracks downloaded/failed/pending across runs |
| Resumable transfers | `.part` files resume via HTTP Range with SHA256 verification |
| TOML config | CLI overrides config. `setup` wizard for easy generation. [Guide](https://github.com/rhoopr/icloudpd-rs/wiki/Configuration) |
| Watch mode | Continuous sync on an interval, systemd notify, PID file, graceful shutdown |
| Multi-library | `--library all` syncs personal + shared libraries in one run |
| File organization | Date-based folders, live photo MOV pairing, EXIF datetime writing |
| Docker & headless | Multi-arch images (amd64/arm64), `submit-code` for non-interactive 2FA |
| Notifications | Hook scripts on `2fa_required`, `sync_complete`, `sync_failed`, `session_expired` |
| Content filtering | Skip videos/photos/live photos, date ranges, `--recent N` |
| Retry & recovery | Exponential backoff, `retry-failed` subcommand, transient/permanent error classification |
| Diagnostics | `status`, `verify --checksums`, `reset-state`, `import-existing` subcommands |

## Subcommands

| Command | What it does |
|---|---|
| `sync` | Download photos (default when no subcommand given) |
| `setup` | Interactive config wizard |
| `status` | Show sync status and DB summary |
| `verify` | Check downloaded files exist, optionally verify checksums |
| `retry-failed` | Reset failed downloads to pending and re-sync |
| `reset-state` | Delete the state DB and start fresh |
| `import-existing` | Import local files into the state DB (avoids re-downloading) |
| `submit-code` | Submit 2FA code non-interactively (Docker / headless) |

---

## Documentation

- [Wiki](https://github.com/rhoopr/icloudpd-rs/wiki) - configuration, Docker, troubleshooting
- [Migration Guide](docs/migration-from-python.md) - switching from Python icloudpd
- [Changelog](CHANGELOG.md) - release history
- [Roadmap](docs/roadmap.md) - planned features through v1.0.0
- [How iCloud's Incremental Sync Works](https://robhooper.xyz/blog-synctoken) - technical deep dive on CloudKit syncTokens

---

## Contributing

Contributions welcome. Open an issue first if you're planning something big.

```sh
cargo fmt -- --check && cargo clippy && cargo test
```

## License

MIT - see [LICENSE.md](LICENSE.md)

## Acknowledgments

Built on the reverse-engineering work of the [icloud-photos-downloader](https://github.com/icloud-photos-downloader/icloud_photos_downloader) maintainers.
