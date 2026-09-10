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

kei copies cloud-hosted photos and videos into folders you control. Today that includes iCloud Photos. The goal is a fast, parallel local backup you can run once, keep as a mirror, or leave unattended in Docker.

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

> [!IMPORTANT]
> **v0.20 moved durable sync settings into TOML.**
>
> Keep CLI flags for one run, env vars for secrets and service glue, and saved settings in `config.toml`.
>
> If an old command fails with a removed flag such as `--download-dir`, move that value into the config file:
>
> ```toml
> [download]
> directory = "/photos"
> ```
>
> Use the [v0.20 migration guide](docs/v0.20-migration.md) and [example.config.toml](example.config.toml) for the full option-by-option TOML reference, including defaults and valid values.

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

### Headless authentication

Use a password file or password command when stdin is not a terminal. To copy
a password from a file into kei's credential store, run:

```sh
kei password --password-file /run/secrets/icloud_password set
```

If a foreground `login` or one-shot `sync` needs 2FA, kei exits with auth code
`3` and prints the recovery commands. Complete the login, then retry the
foreground command:

```sh
kei login get-code
kei login submit-code 123456
```

Watch and service processes stay running while they wait for the submitted
code. They resume from the saved authentication state.

## What kei gives you

- Fast, parallel downloads for large cloud photo libraries.
- A local copy of your media, with no Apple Photos library required.
- Folder layouts for dates, albums, smart folders, shared libraries, and unfiled photos.
- Original photos and videos, plus Live Photos, RAW siblings, and edited versions when you ask for them.
- Resumable downloads and checksum checks before files are marked complete.
- One-shot sync, watch mode, Docker, systemd, launchd, and Windows service support.
- Maintenance commands for status checks, local diagnostics, manifest export, verification, reconcile, reset, and existing-file import.

Today kei syncs iCloud Photos. Immich, Google Takeout, Nextcloud, and Ente support are on the roadmap.

> [!IMPORTANT]
> kei needs iCloud Photos web access. If `Advanced Data Protection` is on, turn it off and enable "Access iCloud Data on the Web" in your Apple ID settings. See [Authentication](https://github.com/rhoopr/kei/wiki/Authentication#advanced-data-protection-adp).

## Product direction

kei is built around reliable local backup: safe file writes, explainable state,
service-friendly operation, and no destructive behavior unless you explicitly
ask for it.

Read the [product charter](docs/product-charter.md) and
[roadmap](docs/roadmap.md) for current priorities.

## Common setups

### Keep a daily mirror

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

### Refresh metadata after a fix

If a kei upgrade fixes a metadata decode or capture bug, re-apply the corrected
metadata to media you already downloaded, without downloading it again:

```sh
kei sync --refresh-metadata
```

This one-shot repair re-enumerates the selected libraries from the provider and
refreshes every downloaded version it encounters before current resolution,
filename, or live-photo download policy is applied. Embedded and sidecar tags
follow your configured metadata outputs. The repair requires a complete library
sweep, cannot use album, smart-folder, media, or date/recent filters, and does
not run under `kei service run`. It is a manual recovery step; the permanent
automatic fix is tracked in [#687](https://github.com/rhoopr/kei/issues/687).

Capture and addition dates now retain Apple's milliseconds in the catalogue.
Run this refresh to recover fractions lost by older versions; enable
`metadata.xmp_sidecar` to correct sidecar capture dates too. Disable embedded
metadata outputs if media bytes must remain untouched. This requires no schema
migration or automatic precision-backfill sweep. Older kei binaries cannot read
catalogue rows containing the new fractional timestamps; do not downgrade after
they have been written. Native embedded subsecond writing is unchanged.

Use this command after upgrading from a version that rendered capture times in
the backup host's timezone. It rewrites XMP sidecars in full. For a timestamp
already present in a file, it adds the capture offset only where the timestamp
reads as capture-local, because an offset attached to a host-local timestamp
would state a capture time the photo never had. When embedded datetime output
is configured, a file with no capture timestamp receives one, with the offset
when Apple supplied it. The command does not move existing media between date
folders.

To replace embedded timestamps written by an older kei version, run:

```sh
kei sync --refresh-metadata --repair-capture-timestamps
```

Set `metadata.set_exif_datetime = true` before you run this command. It can
overwrite a camera-supplied timestamp. It changes only downloaded files that
kei tracks in state and for which Apple supplies a usable capture offset. The
rewrite uses kei's checksum and stable-input guards. It writes the capture-local
timestamp and matching offset together. Files without a usable offset keep
their existing timestamp.

### Replace a truncated local file

First, mark missing and truncated files for retry:

```sh
kei reconcile
```

Use an explicit repair sync when a truncated file should keep its recorded
path instead of producing a size-suffixed sibling:

```sh
kei sync --repair-truncated
```

This one-shot option applies only to a recorded path that `reconcile` marked
truncated. Kei verifies the new download and confirms that the existing bytes
did not change before it atomically replaces the file. Normal downloads and
missing-file retries still use no-overwrite publication. This option does not
run under `kei service run`.

Coming from `icloudpd`? Read [Migrating from icloudpd](docs/migration-from-icloudpd.md).

## Docs

- [Install](https://github.com/rhoopr/kei/wiki/Install)
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
