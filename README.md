<p align="center">
  <img src="assets/logo.png" alt="kei logo" width="200">
</p>

<h1 align="center">kei: photo sync engine</h1>

<p align="center">
  <img src="https://img.shields.io/badge/Rust_MSRV-1.91%2B-dea584?logo=rust" alt="Rust MSRV 1.91+">
  <a href="LICENSE"><img src="https://img.shields.io/github/license/rhoopr/kei?color=8b959e" alt="License: MIT"></a>
  <a href="https://github.com/rhoopr/kei/releases"><img src="https://img.shields.io/github/v/release/rhoopr/kei?color=blue&label=version" alt="Version"></a>
  <a href="https://github.com/rhoopr/kei/actions/workflows/docker.yml"><img src="https://img.shields.io/github/actions/workflow/status/rhoopr/kei/docker.yml?branch=main&label=build&logo=github" alt="Build"></a>
  <br>
  <a href="https://github.com/rhoopr/kei/releases"><img src="https://img.shields.io/github/downloads/rhoopr/kei/total?logo=github&label=downloads" alt="Downloads"></a>
  <a href="https://github.com/rhoopr/homebrew-kei"><img src="https://img.shields.io/badge/homebrew-tap-FBB040?logo=homebrew" alt="Homebrew"></a>
  <a href="https://ghcr.io/rhoopr/kei"><img src="https://img.shields.io/badge/ghcr.io-kei-blue?logo=docker" alt="Docker"></a>
</p>

Sync your cloud photos to local storage. Fast, resumable, single binary, runs unattended.

- Parallel downloads with incremental sync (seconds on large libraries after the first run)
- Resumable transfers verified by size and content hash
- Watch mode, systemd integration, headless 2FA, Docker-ready

iCloud Photos is supported today. Google Takeout and Immich are next.

> [!TIP]
> Coming from `icloudpd`? The [Migration Guide](docs/migration-from-python.md) maps every flag and shows how to pick up where you left off without re-downloading.

## Install

```sh
brew install rhoopr/kei/kei          # Homebrew
docker pull ghcr.io/rhoopr/kei:latest # Docker
```

Pre-built binaries for macOS, Linux, and Windows are on the [Releases page](https://github.com/rhoopr/kei/releases). For Docker Compose, building from source, FreeBSD, and other install paths, see the [Install wiki page](https://github.com/rhoopr/kei/wiki/Install).

> [!IMPORTANT]
> kei can't access your photos if Advanced Data Protection is on. Turn ADP off and enable "Access iCloud Data on the Web" in your Apple ID settings. Details: [Authentication wiki](https://github.com/rhoopr/kei/wiki/Authentication#advanced-data-protection-adp).

## Quick start

```sh
kei sync -u you@example.com -d ~/Photos/iCloud --save-password
```

You'll be prompted for your password, then asked to approve 2FA on a trusted device. Downloads start right after. After the first run, just `kei sync` - username, directory, and password are all remembered.

For a guided walkthrough, run `kei config setup` instead.

## Docs

Everything else lives on the [wiki](https://github.com/rhoopr/kei/wiki): full CLI reference, filtering and folder templates, watch mode, Docker Compose, credentials, troubleshooting, and more.

- [Commands](https://github.com/rhoopr/kei/wiki/Home#commands) - `sync`, `login`, `list`, `password`, `config`, `reset`, `status`, `verify`, `import-existing`
- [Configuration](https://github.com/rhoopr/kei/wiki/Configuration) - TOML file, env vars, precedence
- [Docker](https://github.com/rhoopr/kei/wiki/Docker) - Compose files and headless 2FA
- [Credentials](https://github.com/rhoopr/kei/wiki/Credentials) - keyring, encrypted file, password files and commands
- [Changelog](CHANGELOG.md)
- [How iCloud's Incremental Sync Works](https://robhooper.xyz/blog-synctoken) - deep dive on CloudKit syncTokens

## Contributing

Contributions welcome. Open an issue first if you're planning something big.

```sh
just gate    # pre-push gate: fmt, clippy, tests, doc, audit, typos
just --list  # see every recipe
```

See [CONTRIBUTING.md](CONTRIBUTING.md) and [tests/README.md](tests/README.md) for the test catalog.

## License

MIT - see [LICENSE](LICENSE)

## Acknowledgments

kei started as `icloudpd-rs`, a Rust rewrite of [icloud-photos-downloader](https://github.com/icloud-photos-downloader/icloud_photos_downloader). Thanks to the original maintainers for their reverse-engineering work.
