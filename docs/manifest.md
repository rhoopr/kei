# Manifest export

`kei manifest` exports the local state catalog. It reads the state database
only. It doesn't contact iCloud, download files, rewrite files, or advance sync
tokens.

Use it when you want a local inventory for backup checks, support, or external
scripts:

```bash
kei manifest --format json
kei manifest --format csv
```

The command still needs to know which account's state DB to read. Set
`ICLOUD_USERNAME` or `[auth].username` in your config. It doesn't need a
password or a valid iCloud session.

## JSON

JSON output is a pretty-printed array. Each item is one asset version known to
the local state DB:

```json
[
  {
    "library": "PrimarySync",
    "asset_id": "ASSET_RECORD_NAME",
    "version": "original",
    "filename": "IMG_0001.JPG",
    "local_path": "/photos/2024/01/IMG_0001.JPG",
    "checksum": "icloud-checksum",
    "local_checksum": "local-file-sha256",
    "download_checksum": "downloaded-bytes-sha256",
    "size_bytes": 123456,
    "created_at": "2024-01-02T03:04:05Z",
    "added_at": "2024-01-02T03:05:00Z",
    "downloaded_at": "2024-01-02T03:06:00Z",
    "last_seen_at": "2024-01-02T03:06:30Z",
    "media_type": "photo",
    "status": "downloaded",
    "albums": ["Favorites"]
  }
]
```

This is an illustrative inventory, not a sync-report sample. Capture/addition
dates can include milliseconds. `checksum` is provider MMCS identity, not a
local content hash. `local_checksum` describes local bytes; historical
`download_checksum` is not verified original-source provenance. Manifest output
is not proof of backup completeness. See [maintenance](backup-maintenance.md).

Fields may be `null` when kei doesn't have that value. The schema may grow in
later releases, so consumers should ignore fields they don't recognize.

## CSV

CSV output has the same information with one row per asset version. The
`albums` cell is a JSON array string so album names with commas stay
unambiguous.

Columns:

```text
library,asset_id,version,filename,local_path,checksum,local_checksum,download_checksum,size_bytes,created_at,added_at,downloaded_at,last_seen_at,media_type,status,albums
```
