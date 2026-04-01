<p align="center">
  <img src="assets/logo.png" alt="kei logo" width="200">
</p>

<h1 align="center">kei: photo sync engine</h1>

<p align="center">
  <a href="https://github.com/rhoopr/kei/releases"><img src="https://img.shields.io/github/v/release/rhoopr/kei?color=blue&label=version" alt="Version"></a>
  <a href="https://github.com/rhoopr/kei/actions"><img src="https://img.shields.io/github/actions/workflow/status/rhoopr/kei/ci.yml?label=build" alt="Build"></a>
  <a href="LICENSE.md"><img src="https://img.shields.io/github/license/rhoopr/kei?color=8b959e" alt="License: MIT"></a>
  <a href="https://github.com/rhoopr/kei/releases"><img src="https://img.shields.io/github/downloads/rhoopr/kei/total?color=green" alt="Downloads"></a>
  <a href="https://github.com/rhoopr/homebrew-kei"><img src="https://img.shields.io/badge/homebrew-tap-FBB040?logo=homebrew" alt="Homebrew"></a>
  <a href="https://ghcr.io/rhoopr/kei"><img src="https://img.shields.io/badge/ghcr.io-kei-blue?logo=docker" alt="Docker"></a>
</p>

---

> [!IMPORTANT]
> **`icloudpd-rs` is now `kei`.** We're building a universal photo sync engine. iCloud is just the first source - Google Takeout, Immich, and more are coming. Same fast downloads, same single binary, way bigger ambitions. Upgrading? Your config and cookies migrate automatically on first run.

kei syncs your photos from cloud services to local storage. Single binary, small footprint, runs unattended.

Right now kei supports iCloud Photos. Google Takeout, Immich, and other sources are on the roadmap.

It scans large libraries in seconds using incremental sync, downloads in parallel with resumable transfers, and tracks everything in a local SQLite database so it never re-downloads what it already has.

> [!TIP]
> Coming from `icloudpd`? The [Migration Guide](docs/migration-from-python.md) maps every flag and shows how to pick up where you left off without re-downloading.

## Install

**Homebrew**

```sh
brew install rhoopr/kei/kei
```

**Docker**

```sh
docker pull ghcr.io/rhoopr/kei:latest
```

See the [Docker guide](https://github.com/rhoopr/kei/wiki/Docker) for compose files and headless 2FA.

**Pre-built binaries**

Grab one from [GitHub Releases](https://github.com/rhoopr/kei/releases). macOS (Apple Silicon + Intel), Linux (ARM64 + x86_64), Windows (x86_64).

**From source**

```sh
git clone https://github.com/rhoopr/kei.git kei && cd kei
cargo build --release
```

## Quick start

The setup wizard generates a config file interactively:

```sh
kei setup
```

This writes `~/.config/kei/config.toml`. Then just run:

```sh
kei
```

Or skip the wizard:

```sh
kei -u you@example.com -d ~/Photos/iCloud
```

You'll be prompted for your password (or set `ICLOUD_PASSWORD`), then asked to approve 2FA on a trusted device. Downloads start right after.

## Usage

```sh
# Specific albums, skip videos, last 100 photos only
kei -u you@example.com -d ~/Photos --album "Favorites" --recent 100 --skip-videos

# All libraries (personal + shared) in one run
kei -u you@example.com -d ~/Photos --library all

# Keep syncing every hour
kei -u you@example.com -d ~/Photos --watch-with-interval 3600

# Preview what would download
kei -u you@example.com -d ~/Photos --only-print-filenames

# Dry run (no writes to disk)
kei -u you@example.com -d ~/Photos --dry-run
```

Run `kei --help` for all flags.

## How it works

kei downloads on a streaming pipeline. It starts fetching files as soon as the first API page comes back, rather than waiting to enumerate the whole library. After the first full sync, it uses Apple's CloudKit syncToken to pull only what changed - a no-change check takes 1-2 API calls.

Downloads run with configurable concurrency (default 10). Partial downloads are saved as `.kei-tmp` files and resumed via HTTP Range headers. Every file is verified against its SHA256 checksum.

State lives in a SQLite database alongside your session cookies in `~/.config/kei/`. The DB tracks what's been downloaded, what failed, and where files landed on disk. This is what makes `retry-failed`, `verify`, and `import-existing` possible.

## Subcommands

| Command | |
|---|---|
| `sync` | Download photos. Default when no subcommand is given. |
| `setup` | Interactive config wizard. |
| `status` | Show sync stats and database summary. |
| `verify` | Check that downloaded files exist. `--checksums` to verify SHA256. |
| `retry-failed` | Reset failed downloads to pending and re-sync. |
| `reset-state` | Delete the state database and start fresh. |
| `import-existing` | Import local files into the state DB so they aren't re-downloaded. |
| `submit-code` | Submit a 2FA code non-interactively. For Docker and headless setups. |

## Features

- Parallel downloads with streaming pipeline - files start downloading before enumeration finishes
- Incremental sync via CloudKit syncTokens - only fetches what changed
- Resumable transfers with `.kei-tmp` partial files and SHA256 verification
- SQLite state tracking across runs (downloaded, failed, pending)
- Watch mode with configurable interval, systemd notify, PID file, graceful shutdown
- Multi-library sync (`--library all` for personal + shared)
- Date-based folder structure, live photo MOV pairing, EXIF datetime stamping
- Multi-arch Docker images (amd64/arm64) with headless 2FA via `submit-code`
- Notification scripts on events: `2fa_required`, `sync_complete`, `sync_failed`, `session_expired`
- Content filtering: skip videos/photos/live photos, date ranges, `--recent N`
- Exponential backoff retries with transient vs. permanent error classification
- TOML config file with `setup` wizard, CLI flags override config values

## Docs

- [Wiki](https://github.com/rhoopr/kei/wiki) - configuration, Docker, troubleshooting
- [Migration Guide](docs/migration-from-python.md) - switching from `icloudpd`
- [Changelog](CHANGELOG.md)
- [Roadmap](docs/roadmap.md)
- [How iCloud's Incremental Sync Works](https://robhooper.xyz/blog-synctoken) - deep dive on CloudKit syncTokens

## Contributing

Contributions welcome. Open an issue first if you're planning something big.

```sh
cargo fmt -- --check && cargo clippy && cargo test
```

## License

MIT - see [LICENSE.md](LICENSE.md)

## Acknowledgments

kei started as `icloudpd-rs`, a Rust rewrite of [icloud-photos-downloader](https://github.com/icloud-photos-downloader/icloud_photos_downloader). Thanks to the original maintainers for their reverse-engineering work.
