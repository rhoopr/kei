<p align="center">
  <img src="assets/logo.png" alt="kei logo" width="200">
</p>

<h1 align="center">kei: media sync engine</h1>

<p align="center">
  Sync your cloud photos and videos to local storage. Fast, resumable, runs unattended.<br><br>
  <a href="https://github.com/rhoopr/kei/blob/main/Cargo.toml"><img src="https://img.shields.io/badge/Rust_MSRV-1.91%2B-dea584?logo=rust" alt="Rust MSRV 1.91+"></a>
  <a href="LICENSE"><img src="https://img.shields.io/github/license/rhoopr/kei?color=8b959e" alt="License: MIT"></a>
  <a href="https://github.com/rhoopr/kei/releases"><img src="https://img.shields.io/github/v/release/rhoopr/kei?color=blue&label=version" alt="Version"></a>
  <a href="https://github.com/rhoopr/kei/actions/workflows/docker.yml"><img src="https://img.shields.io/github/actions/workflow/status/rhoopr/kei/docker.yml?branch=main&label=build&logo=github" alt="Build"></a>
  <a href="https://ghcr.io/rhoopr/kei"><img src="https://img.shields.io/badge/dynamic/json?url=https%3A%2F%2Fgithub.com%2Fipitio%2Fbackage%2Fraw%2Findex%2Frhoopr%2Fkei%2Fkei.json&query=%24.downloads&logo=github&label=pulls" alt="Pulls"></a>
  <a href="https://github.com/rhoopr/kei/releases"><img src="https://img.shields.io/github/downloads/rhoopr/kei/total?logo=github&label=downloads" alt="Downloads"></a>
  <a href="https://github.com/rhoopr/homebrew-kei"><img src="https://img.shields.io/badge/homebrew-tap-FBB040?logo=homebrew" alt="Homebrew"></a>
  <a href="https://ghcr.io/rhoopr/kei"><img src="https://img.shields.io/badge/ghcr.io-kei-blue?logo=docker" alt="Docker"></a></p>

- Parallel downloads with incremental sync (seconds on large libraries after the first run)
- Resumable transfers verified by size and content hash
- Watch mode, systemd integration, headless 2FA, Docker-ready
- iCloud Photos supported today. Google Takeout and Immich next.

---

> [!IMPORTANT]
> v0.13 (next release) reshapes selection and folder-structure flags. `--exclude-album NAME` becomes `--album '!NAME'`. `--library` accepts multiple values. `kei sync` with no flags now runs per-album passes plus an unfiled pass. Legacy `{album}` in `--folder-structure` auto-migrates with a warning until v0.20. Full migration guide: [docs/v0.13-migration.md](docs/v0.13-migration.md).
>
> | Flag | Default |
> |---|---|
> | `--album` | `all` |
> | `--smart-folder` | `none` |
> | `--library` | `primary` |
> | `--unfiled` | `true` |
> | `--folder-structure` | `%Y/%m/%d` |
> | `--folder-structure-albums` | `{album}` |
> | `--folder-structure-smart-folders` | `{smart-folder}` |

> [!TIP]
> Coming from `icloudpd`? The [Migration Guide](docs/migration-from-python.md) shows how to `kei sync` without re-downloading.

---

## Install

```sh
brew install rhoopr/kei/kei          # Homebrew
docker pull ghcr.io/rhoopr/kei:latest # Docker
```

Pre-built binaries for macOS, Linux, and Windows are on the [Releases page](https://github.com/rhoopr/kei/releases). For Docker Compose, building from source, FreeBSD, and other install paths, see the [Install wiki page](https://github.com/rhoopr/kei/wiki/Install).

> [!IMPORTANT]
> kei can't access your photos if Advanced Data Protection is on. Turn ADP off and enable "Access iCloud Data on the Web" in your Apple ID settings. Details: [Authentication wiki](https://github.com/rhoopr/kei/wiki/Authentication#advanced-data-protection-adp).

---

## Quick start

```sh
kei sync -u you@example.com -d ~/Photos/iCloud --save-password
```

You'll be prompted for your password, then asked to approve 2FA on a trusted device. Downloads start right after. After the first run, just `kei sync` - username, directory, and password are all remembered.

For a guided walkthrough, run `kei config setup` instead.

---

## Docs

Everything lives on the [wiki](https://github.com/rhoopr/kei/wiki): full CLI reference, filtering and folder templates, watch mode, Docker Compose, credentials, troubleshooting, and more.

- [Commands](https://github.com/rhoopr/kei/wiki/Home#commands) - `sync`, `login`, `list`, `password`, `config`, `reset`, `status`, `verify`, `import-existing`
- [Configuration](https://github.com/rhoopr/kei/wiki/Configuration) - TOML file, env vars, precedence
- [Docker](https://github.com/rhoopr/kei/wiki/Docker) - Compose files and headless 2FA
- [Credentials](https://github.com/rhoopr/kei/wiki/Credentials) - keyring, encrypted file, password files and commands
- [Changelog](CHANGELOG.md)
- [How iCloud's Incremental Sync Works](https://robhooper.xyz/blog-synctoken) - deep dive on CloudKit syncTokens

---

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
