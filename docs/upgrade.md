# Upgrade an existing kei backup

These steps apply to v0.30, which replaces the sync engine. If you are installing v0.26.0, use the [guide at the v0.26.0 tag](https://github.com/rhoopr/kei/blob/v0.26.0/docs/v0.26.0-upgrade.md). For another older release, read the documentation at its tag.

## Before the first run

1. Stop every writer, including watch processes, services and containers, before running any command against the backup, even `kei status`. Back up the configuration, the whole data directory (including SQLite `-wal`, `-shm` and `-journal` files) and the media tree together. Verify the copy and keep the previous binary or image.
2. Work on a copy of your config. Follow the [rendition-policy migration](rendition-policy-migration.md) before adding `config_version = 1`. Review the edits, RAW/JPEG renditions and Live Photo components that your first sync can download. If you used `edited_naming = "primary"`, replace it with `edited_as_primary = true`; see [edited filenames](edited-primary.md).
3. Check service arguments as well as TOML. Remove `--recent`, `--recent-scope`, `[filters].recent`, `[filters].recent_scope` and `[filters].filename_exclude`. If you want a sample, set inclusive `skip_created_before` and `skip_created_after` dates such as `2025-01-01` and `2025-01-07`, chosen from your own library. Removing a count limit without replacing it can select much more media.
4. If your database predates account-bound state (v0.24.x or earlier), follow [account-state adoption](account-state-migration.md) first. Verify ownership independently and sign in with the same account and region.
5. Review `kei config show --config /path/to/config.toml`, then run `kei sync --config /path/to/config.toml`. Check the selected libraries, albums, date range and destination in the first cycle's log. Keep the cold backup until you have checked the result.

## What the first sync does

v0.30 keeps its state in a new file, `<account>.sync.db`, next to the old `<account>.db`. The first command that opens state builds the new file from the old one:

- The old file is opened read-only and its account is checked against the signed-in account. A mismatch refuses, and nothing is written.
- Each local copy the old state recorded as verified is carried over: its path, size, recorded SHA-256 and iCloud checksum. The previous release recorded absolute paths, so copies outside the configured download directory are dropped; keep `[download].directory` the same for the upgrade. When the old state gave one path to more than one photo, kei hashes the file and keeps only the record that matches it. Rows it cannot read are skipped and counted in the log.
- Nothing else is carried over: no provider inbox, catalog, selection generations, journals, legacy trust records or `.kei-history` versions.
- The new file is checked for integrity and renamed into place. The old file is left where it was.

The first cycle then lists every library from the start. When it plans a copy, it takes over the carried-over copy of the same photo and rendition without downloading it again. Listing a large library takes a while before downloads begin; each page is saved, so an interrupted listing resumes where it stopped.

### Files under an older name

Earlier releases sometimes saved a photo under a shorter name than the one current naming produces, for example `IMG_0001.HEIC` where kei would now choose `IMG_0001-1234567.HEIC` (see [#1119](https://github.com/rhoopr/kei/issues/1119)). If the old state recorded that file for the same photo and rendition in the same folder, v0.30 takes it over under the name it already has. It does not download a second copy under the new name. New photos use current naming. A file in another folder, or one the old state did not record, is not taken over; kei downloads the copy to its planned path and leaves the other file alone.

### Original bytes

Some metadata writers need to know that a file still holds the bytes kei downloaded: filling a missing orientation, and the GPS accuracy field in sidecars. A carried-over copy keeps that proof only when the old state recorded the downloaded bytes' checksum (`source_checksum`) and it matches the copy's recorded checksum. Copies from older rows, copies whose bytes an earlier metadata write changed, and files taken over without a download start without it. Those writers skip such copies, and every other metadata field is written as usual. A copy gains the proof again when kei next downloads it.

### Cleaning up the old state

After a cycle in which every library finished its listing without an error, kei deletes the old `<account>.db`, its `-wal`, `-shm` and `-journal` files, and the `.kei-schema-backup-*` bundles that name it, and logs the space freed. A library that iCloud reports as still indexing delays this until the listing owed after indexing has finished.

Until then you can roll back: stop kei, delete `<account>.sync.db` and its `-wal` and `-shm` files, and start the previous binary. Roll back in place: the previous release recorded absolute paths, so it expects the photos where they were. After cleanup, roll back by restoring the backup from step 1 to the same locations.

## What changes in daily use

- Sync never deletes files. A photo removed from iCloud, or no longer selected, keeps its local copy.
- A copy is replaced, for a new version from iCloud or a metadata write, only while it still holds the bytes kei recorded. A file you changed is kept and counted as locally modified in `kei status`.
- Failed downloads are retried on each of the next three cycles, then after an hour, doubling to at most once a day. Failed downloads alone do not make `/healthz` unhealthy while other downloads succeed.
- Metadata writing is a separate pass after downloads. It is off unless a writer is enabled, and it never holds back the sync position.
- Changing the download folder downloads again the copies that are missing from the new folder. Changing the folder layout or naming applies to copies planned after the change; existing copies keep their paths.
- `import-existing --strict` is refused. Plain `import-existing` adopts files at their planned paths with the expected size.
- Long steps log progress at most every 30 seconds.

See [the sync core](sync-core.md) for the full design and [Backup maintenance](backup-maintenance.md) for checking and repairing a backup.
