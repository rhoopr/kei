# Migrating from `icloudpd`

If you're already running [icloud-photos-downloader](https://github.com/icloud-photos-downloader/icloud_photos_downloader) (`icloudpd`), switching to kei takes about five minutes. Your existing photos stay where they are - you don't need to re-download anything.

## Step 1: Import your existing files

Point kei at the same directory you've been downloading to:

```sh
kei import-existing --username you@example.com --directory ~/Photos/iCloud
```

This scans your local files and builds a SQLite state database so kei knows what's already been downloaded. The next `sync` run will only fetch what's new or previously failed.

The import matches files by computing the expected path for each iCloud asset (using `--folder-structure` and `--directory`) and checking if a file exists at that path with a matching size. **If your folder structure or directory doesn't match what Python used, the import will silently count those files as unmatched** - it won't error out or download duplicates. The unmatched count is printed at the end. If most files show as unmatched, double-check that your `--folder-structure` matches your existing layout.

The import is idempotent - running it multiple times is safe. It uses `upsert` operations, so re-importing the same files just updates the existing database entries.

## Step 2: Run your first sync

```sh
kei sync --username you@example.com --directory ~/Photos/iCloud
```

You'll need to authenticate fresh - kei can't reuse Python's `~/.pyicloud` session cookies (different format). After the first 2FA approval, sessions are persisted to `~/.config/kei/` (see `--data-dir`) and reused on subsequent runs.

## CLI flag mapping

Most flags are the same or very close. Here's the full mapping:

### Flags that work identically

| Flag | Notes |
|------|-------|
| `-u, --username` | |
| `-p, --password` | |
| `-d, --directory` | |
| `-a, --album` | Multiple `--album` flags supported, same as Python |
| `-l, --list-albums` | Deprecated; use `kei list albums` |
| `--library` | Default: `PrimarySync`. Use `all` to sync every library at once |
| `--list-libraries` | Deprecated; use `kei list libraries` |
| `--recent` | |
| `--skip-videos` | |
| `--skip-photos` | |
| `--skip-live-photos` | Deprecated; use `--live-photo-mode skip` |
| `--skip-created-before` | ISO date (`2024-01-01`) or relative interval (`20d`) |
| `--skip-created-after` | ISO date (`2024-01-01`) or relative interval (`20d`) |
| `--set-exif-datetime` | |
| `--force-size` | |
| `--keep-unicode-in-filenames` | |
| `--live-photo-mov-filename-policy` | `suffix` or `original` |
| `--align-raw` | `as-is`, `original`, `alternative` |
| `--file-match-policy` | `name-size-dedup-with-suffix` or `name-id7` |
| `--live-photo-size` | `original`, `medium`, `thumb` |
| `--only-print-filenames` | Prints paths that would be downloaded, one per line. Doesn't download or delete. |
| `--no-progress-bar` | |
| `--dry-run` | |
| `--auth-only` | Deprecated; use `kei login` |
| `--domain` | `com` or `cn` |
| `--watch-with-interval` | Seconds between cycles |
| `--log-level` | `debug`, `info`, `warn`, `error` (Python had `debug`, `info`, `error`) |

### Flags that changed

