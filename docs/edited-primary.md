# Choose which rendition gets the original filename

Set `edited_as_primary = true` under `[photos]` to give the edit the original filename and use `_original` for the original. With `false` (the default), the original keeps its name and the edit uses `_edited`. Without an edit, the original keeps its name in both modes. When upgrading from `edited_naming = "primary"`, replace it with `edited_as_primary = true`.

With `edited_as_primary = true` and both renditions selected, `IMG.HEIC` contains adjusted bytes and `IMG_original.HEIC` contains the original. Each uses its actual provider extension, so an adjusted JPEG can be `IMG.JPG`. Selection settings still control which renditions download.

## When a photo is edited or reverted

The plain filename belongs to whichever rendition is current. kei never moves or renames a file; it changes what the plain filename holds:

- **Edited after kei saved it.** `IMG.HEIC` is replaced with the edit, and the original is downloaded as `IMG_original.HEIC`.
- **Edited again.** `IMG.HEIC` is replaced with the new edit. `IMG_original.HEIC` stays as it is.
- **Reverted.** `IMG.HEIC` is replaced with the original. `IMG_original.HEIC` stays on disk, because sync never deletes; if the photo is edited again, kei uses it again instead of downloading the original a second time.

`IMG.HEIC` is replaced only while it still holds the bytes kei recorded for it. If you changed the file locally, it is kept, reported as locally modified, and no longer follows edits. Earlier edits are not kept: an edit replaces the previous one, as in iCloud.

A Live Photo's motion file follows the same rule as its still. With the suffix MOV policy, a complete adjusted pair uses `IMG.HEIC` and `IMG_HEVC.MOV`; original members use `IMG_original.HEIC` and `IMG_HEVC_original.MOV`. The original MOV policy uses `IMG.MOV` and `IMG_original.MOV`.

When an unrelated file already occupies a name kei wants, kei picks another name for the new copy as it does for any collision, and the copy keeps that name.

## Library scanners such as Immich

For an Immich External Library, exclude `**/*_original.*`. When the plain filename takes an edit, the file gets the time Apple recorded for the edit as its modification time; a revert gets the time of the replacement, so a library rescan after a sync notices both. First downloads keep the capture time. This does not establish Immich's pairing or deduplication behavior.

## Switching the setting and upgrading

Changing `edited_as_primary` does not rename existing files. A photo saved without an edit follows the new setting from its next edit. A photo that already had both renditions saved keeps both names, and later edits and reverts update the file that held the current rendition.

Upgrading from a previous release carries over the copies it recorded, including an existing `IMG.HEIC` and `IMG_original.HEIC` layout. Earlier releases kept replaced edits under `.kei-history/`. This release neither writes nor reads that folder; you can keep it, or delete it once you no longer need those versions.
