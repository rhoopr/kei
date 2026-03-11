> **Blog:** [GitHub Is a Single Point of Failure (I Got Auto-Suspended 🫠)](https://robhooper.xyz/github-suspension)

# icloudpd-rs

[![License: MIT](https://img.shields.io/github/license/rhoopr/icloudpd-rs?color=8b959e)](LICENSE.md) [![Rust](https://img.shields.io/badge/rust-1.85%2B-orange.svg?logo=rust)](https://www.rust-lang.org/)
[![Version](https://img.shields.io/github/v/release/rhoopr/icloudpd-rs?color=blue)](https://github.com/rhoopr/icloudpd-rs/releases) [![Build](https://img.shields.io/github/actions/workflow/status/rhoopr/icloudpd-rs/ci.yml?label=build)](https://github.com/rhoopr/icloudpd-rs/actions) [![Homebrew](https://img.shields.io/badge/homebrew-tap-FBB040?logo=homebrew)](https://github.com/rhoopr/homebrew-icloudpd-rs) [![Docker](https://img.shields.io/badge/ghcr.io-icloudpd--rs-blue?logo=docker)](https://ghcr.io/rhoopr/icloudpd-rs)

A fast, reliable iCloud Photos downloader and **icloudpd alternative**. Single binary, no Python runtime, no dependencies.

> **v0.4.0** - Incremental sync is here. After your first full download, icloudpd-rs tracks changes via Apple's CloudKit syncToken and only fetches what's new. A no-change check takes 1-2 API calls instead of ~75. Also adds `--library all` to sync personal + shared libraries in one run. [Release notes](https://github.com/rhoopr/icloudpd-rs/releases/tag/v0.4.0)

Inspired by the excellent [icloud-photos-downloader](https://github.com/icloud-photos-downloader/icloud_photos_downloader) (icloudpd), which did the hard work of reverse-engineering Apple's private APIs. icloudpd-rs is a ground-up rewrite that adds parallel downloads, persistent state, and resumable transfers - things that are hard to retrofit into an existing codebase:

- **Parallel downloads** - Network speed is the bottleneck, **5x+ faster** than icloudpd in gigabit benchmarks.
- **Incremental sync** - After the first run, only changed/new photos are fetched. A no-change check completes in 1-2 API calls instead of ~75.
- **Fast library scanning** - 20k-photo library indexed in ~30s, **15x faster** than sequential API calls.
- **SQLite state tracking** - Subsequent syncs skip what's already downloaded, instantly.
- **Resumable transfers** - Partial downloads pick up where they left off, with SHA256 verification.
- **Single binary** - Download and run. No runtime, no package manager, no virtual environments.

> [!TIP]
> **Coming from Python icloudpd?** See the **[Migration Guide](docs/migration-from-python.md)** - it maps every flag and shows how to pick up where you left off without re-downloading anything.

## Quick start

```sh
icloudpd-rs --username you@example.com --directory ~/Photos/iCloud
```

You'll be prompted for your password, then asked to approve 2FA on a trusted device. Downloads start right after authentication. Use `--dry-run` to preview, or `--auth-only` to just verify credentials.

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

Grab the right one from [GitHub Releases](https://github.com/rhoopr/icloudpd-rs/releases):

| Platform | Architecture | Download |
|----------|--------------|----------|
| macOS | Apple Silicon | `icloudpd-rs-macos-aarch64.tar.gz` |
| macOS | Intel | `icloudpd-rs-macos-x86_64.tar.gz` |
| Linux | ARM64 | `icloudpd-rs-linux-aarch64.tar.gz` |
| Linux | x86_64 | `icloudpd-rs-linux-x86_64.tar.gz` |
| Windows | x86_64 | `icloudpd-rs-windows-x86_64.zip` |

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

# Use a config file
icloudpd-rs --config ~/my-config.toml

# Only recent 100 photos, skip videos
icloudpd-rs -u you@example.com -d ~/Photos --recent 100 --skip-videos

# Specific albums
icloudpd-rs -u you@example.com -d ~/Photos --album "Favorites" --album "Travel"

# All libraries (personal + shared) in one run
icloudpd-rs -u you@example.com -d ~/Photos --library all

# Keep syncing every hour
icloudpd-rs -u you@example.com -d ~/Photos --watch-with-interval 3600

# Get notified when 2FA is needed or sync completes
icloudpd-rs -u you@example.com -d ~/Photos --notification-script ./notify.sh

# Submit a 2FA code non-interactively (Docker / headless)
icloudpd-rs submit-code 123456

# Force full sync (skip incremental delta)
icloudpd-rs -u you@example.com -d ~/Photos --no-incremental

# Clear sync tokens and start fresh incremental tracking
icloudpd-rs -u you@example.com -d ~/Photos --reset-sync-token

# Check sync status, retry failures, verify downloads
icloudpd-rs status -u you@example.com
icloudpd-rs retry-failed -u you@example.com -d ~/Photos
icloudpd-rs verify -u you@example.com --checksums
icloudpd-rs reset-state -u you@example.com --yes
icloudpd-rs import-existing -u you@example.com -d ~/Photos
```

Set `ICLOUD_PASSWORD` as an environment variable to avoid being prompted.

Run `icloudpd-rs --help` for the full flag list, or check the **[Wiki](https://github.com/rhoopr/icloudpd-rs/wiki)** for detailed guides on every option.

## Features

| Feature | Details |
|---------|---------|
| Parallel downloads | Configurable concurrency, downloads start as the first API page returns |
| Incremental sync | CloudKit syncToken delta sync - only fetches changes since the last run. [Details](https://github.com/rhoopr/icloudpd-rs/wiki/State-Tracking#incremental-sync) |
| State tracking | SQLite DB tracks downloaded/failed/pending - no re-scanning |
| Resumable transfers | Partial downloads resume via HTTP Range with SHA256 verification |
| TOML config | Optional `config.toml` with `[auth]`, `[download]`, `[filters]`, `[photos]`, `[watch]`, `[notifications]` sections. CLI flags override config values. [Guide](https://github.com/rhoopr/icloudpd-rs/wiki/Configuration) |
| Docker | Multi-arch (amd64/arm64) on `ghcr.io/rhoopr/icloudpd-rs`. [Guide](https://github.com/rhoopr/icloudpd-rs/wiki/Docker) |
| Headless MFA | `submit-code` subcommand for Docker/cron 2FA without interactive prompts |
| Notification scripts | Fire a script on `2fa_required`, `sync_complete`, `sync_failed`, `session_expired` |
| Watch mode | Continuous sync with interval, systemd notify, graceful shutdown |
| Multi-library | `--library all` downloads from personal + shared libraries in one run |
| File organization | Date-based folders, live photo MOV pairing, EXIF writing, smart albums |
| Auth | Apple SRP-6a, 2FA, persistent sessions, automatic refresh, lock files |

### Not yet implemented

**Coming in v0.5** (next release):
- [Auto-delete / Recently Deleted scan](https://github.com/rhoopr/icloudpd-rs/issues/28) - detect iCloud deletions and optionally remove local copies
- [Delete after download](https://github.com/rhoopr/icloudpd-rs/issues/29) - remove photos from iCloud after successful download

**Planned:**
- [XMP sidecar export](https://github.com/rhoopr/icloudpd-rs/issues/19)
- [OS keyring integration](https://github.com/rhoopr/icloudpd-rs/issues/22)
- [HEIC to JPEG conversion](https://github.com/rhoopr/icloudpd-rs/issues/52)
- [Prometheus metrics](https://github.com/rhoopr/icloudpd-rs/issues/55)

See [all open issues](https://github.com/rhoopr/icloudpd-rs/issues) for the full list.

## Documentation

- **[Wiki](https://github.com/rhoopr/icloudpd-rs/wiki)** - guides for every CLI option, configuration, Docker, and troubleshooting
- **[Migration Guide](docs/migration-from-python.md)** - switching from Python icloudpd
- **[Changelog](CHANGELOG.md)** - release notes and full diff from the Python version

## Contributing

Contributions welcome. If you're planning something big, open an issue first.

```sh
cargo fmt -- --check && cargo clippy && cargo test
```

## License

MIT - see [LICENSE.md](LICENSE.md)

## Acknowledgments

Built on the reverse-engineering work of the original [icloud-photos-downloader](https://github.com/icloud-photos-downloader/icloud_photos_downloader) maintainers.