| Python | Rust | What changed |
|--------|------|-------------|
| `--folder-structure "{:%Y/%m/%d}"` | `--folder-structure "%Y/%m/%d"` | Both Python `{:%Y}` and plain `%Y` strftime syntax accepted. You can keep using the Python format. |
| `--size original` | `--size original` | Same values, but Python allows multiple `--size` flags (not yet supported in Rust - [#14](https://github.com/rhoopr/kei/issues/14)) |
| `--cookie-directory ~/.pyicloud` | `--data-dir ~/.config/kei/` | New flag name and default path. `--cookie-directory` still accepted as hidden alias. |
| `--threads-num` (deprecated, always 1) | `--threads-num 10` | Actually works in Rust. Default: 10 parallel downloads. |
| `--notification-script` | `--notification-script` | Same flag name, but kei passes `KEI_EVENT`, `KEI_MESSAGE`, `KEI_ICLOUD_USERNAME` env vars. Python only fired on 2FA expiry; kei also fires on `sync_complete`, `sync_failed`, `session_expired`. |

### Flags not yet implemented

| Python flag | Status | Tracking |
|-------------|--------|----------|
| `--until-found` | Replaced by SQLite state - not needed | - |
| `--auto-delete` | Planned | [#28](https://github.com/rhoopr/kei/issues/28) |
| `--delete-after-download` | Planned | [#29](https://github.com/rhoopr/kei/issues/29) |
| `--keep-icloud-recent-days` | Planned | [#30](https://github.com/rhoopr/kei/issues/30) |
| `--xmp-sidecar` | Planned | [#19](https://github.com/rhoopr/kei/issues/19) |
| `--smtp-*` (all SMTP flags) | Planned | [#31](https://github.com/rhoopr/kei/issues/31) |
| `--use-os-locale` | Not planned | - |
| `--password-provider` | Replaced by `--password-file`, `--password-command`, or `kei password set` | - |
| `--mfa-provider` | Not applicable - uses trusted device with `get-code` + `submit-code` | - |

### New in kei (no Python equivalent)

| Flag / command | What it does |
|----------------|-------------|
| `--config <path>` | TOML config file. [Guide](https://github.com/rhoopr/kei/wiki/Configuration) |
| `--password-file <path>` | Read password from a file. Supports Docker secrets (`/run/secrets/icloud_password`). |
| `--password-command <cmd>` | Obtain password from a shell command (1Password, Vault, pass). |
| `--save-password` | Persist password to OS keyring or encrypted file after successful auth. |
| `password set\|clear\|backend` | Manage stored credentials. `credential` still works as hidden alias. See [Credentials](https://github.com/rhoopr/kei/wiki/Credentials). |
| `--data-dir` | Session, state, and credential storage directory (replaces `--cookie-directory`). |
| `--max-retries` | Retry limit per download (Python hardcoded `MAX_RETRIES = 0`) |
| `--retry-delay` | Base delay for exponential backoff |
| `--temp-suffix` | Suffix for partial downloads (default: `.kei-tmp`) |
| `--no-incremental` | Force full library scan instead of syncToken delta sync. Use when you suspect the incremental state is stale, or to verify that incremental results match a full enumeration. |
| `reset sync-token` | Clear stored sync tokens so the next sync does a full re-enumeration. Flag `--reset-sync-token` on sync still accepted as hidden alias. |
| `--notify-systemd` | systemd sd_notify integration |
| `--pid-file` | PID file for service managers |
| `login get-code` | Trigger Apple to send a 2FA code to trusted devices |
| `login submit-code <code>` | Submit 2FA code non-interactively (for Docker/headless) |
| `status` | Show sync status and database summary |
| `sync --retry-failed` | Reset failed downloads and re-sync |
| `reset state` | Wipe the state database |
| `import-existing` | Import local files into state DB |
| `verify` | Verify downloads exist and optionally check checksums |
| `config show` | Dump resolved config as TOML |
| `config setup` | Interactive config wizard (was top-level `setup`) |
| `--live-photo-mode` | Control live photo handling: `both`, `image-only`, `video-only`, `skip`. Replaces `--skip-live-photos`. |
| `--exclude-album` | Exclude specific albums from sync. Multi-value. |
| `--filename-exclude` | Exclude files by glob pattern (e.g., `*.AAE`, `Screenshot*`). Case-insensitive, multi-value. |
| `{album}` in `--folder-structure` | Organize by album name: `--folder-structure "{album}/%Y/%m"`. |
| `KEI_*` env vars | Every CLI flag has an env var (`KEI_DIRECTORY`, `KEI_DATA_DIR`, `KEI_SIZE`, etc.). Useful for Docker. |

## Docker migration

If you're using a `icloudpd` Docker wrapper (like boredazfcuk's), here are the key differences:

| Python Docker | kei Docker |
|---------------|-------------------|
| Multiple env vars for every setting | `ICLOUD_USERNAME`, `ICLOUD_PASSWORD`, `TZ` + optional `config.toml` |
| Cron-based scheduling | Built-in `--watch-with-interval` (set `interval` in config) |
| Interactive 2FA via console | `docker exec kei kei login get-code` then `docker exec kei kei login submit-code 123456` |
| Various notification mechanisms | `--notification-script` with env vars |

Minimal `docker-compose.yml`:

```yaml
services:
  kei:
    image: ghcr.io/rhoopr/kei:latest
    container_name: kei
    restart: unless-stopped
    stop_grace_period: 30s
    environment:
      - ICLOUD_USERNAME=${ICLOUD_USERNAME}
      - TZ=${TZ:-UTC}
    volumes:
      - ./config:/config
      - /path/to/photos:/photos
```

For password management, use `kei password set` (encrypted store), Docker secrets (`--password-file /run/secrets/icloud_password`), or an external secret manager (`--password-command`). See the [Credentials](https://github.com/rhoopr/kei/wiki/Credentials) wiki page for details. Avoid `ICLOUD_PASSWORD` in environment variables - it's visible in `docker inspect`.

Optional `config/config.toml`:

```toml
[watch]
interval = 3600

[notifications]
script = "/config/notify.sh"
```

If you already have photos downloaded from the Python version, mount the same directory and run the import:

```sh
docker exec kei kei import-existing --directory /photos
```

## Known differences in output

- **EXIF file size** - When `--set-exif-datetime` is used, files written by kei may differ by 29-58 bytes compared to the Python version. This is due to a different EXIF library (`little_exif` vs `piexif`) and doesn't affect image quality or metadata correctness. The photos are visually identical.

## What you don't need to worry about

- **`--until-found`** - The SQLite state database replaces this entirely. kei knows exactly which assets have been downloaded, so it doesn't need to scan backwards looking for familiar files.
- **Re-downloading** - `import-existing` populates the database from your existing files. After that, only new or failed assets are fetched.
- **Full re-scans** - After the first sync, kei uses Apple's CloudKit syncToken to fetch only what's changed. A no-change cycle takes 1-2 API calls. `icloudpd` re-enumerates the entire library every run.
- **Cookie migration** - You can't reuse Python cookies, but a fresh auth takes 30 seconds. The new session persists the same way.
- **Folder structure compatibility** - Both Python-style `{:%Y/%m/%d}` and plain `%Y/%m/%d` format strings are accepted. Your existing folder layout works as-is.
