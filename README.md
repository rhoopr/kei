<p align="center">
  <img src="assets/logo.png" alt="kei logo" width="200">
</p>

<h1 align="center">kei: photo sync engine</h1>

<p align="center">
  <img src="https://img.shields.io/badge/built_with-Rust-dea584?logo=rust" alt="Built with Rust">
  <a href="LICENSE.md"><img src="https://img.shields.io/github/license/rhoopr/kei?color=8b959e" alt="License: MIT"></a>
  <a href="https://github.com/rhoopr/kei/releases"><img src="https://img.shields.io/github/v/release/rhoopr/kei?color=blue&label=version" alt="Version"></a>
  <a href="https://github.com/rhoopr/kei/actions/workflows/docker.yml"><img src="https://img.shields.io/github/actions/workflow/status/rhoopr/kei/docker.yml?branch=main&label=build&logo=github" alt="Build"></a>
  <br>
  <a href="https://github.com/rhoopr/kei/releases"><img src="https://img.shields.io/github/downloads/rhoopr/kei/total?logo=github&label=downloads" alt="Downloads"></a>
  <a href="https://github.com/rhoopr/homebrew-kei"><img src="https://img.shields.io/badge/homebrew-tap-FBB040?logo=homebrew" alt="Homebrew"></a>
  <a href="https://ghcr.io/rhoopr/kei"><img src="https://img.shields.io/badge/ghcr.io-kei-blue?logo=docker" alt="Docker"></a>
</p>

Fast, parallel photo sync from the cloud to local storage. Single binary, runs unattended.

- **Parallel downloads** - configurable concurrency, starts downloading before enumeration completes
- **Incremental sync** - scans large libraries in seconds via CloudKit sync tokens, only fetches what changed
- **Resumable transfers** - partial downloads resume via HTTP Range, verified by size and content hash
- **Single binary** - no runtime dependencies, runs on macOS, Linux, and Windows
- **Unattended operation** - watch mode, systemd integration, headless 2FA, Docker-ready

iCloud Photos is supported today. Google Takeout and Immich are next.

> [!TIP]
> Coming from `icloudpd`? The [Migration Guide](docs/migration-from-python.md) maps every flag and shows how to pick up where you left off without re-downloading.

---

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

```sh
kei sync -u you@example.com -d ~/Photos/iCloud
```

You'll be prompted for your password (or set `ICLOUD_PASSWORD`), then asked to approve 2FA on a trusted device. Downloads start right after. On first run, kei saves your username and directory to `~/.config/kei/config.toml` so subsequent runs are just:

```sh
kei sync
```

Or use the interactive wizard: `kei config setup`.

For long-running setups (Docker, cron, systemd), use `--password-file`, `--password-command`, or `kei password set` to avoid storing passwords in environment variables. See the [Credentials](https://github.com/rhoopr/kei/wiki/Credentials) wiki page.

## Usage

```sh
# Specific albums, skip videos, last 100 photos only
kei sync --album "Favorites" --recent 100 --skip-videos

# All libraries (personal + shared) in one run
kei sync --library all

# Keep syncing every hour
kei sync --watch-with-interval 3600

# Preview what would download
kei sync --only-print-filenames

# Dry run (no writes to disk)
kei sync --dry-run
```

Run `kei sync --help` for all flags, or see the [wiki](https://github.com/rhoopr/kei/wiki) for the full CLI reference.

## How it works

kei downloads on a streaming pipeline - it starts fetching files as soon as the first API page comes back, rather than waiting to enumerate the whole library. After the first full sync, it uses Apple's CloudKit syncToken to pull only what changed. A no-change check takes 1-2 API calls.

Downloads run with configurable concurrency (default 10). Partial downloads are saved as `.kei-tmp` files and resumed via HTTP Range headers. Every file is verified against its expected size and content-type before being committed.

State lives in a SQLite database alongside your session data (see `--data-dir`). The DB tracks what's been downloaded, what failed, and where files landed on disk.

## Commands

| Command | |
|---|---|
| `sync` | Download photos |
| `login` | Authenticate and complete 2FA |
| `list` | List albums or libraries |
| `password` | Manage stored credentials (`set`, `clear`, `backend`) |
| `config` | Show resolved config (`show`) or run the setup wizard (`setup`) |
| `reset` | Delete state database (`state`) or clear sync tokens (`sync-token`) |
| `status` | Show sync stats and database summary |
| `verify` | Check downloads exist; `--checksums` for SHA256 |
| `import-existing` | Import local files so they aren't re-downloaded |

## Features

- SQLite state tracking - never re-downloads what it already has
- Watch mode with systemd notify, PID file, graceful shutdown
- Multi-library sync (`--library all` for personal + shared)
- Flexible password sources: prompt, env var, file, shell command, OS keyring
- Content filtering: live photo mode, filename globs, album exclusions, date ranges, `--recent N`
- Flexible folder structure with `{album}` token and full strftime support, EXIF datetime stamping
- Multi-arch Docker images (amd64/arm64) with headless 2FA
- Notification scripts on events (2FA required, sync complete, failures)
- TOML config with env var overrides (`KEI_*`) for every flag
- Structured exit codes (0 success, 2 partial, 3 auth) for scripting

## Docs

- [Wiki](https://github.com/rhoopr/kei/wiki) - full CLI reference, configuration, Docker, troubleshooting
- [Migration Guide](docs/migration-from-python.md) - switching from `icloudpd`
- [Changelog](CHANGELOG.md)
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
