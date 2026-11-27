# Backup maintenance

Keep an independent backup before local repairs. Do not delete conflicting files, temporary siblings or the state database to make a warning go away. Sync never deletes files, and it replaces a file only while the file still holds the bytes kei recorded for it.

## Choose the operation

| Need | Operation | What can change |
| --- | --- | --- |
| See saved, owed, failed and locally modified copies | `kei status` | Nothing |
| Check every copy's bytes against its recorded SHA-256 | `kei verify --checksums` | Nothing; differences are reported, not repaired |
| List saved copies whose file is missing | `kei reconcile --dry-run` | Nothing |
| Download missing copies again | `kei reconcile`, then `kei sync` | State only; the next sync downloads them |
| Inspect one changed copy | `kei reconcile --inspect-path /absolute/path` | Nothing; prints its fingerprint |
| Replace that copy | Repeat inspection with `--confirm-damage SHA256:SIZE`, then `kei sync` | That one file, if it still has the inspected bytes |
| Rewrite metadata for every saved copy | `kei sync --refresh-metadata` | Configured metadata outputs |
| Replace embedded capture timestamps | `kei sync --refresh-metadata --repair-capture-timestamps` | Embedded timestamps, including camera values |
| Move the backup | Change `[download].directory`, then `kei sync` | Missing copies are downloaded into the new folder |

Opening state with a newer binary can upgrade it, even for `kei status`. Follow the [upgrade checklist](upgrade.md) first.

## Refresh catalogue and configured outputs

Use a separate repair configuration that keeps your existing `data_dir`, credentials and download directory. Remove the `albums` and `smart_folders` keys entirely, even when they contain `all` or `none`, and remove date bounds: the refresh requires a complete sweep of the selected libraries. This selection covers all visible libraries and enables the unfiled pass:

```toml
[filters]
libraries = ["all"]
unfiled = true
exclude = []
```

Then run:

```sh
kei sync --config /path/to/repair.toml --refresh-metadata
```

The refresh forgets what the metadata pass last wrote, so every saved copy is checked against the catalog and rewritten where its configured outputs differ. It does not download media again. Only copies that still hold the bytes kei recorded are written; locally modified copies are left alone and counted. Embedded writes change media bytes and record the new checksum; disable embedded outputs if media bytes must stay unchanged. See [Metadata output](metadata.md).

The refresh runs once even when the configuration sets a watch interval. It rejects `--dry-run`, `--only-print-filenames`, a narrowed selection and `service run`.

### Replace capture timestamps

Enable `metadata.set_exif_datetime = true` in the repair configuration, then authorize replacement explicitly:

```sh
kei sync --config /path/to/repair.toml --refresh-metadata --repair-capture-timestamps
```

This can overwrite camera-supplied timestamps with Apple's capture-local time and its offset. Ordinary metadata writes keep existing timestamps.

## Repair a confirmed damaged copy

Inspect before changing state:

```sh
kei reconcile --inspect-path /photos/Album/photo.jpg
kei reconcile --inspect-path /photos/Album/photo.jpg --confirm-damage SHA256:SIZE
kei sync
```

Replace `SHA256:SIZE` with the fingerprint the first command printed. Confirm only after establishing that this copy is damaged: a changed checksum or a shorter file may be an intentional edit. Confirmation covers that one copy and those bytes. If the file changes again before the next sync, the replacement is refused; inspect it again and confirm the new fingerprint. A copy reported as locally modified can be handed back to kei the same way.

The next sync downloads the current version from iCloud and publishes it over the confirmed bytes. If the download fails, the damaged file stays and the copy stays owed.

`kei verify --checksums` hashes every copy and reports differences without treating them as damage. Ordinary sync hashes a copy only when it needs the copy's bytes, for a new version or a metadata write; a copy that changed is then kept and reported as locally modified. `--repair-truncated` is retired because file size cannot tell damage from a local edit.

## Missing files

`kei reconcile` lists saved, selected copies whose file is missing and owes them again; the next sync downloads them to the same paths. `--dry-run` only lists them. Copies carried over from an earlier release that nothing has taken over yet are listed too; the next sync downloads those as new copies.

## Move the backup or change its layout

When `[download].directory` changes, the next sync looks for each saved copy in the new folder and downloads again the ones it cannot find. Move the files first if you want to keep them; kei does not move or delete anything in the old folder.

Changing `folder_structure`, naming or `edited_as_primary` does not rename existing files. A copy keeps the path it was given, and new copies use the new settings. Copies that stop being selected keep their files.

## Unfinished work and health

A failed download stays owed. It is retried on each of the next three cycles, then after an hour, doubling to at most once a day; a new version from iCloud resets the wait. `kei status --failed` lists them with their last error. Failed downloads make `/healthz` report a problem only when a cycle saved nothing else.

While iCloud reports that a library is still indexing, it is listed and downloaded anyway, but queried smart folders such as Screenshots wait and the library is listed once more from the start after indexing stops. Until that listing finishes, the cycle is reported incomplete. A library whose indexing state iCloud reports as failed or missing is logged with a warning and otherwise synced normally.

When reporting a problem, include `kei --version`, the Docker image ID if you use one, `kei status`, health output and the warnings from one cycle. Redact account details and local paths. Do not upload the state database, session files or raw provider responses.

## Retired maintenance

The v0.30 sync engine does not keep the earlier engine's provider inbox, checkpoints, selection generations, legacy coverage certificates or preservation evidence. These no longer apply:

- `kei reconcile trust-legacy-files plan` and `apply` refuse and point to `kei sync`. Existing trust records were not carried over; see [trusted legacy files](trusted-legacy-files.md) for the earlier behaviour.
- `kei doctor --missing-reference-inventory` refuses.
- `[download].legacy_preservation_allow_hardlinks` is still accepted and has no effect.
- The `unattributed_legacy_assets` and `unattributed_legacy_pending` fields stay in health and report output for compatibility and are always zero. `asset_delta_hydration_incomplete` holds and sparse-identity retry schedules no longer exist; a photo whose details iCloud has not sent yet stays owed until it does.
- `--retry-failed` is rejected; ordinary sync retries unfinished work.
