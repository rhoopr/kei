# Edited media at the ordinary filename

The default naming policy remains `suffix`: adjusted media uses the existing `_edited` name. To keep the selected adjusted version at the ordinary filename:

```toml
[photos]
resolution = "original"
edited = true
edited_naming = "primary"
```

With both renditions, `IMG.HEIC` contains adjusted bytes and `IMG_original.HEIC` contains the original. Each uses its actual provider extension, so an adjusted JPEG can be `IMG.JPG`. Without an adjusted rendition, the original uses the ordinary name. Selection settings still control which original, alternative, preview and Live Photo renditions download.

This option explicitly enables management of paths kei proves it owns. An existing archive can migrate using its recorded ownership and local SHA-256 evidence. Before replacing a current path, kei preserves its exact media and associated sidecar as an independent copy. Further edits retain earlier versions beneath `.kei-history/<family>/<revision>/`; history is never pruned automatically. A confirmed revert restores verified original bytes to the ordinary path and keeps an existing `_original` archive. Unknown files, symlinks, hardlinks, locally changed contents and conflicting ownership prevent a handover.

For an Immich External Library, exclude both `**/*_original.*` and `**/.kei-history/**`. Extra alternative or preview selections remain visible. Trigger an external-library rescan after a successful sync. This implementation does not establish Immich's pairing or deduplication behavior.

With the suffix MOV policy, a complete adjusted Live Photo pair uses `IMG.HEIC` and `IMG_HEVC.MOV`; original members use `IMG_original.HEIC` and `IMG_HEVC_original.MOV`. The original MOV policy uses `IMG.MOV` and `IMG_original.MOV`. Missing adjusted motion does not make original motion part of the adjusted still's visible family. Separate files cannot publish simultaneously on every supported filesystem; an interrupted family remains pending and resumes from its journal.

Removing the setting or changing it to `suffix` requests a reverse transition for managed families. History remains retained. Folder/root changes preserve old copies and create the selected layout at the new destination. Preserve the state database with the media: filenames alone do not prove historical ownership. Import can establish owned receipts but does not relocate files by itself.

Schema 35 retains layout state and history receipts. Older binaries refuse the newer schema. For downgrade, restore a matching cold backup of the database and media or use a supported migration; never force `PRAGMA user_version` backwards.

When an unrelated file occupies the ordinary family, kei chooses a deterministic family hash qualifier. The qualifier precedes the terminal `_original` role, so every archive-original member still matches `*_original.*`. The whole still/motion family uses the same qualifier; a conflicting qualified slot holds the operation rather than selecting an unstable ordinal.

`status` and JSON reports expose the durable number of bound families, independently preserved files, pending operations and held operations. These are inventory counts, separate from each cycle's network download counters. `manifest` adds `preserved_files` provenance only when history exists, including native path encoding, provider and local checksums, source checksum, sidecar checksum, family, operation, generation and phase. CSV adds that JSON column only for an export containing preserved files. A superseded rendition no longer advertises the reused primary as its `local_path`.

On Linux, an owned current-path replacement uses kei's existing recovery journal when atomic exchange is unavailable. Filesystems that also reject exclusive alias retirement hold migration or extension-change work with all contents retained. This condition requires supported exclusive rename semantics to finish; it is not an archive-pruning permission.
