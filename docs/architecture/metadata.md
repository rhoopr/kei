# Metadata write boundaries

Metadata writing is opt-in. Without an enabled writer in `[metadata]`, no pass runs and no file is touched. Album names and people refine a writer; they do not enable one. Capture-timestamp replacement needs `--refresh-metadata --repair-capture-timestamps` and `set_exif_datetime`.

## The pass

`src/sync_core/metadata.rs` runs after the download step in each cycle, only when downloads are enabled. It never reads or moves a zone's sync position and never changes whether a copy counts as saved.

A copy is visited when its metadata may be stale: after it is downloaded or adopted, when its photo changes in iCloud (any re-observation replans it), when the metadata settings change, and on `--refresh-metadata`. Each visit:

1. Settles an interrupted replacement at the path, then hashes the file.
2. Accepts a pending receipt: if the file holds the bytes kei recorded just before publishing, those bytes become the recorded copy.
3. Leaves the copy alone and marks it locally modified when the file no longer holds the recorded bytes.
4. Builds the payload from the stored records (`src/download/metadata_pass/`): capture time and offset, rating, location, title, description, keywords, and album titles when enabled. People stay empty: iCloud's person names are not part of the change feed.
5. Skips the copy when the payload hash matches the one last written.
6. Plans the embedded write against what the file already holds (`planning.rs`), records a receipt with the prepared output's SHA-256 and the fields it owns, and publishes.
7. Writes or updates the `.xmp` sidecar when enabled.
8. Records the payload hash and, after an embedded write, the new SHA-256 and owned fields.

A failure leaves only that copy to retry on the next pass.

## Field ownership

`files.meta_fields` lists the embedded fields kei wrote into a copy. A field iCloud explicitly cleared is removed only when kei owns it; camera and third-party values stay. A new download resets ownership, since the new bytes carry none of kei's writes. Copies carried over from a previous release start with no owned fields.

Datetime and GPS are written only when the file has none, except under explicit capture-timestamp repair. Rating and description follow iCloud. Orientation is filled only for the untransformed original, proven by the bytes kei downloaded.

## GPS provenance

Sidecars read the media's own GPS facts on each attempt. Native horizontal accuracy is included only when the file's coordinates equal iCloud's and the file still holds the bytes kei downloaded; an earlier embed could otherwise have supplied those coordinates. A read failure keeps the existing sidecar values and retries.

## HEIF publication and recovery

HEIF-family embedded XMP updates use the byte-preserving item-map writer and
reject layouts that cannot be changed without re-encoding unknown item-graph
data. A file may hold one XMP item per image, so probe and write both resolve
the packet through its `cdsc` association with the primary image and refuse
item maps where several are equally plausible. An unassociated packet answers
for the primary only when it is the sole candidate. The HEIF probe reads both
XMP and the standalone Exif item associated with the primary image before
planning datetime or GPS writes. A new XMP item receives a `cdsc` reference
naming the primary image. When the primary is the first input of one `tmap`
and its sole Exif descriptor already names exactly the primary and that tone
map, and no existing XMP item already describes that tone map, the XMP receives
the same two targets so Apple keeps the HDR rendition. Missing, conflicting,
additional, or ambiguous relationship or ownership evidence fails closed.
External item data references and top-level boxes whose absolute offsets are
not adjusted are also rejected.
Before publication, validation confirms that every construction-method-0 item
other than the resolved XMP packet, and every opaque `meta` sub-box, remains
byte-identical, and that re-reading the rewritten file resolves the packet just
written, allowing for the space padding an in-place replacement leaves in the
reused extent. Every embedded writer exclusively creates a unique sibling and
replaces the source only while the displaced bytes and prepared replacement
still match their approved fingerprints. Atomic exchange is preferred.
The metadata pass passes the recorded fingerprint into the writer, closing the
interval between its own check and the writer's read. Existing regular
files and links at candidate temporary paths remain untouched. Safe pre-exchange
failures remove only the uniquely owned prepared file. Concurrent edits preserve
the source, mark the copy locally modified, and leave any ambiguously displaced
entry at its reported sibling path. The
replacement file retains the source permissions and mtime.

On Linux, filesystems that reject `RENAME_EXCHANGE` use a journaled fallback.
An exclusively created, owner-only sibling directory holds a version-1 JSON
manifest with lossless filename bytes and both SHA-256 fingerprints. Its name
is `.kei-replace-` plus the SHA-256 hash of the destination filename. The
prepared file is hard-linked into that directory and synced before the
original is displaced. Installation and restoration use hard links that refuse
an existing destination. The final path can be absent between displacement and
installation. Directory syncs precede the durable commit marker. Normal
completion removes only byte-verified recovery entries.

Before comparing a copy's bytes, the download step and the metadata pass
recover the journal for that path, and the sidecar writer recovers the sidecar's.
Directory reads require `/proc/self/fd`. Journal links are rejected. Previews
do not recover journals. Recovery restores
uncommitted originals or completes cleanup of committed replacements. Unknown
versions, malformed manifests, changed bytes, and conflicting destinations
retain the journal and fail the operation. Journal file locks prevent local
writers from recovering an active transaction. Terminal cleanup holds the lock
through manifest unlink, then closes all manifest handles before directory
removal. Only `ENOTEMPTY` from removal of a reverified empty, manifest-less
journal is deferred with a warning; publication still verifies the target and
removes only its matching prepared inode and bytes. Unknown entries, changed
bytes or namespaces, other removal errors, and directory fsync failures remain
errors. Empty journal recovery applies the same bounded cleanup. Historical
prepared files are not swept by this path. NFS mounts with `nolock` do not
provide cross-host locking; do not run kei on the same destination from
multiple NFS clients. Recovery does not mark a copy saved by itself.

## Movie preparation

XMP Toolkit preparation checks movie box boundaries with bounded file reads before
updating the temporary copy. Nested QuickTime `meta` boxes without a supported
FullBox prefix are rejected because the toolkit can corrupt their existing child
boxes. The optional four-byte zero terminator is accepted only at the end of a
`udta` child list. Rejection preserves the original media and leaves the copy's
metadata to retry on the next pass.
