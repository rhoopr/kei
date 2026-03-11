# Migrating from Python icloudpd

If you're already running [icloud-photos-downloader](https://github.com/icloud-photos-downloader/icloud_photos_downloader) (Python icloudpd), switching to icloudpd-rs takes about five minutes. Your existing photos stay where they are - you don't need to re-download anything.

## Step 1: Import your existing files

Point icloudpd-rs at the same directory you've been downloading to:

```sh
icloudpd-rs import-existing --username you@example.com --directory ~/Photos/iCloud
```

This scans your local files and builds a SQLite state database so icloudpd-rs knows what's already been downloaded. The next `sync` run will only fetch what's new or previously failed.

The import matches files by computing the expected path for each iCloud asset (using `--folder-structure` and `--directory`) and checking if a file exists at that path with a matching size. **If your folder structure or directory doesn't match what Python used, the import will silently count those files as unmatched** - it won't error out or download duplicates. The unmatched count is printed at the end. If most files show as unmatched, double-check that your `--folder-structure` matches your existing layout.

The import is idempotent - running it multiple times is safe. It uses `upsert` operations, so re-importing the same files just updates the existing database entries.

## Step 2: Run your first sync

```sh
icloudpd-rs --username you@example.com --directory ~/Photos/iCloud
```

You'll need to authenticate fresh - icloudpd-rs can't reuse Python's `~/.pyicloud` session cookies (different format). After the first 2FA approval, sessions are persisted to `~/.icloudpd-rs/` and reused on subsequent runs.

## CLI flag mapping

Most flags are the same or very close. Here's the full mapping:

### Flags that work identically

| Flag | Notes |
|------|-------|
| `-u, --username` | |
| `-p, --password` | |
| `-d, --directory` | |
| `-a, --album` | Multiple `--album` flags supported, same as Python |
| `-l, --list-albums` | |
| `--library` | Default: `PrimarySync`. Use `all` to sync every library at once |
| `--list-libraries` | |
| `--recent` | |
| `--skip-videos` | |
| `--skip-photos` | |
| `--skip-live-photos` | |
| `--skip-created-before` | ISO date (`2024-01-01`) or relative interval (`20d`) |
| `--skip-created-after` | ISO date (`2024-01-01`) or relative interval (`20d`) |
| `--set-exif-datetime` | |
| `--force-size` | |
| `--keep-unicode-in-filenames` | |
| `--live-photo-mov-filename-policy` | `suffix` or `original` |
| `--align-raw` | `as-is`, `original`, `alternative` |
| `--file-match-policy` | `name-size-dedup-with-suffix` or `name-id7` |
| `--live-photo-size` | `original`, `medium`, `thumb` |
| `--no-progress-bar` | |
| `--dry-run` | |
| `--auth-only` | |
| `--domain` | `com` or `cn` |
| `--watch-with-interval` | Seconds between cycles |
| `--log-level` | `debug`, `info`, `warn`, `error` (Python had `debug`, `info`, `error`) |

### Flags that changed

