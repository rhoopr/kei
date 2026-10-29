# Edited media at the ordinary filename

The default naming policy remains `suffix`: adjusted media uses the existing `_edited` name. To keep the selected adjusted version at the ordinary filename:

```toml
[photos]
resolution = "original"
edited = true
edited_naming = "primary"
```

For new downloads with both renditions and available destinations, `IMG.HEIC` contains adjusted bytes and `IMG_original.HEIC` contains the original. Each uses its actual provider extension, so an adjusted JPEG can be `IMG.JPG`. Without an adjusted rendition, the original uses the ordinary name. Selection settings still control which original, alternative, preview and Live Photo renditions download.

The setting controls names for new downloads. Changing it does not rename, move, replace or backfill files already in the archive. A new download uses the current settings and the normal no-overwrite and collision checks. Explicit metadata refresh remains a separate opt-in operation; it does not request a naming migration.

For an Immich External Library, consider excluding `**/*_original.*` when the original rendition should not appear as a separate item. Existing archives may also contain `.kei-history` from previously managed layouts; retain and exclude that directory where appropriate. Trigger an external-library rescan after a successful sync. This does not establish Immich pairing or deduplication behavior.

With the suffix MOV policy, a newly downloaded adjusted Live Photo pair uses `IMG.HEIC` and `IMG_HEVC.MOV`; original members use `IMG_original.HEIC` and `IMG_HEVC_original.MOV`. The original MOV policy uses `IMG.MOV` and `IMG_original.MOV`. Missing adjusted motion does not make original motion part of the adjusted still's visible family.

Previously recorded layout operations remain durable work. Kei recovers those operations using their recorded paths and proof before ordinary sync; unresolved or unsafe operations stay pending. Keep the state database with the media because filenames alone do not prove ownership. Import establishes receipts but does not relocate files by itself.

Schema 35 retains layout state and history receipts. Older binaries refuse the newer schema. For downgrade, restore a matching cold backup of the database and media or use a supported migration; never force `PRAGMA user_version` backwards.

An occupied destination remains subject to normal collision and no-overwrite checks. Existing files are not removed to make room for the current naming choice.

`status` and JSON reports expose the durable number of bound families, independently preserved files, pending operations and held operations. These are inventory counts, separate from each cycle's network download counters. `manifest` adds `preserved_files` provenance only when history exists, including native path encoding, provider and local checksums, source checksum, sidecar checksum, family, operation, generation and phase. CSV adds that JSON column only for an export containing preserved files. A superseded rendition no longer advertises the reused primary as its `local_path`.

Previously recorded operations can require filesystem support for exclusive publication or retirement. If recovery cannot safely finish, kei retains the operation and its contents for retry.
