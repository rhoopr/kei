<p align="center"><img src="assets/logo.png" alt="kei logo" width="500"></p>
<h1 align="center">kei: fast, parallel backups<br>
  for cloud-hosted photos and videos.</h1>

<p align="center">
  <a href="https://github.com/rhoopr/kei/blob/main/Cargo.toml"><img src="https://img.shields.io/badge/Rust_MSRV-1.91%2B-dea584?logo=rust" alt="Rust MSRV 1.91+"></a>
  <a href="LICENSE"><img src="https://img.shields.io/github/license/rhoopr/kei?color=8b959e" alt="License: MIT"></a>
  <a href="https://github.com/rhoopr/kei/releases"><img src="https://img.shields.io/github/v/release/rhoopr/kei?color=blue&label=version" alt="Version"></a>
  <a href="https://github.com/rhoopr/kei/actions/workflows/docker.yml"><img src="https://img.shields.io/github/actions/workflow/status/rhoopr/kei/docker.yml?branch=main&label=build&logo=github" alt="Build"></a><br>
  <a href="https://github.com/rhoopr/homebrew-kei"><img src="https://img.shields.io/badge/homebrew-tap-FBB040?logo=homebrew" alt="Homebrew"></a>
  <a href="https://github.com/rhoopr/kei/releases"><img src="https://img.shields.io/github/downloads/rhoopr/kei/total?logo=github&label=downloads" alt="Downloads"></a>
  <a href="https://ghcr.io/rhoopr/kei"><img src="https://img.shields.io/badge/ghcr.io-kei-blue?logo=docker" alt="Docker"></a>
  <a href="https://ghcr.io/rhoopr/kei"><img src="https://img.shields.io/badge/dynamic/json?url=https%3A%2F%2Fgithub.com%2Fipitio%2Fbackage%2Fraw%2Findex%2Frhoopr%2Fkei%2Fkei.json&query=%24.downloads&logo=docker&label=pulls" alt="Pulls"></a></p>

kei copies cloud-hosted photos and videos into folders you control. Today that includes iCloud Photos. The goal is a fast, parallel local backup you can run once, keep up to date, or leave unattended in Docker.

It handles the parts that make photo backups annoying: big libraries, shared libraries, albums, Live Photos, RAW files, edited versions, retries, interrupted downloads, and existing archives you don't want to download twice.

> [!WARNING]
> kei is pre-release software. Minor versions may contain breaking changes.
> 
> Read [CHANGELOG.md](CHANGELOG.md) before updating.

## Install

```sh
brew install rhoopr/kei/kei
```

```sh
docker pull ghcr.io/rhoopr/kei:latest
```

Pre-built binaries for macOS, Linux, and Windows are on the [Releases page](https://github.com/rhoopr/kei/releases). For Docker Compose, source builds, FreeBSD, and NAS setup, see [Install](https://github.com/rhoopr/kei/wiki/Install).

## Start

```sh
kei config setup
kei sync
```

The setup wizard creates `~/.config/kei/config.toml`, asks where photos should land, and can save your password in the OS keyring. You'll still approve 2FA on a trusted Apple device the first time.

A tiny config looks like this:

```toml
[auth]
username = "you@example.com"

[download]
directory = "~/Photos/iCloud"
```

After the first run:

```sh
kei status
kei sync --recent 30d
kei list albums
```

For unattended login and two-factor recovery, see [Authentication](https://github.com/rhoopr/kei/wiki/Authentication).
Keep saved settings in TOML; use [example.config.toml](example.config.toml) for defaults and supported options.

## What kei gives you

- Fast, parallel downloads for large cloud photo libraries.
- A local copy of your media, with no Apple Photos library required.
- Folder layouts for dates, albums, smart folders, shared libraries, and unfiled photos.
- Original photos and videos, plus Live Photos, RAW siblings, and edited versions when you ask for them.
- Resumable downloads and checksum checks before files are marked complete.
- One-shot sync, watch mode, Docker, systemd, launchd, and Windows service support.
- Maintenance commands for status checks, local diagnostics, manifest export, verification, reconcile, reset, and existing-file import.

> [!IMPORTANT]
> kei needs iCloud Photos web access. If `Advanced Data Protection` is on, turn it off and enable "Access iCloud Data on the Web" in your Apple ID settings. See [Authentication](https://github.com/rhoopr/kei/wiki/Authentication#advanced-data-protection-adp).

## Common setups

### Run a daily backup

```toml
[auth]
username = "you@example.com"

[download]
directory = "/photos"

[watch]
interval = 86400
```

Run it with `kei service run`, Docker Compose, or `kei install`.

### Sync only part of the library

```toml
[filters]
albums = ["Family", "Trips"]
unfiled = false
media = ["photos", "videos", "live-photos"]
```

You can also sync recent media for one run:

```sh
kei sync --recent 100
kei sync --recent 30d
```

### Adopt files you already have

```sh
kei import-existing
kei sync
```

### Check or repair a backup

Use `kei status` to inspect recorded progress and `kei verify --checksums` to
check local bytes. Repair commands can change state or replace recorded files.
Read [Backup maintenance](docs/backup-maintenance.md) before using them.

Metadata capture in the catalogue is automatic. Embedded metadata and XMP
sidecars are opt-in. See [Metadata output](docs/metadata.md) for supported
formats, safe refusals, and explicit timestamp repair.

Coming from `icloudpd`? Read [Migrating from icloudpd](docs/migration-from-icloudpd.md).

## Docs

- [Install](https://github.com/rhoopr/kei/wiki/Install)
- [Upgrade and compatibility](docs/v0.24-upgrade.md)
- [Backup maintenance](docs/backup-maintenance.md)
- [Metadata output](docs/metadata.md)
- [Configuration](https://github.com/rhoopr/kei/wiki/Configuration)
- [Docker](https://github.com/rhoopr/kei/wiki/Docker)
- [Service mode](https://github.com/rhoopr/kei/wiki/Service)
- [Commands](https://github.com/rhoopr/kei/wiki/Home#commands)
- [Credentials](https://github.com/rhoopr/kei/wiki/Credentials)
- [Status](https://github.com/rhoopr/kei/wiki/Status)
- [Doctor](https://github.com/rhoopr/kei/wiki/Doctor)
- [Manifest export](https://github.com/rhoopr/kei/wiki/Manifest)
- [Product charter](docs/product-charter.md)
- [Roadmap](docs/roadmap.md)

Open an issue for bugs or sharp edges: [Issues](https://github.com/rhoopr/kei/issues)

## License

MIT - see [LICENSE](LICENSE)

## Acknowledgments

kei's iCloud support builds on reverse-engineering from [icloud-photos-downloader](https://github.com/icloud-photos-downloader/icloud_photos_downloader). kei was originally published as `icloudpd-rs` before broadening beyond iCloud Photos.
