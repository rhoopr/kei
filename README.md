# icloudpd-rs

[![License: MIT](https://img.shields.io/github/license/rhoopr/icloudpd-rs?color=8b959e)](LICENSE.md) [![Rust](https://img.shields.io/badge/rust-1.85%2B-orange.svg?logo=rust)](https://www.rust-lang.org/) ![GitHub Downloads](https://img.shields.io/github/downloads/rhoopr/icloudpd-rs/total)
[![Version](https://img.shields.io/github/v/release/rhoopr/icloudpd-rs?color=blue)](https://github.com/rhoopr/icloudpd-rs/releases) [![Build](https://img.shields.io/github/actions/workflow/status/rhoopr/icloudpd-rs/ci.yml?label=build)](https://github.com/rhoopr/icloudpd-rs/actions) [![Homebrew](https://img.shields.io/badge/homebrew-tap-FBB040?logo=homebrew)](https://github.com/rhoopr/homebrew-icloudpd-rs)

A Rust rewrite of [icloud-photos-downloader](https://github.com/icloud-photos-downloader/icloud_photos_downloader) for downloading your iCloud Photos library to a local directory.

## About the project

This is a ground-up Rust rewrite of the Python [icloudpd](https://github.com/icloud-photos-downloader/icloud_photos_downloader). The goal was a single binary with no runtime dependencies that takes advantage of Rust's concurrency and performance to sync large iCloud Photos libraries quickly. It also adds SQLite-based state management so syncs are resumable and subsequent runs only download what's new or previously failed.

> [!TIP]
> **[Check out the Wiki](https://github.com/rhoopr/icloudpd-rs/wiki)** for in-depth guides on every CLI option, configuration example, and troubleshooting tip.

### Current

- **Single binary** — no Python, no pip, no virtual environments. Just download and run.
- **Parallel downloads** — defaults to 10 concurrent threads, configurable. Downloads start streaming as soon as the first API page comes back rather than enumerating the whole library first.
- **State tracking** — a SQLite database tracks what's been downloaded, what failed, and where to resume, so subsequent syncs skip what's already done.
- **Resumable transfers** — partial downloads pick up where they left off via HTTP Range requests, with SHA256 verification.
- **Auth** — implements Apple's SRP-6a variant with 2FA support, persistent sessions, automatic token refresh, and lock files to prevent concurrent instance conflicts.
- **File organization** — date-based folder structures (`%Y/%m/%d` etc.), live photo MOV pairing, EXIF date writing, and smart album support (favorites, bursts, time-lapse, slo-mo).
- **Long-running operation** — watch mode for continuous syncing, systemd notify support, mid-sync session recovery, and graceful shutdown (first signal finishes in-flight downloads, second force-exits).

See the [Changelog](CHANGELOG.md) for the full details and differences from the Python version.

### Planned

- XMP sidecar export for metadata preservation
- OS keyring integration for password storage
- Docker images
- iCloud lifecycle management (delete-after-download)

See [open issues](https://github.com/rhoopr/icloudpd-rs/issues) for the full list.

## Installation

### Homebrew (macOS & Linux)

```sh
brew tap rhoopr/icloudpd-rs
brew install icloudpd-rs
```

### Pre-built binaries

Grab the right one for your platform from [GitHub Releases](https://github.com/rhoopr/icloudpd-rs/releases):

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

### From source

Requires Rust 1.85+.

```sh
git clone https://github.com/rhoopr/icloudpd-rs.git
cd icloudpd-rs
cargo build --release
./target/release/icloudpd-rs --help
```

### Requirements

- An iCloud account with two-factor authentication enabled

## Quick start

```sh
# Authenticate and download everything
icloudpd-rs --username you@example.com --directory ~/Photos/iCloud

# You'll be prompted for your password, then asked to approve 2FA on a trusted device
# Downloads start right after authentication
```

You can use `--dry-run` to see what would be downloaded without actually downloading anything, or `--auth-only` to just verify your credentials.

## Usage

### Commands

The default command is `sync`, so you can omit it if you just want to download.

```sh
# These are equivalent
icloudpd-rs --username you@example.com --directory ~/Photos
icloudpd-rs sync --username you@example.com --directory ~/Photos

# Check what's in the state database
icloudpd-rs status --username you@example.com

# Retry things that failed last time
icloudpd-rs retry-failed --username you@example.com --directory ~/Photos

# If you already have photos locally, import them into the state database
# so they don't get re-downloaded
icloudpd-rs import-existing --username you@example.com --directory ~/Photos

# Verify downloaded files against checksums
icloudpd-rs verify --username you@example.com --checksums

# Wipe the state database and start over
icloudpd-rs reset-state --username you@example.com --yes
```

### Common options

```sh
# Download specific albums
icloudpd-rs -u you@example.com -d ~/Photos --album "Favorites" --album "Travel"

# Only the most recent 100 photos
icloudpd-rs -u you@example.com -d ~/Photos --recent 100

# Keep syncing every hour
icloudpd-rs -u you@example.com -d ~/Photos --watch-with-interval 3600

# Only photos created after a certain date
icloudpd-rs -u you@example.com -d ~/Photos --skip-created-before 2024-01-01

# Skip videos
icloudpd-rs -u you@example.com -d ~/Photos --skip-videos
```

### Environment variables

> [!NOTE]
> You can set `ICLOUD_PASSWORD` to avoid being prompted (and to keep your password out of the process list).

Run `icloudpd-rs --help` for everything else.

## Documentation

- [Wiki](https://github.com/rhoopr/icloudpd-rs/wiki) — detailed guides for all CLI options and features
- [Changelog](CHANGELOG.md) — release notes and differences from the Python version
- [Issues](https://github.com/rhoopr/icloudpd-rs/issues) — bug reports and feature requests

## Contributing

Contributions welcome. If you're planning something significant, open an issue first so we can discuss it.

```sh
cargo fmt -- --check && cargo clippy && cargo test
```

## License

MIT — see [LICENSE.md](LICENSE.md)

## Acknowledgments

This project is a Rust reimplementation inspired by [icloud-photos-downloader](https://github.com/icloud-photos-downloader/icloud_photos_downloader). Thanks to the original maintainers for their work reverse-engineering Apple's private APIs.