| Python | Rust | What changed |
|--------|------|-------------|
| `--folder-structure "{:%Y/%m/%d}"` | `--folder-structure "%Y/%m/%d"` | Both Python `{:%Y}` and plain `%Y` strftime syntax accepted. You can keep using the Python format. |
| `--size original` | `--size original` | Same values, but Python allows multiple `--size` flags (not yet supported in Rust - [#14](https://github.com/rhoopr/icloudpd-rs/issues/14)) |
| `--cookie-directory ~/.pyicloud` | `--cookie-directory ~/.icloudpd-rs` | Different default path and cookie format (JSON vs LWPCookieJar). Sessions aren't portable between the two. |
| `--threads-num` (deprecated, always 1) | `--threads-num 10` | Actually works in Rust. Default: 10 parallel downloads. |
| `--notification-script` | `--notification-script` | Same flag name, but Rust version passes `ICLOUDPD_EVENT`, `ICLOUDPD_MESSAGE`, `ICLOUDPD_USERNAME` env vars. Python only fired on 2FA expiry; Rust also fires on `sync_complete`, `sync_failed`, `session_expired`. |

### Flags not yet implemented

| Python flag | Status | Tracking |
|-------------|--------|----------|
| `--until-found` | Replaced by SQLite state - not needed | - |
| `--auto-delete` | **Coming in v0.4** | [#28](https://github.com/rhoopr/icloudpd-rs/issues/28) |
| `--delete-after-download` | **Coming in v0.4** | [#29](https://github.com/rhoopr/icloudpd-rs/issues/29) |
| `--keep-icloud-recent-days` | Planned | [#30](https://github.com/rhoopr/icloudpd-rs/issues/30) |
| `--xmp-sidecar` | Planned | [#19](https://github.com/rhoopr/icloudpd-rs/issues/19) |
| `--smtp-*` (all SMTP flags) | Planned | [#31](https://github.com/rhoopr/icloudpd-rs/issues/31) |
| `--only-print-filenames` | Planned | [#17](https://github.com/rhoopr/icloudpd-rs/issues/17) |
| `--use-os-locale` | Not planned | - |
| `--password-provider` | Not applicable - uses `ICLOUD_PASSWORD` env var or interactive prompt | - |
| `--mfa-provider` | Not applicable - uses trusted device or `submit-code` subcommand | - |

### New in icloudpd-rs (no Python equivalent)

| Flag / command | What it does |
|----------------|-------------|
| `--config <path>` | TOML config file. [Guide](https://github.com/rhoopr/icloudpd-rs/wiki/Configuration) |
| `--max-retries` | Retry limit per download (Python hardcoded `MAX_RETRIES = 0`) |
| `--retry-delay` | Base delay for exponential backoff |
| `--temp-suffix` | Suffix for partial downloads (default: `.icloudpd-tmp`) |
| `--no-incremental` | Force full library scan instead of syncToken delta sync. Use when you suspect the incremental state is stale, or to verify that incremental results match a full enumeration. |
| `--reset-sync-token` | Clear stored sync tokens before syncing. Unlike `--no-incremental`, this also stores the fresh token from the full scan, so the next run resumes incremental from that point. Use after recovering from a bad state or after a long gap between syncs. |
| `--notify-systemd` | systemd sd_notify integration |
| `--pid-file` | PID file for service managers |
| `submit-code <code>` | Submit 2FA code non-interactively (for Docker/headless) |
| `status` | Show sync status and database summary |
| `retry-failed` | Reset failed downloads and re-sync |
| `reset-state` | Wipe the state database |
| `import-existing` | Import local files into state DB |
| `verify` | Verify downloads exist and optionally check checksums |

## Docker migration

If you're using a Python icloudpd Docker wrapper (like boredazfcuk's), here are the key differences:

| Python Docker | icloudpd-rs Docker |
|---------------|-------------------|
| Multiple env vars for every setting | `ICLOUD_USERNAME`, `ICLOUD_PASSWORD`, `TZ` + optional `config.toml` |
| Cron-based scheduling | Built-in `--watch-with-interval` (set `interval` in config) |
| Interactive 2FA via console | `docker exec icloudpd-rs icloudpd-rs submit-code 123456` |
| Various notification mechanisms | `--notification-script` with env vars |

Minimal `docker-compose.yml`:

```yaml
services:
  icloudpd-rs:
    image: ghcr.io/rhoopr/icloudpd-rs:latest
    container_name: icloudpd-rs
    restart: unless-stopped
    environment:
      - ICLOUD_USERNAME=${ICLOUD_USERNAME}
      - ICLOUD_PASSWORD=${ICLOUD_PASSWORD}
      - TZ=${TZ:-UTC}
    volumes:
      - ./config:/config
      - /path/to/photos:/photos
```

Optional `config/config.toml`:

```toml
[watch]
interval = 3600

[notifications]
script = "/config/notify.sh"
```

If you already have photos downloaded from the Python version, mount the same directory and run the import:

```sh
docker exec icloudpd-rs icloudpd-rs import-existing --directory /photos
```

## Known differences in output

- **EXIF file size** - When `--set-exif-datetime` is used, files written by icloudpd-rs may differ by 29-58 bytes compared to the Python version. This is due to a different EXIF library (`little_exif` vs `piexif`) and doesn't affect image quality or metadata correctness. The photos are visually identical.

## What you don't need to worry about

- **`--until-found`** - The SQLite state database replaces this entirely. icloudpd-rs knows exactly which assets have been downloaded, so it doesn't need to scan backwards looking for familiar files.
- **Re-downloading** - `import-existing` populates the database from your existing files. After that, only new or failed assets are fetched.
- **Full re-scans** - After the first sync, icloudpd-rs uses Apple's CloudKit syncToken to fetch only what's changed. A no-change cycle takes 1-2 API calls. Python icloudpd re-enumerates the entire library every run.
- **Cookie migration** - You can't reuse Python cookies, but a fresh auth takes 30 seconds. The new session persists the same way.
- **Folder structure compatibility** - Both Python-style `{:%Y/%m/%d}` and plain `%Y/%m/%d` format strings are accepted. Your existing folder layout works as-is.
