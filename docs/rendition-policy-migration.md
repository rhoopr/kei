# Rendition-policy migration

Selected media now includes all available full-resolution originals, current edits
and RAW/JPEG renditions. Medium and thumbnail previews are not substitutes for
missing originals. Sync and import-existing use the same selection.

Existing config files must explicitly accept this change. Kei refuses an
unversioned file before provider requests or config, state and media writes.
This also applies when the old file omitted `edited` and `alternative`: their
old defaults were false.

## Update an existing config

1. Remove `resolution`, `live_resolution`, `live_photo_mode`, `edited`,
   `alternative`, `raw_policy` and `force_resolution` from `[photos]`.
2. Replace `[filters].media` with `[filters].exclude` as needed. The four
   independent exclusions are `photos`, `videos`, `live-photo-stills` and
   `live-photo-videos`. An empty list includes all four components. Leave at
   least one component included.
3. Add `config_version = 1` at the top of the file, before any section heading.

For example, to retain ordinary photos and both Live Photo components while
excluding ordinary videos:

```toml
config_version = 1

[filters]
exclude = ["videos"]
```

To retain Live Photo stills but exclude their videos, add `live-photo-videos` to
the list. Excluding `photos` affects ordinary photos only; it does not exclude
Live Photo stills. Excluding `videos` likewise leaves Live Photo videos selected.

Keep your existing authentication, destination, library, album and date settings.
Review the result with `kei config show --config /path/to/config.toml`, then run
`kei sync --config /path/to/config.toml --dry-run` before downloading the expanded
selection. New setup-generated files already contain the version marker.

## Existing files

New original files keep their original names. New current edits use an `_edited`
suffix. Recorded managed-primary bindings and interrupted publications retain
their established layouts. Migration does not rename existing media, repair
historical inconsistencies or authorize metadata rewrites.

Import-existing adopts matching local files; missing renditions remain available
for a later sync. Config migration does not convert an old preview-only archive
into a full-resolution backup.
