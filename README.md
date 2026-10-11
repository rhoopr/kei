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

## Before you install

- Use macOS, Linux, or Windows with a native binary, or Docker on a supported
  host/NAS. Choose writable storage with room for the selected originals,
  videos, companion files, and temporary downloads. Start in a new, empty folder.
- Have your Apple Account password and a trusted Apple device ready for the
  six-digit 2FA code. SMS-only 2FA, app-specific passwords, and accounts with
  registered FIDO/WebAuthn security keys are not supported.
- kei requires iCloud Photos web access and Advanced Data Protection (ADP)
  turned off. Turning off ADP removes end-to-end encryption for Photos and the
  other data categories it protects; Apple then holds the recovery keys under
  standard data protection. Read [Apple's explanation](https://support.apple.com/en-us/108756)
  and [kei authentication requirements](https://github.com/rhoopr/kei/wiki/Authentication)
  before deciding. If you want to keep ADP enabled, stop here: kei cannot back up
  that library. Web access alone does not make ADP compatible.

Already have an `icloudpd` archive? Start with
[Migrating from icloudpd](docs/migration-from-icloudpd.md), including the import
preview and path checks, before pointing kei at it. For an existing kei install,
read [Upgrade and compatibility](docs/v0.25-upgrade.md) before opening its state.

## Choose your installation route

### Native CLI

On macOS with Homebrew:

```sh
brew install rhoopr/kei/kei
```

For macOS, Linux, or Windows without Homebrew, download the matching binary from
[Releases](https://github.com/rhoopr/kei/releases), unpack it, and put `kei` on
your PATH. Confirm the installed version with `kei --version`. Source builds
and FreeBSD instructions are in [Install](https://github.com/rhoopr/kei/wiki/Install).

### Docker or NAS

Follow [First backup with Docker](docs/docker-first-backup.md) for persistent
mounts, configuration, password storage, login, and a small trial sync. Run
those commands on the Docker host; the native commands below assume `kei` is
installed directly on your computer.

## Make a small first backup

For a new native setup, run:

```sh
kei config setup
```

Choose a fresh destination such as `~/Photos/kei-first-backup`, your main library,
and **Manually when needed**. Leave date/recent limits unset for now. Review the
selected albums, media types, and output paths before saving. At **Want to kick
off your first sync right now?**, answer **no** for this trial. The default is
**yes**, which immediately starts a one-shot sync with the saved selection.
Even if you choose watch mode, that wizard-started sync runs only once; later
`kei sync` runs can inherit the saved watch interval.

The wizard writes `~/.config/kei/config.toml` and offers credential storage.
If it writes an environment file, follow its printed load instructions before
the next command. Then run the bounded trial:

```sh
kei sync --recent 10
kei status
```

Approve the trusted-device prompt and enter the six-digit code when asked.
`--recent 10` caps recent asset candidates per selected library before filters;
it does not promise ten files or ten successful downloads. Album copies, Live
Photo companions, and selected renditions can produce more files, and filters
can leave fewer. This is a sample, not a complete library backup.

### Check the result

1. Read the sync summary and any warnings. Failed, pending, unresolved, or
   incomplete work needs attention even if some files downloaded successfully.
2. Open the destination in your file manager. Find expected recent photos in
   the configured album/date folders and open a photo and video when selected.
   Live Photos normally include a still image and a MOV companion.
3. Use `kei status` to inspect recorded progress. Its clean status describes
   recorded work, not proof that every iCloud item or rendition was selected.
   Optional `kei verify --checksums` checks recorded local bytes; it cannot prove
   cloud completeness or restore damaged files.

Photos live under `[download].directory`. Config, sessions, credentials, and
SQLite state normally live under `~/.config/kei`; with a custom config path,
state defaults to that file's parent unless `data_dir` or `KEI_DATA_DIR` overrides
it. Keep this state as well as the media. Default renditions include originals
and Live Photo companions; edited versions and alternative/RAW renditions need
explicit selection. See [example.config.toml](example.config.toml).

## Expand the backup

Once the trial looks right, `kei sync` without the one-run `--recent` flag syncs
your saved selection. Review the config first: saved date/recent filters still
apply, and the wizard can select all visible libraries, including shared ones.
Check albums, smart folders, unfiled media, edited versions, and RAW options
before calling the result a full backup.

As an alternative to the wizard, a minimal config is:

```toml
[auth]
username = "you@example.com"

[download]
directory = "~/Photos/iCloud"
```

This uses the primary library, all its user albums, and an unfiled pass by
default. Keep persistent settings in TOML. For a one-run date window instead
of the full saved selection, use `kei sync --recent 30d`.

For a daily unattended run, add this table to your existing config:

```toml
[watch]
interval = 86400
```

`kei service run` runs in the foreground; `kei install` installs a native
service. Follow [Service mode](https://github.com/rhoopr/kei/wiki/Service) and
[Authentication](https://github.com/rhoopr/kei/wiki/Authentication) for startup,
saved credentials, and future 2FA recovery. Docker users should finish the
[container guide](docs/docker-first-backup.md#run-unattended).

## Get help or maintain a backup

For a failed first run, check the destination permissions, free space, selection,
and [authentication requirements](https://github.com/rhoopr/kei/wiki/Authentication).
See [Status](https://github.com/rhoopr/kei/wiki/Status) and
[Doctor](https://github.com/rhoopr/kei/wiki/Doctor) for diagnostics.

For bugs or confusing results, open an [issue](https://github.com/rhoopr/kei/issues)
with the version, expected result, and symptoms. When available, run
`kei support-export --output kei-support.json` with the same config and data
directory. It collects saved evidence offline without a new sync or automatic
upload. Review the file locally before attaching it; older versions and partial
exports are welcome. See [Support export](docs/support-export.md), including
Docker instructions.

Repair commands can change state or replace recorded files. Read
[Backup maintenance](docs/backup-maintenance.md) first. Do not delete state or
conflicting files to clear a warning. Legacy preservation normally refuses
multiply linked files; its [hardlink waiver](docs/backup-maintenance.md#legacy-preservation-and-hardlinks)
can affect shared inodes and does not make kei read-only.

## What kei gives you

- Parallel downloads, resumable transfers, and checksum checks before completion.
- Local media without an Apple Photos library, with configurable album/date layouts.
- Shared libraries, Live Photos, and optional edited and alternative/RAW renditions.
- One-shot sync, watch mode, Docker, systemd, launchd, and Windows service support.
- Status, diagnostics, manifest export, verification, repair, and existing-file import.

Metadata capture in the catalogue is automatic. Embedded metadata and XMP
sidecars are opt-in. See [Metadata output](docs/metadata.md) for supported
formats, safe refusals, and explicit timestamp repair.

## Docs

- [Install](https://github.com/rhoopr/kei/wiki/Install)
- [Upgrade and compatibility](docs/v0.25-upgrade.md)
- [Account state migration](docs/account-state-migration.md)
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
