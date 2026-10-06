# Migrating from `icloudpd`

> Last updated on 5-26-26 for v0.20.0.

This guide is for moving an existing
[`icloud-photos-downloader`](https://github.com/icloud-photos-downloader/icloud_photos_downloader)
(`icloudpd`) library to kei without downloading everything again. It matches
kei v0.20.

The short version:

1. Put the existing photo directory in `[download].directory`.
2. Put the path-shaping options Python used in TOML.
3. Run `kei import-existing`, then `kei sync` from then on.

kei can't reuse Python's `~/.pyicloud` cookies, so the first run still needs a
fresh Apple login and 2FA approval. Your local photo files can stay in place.

For containers, use the Docker `:latest` image or pin `:0.20.0`.

## If you're coming from old `icloudpd-rs`

Current kei releases don't auto-copy files from the old `icloudpd-rs` paths at
startup. If you still have data there, copy only what you need before the first
run:

```sh
mkdir -p ~/.config/kei/cookies
cp ~/.config/icloudpd-rs/config.toml ~/.config/kei/config.toml
cp ~/.icloudpd-rs/* ~/.config/kei/cookies/
```

Skip any command whose source path doesn't exist. The old files can stay in
place until you're sure kei is working.

## Step 1: import the existing tree

Point kei at the directory that `icloudpd` has been writing to:

```toml
[download]
directory = "~/Photos/iCloud"
```

Then run the import with that config:

```sh
ICLOUD_USERNAME=you@example.com kei import-existing --config ~/.config/kei/config.toml
```

`import-existing` signs in, enumerates the selected iCloud libraries, computes
the paths kei would use for each asset, and adopts any local file with the
right path and size into the SQLite state database. The next `kei sync` skips
those adopted files and downloads only new or previously failed assets.

Import completes enumeration of every selected library and album pass before
writing adoption rows. An interrupted or failed listing leaves those rows
unwritten. The prepared candidate list uses memory proportional to selected
renditions and cached directory entries.

The `name-id7` suffix uses the same selected child identity as sync, including
its guarded legacy compatibility behavior. When two children share a short
suffix, import can use a unique full child-ID collision filename. A shared
short-name file needs an existing current state record whose local hash verifies
that exact file; otherwise import reports the rendition as unmatched and logs
an ambiguity warning. Multiple same-size ordinal siblings and colliding
sanitized child IDs also remain unmatched without verified durable ownership.
Import does not rename or overwrite these files.


Run a dry import first when you're not sure the config matches the old tree:

```sh
ICLOUD_USERNAME=you@example.com kei import-existing --config ~/.config/kei/config.toml --dry-run
```

If most files show as unmatched, stop and fix the path options before running a
real import. A mismatch doesn't error, and it doesn't rewrite your files. It
just leaves those assets unknown to kei, which means a later sync may download
another copy. The one cause no path option fixes is a
[date folder](#date-folders) difference, where Python filed an asset under the
timezone of the machine it ran on and kei expects the timezone the photo was
taken in.

The import is safe to repeat. Re-importing the same files updates the existing
state rows.

### Strict adoption check

By default, import matches an existing file by expected path and size, then
stores kei's own SHA-256 for future verification. If you want a safer first
adoption pass, enable strict mode:

```toml
[import]
strict = true
```

or use it for one run:

```sh
kei import-existing --config ~/.config/kei/config.toml --strict
```

Strict mode fetches a small prefix from iCloud and compares it with the local
file before adopting a same-name, same-size match. Mismatches show up as
`Strict refusals` in the import summary. `--dry-run --strict` runs the same
check without writing state rows.

## Match Python's path layout

Most failed migrations come from one of these differences.

### Folder structure

Python's default `--folder-structure "{:%Y/%m/%d}"` maps to plain strftime in TOML:

```toml
[download]
directory = "~/Photos/iCloud"
folder_structure = "%Y/%m/%d"
```

kei splits folder templates by pass type:

| TOML field | Used for | Default |
|---|---|---|
| `[download].folder_structure` | Unfiled photos | `%Y/%m/%d` |
| `[download].folder_structure_albums` | User album passes | `{album}` |
| `[download].folder_structure_smart_folders` | Smart folder passes | `{smart-folder}` |

If your Python tree included album names, put the album template in TOML:

```toml
[download]
directory = "~/Photos/iCloud"
folder_structure_albums = "{album}/%Y/%m/%d"
```

On v0.20 and later, `{album}` is only accepted in `folder_structure_albums`.

### Date folders

Python renders date folders in the timezone of the machine running it. kei
renders them in the timezone the photo was taken in, from the per-asset offset
Apple stores alongside each asset. kei therefore places an asset in the same
folder whatever host it runs on, while Python's layout depends on how the
machine that built the tree was configured. The two agree only where that
machine was set to the capture timezone, so a photo taken while travelling, or
a tree Python built on a UTC server or container, can sort into a different
date folder under kei. Assets for which Apple supplies no usable offset keep
host-local rendering and land where Python put them.

`import-existing` adopts a file only at the path kei derives, so an asset whose
folders differ stays unadopted. kei does not move media that already sits in a
Python-created folder. A full sync downloads an affected asset into its
capture-local folder and leaves the earlier copy in place, so expect both
copies until you remove the older one.

Sidecar timestamps are the exception. Python's `--xmp-sidecar` builds its
timestamps from the same per-asset offset, so both tools record the same
capture time and offset even where the folder dates differ.

### Filename policy

If Python was run with `--file-match-policy name-id7`, put the same policy in TOML:

```toml
[photos]
file_match_policy = "name-id7"
```

kei also matches Python's size-suffixed duplicate fallback, invalid filename
replacement, RAW naming, live-photo MOV companions, and Unicode stripping by
default. If you used `--keep-unicode-in-filenames`, set `[photos].keep_unicode_in_filenames = true` before import.

### Live photos, RAW, and sizes

Use the same media-shaping settings you used with Python:

```toml
[photos]
resolution = "original"
live_photo_mode = "both"
live_resolution = "original"
live_photo_mov_filename_policy = "suffix"
raw_policy = "as-is"
```

`import-existing --recent 100` is supported. The days form, such as
`--recent 30d`, is sync-only and import will reject it.

### Libraries and albums

The runtime default is the primary iCloud Photos library. The setup wizard
defaults to all visible libraries and writes a `{library}` segment for unfiled
paths when that avoids mixing primary and shared files.

If you want shared libraries too, import with the same library selector that
sync will read from TOML:

```sh
kei import-existing --library all --config ~/.config/kei/config.toml
```

```toml
[filters]
libraries = ["all"]
```

Then run sync with that config:

```sh
kei sync --config ~/.config/kei/config.toml
```

For multi-library trees, add `{library}` to the active templates if you want
zone-separated directories:

```toml
[download]
directory = "~/Photos/iCloud"
folder_structure = "{library}/%Y/%m/%d"
folder_structure_albums = "{library}/{album}/%Y/%m/%d"
```

`import-existing` doesn't have `--album`, `--smart-folder`, or `--unfiled` CLI
flags. It reads those selectors from `config.toml`, using the same defaults as
sync: all user albums, no smart folders, and an unfiled pass. If you use a
config file for sync selection, pass that same `--config` during import.

Explicit album and smart-folder selectors are collection-scoped in v0.20. When
`[filters].albums` or `[filters].smart_folders` is set, sync and
`import-existing` look for those passes across every visible library. The
`--library` import flag and `[filters].libraries` still scope unfiled photos.

## Step 2: sync

After the import looks good:

```sh
kei sync --config ~/.config/kei/config.toml
```

Put the account and sync directory in TOML:

```toml
[auth]
username = "you@example.com"

[download]
directory = "~/Photos/iCloud"
```

For the full TOML option dictionary with defaults and valid values, see
[`example.config.toml`](../example.config.toml) and the wiki
[Configuration](https://github.com/rhoopr/kei/wiki/Configuration) page.

After the first 2FA approval, kei stores session state under `data_dir`
(default `~/.config/kei`; `KEI_DATA_DIR` can override it for automation). Run
`kei password set` or `kei sync --save-password` if you want kei to store the
password for future runs.

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
| `--folder-structure "{:%Y/%m/%d}"` | `[download].folder_structure = "%Y/%m/%d"` | Python wrapper syntax is still accepted. Use `folder_structure_albums` and `folder_structure_smart_folders` for the other passes. |
| `--size original` | `[photos].resolution = "original"` | Values: `original`, `medium`, `thumb`, `none`. Use `edited = true` for adjusted renditions and `alternative = true` for alternative or RAW renditions. |
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
| `--watch-with-interval` | `[watch].interval`; Docker uses `kei service run` | Built-in watch mode moved to durable config and service commands. |
| `--log-level` | `--log-level` or `log_level` | `debug`, `info`, `warn`, `error`. |
| `--only-print-filenames` | `--only-print-filenames` | Per-run flag that prints paths and doesn't download. |
| `--dry-run` | `--dry-run` | Per-run flag that doesn't modify local state or iCloud. |
| `--notification-script` | `[notifications].script` | kei sends `KEI_EVENT`, `KEI_MESSAGE`, `KEI_ICLOUD_USERNAME`, per-cycle `KEI_*` stats, and `KEI_REPORT_JSON` when `[report].json` is configured. |

## Python flags without a kei equivalent

| Python flag | kei status |
|---|---|
| `--until-found` | Replaced by SQLite state. |
| `--auto-delete` | Planned. |
| `--delete-after-download` | Planned. |
| `--keep-icloud-recent-days` | Planned. |
| `--smtp-*` | Planned. |
| `--use-os-locale` | Not planned. |
| `--password-provider` | Use `--password-file`, `--password-command`, or `kei password set`. |
| `--mfa-provider` | Not applicable. Use trusted-device 2FA with `kei login get-code` and `kei login submit-code`. |

`[metadata].xmp_sidecar` is implemented in kei, along with
`[metadata].embed_xmp`. They write iCloud metadata that Python didn't handle
the same way.

## Useful kei-only options

| Option or command | What it does |
|---|---|
| `kei config setup` | Interactive config wizard. |
| `kei config show` | Print resolved config as TOML. |
| `--config <path>` | Use a TOML config file. |
| `--password-file <path>` | Read the password from a file or Docker secret. |
| `--password-command <cmd>` | Read the password from a shell command. |
| `kei password set` | Store the password in the OS keyring or encrypted file backend. |
| `--save-password` | Save the password after a successful auth. |
| `[filters].libraries` | Select primary, shared, all, named, or excluded iCloud libraries. |
| `[filters].smart_folders` | Sync Favorites, Screenshots, Hidden, Recently Deleted, and other Apple smart folders across visible libraries when explicitly set. |
| `[filters].unfiled = false` | Disable the separate pass for photos not in any user album. |
| `[filters].filename_exclude` | Skip files by glob, such as `*.AAE` or `Screenshot*`. |
| `[download].bandwidth_limit` | Cap total download throughput, for example `10M` or `1.5Mi`. |
| `[metadata].set_exif_rating`, `.set_exif_gps`, `.set_exif_description` | Write more iCloud metadata into downloaded media. |
| `[metadata].embed_xmp`, `[metadata].xmp_sidecar` | Embed XMP or write `.xmp` sidecars. |
| `[download.retry].per_transfer` | Retry downloads inside one transfer. Default is 3. |
| `[download.retry].per_asset` | Cap lifetime attempts per asset. Default is 10; 0 disables the cap. |
| `[download].temp_suffix` | Temp file suffix for partial downloads. Default is `.kei-tmp`. |
| `[report].json` | Write the latest sync report atomically as JSON. |
| `[server].bind`, `[server].port` | Serve `/healthz` and `/metrics` during watch mode. |
| `[watch].reconcile_every_n_cycles` | Periodic read-only reconciliation warning pass during watch mode. |
| `--friendly`, `--no-friendly` | Control the interactive progress UI. |
| `kei status` | Show database and sync status. |
| `kei verify` | Check downloaded files exist, with optional checksums. |
| `kei reconcile` | Mark missing local files as failed so the next sync re-downloads them. |

## Removed kei compatibility aliases

v0.20 removed the remaining v0.13 compatibility aliases:

| Deprecated | Use |
|---|---|
| `--exclude-album`, `KEI_EXCLUDE_ALBUM` | `[filters].albums = ["!NAME"]` |
| `{album}` inside `--folder-structure` | `[download].folder_structure_albums = "{album}/..."` |
| `[filters].album` | `[filters].albums = ["NAME"]` |
| `[filters].exclude_albums` | `[filters].albums = ["!NAME"]` |
| `[filters].library` | `[filters].libraries` |

## Docker migration

Use `ghcr.io/rhoopr/kei:latest` or pin `ghcr.io/rhoopr/kei:0.20.0`. It uses `/config` and
`/photos` volumes. The image runs `kei service run --config /config/config.toml`
by default, with `KEI_DATA_DIR=/config` as container runtime glue. Set
`[download].directory = "/photos"` in the config file. `service run` uses a
24-hour watch fallback when `[watch].interval` is unset.

A minimal compose file:

```yaml
services:
  kei:
    image: ghcr.io/rhoopr/kei:0.20.0
    container_name: kei
    restart: unless-stopped
    stop_grace_period: 30s
    environment:
      - ICLOUD_USERNAME=${ICLOUD_USERNAME}
      - TZ=${TZ:-UTC}
      # Already set by the image. Keep this for lower VSZ in long watch runs.
      # - MALLOC_ARENA_MAX=2
    volumes:
      - ./config:/config
      - /path/to/photos:/photos
```

Set `PUID` and `PGID` when the container should write files as a host user,
which is common on NAS installs.

Avoid `ICLOUD_PASSWORD` in Docker if you can; it shows up in `docker inspect`.
Use a Docker secret with `--password-file /run/secrets/icloud_password`, an
external secret manager via `--password-command`, or run `kei password set`
inside the container if your credential backend supports it.

On glibc systems, long-running multi-threaded processes can reserve large
virtual malloc arenas even when their resident memory is small. The Docker
image sets `MALLOC_ARENA_MAX=2`, and `kei install` writes the same setting into
Linux systemd units. If you run kei from your own service manager and care
about VSZ, set `MALLOC_ARENA_MAX=2` before `kei service run`.

If the container exits with a v0.20 `/config/config.toml` error, it found old
sync env vars such as `KEI_DOWNLOAD_DIR` but no mounted config file. Move those
durable settings into `/config/config.toml` and keep env for secrets or runtime
glue.

For a one-shot import against an existing mounted tree:

```sh
docker exec kei kei import-existing --config /config/config.toml
```

For headless 2FA:

```sh
docker exec --user $(id -u):$(id -g) kei kei login get-code
docker exec --user $(id -u):$(id -g) kei kei login submit-code 123456
```

## Known output differences

- EXIF writes can change file size compared with Python because kei uses a
  different metadata writer. The image/video content is the same.
- kei preserves the source extension case Apple reports. Python often
  lowercased extensions.
- If Apple doesn't send an original filename, kei falls back to a sanitized
  title when available, then the record name.
- `[metadata].embed_xmp` and `[metadata].xmp_sidecar` add metadata that
  Python-created files won't have.

## What you don't need to worry about

- Re-downloading the whole library after import. Adopted rows are trusted until
  relevant download settings change.
- Python cookies. Re-auth once; kei stores its own session state.
- Full scans on every run. After the first sync, kei uses CloudKit sync tokens
  for incremental changes.
- `--until-found`. The state DB is the marker of what has already been
  downloaded.
