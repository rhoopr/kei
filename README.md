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

kei is a command-line tool that backs up iCloud Photos to folders you control. Use it on your computer, server or NAS for a one-time backup or regular automatic syncs.

It supports large libraries, Shared Libraries, albums, Live Photos, RAW files and edited versions. It can resume interrupted downloads and check existing files before downloading them again.

> [!WARNING]
> kei is pre-release software. Minor versions may contain breaking changes.
> 
> Read the [update guide](docs/v0.26.0-upgrade.md) and [changelog](CHANGELOG.md) before updating.

## Before you install

- Use macOS, Linux or Windows, or run kei in Docker on a supported computer or NAS. Choose a folder kei can write to, with room for your photos, videos, accompanying files and temporary downloads. For a new backup, start in an empty folder.
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

Already have an `icloudpd` archive? Start with [Migrating from icloudpd](docs/migration-from-icloudpd.md), including the import preview and path checks, before pointing kei at it. For an existing kei install, read [Upgrade and compatibility](docs/v0.26.0-upgrade.md) before opening its state.

## Choose your installation route

<a id="native-cli"></a>

### Install on your computer

On macOS with Homebrew:

```sh
brew install rhoopr/kei/kei
```

For macOS, Linux or Windows without Homebrew, download the matching file from [Releases](https://github.com/rhoopr/kei/releases), unpack it, and put `kei` in a folder on your PATH so you can run it from a terminal. Check the installed version with `kei --version`. Instructions for building from source and using FreeBSD are in [Install](https://github.com/rhoopr/kei/wiki/Install).

### Docker or NAS

Follow [First backup with Docker](docs/docker-first-backup.md) to set up folders that survive container replacement, save your settings, sign in and try a small backup. Run those commands on the machine running Docker. The commands below assume `kei` is installed directly on your computer.

## Make a small first backup

For a new installation on your computer, run:

```sh
kei config setup
```

Choose an empty destination such as `~/Photos/kei-first-backup`, your main library, and **Manually when needed**. Leave date and recent-item limits unset for now. Check the selected albums, file types and destination folders before saving. At **Want to kick off your first sync right now?**, answer **no** so you can try a small backup first. The default answer, **yes**, immediately runs one sync using your saved choices.

If you choose watch mode instead, the setup wizard still runs only once. Later `kei sync` commands can use the repeat interval you saved.

The setup wizard saves your settings in `~/.config/kei/config.toml` and offers to save your login details. If it creates an environment file, follow the printed instructions to load it before the next command. Then try a small backup:

```sh
kei sync --recent 10
kei status
```

Approve the sign-in prompt on your trusted Apple device and enter the six-digit code when asked.

`--recent 10` considers up to ten recent photo or video items in each selected library before applying filters. The number of downloaded files can differ: album copies, Live Photo videos and additional versions can produce more files, while filters or failed downloads can leave fewer. This is only a sample of your library.

### Check the result

1. Read the sync summary and warnings. Failed or unfinished work still needs attention, even if some files downloaded successfully.
2. Open the destination in your file manager. Look for the recent photos you expected in the album and date folders, and open a photo and video if you selected both. Live Photos normally include a still image and a MOV video.
3. Run `kei status` to check progress. A clean result does not mean you selected every item or version in iCloud. You can also run `kei verify --checksums` to check saved files against their recorded checksums. That check cannot tell you whether the backup includes everything in iCloud or repair damaged files.

Photos are saved in the folder set by `[download].directory`. Settings normally live in `~/.config/kei/config.toml`; saved sessions and backup state normally live in `~/.config/kei/cookies`, even with a custom configuration file. Set `data_dir` to change the session and state directory; `KEI_DATA_DIR` takes precedence over that setting. Keep the state as well as your photos and videos.

By default, kei downloads originals and the videos that accompany Live Photos. Select edited versions and other versions, including RAW, explicitly if you want them. See [example.config.toml](example.config.toml).

<a id="expand-the-backup"></a>

## Run your normal backup

Once the small backup looks right, run `kei sync` without `--recent` to use your saved choices. Check those settings first: any saved date or recent-item limits still apply, and the setup wizard can select all available libraries, including Shared Libraries. Check albums, smart folders, photos outside albums, edited versions and RAW options before treating the result as a full backup.

As an alternative to the wizard, a minimal config is:

```toml
[auth]
username = "you@example.com"

[download]
directory = "~/Photos/iCloud"
```

This uses your primary library, all its regular albums, and photos and videos outside those albums. Keep settings you want to reuse in the configuration file. To limit one run to the last 30 days, use `kei sync --recent 30d`.

For a daily unattended run, add this table to your existing config:

```toml
[watch]
interval = 86400
```

`kei service run` keeps running in your terminal. kei does not register itself as a system service; use systemd, launchd, Windows SCM, or Docker to start it at boot. Follow [Service mode](https://github.com/rhoopr/kei/wiki/Service) and [Authentication](https://github.com/rhoopr/kei/wiki/Authentication) for startup, saved login details and what to do when Apple asks you to sign in again. Docker users should finish the [container guide](docs/docker-first-backup.md#run-unattended).

A minimal systemd user unit looks like this. Set `notify_systemd = true` under `[watch]` when using `Type=notify`.

```ini
[Unit]
Description=kei Media Sync Engine
After=network-online.target
Wants=network-online.target

[Service]
Type=notify
ExecStart=%h/.local/bin/kei service run --config %h/.config/kei/config.toml
Restart=on-failure
RestartSec=10s
WatchdogSec=120
NotifyAccess=main

[Install]
WantedBy=default.target
```

### Stop or remove an existing kei service

A service registered by an earlier kei version still works. Stop and remove it with the same manager that started it. These commands remove only the registration. They do not delete your photos, videos, configuration, credentials, kei state, or logs.

```sh
# Linux, per-user
systemctl --user disable --now kei.service
rm ~/.config/systemd/user/kei.service
systemctl --user daemon-reload

# Linux, system-wide (as root)
sudo systemctl disable --now kei.service
sudo rm /etc/systemd/system/kei.service
sudo systemctl daemon-reload

# macOS
launchctl bootout gui/$(id -u)/com.rhoopr.kei
rm ~/Library/LaunchAgents/com.rhoopr.kei.plist
```

On Windows, run an elevated prompt:

```powershell
sc.exe stop com.rhoopr.kei
sc.exe delete com.rhoopr.kei
```

<a id="get-help-or-maintain-a-backup"></a>

## Get help

If a sync fails, check that kei can write to the destination, that there is enough free space, and that you selected the library and albums you intended. Check the [sign-in requirements](https://github.com/rhoopr/kei/wiki/Authentication) too. The [Status](https://github.com/rhoopr/kei/wiki/Status) and [Doctor](https://github.com/rhoopr/kei/wiki/Doctor) guides explain how to check your setup.

For a bug or confusing result, open an [issue](https://github.com/rhoopr/kei/issues) with your kei version, what you expected and what happened. When available, run `kei support-export --output kei-support.json` with the same configuration and data directory. It writes a redacted snapshot of current config, state and health without another sync and uploads nothing automatically. It does not keep or read prior operation history. Review the file before attaching it. You can still report a problem if you're using an older version or the export is incomplete. See [Support snapshot](docs/support-export.md) for details and Docker instructions.

Some repair commands change kei's saved state or replace files. Read [Backup maintenance](docs/backup-maintenance.md) first. Don't delete state or conflicting files just to clear a warning. When preserving older files, kei normally refuses files with hardlinks, where several paths refer to the same file. The [hardlink waiver](docs/backup-maintenance.md#legacy-preservation-and-hardlinks) allows this case, but a change through one path can affect the others. It does not prevent kei from writing to files.

## What kei gives you

- Downloads several files at once, resumes interrupted transfers, and checks downloaded data before marking a file complete.
- Saves ordinary photo and video files without needing an Apple Photos library, arranged by album and date as you choose.
- Supports Shared Libraries, Live Photos, and optional edited and other versions, including RAW.
- Runs once or on a schedule, in Docker or as a background service on Linux, macOS or Windows.
- Provides progress reports, troubleshooting tools, file lists, backup checks, repair commands and import of existing files.

Kei automatically saves photo information in its own database. Writing that information into media files or separate XMP files requires you to enable it. See [Metadata output](docs/metadata.md) for supported formats, cases where kei won't write, and options for repairing timestamps.

## Docs

- [Install](https://github.com/rhoopr/kei/wiki/Install)
- [Upgrade and compatibility](docs/v0.26.0-upgrade.md)
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

Report bugs or confusing behavior in [Issues](https://github.com/rhoopr/kei/issues).

## License

MIT - see [LICENSE](LICENSE)

## Acknowledgments

kei's iCloud support builds on reverse-engineering from [icloud-photos-downloader](https://github.com/icloud-photos-downloader/icloud_photos_downloader). kei was originally published as `icloudpd-rs` before broadening beyond iCloud Photos.
