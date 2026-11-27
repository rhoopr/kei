# Manifest export

`kei manifest` exports the local state catalog. It reads the state database
only. It doesn't contact iCloud, download files, rewrite files, or change the
sync position.

Use it when you want a local inventory for backup checks, support, or external
scripts:

```bash
kei manifest --format json
kei manifest --format csv
```

The command still needs to know which account's state DB to read. Set
`ICLOUD_USERNAME` or `[auth].username` in your config. It doesn't need a
password or a valid iCloud session. The database must have a matching account
owner; [legacy state needs explicit adoption](account-state-migration.md). Configure
the same realm used when binding the database. Opening state written by an
earlier release upgrades it first; see the [upgrade checklist](upgrade.md).

## JSON

JSON output is a pretty-printed array. Each item is one local copy: one
rendition of one photo in one album folder, or its unfiled copy.

```json
[
  {
    "library": "PrimarySync",
    "asset_id": "ASSET_RECORD_NAME",
    "version": "original",
    "local_path": "2024/01/02/IMG_0001.JPG",
    "album": "",
    "size_bytes": 123456,
    "local_checksum": "local-file-sha256",
    "status": "done",
    "selected": true
  }
]
```

- `local_path` is relative to the download directory.
- `album` is the album's record name, `smart:<name>` for a smart folder, or
  empty for the unfiled copy.
- `local_checksum` is the SHA-256 kei recorded for the file's bytes, or `null`
  before the copy is saved.
- `status` is `pending` (owed), `adopt` (a file at the path waits to be
  checked), `done`, `failed` (retried later), `modified` (changed locally and
  left alone) or `carried_over` (recorded by the previous release and not yet
  taken over by a sync). A carried-over row's `asset_id` is the identity the
  previous release recorded: the photo's asset record name, or for older rows
  its master record name.
- `selected` is false for a copy the current selection no longer wants. Its
  file is kept.

Manifest output describes what kei recorded. It is not proof of backup
completeness; run `kei verify --checksums` to check the files. See
[maintenance](backup-maintenance.md).

Fields may be `null` when kei doesn't have that value. The schema may grow in
later releases, so consumers should ignore fields they don't recognize.

## CSV

CSV output has the same information with one row per copy. A `null` checksum
is an empty cell.

Columns:

```text
library,asset_id,version,local_path,album,size_bytes,local_checksum,status,selected
```
