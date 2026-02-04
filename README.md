# icloudpd-rs

[![Version](https://img.shields.io/badge/version-0.1.0-blue.svg)](https://github.com/rhoopr/icloudpd-rs/releases) [![Homebrew](https://img.shields.io/badge/homebrew-tap-FBB040?logo=homebrew)](https://github.com/rhoopr/homebrew-icloudpd-rs) [![License: MIT](https://img.shields.io/github/license/rhoopr/icloudpd-rs?color=8b959e)](LICENSE.md) [![Rust](https://img.shields.io/badge/rust-1.85%2B-orange.svg?logo=rust)](https://www.rust-lang.org/) [![Build](https://img.shields.io/github/actions/workflow/status/rhoopr/icloudpd-rs/ci.yml?branch=main&label=build)](https://github.com/rhoopr/icloudpd-rs/actions)

A fast, reliable iCloud Photos downloader written in Rust.

**icloudpd-rs** is a ground-up rewrite of [icloud-photos-downloader](https://github.com/icloud-photos-downloader/icloud_photos_downloader), designed for unattended operation with large photo libraries.

<p align="center">
  <a href="#features">Features</a> •
  <a href="#installation">Installation</a> •
  <a href="#quick-start">Quick Start</a> •
  <a href="#usage">Usage</a> •
  <a href="https://github.com/rhoopr/icloudpd-rs/wiki">Documentation</a>
</p>

---

## Why icloudpd-rs?

- **Single binary.** No runtime dependencies. Download and run.
- **Parallel downloads.** Configurable concurrency (default: 10 threads) for efficient bulk transfers.
- **Stateful sync.** SQLite database tracks what's downloaded, what failed, and where to resume.
- **Built for daemons.** Automatic session refresh, mid-sync re-authentication, and graceful shutdown.
- **Resumable transfers.** Partial downloads resume via HTTP Range with full SHA256 verification.


## Features

### Authentication
- SRP-6a with Apple's protocol variants (`s2k`/`s2k_fo` negotiation)
- Two-factor authentication via trusted device codes
- Persistent sessions with automatic token refresh
- Lock files prevent concurrent instance corruption

### Downloads
- Streaming pipeline—downloads begin as first API page returns
- Resumable `.part` files with SHA256 checksum verification
- Exponential backoff with transient/permanent error classification
- Two-phase cleanup pass re-fetches expired CDN URLs

### Organization
- Date-based folder structures (`--folder-structure %Y/%m/%d`)
- Live photo MOV handling with collision detection
- EXIF date tag read/write (`--set-exif-datetime`)
- Smart album support (favorites, bursts, time-lapse, slo-mo)

### Operations
- Watch mode with configurable intervals (`--watch-with-interval`)
- Mid-sync session recovery (up to 3 re-auth attempts)
- Graceful shutdown: first signal finishes in-flight downloads, second force-exits
- Dry-run mode for safe previews

See the [Changelog](CHANGELOG.md) for detailed feature notes and differences from Python icloudpd.

## Installation

### Homebrew (macOS & Linux)

```sh
brew tap rhoopr/icloudpd-rs
brew install icloudpd-rs
```

### Pre-built Binaries

Download the latest release for your platform from [GitHub Releases](https://github.com/rhoopr/icloudpd-rs/releases):

| Platform | Architecture | Download |
|----------|--------------|----------|
| macOS | Apple Silicon | `icloudpd-rs-macos-aarch64.tar.gz` |
| macOS | Intel | `icloudpd-rs-macos-x86_64.tar.gz` |
| Linux | ARM64 | `icloudpd-rs-linux-aarch64.tar.gz` |
| Linux | x86_64 | `icloudpd-rs-linux-x86_64.tar.gz` |
| Windows | x86_64 | `icloudpd-rs-windows-x86_64.zip` |

```sh
# Example: macOS Apple Silicon
curl -LO https://github.com/rhoopr/icloudpd-rs/releases/latest/download/icloudpd-rs-macos-aarch64.tar.gz
tar xzf icloudpd-rs-macos-aarch64.tar.gz
./icloudpd-rs --help
```

### From Source

```sh
git clone https://github.com/rhoopr/icloudpd-rs.git
cd icloudpd-rs
cargo build --release
./target/release/icloudpd-rs --help
```

Requires Rust 1.85 or later.

### Requirements

- An iCloud account with two-factor authentication enabled

## Quick Start

```sh
# First run: authenticate and download
icloudpd-rs --username you@example.com --directory ~/Photos/iCloud

# Enter your password when prompted, then approve the 2FA request on a trusted device
# Downloads begin immediately after authentication
```

> [!TIP]
> Use `--dry-run` to preview what would be downloaded. Use `--auth-only` to verify credentials without downloading.

## Usage

### Commands

icloudpd-rs uses subcommands for different operations. The default command is `sync`.

```sh
# Download photos (default)
icloudpd-rs --username you@example.com --directory ~/Photos

# Equivalent explicit form
icloudpd-rs sync --username you@example.com --directory ~/Photos

# Check sync status and database summary
icloudpd-rs status --username you@example.com

# Retry previously failed downloads
icloudpd-rs retry-failed --username you@example.com --directory ~/Photos

# Import existing local files into state database
icloudpd-rs import-existing --username you@example.com --directory ~/Photos

# Verify downloaded files and checksums
icloudpd-rs verify --username you@example.com --checksums

# Reset state database and start fresh
icloudpd-rs reset-state --username you@example.com --yes
```

### Common Options

```sh
# Download specific albums
icloudpd-rs -u you@example.com -d ~/Photos --album "Favorites" --album "Travel"

# Download only recent photos
icloudpd-rs -u you@example.com -d ~/Photos --recent 100

# Continuous sync every hour
icloudpd-rs -u you@example.com -d ~/Photos --watch-with-interval 3600

# Filter by date range
icloudpd-rs -u you@example.com -d ~/Photos --skip-created-before 2024-01-01

# Skip videos, download only photos
icloudpd-rs -u you@example.com -d ~/Photos --skip-videos
```

### Environment Variables

| Variable | Description |
|----------|-------------|
| `ICLOUD_PASSWORD` | iCloud password (avoids prompt and process listing exposure) |

Run `icloudpd-rs --help` for the complete option reference.

## Documentation

| Resource | Description |
|----------|-------------|
| [Wiki](https://github.com/rhoopr/icloudpd-rs/wiki) | Detailed guides for all CLI options and features |
| [Changelog](CHANGELOG.md) | Release notes and differences from Python icloudpd |
| [Issues](https://github.com/rhoopr/icloudpd-rs/issues) | Bug reports and feature requests |

## Roadmap

Planned enhancements include:

- XMP sidecar export for metadata preservation
- Shared library downloads
- OS keyring integration for secure password storage
- Docker images and systemd/launchd service files
- iCloud lifecycle management (delete-after-download)

See [open issues](https://github.com/rhoopr/icloudpd-rs/issues) for the complete list.

## Contributing

Contributions are welcome. Please open an issue to discuss significant changes before submitting a pull request.

```sh
# Run tests and checks
cargo fmt -- --check && cargo clippy && cargo test
```

## License

MIT — see [LICENSE.md](LICENSE.md)

## Acknowledgments

This project is a Rust reimplementation inspired by [icloud-photos-downloader](https://github.com/icloud-photos-downloader/icloud_photos_downloader). Thanks to the original maintainers for their work reverse-engineering Apple's private APIs.
