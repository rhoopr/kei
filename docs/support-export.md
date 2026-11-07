# Support diagnostic snapshot

Run this with the same configuration and data directory as your sync process:

```sh
kei --config ~/.config/kei/config.toml support-export --output kei-support.json
```

For Docker, use the existing container and mounted configuration:

```sh
docker exec kei kei --config /config/config.toml support-export --output /config/kei-support.json
```

Fresh container launches also skip ownership initialization for this command, including when PUID/PGID are set. Prepare a writable output directory owned by that UID:GID yourself; collection does not fix its permissions. Older images may initialize volume ownership on fresh launches, so use the existing-container route above or explicitly bypass their wrapper with `--entrypoint /usr/local/bin/kei` and `--user UID:GID`.

Copy the JSON file from your configuration mount, review it locally, and attach it to your bug report if you choose. Describe the symptoms, when they occurred, and what you expected. Nothing is uploaded automatically. Choose a new filename for each snapshot; existing files are never replaced.

The command works offline. It does not log in, execute password commands, call the provider, run a sync, scan media, calculate checksums, test writes, adopt files, repair state, or migrate a database. Collection writes only the requested output file. No new sync is needed to collect current local state.

Older versions may not have this command. If it is missing, fails, or reports partial evidence, submit your report anyway. Include the version or image tag and describe the unavailable section. Keep the partial file when one was produced. Do not attach your database, raw cycle report, authentication directory, configuration, or logs without reviewing and redacting them separately.

## What the file contains

The version 1 export has a versioned allowlist and these sections:

- Exact available build revision, version, platform, features and detected installation method. A missing Docker digest is explicitly unavailable.
- Current file/environment configuration shape: counts, booleans and fixed modes. Names and paths are excluded.
- Observed local filesystem family, health timestamps and failure counts.
- Bounded read-only account state totals and durable retry, unresolved identity, metadata, publication and reconciliation work counts.
- An explicitly unavailable operation-history section with no records. This build does not retain normal-operation history.

`configuration.values.threads` follows the current file's valid download settings: an explicit count from 1 to 64 wins; otherwise a bandwidth limit selects 1, and no limit selects 10. A missing configuration file uses that default. The value is `null` when the file cannot be read or parsed, or its thread count or bandwidth limit is invalid. Sync-time overrides are not available in this snapshot.

No credentials, cookies, tokens, Apple IDs, provider IDs, album names, private paths, checksums, provider URLs, responses or arbitrary error text are exported.

## Limits and partial evidence

This build does not retain normal sync, service or import operation history. A snapshot describes current local state only. It cannot show pre-incident events or a cross-restart operation window.

Old `*.support.json`, `*.support-lock` and `*.support-tmp` files from earlier builds are left untouched. Export does not read, merge, rename or delete them, and does not create new ones. Do not expect old retained history to appear in this file.

Read inputs are bounded: configuration 1 MiB, health 32 KiB and the immutable state snapshot. State queries inspect at most 10,001 rows per table; the extra row signals that a 10,000-row sample is incomplete. Metadata-capture revision progress retains at most 32 anonymous primary/other scope records. Omitted row counts are unknown. Export size is capped at 2 MiB. Counts refer to their documented operation/account scope.

The collector rejects live nonempty WAL/journal evidence and detects observed main-file changes during collection. It reports an unavailable snapshot instead of reading a stale WAL checkpoint. Stop the sync process cleanly before trying again; a new sync is unnecessary. Unsupported, older, missing, unreadable or mismatched-account state is reported without migration. Immutable SQLite collection creates no WAL, SHM or journal files.

A missing health file or an unavailable state snapshot marks the export partial. Missing operation history alone does not mark current-state collection partial; it is reported separately.

Ordinary logs are not collected here. Attach the relevant service or supervisor log window separately after checking it for private information.

Collection writes only the requested output file. Existing output paths are never replaced, including media, state, configuration and symlinks. The output is created exclusively with private permissions on Unix and is synced before success is reported. Parent directories are not created.
