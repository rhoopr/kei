# Migrating from `icloudpd`

This guide is for moving a Python [icloudpd](https://github.com/icloud-photos-downloader/icloud_photos_downloader) photo archive to kei v0.26.0. Your photos can stay where they are. Preview the import first so you can see which files kei recognizes before allowing a normal sync to download anything.

Import is not automatic deduplication. Kei must match a local file to the file it expects from iCloud. Files it cannot match may be downloaded again under a different name or folder.

### v0.30 development upgrade note

The v0.26.0 instructions below describe that release. The v0.30 development branch removes relative date windows, including `--recent 30d` and `Nd` values in either date bound. Before using that branch, remove relative settings from `[filters]` or replace them with fixed `skip_created_before` / `skip_created_after` dates such as `"2025-01-01"` and `"2025-01-31"`. Remove a setting to omit its limit; use `recent = 100` for a count limit. Sync also accepts `--skip-created-before 2025-01-01 --skip-created-after 2025-01-31`. Date-only boundaries are inclusive and use the photo's capture-local calendar date.

Kei rejects retired relative settings before authentication or sync-state/media changes, even if a CLI override supplies a fixed date. It does not rewrite your config or guess a fixed boundary from the current date. Count-form `--recent 100` still works for sync and import; import does not gain date filtering.

## Before you start

- Stop icloudpd, kei and any other programs writing to the archive. Pause scheduled runs.
- Make a separate backup of the photos, videos, accompanying metadata files and any existing kei configuration and state. Copy the complete state directory while kei is stopped, including database companion files. Keep the backup unchanged.
- Use the same Apple Account and iCloud region as the original download. Keep different accounts' configuration and state separate. Check the [sign-in requirements](../README.md#before-you-install), including the Advanced Data Protection restriction.
- Kei cannot use Python's `~/.pyicloud` cookies. Sign in with kei using your Apple Account password and trusted-device verification code. Kei saves its own session for later runs.
- If you already use kei, read the [v0.26.0 update guide](v0.26.0-upgrade.md) first. Opening existing state can update it automatically, even during an import preview.
- Don't let the setup wizard start an immediate sync against your existing archive. If you use the wizard, answer **no** when it offers to start syncing.

Install [v0.26.0](https://github.com/rhoopr/kei/releases/tag/v0.26.0). For Docker, use `ghcr.io/rhoopr/kei:0.26.0` and the [Docker instructions below](#docker-migration).

## If you're coming from old `icloudpd-rs`

An old kei or icloudpd-rs database needs different handling from a Python photo folder. Preserve the original setup and follow [account-state adoption](account-state-migration.md) and the [older-version update instructions](v0.25-upgrade.md#back-up-and-adopt-existing-state).

Do not copy or rename old authentication files into kei's current session directory. Older guides showed cookie-copy commands that no longer apply. Current kei needs its own login, and adopting old state requires confirming that it belongs to the configured account. Don't confirm ownership if it is uncertain.

## Step 1: import the existing tree

Create or review your configuration before running import. Set the account and existing photo folder:

```toml
[auth]
username = "you@example.com"

[download]
directory = "~/Photos/iCloud"
```

Match your old folder names, filenames, selected libraries and file versions using the sections below. Keep the same configuration for import and later syncs.

Preview the matches:

```sh
kei import-existing --config ~/.config/kei/config.toml --dry-run
```

The preview signs in and reads the selected libraries from iCloud. It does not record files as imported or change your media. It can still create login/state files or update an existing kei database, which is why the backup comes first.

Read the summary. If many expected files are unmatched, stop and check the settings before continuing. An unmatched file is left alone, but a later sync may download another copy. Pay particular attention to [date-folder differences](#date-folders).

Once the preview looks right, run the import:

```sh
kei import-existing --config ~/.config/kei/config.toml
```

Kei records recognized files as already downloaded. A later sync can reuse them when its checks and settings still agree. This does not prove that every file in iCloud is backed up or that an old file is undamaged.

Import reads all selected libraries and albums before recording matches. If that listing fails or is interrupted, no files are recorded as imported. If it stops later while recording matches, some may already have been saved; repeating the import is supported. Large selections can use substantial memory.

### How files are matched

By default, kei checks the expected path and size, then saves a checksum of the local file for future checks. It also checks existing ownership records so one item cannot claim another item's file.

Kei recognizes supported icloudpd naming patterns, but a matching name and size are not always enough. If two iCloud items could match the same file, kei leaves it unmatched unless its existing records can resolve the conflict. This includes collisions in shortened `name-id7` names, numbered duplicates and names changed to remove unsupported characters. Import does not rename or overwrite those files.

### Strict adoption check

For an extra check, add `--strict` to the preview and actual import:

```sh
kei import-existing --config ~/.config/kei/config.toml --dry-run --strict
kei import-existing --config ~/.config/kei/config.toml --strict
```

This compares the first 64 KiB of each candidate file with iCloud, or the whole file if it is smaller. It is not a full-file comparison for larger files. A mismatch or failed comparison is counted under `Strict refusals` and the file is not imported.

To keep this check enabled:

```toml
[import]
strict = true
```

## Match Python's path layout

Use the settings from your actual icloudpd setup. Don't assume the defaults match your archive.

### Folder structure

Python's `--folder-structure "{:%Y/%m/%d}"` becomes:

```toml
[download]
directory = "~/Photos/iCloud"
folder_structure = "%Y/%m/%d"
```

Kei has separate folder settings for different kinds of albums:

| Setting | Files it places | Default |
|---|---|---|
| `[download].folder_structure` | Photos and videos outside user albums | `%Y/%m/%d` |
| `[download].folder_structure_albums` | User albums | `{album}` |
| `[download].folder_structure_smart_folders` | Smart folders | `{smart-folder}` |

If your existing folders include album names and dates:

```toml
[download]
directory = "~/Photos/iCloud"
folder_structure_albums = "{album}/%Y/%m/%d"
```

Use `{album}` only in `folder_structure_albums`, not in `folder_structure`.

### Date folders

Kei uses the photo's capture timezone when Apple supplies a usable offset. Otherwise it uses the timezone of the computer running kei. Your Python archive may have used a different timezone, so the expected date folder can differ, especially for travel photos or downloads made on a UTC server.

Import does not move files between date folders or search every folder for a possible match. If the paths differ, a normal sync may download another copy in the expected folder and leave the old copy in place. Review those differences before syncing; don't delete the original to force a match.

Dates written into XMP files can still agree even when the folder dates differ. Matching timestamps alone do not prove that kei can reuse a file.

### Filename policy

If you used Python's `--file-match-policy name-id7`, set:

```toml
[photos]
file_match_policy = "name-id7"
```

Kei supports icloudpd's size-suffixed duplicate names, replacement of unsupported filename characters, RAW naming and Live Photo video names. It normally removes Unicode characters from filenames. If you used `--keep-unicode-in-filenames`, set `[photos].keep_unicode_in_filenames = true` before import.

Always check the preview: collisions and existing ownership records can still prevent a match.

### Live photos, RAW, and sizes

Match the file versions your Python setup downloaded. For example:

```toml
[photos]
resolution = "original"
live_photo_mode = "both"
live_resolution = "original"
live_photo_mov_filename_policy = "suffix"
raw_policy = "as-is"
```

Edited and alternative versions, including RAW, need the corresponding settings. See [example.config.toml](../example.config.toml) for the available choices.

You can try a smaller import with `--recent 100`. Import accepts a count, not a date window: `--recent 30d` works only with sync.

### Libraries and albums

Without explicit selection settings, kei uses the primary library, its user albums and photos outside those albums. Smart folders are not selected by default. The setup wizard can select all visible libraries and add a `{library}` folder to keep files from different libraries apart, so review what it saved.

To include all libraries, set this in the configuration used by both import and sync:

```toml
[filters]
libraries = ["all"]
```

You can also select libraries for an individual import:

```sh
kei import-existing --library all --config ~/.config/kei/config.toml --dry-run
```

That command-line choice is not saved for later syncs.

If your existing archive separates libraries, match that layout in the active folder settings:

```toml
[download]
directory = "~/Photos/iCloud"
folder_structure = "{library}/%Y/%m/%d"
folder_structure_albums = "{library}/{album}/%Y/%m/%d"
```

Import reads album, smart-folder and unfiled choices from the configuration; it has no `--album`, `--smart-folder` or `--unfiled` flags.

Explicit album and smart-folder selections are searched across all visible libraries. The library selector still controls photos outside albums. Setting `libraries = ["primary"]` therefore does not restrict an explicitly named album to the primary library. Check the preview before importing.

## Step 2: sync

Review the actual import summary before starting:

```sh
kei sync --config ~/.config/kei/config.toml
```

Use the same account, folders and selection settings. If unexpected bulk downloads start, stop and review which files were unmatched. Don't reset kei's state or remove existing photos to force progress.

Keep the saved state as well as your media. It normally lives in `~/.config/kei`; with a custom configuration file, it normally lives beside that file. `data_dir` or `KEI_DATA_DIR` can override the location.

Use `kei password set` if you want kei to save a prompted password. The `--save-password` flag on sync only saves a password supplied directly on the command line or through the environment; it does not save prompted, password-file or password-command input.

For all settings, see [example.config.toml](../example.config.toml) and [Configuration](https://github.com/rhoopr/kei/wiki/Configuration). For problems, keep the normal output and use [support export](support-export.md); review anything you share for private information.

## Common flag mapping

| `icloudpd` | kei | Notes |
|---|---|---|
| `-u`, `--username` | `[auth].username` | `ICLOUD_USERNAME` still works for automation. |
| `-p`, `--password` | `--password`, `[auth].password_file`, `[auth].password_command`, or `kei password set` | Avoid putting passwords in process lists when you can. |
| `-d`, `--directory` | `[download].directory` | Import and sync read the same TOML directory. |
| `-a`, `--album` | `[filters].albums = ["Name"]` | Use `["!Name"]` for exclusions. |
| `--exclude-album NAME` | `[filters].albums = ["!NAME"]` | Removed in v0.20. |
| `--list-albums` | `kei list albums` | The old flag was removed in v0.20. |
| `--list-libraries` | `kei list libraries` | The old flag was removed in v0.20. |
| `--cookie-directory` | `data_dir` | The old flag was removed in v0.20. New default is `~/.config/kei`; `KEI_DATA_DIR` still works for automation. |
| `--folder-structure "{:%Y/%m/%d}"` | `[download].folder_structure = "%Y/%m/%d"` | Python wrapper syntax is still accepted. Use `folder_structure_albums` and `folder_structure_smart_folders` for albums and smart folders. |
| `--size original` | `[photos].resolution = "original"` | Values: `original`, `medium`, `thumb`, `none`. Use `edited = true` for edited versions and `alternative = true` for other versions, including RAW. |
| `--threads-num` | `[download].threads` | Default is 10. With `[download].bandwidth_limit` and no explicit thread count, default drops to 1. |
| `--skip-videos`, `--skip-photos` | `[filters].media` | Use `media = ["photos", "live-photos"]` to skip videos, or `media = ["videos", "live-photos"]` to skip normal photos. |
| `--skip-live-photos` | `[photos].live_photo_mode = "skip"` | The old flag was removed in v0.20. |
| `--recent 100` | `--recent 100` | Count form works for sync and import. |
| `--recent 30d` | `--recent 30d` for sync | Import rejects the days form. |
| `--set-exif-datetime` | `[metadata].set_exif_datetime = true` | kei also has EXIF rating, GPS, and description settings. |
| `--keep-unicode-in-filenames` | `[photos].keep_unicode_in_filenames = true` | Set this before import. |
| `--live-photo-mov-filename-policy` | `[photos].live_photo_mov_filename_policy` | `suffix` or `original`. |
| `--file-match-policy` | `[photos].file_match_policy` | `name-size-dedup-with-suffix` or `name-id7`. |
| `--align-raw` | `[photos].raw_policy` | `as-is`, `prefer-raw`, or `prefer-jpeg`. |
| `--domain` | `[auth].domain` | `com` or `cn`. |
| `--watch-with-interval` | `[watch].interval`; Docker uses `kei service run` | Save the repeat interval in the configuration and use the service commands. |
| `--log-level` | `--log-level` or `log_level` | `debug`, `info`, `warn`, `error`. |
| `--only-print-filenames` | `--only-print-filenames` | Per-run flag that prints paths and doesn't download. |
| `--dry-run` | `--dry-run` | Preview only. Import still signs in and can create or update its state files, but does not record matched files as imported. |
| `--notification-script` | `[notifications].script` | kei sends `KEI_EVENT`, `KEI_MESSAGE`, `KEI_ICLOUD_USERNAME`, per-cycle `KEI_*` stats, and `KEI_REPORT_JSON` when `[report].json` is configured. |

## Python flags without a kei equivalent

| Python flag | kei status |
|---|---|
| `--until-found` | Kei keeps a saved record of completed downloads. |
| `--auto-delete` | Not supported in v0.26.0. |
| `--delete-after-download` | Not supported in v0.26.0. |
| `--keep-icloud-recent-days` | Not supported in v0.26.0. |
| `--smtp-*` | Not supported in v0.26.0. |
| `--use-os-locale` | Not supported in v0.26.0. |
| `--password-provider` | Use `--password-file`, `--password-command`, or `kei password set`. |
| `--mfa-provider` | Use kei login with your password and trusted-device verification code; Python MFA providers are not reused. |

`[metadata].xmp_sidecar` is implemented in kei, along with
`[metadata].embed_xmp`. They write iCloud metadata that Python didn't handle
the same way.

## Useful kei-only options

| Option or command | What it does |
|---|---|
| `kei config setup` | Interactive config wizard. |
| `kei config show` | Show the settings kei will use. |
| `--config <path>` | Use a TOML config file. |
| `--password-file <path>` | Read the password from a file or Docker secret. |
| `--password-command <cmd>` | Read the password from Unix `sh -c`; unsupported on native Windows. |
| `kei password set` | Save the password in the operating system keyring or an encrypted file. |
| `--save-password` | For sync and service runs, save a password supplied directly by command line or environment after login. Use `password set` for other sources. |
| `[filters].libraries` | Select primary, shared, all, named, or excluded iCloud libraries. |
| `[filters].smart_folders` | Sync Favorites, Screenshots, Hidden, Recently Deleted, and other Apple smart folders across visible libraries when explicitly set. |
| `[filters].unfiled = false` | Skip photos and videos outside user albums. |
| `[filters].filename_exclude` | Skip files by glob, such as `*.AAE` or `Screenshot*`. |
| `[download].bandwidth_limit` | Limit total download speed, for example `10M` or `1.5Mi`. |
| `[metadata].set_exif_rating`, `.set_exif_gps`, `.set_exif_description` | Write more iCloud metadata into downloaded media. |
| `[metadata].embed_xmp`, `[metadata].xmp_sidecar` | Embed XMP or write `.xmp` sidecars. |
| `[download.retry].per_transfer` | Retry downloads inside one transfer. Default is 3. |
| `[download.retry].per_asset` | Cap lifetime attempts per asset. Default is 10; 0 disables the cap. |
| `[download].temp_suffix` | Temp file suffix for partial downloads. Default is `.kei-tmp`. |
| `[report].json` | Save the latest sync report as a JSON file. |
| `[server].bind`, `[server].port` | Serve `/healthz` and `/metrics` during watch mode. |
| `[watch].reconcile_every_n_cycles` | Periodically check for differences and report warnings during watch mode, without changing files. |
| `--friendly`, `--no-friendly` | Control the interactive progress display. |
| `kei status` | Show database and sync status. |
| `kei verify` | Check downloaded files exist, with optional checksums. |
| `kei reconcile` | Record missing local files so the next sync downloads them again. Read the maintenance guide first. |

## Removed kei compatibility aliases

This section is only for old kei or icloudpd-rs configurations. These aliases were removed in v0.20; use the settings on the right with v0.26.0:

| Deprecated | Use |
|---|---|
| `--exclude-album`, `KEI_EXCLUDE_ALBUM` | `[filters].albums = ["!NAME"]` |
| `{album}` inside `--folder-structure` | `[download].folder_structure_albums = "{album}/..."` |
| `[filters].album` | `[filters].albums = ["NAME"]` |
| `[filters].exclude_albums` | `[filters].albums = ["!NAME"]` |
| `[filters].library` | `[filters].libraries` |

## Docker migration

Use the existing photo folder, but give kei its own persistent configuration folder. Do not start a background sync until the import preview and actual import have been reviewed.

Create `config/config.toml`:

```toml
[auth]
username = "you@example.com"

[download]
directory = "/photos"
```

Add the matching folder, filename and selection settings described above. A Compose file for the preview and import is:

```yaml
services:
  kei:
    image: ghcr.io/rhoopr/kei:0.26.0
    container_name: kei
    stop_grace_period: 30s
    environment:
      TZ: "${TZ:-UTC}"
      KEI_CHOWN_PHOTOS: "0"
    volumes:
      - ./config:/config
      - /path/to/photos:/photos
```

Replace `/path/to/photos` with your existing archive. Keep `/config` between runs: it contains kei's settings, saved login and state.

The container runs as root unless both `PUID` and `PGID` are set. For NAS or shared storage, set them to the host user who should own newly written files. When both are set, startup changes ownership under `/config` and normally under `/photos`. The example uses `KEI_CHOWN_PHOTOS: "0"` to leave existing photo ownership alone. Prepare permissions so the chosen user can read and write the archive. Even an import preview runs the container's startup steps.

From the Compose directory, stop any existing kei worker and pull the released image:

```sh
docker compose stop kei
docker compose pull
```

For an interactive login and import preview:

```sh
docker compose run --rm -it kei login --config /config/config.toml
docker compose run --rm -it kei import-existing --config /config/config.toml --dry-run
```

Enter the password and trusted-device verification code when prompted. The saved session remains in `/config`. After checking the preview, run:

```sh
docker compose run --rm -it kei import-existing --config /config/config.toml
docker compose run --rm -it kei sync --config /config/config.toml
```

Add `--strict` to both import commands if you want the extra comparison. Review each result before proceeding to the next command.

Avoid putting `ICLOUD_PASSWORD` in the Compose environment: it is visible through `docker inspect`. Use a Docker secret with `--password-file /run/secrets/icloud_password`, an existing secret manager through `--password-command`, or `kei password set` inside the container where supported. See [Credentials](https://github.com/rhoopr/kei/wiki/Credentials).

Once normal sync works as expected, follow [Run unattended](docker-first-backup.md#run-unattended). The image's default command is `kei service run --config /config/config.toml`; it repeats every 24 hours when no watch interval is set. Keep scheduled icloudpd runs stopped so both tools do not write to the archive.

Older container setups sometimes supplied settings such as `KEI_DOWNLOAD_DIR` only through environment variables. Current kei requires those settings in `/config/config.toml`; use environment variables only where current documentation supports them.

## Known output differences

- Metadata writes can change file size, which can prevent a match with the old download. Keep metadata-write settings unchanged while checking the import. See [Metadata output](metadata.md) before enabling them.
- Kei preserves the extension case Apple reports, which can differ from your Python filenames.
- If Apple provides no original filename, kei uses a cleaned-up title when available, then its record name.
- Embedded XMP and separate XMP files are optional and can differ from Python's output.

## What you don't need to worry about

You don't need to move every photo into a new folder or copy Python's cookies. Configure kei to match your existing archive and let the preview show what it can reuse.

You do need to review unmatched files and keep the backup. Import cannot promise that nothing will be downloaded again, remove duplicates, repair damaged media or prove that your backup includes every iCloud item. Later syncs can use saved progress, but recovery or changes to your selection can require another scan.
