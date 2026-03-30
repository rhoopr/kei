> **Blog:** [How iCloud's Incremental Sync Actually Works](https://robhooper.xyz/blog-synctoken)

# icloudpd-rs

[![License: MIT](https://img.shields.io/github/license/rhoopr/icloudpd-rs?color=8b959e)](LICENSE.md) [![Rust](https://img.shields.io/badge/rust-1.85%2B-orange.svg?logo=rust)](https://www.rust-lang.org/)
[![Version](https://img.shields.io/github/v/release/rhoopr/icloudpd-rs?color=blue)](https://github.com/rhoopr/icloudpd-rs/releases) [![Build](https://img.shields.io/github/actions/workflow/status/rhoopr/icloudpd-rs/ci.yml?label=build)](https://github.com/rhoopr/icloudpd-rs/actions) [![Homebrew](https://img.shields.io/badge/homebrew-tap-FBB040?logo=homebrew)](https://github.com/rhoopr/homebrew-icloudpd-rs) [![Docker](https://img.shields.io/badge/ghcr.io-icloudpd--rs-blue?logo=docker)](https://ghcr.io/rhoopr/icloudpd-rs)

A fast iCloud Photos downloader and **icloudpd alternative**. Single binary, no Python runtime, no dependencies.

> **v0.4.1** - Adds `--only-print-filenames` to preview what would be downloaded, fixes progress bar overshoot with live photos, and adds `--version` and `--no-progress-bar` for `import-existing`. [Release notes](https://github.com/rhoopr/icloudpd-rs/releases/tag/v0.4.1) | [Changelog](CHANGELOG.md)

A ground-up Rust rewrite of [icloud-photos-downloader](https://github.com/icloud-photos-downloader/icloud_photos_downloader) (icloudpd), which did the hard work of reverse-engineering Apple's private APIs.

- **5x+ faster** - Parallel downloads with configurable concurrency. Network speed is the bottleneck.
- **Incremental sync** - Only fetches what changed since the last run.
- **15x faster scanning** - 20k-photo library indexed in ~30s.
- **Resumable** - Partial downloads resume via HTTP Range with SHA256 verification.
- **Stateful** - SQLite tracks what's downloaded. Subsequent syncs skip known files instantly.
- **Single binary** - Download and run. No runtime, no package manager.

> [!TIP]
> **Coming from Python icloudpd?** See the **[Migration Guide](docs/migration-from-python.md)** - it maps every flag and shows how to pick up where you left off without re-downloading.

## Quick start

```sh
icloudpd-rs --username you@example.com --directory ~/Photos/iCloud
```

You'll be prompted for your password, then asked to approve 2FA on a trusted device. Downloads start right after. Use `--dry-run` to preview, or `--auth-only` to just verify credentials.

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

```yaml
services:
  icloudpd-rs:
    image: ghcr.io/rhoopr/icloudpd-rs:latest
    container_name: icloudpd-rs
    restart: unless-stopped
    environment:
      - ICLOUD_USERNAME=${ICLOUD_USERNAME}
      - ICLOUD_PASSWORD=${ICLOUD_PASSWORD}
      - TZ=${TZ:-UTC}
    volumes:
      - ./config:/config
      - /path/to/photos:/photos
```

When 2FA is needed, submit the code from outside the container:

```sh
docker exec icloudpd-rs icloudpd-rs submit-code 123456
```

See the [Docker wiki page](https://github.com/rhoopr/icloudpd-rs/wiki/Docker) for the full setup guide.

### Pre-built binaries

[GitHub Releases](https://github.com/rhoopr/icloudpd-rs/releases) has builds for macOS (Apple Silicon, Intel), Linux (ARM64, x86_64), and Windows (x86_64).

### From source

```sh
git clone https://github.com/rhoopr/icloudpd-rs.git && cd icloudpd-rs
cargo build --release
./target/release/icloudpd-rs --help
```

## Usage

```sh
# Download everything
icloudpd-rs -u you@example.com -d ~/Photos

# Specific albums, skip videos, last 100 photos only
icloudpd-rs -u you@example.com -d ~/Photos --album "Favorites" --recent 100 --skip-videos

# All libraries (personal + shared) in one run
icloudpd-rs -u you@example.com -d ~/Photos --library all

# Keep syncing every hour, with push notifications
icloudpd-rs -u you@example.com -d ~/Photos --watch-with-interval 3600 --notification-script ./notify.sh

# Use a TOML config file instead of flags
icloudpd-rs --config ~/my-config.toml
```

Set `ICLOUD_PASSWORD` as an environment variable to skip the password prompt.

Run `icloudpd-rs --help` for the full flag list, or check the **[Wiki](https://github.com/rhoopr/icloudpd-rs/wiki)** for detailed guides.

## Features

| Feature | Details |
|---------|---------|
| Parallel downloads | Configurable concurrency; downloads start as the first API page returns |
| Incremental sync | CloudKit syncToken delta sync. [How it works](https://robhooper.xyz/blog-synctoken) |
| State tracking | SQLite DB tracks downloaded/failed/pending across runs |
| Resumable transfers | `.part` files resume via HTTP Range with SHA256 verification |
| TOML config | `[auth]`, `[download]`, `[filters]`, `[photos]`, `[watch]`, `[notifications]` sections. CLI overrides config. [Guide](https://github.com/rhoopr/icloudpd-rs/wiki/Configuration) |
| Watch mode | Continuous sync with interval, systemd notify, PID file, graceful shutdown (SIGINT/SIGTERM) |
| Multi-library | `--library all` syncs personal + shared libraries in one run |
| File organization | Date-based folders (`%Y/%m/%d`), live photo MOV pairing, EXIF datetime writing, smart albums |
| Docker & headless | Multi-arch images (amd64/arm64). `submit-code` subcommand for non-interactive 2FA |
| Notifications | Hook scripts on `2fa_required`, `sync_complete`, `sync_failed`, `session_expired` |
| Auth | Apple SRP-6a, 2FA, persistent sessions, automatic refresh, lock files, `.cn` domain support |
| Content filtering | `--skip-videos`, `--skip-photos`, `--skip-live-photos`, `--skip-created-before/after`, `--recent N` |
| Retry & recovery | Configurable retries with exponential backoff. `retry-failed` subcommand. Transient/permanent error classification |
| Diagnostics | `status`, `verify --checksums`, `reset-state`, `import-existing` subcommands |

### Roadmap

**Coming in v0.5.0** - config, env vars, validation:

- [Env var loading for all CLI params](https://github.com/rhoopr/icloudpd-rs/issues/118) - every flag settable via `ICLOUDPD_*` env vars
- [`--print-config` flag](https://github.com/rhoopr/icloudpd-rs/issues/117) - dump resolved config (CLI + TOML + defaults) for debugging
- [Input](https://github.com/rhoopr/icloudpd-rs/issues/125) and [path](https://github.com/rhoopr/icloudpd-rs/issues/126) validation at startup
- [Higher default concurrency](https://github.com/rhoopr/icloudpd-rs/issues/53) and bandwidth throttling
- [Typed session internals](https://github.com/rhoopr/icloudpd-rs/issues/6)

See the full [Roadmap](docs/roadmap.md) through v1.0.0 and [all open issues](https://github.com/rhoopr/icloudpd-rs/issues).

## Documentation

- **[Wiki](https://github.com/rhoopr/icloudpd-rs/wiki)** - guides for every CLI option, configuration, Docker, and troubleshooting
- **[Migration Guide](docs/migration-from-python.md)** - switching from Python icloudpd
- **[Changelog](CHANGELOG.md)** - release notes

## Contributing

Contributions welcome. If you're planning something big, open an issue first.

```sh
cargo fmt -- --check && cargo clippy && cargo test
```

## License

MIT - see [LICENSE.md](LICENSE.md)

## Acknowledgments

Built on the reverse-engineering work of the [icloud-photos-downloader](https://github.com/icloud-photos-downloader/icloud_photos_downloader) maintainers.
