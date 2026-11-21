# Metadata write boundaries

For configuration and supported fields, see the [metadata guide](../metadata.md). This reference covers authorization, stable inputs, publication, and durable retries.

## Metadata

Provider metadata may be captured in SQLite without changing local media.
Embedding EXIF/XMP or writing sidecars requires explicit configuration.
The iCloud GPS decoder selects the first valid coordinate pair from
`locationV2Enc`, `locationEnc`, then plain latitude/longitude fields. Coordinates
must be finite and within inclusive latitude [-90, 90] and longitude [-180, 180]
bounds. Nonfinite optional altitude is omitted independently of the pair.
The iCloud adapter retains shared metadata once and compact resource facts for
each rendition, independently of download URL availability. Catalogue dimensions
describe the provider-declared resource dimensions, not probed track or displayed
dimensions. Live Photo motion
duration comes from the companion duration ratio; missing or invalid companion
facts remain unknown rather than inheriting still-image values. Planning,
adoption, and import project metadata for the selected resource. RAW preference
can swap logical original and alternative keys; download policy supplies both
resource candidates for those historical keys. Refresh resolves every rendition
against its current provider checksum inside the state transaction, not against
preloaded row identities or current RAW preference. Missing or conflicting matches
update shared library metadata but leave dimensions and duration unknown. Full
metadata snapshots and hashes are computed only when needed, rather than retained
for every possible rendition.
Refresh updates the downloaded rendition family atomically, checking
each prepared capture-repair receipt against its own rendition's incoming hash.
An individual upsert guards the affected rendition and provider checksum without
invalidating another rendition's prepared receipt.
Failed receipt validation or state writes preserve checkpoint failure evidence.
HEIF probes and embedded preparation share one process-wide blocking admission
lane, independent of download concurrency. This conservative admission bounds
simultaneous retained parser work; it is not needed for shared parser state and
also serializes file I/O. Concurrency may be widened only with measured resource
evidence.
Production keeps a compact view of the `pitm`, `iinf`, `iloc`, and `iref` controls.
Other `meta` children, including image-bearing `idat`, stay as file ranges.
The separate file-backed source-GPS locator also budgets its required control
tables rather than the complete `meta` body.
Headers are scanned without a growing top-level or opaque-box inventory.
Preparation patches changed controls and streams all unchanged ranges with
64 KiB buffers; preservation checks compare original file ranges with fixed
buffers. A large `meta` or `idat` payload alone is not a resource refusal.
The retained-control view, selected XMP packet, complete native Exif item, and
replacement XMP packet each have an 8 MiB materialization allowance. This reuses
the prior writer's 8 MiB whole-`meta` allowance for the data still materialized;
the selected Exif/XMP allowances are new operation restrictions, not format limits
or a claim about all valid assets. Complete native TIFF parsing/alias validation
and the XMP toolkit graph still require materialization. Larger operations return
an explicit resource-unsupported error naming the required bytes and allowance,
preserve the original, and retain truthful metadata retry work. The allowance is
not a total-process RSS bound: C++ graphs, parsed structures, source/output control
views, and fixed stack buffers also consume memory. Existing fixture sizes do not
establish a representative camera/iCloud metadata distribution.
Source fingerprints are checked before and after preparation, and guarded
publication rechecks both source and output. Errors, panics, and dropped prepared
work release admission and the existing temporary-file guard removes owned output.
Native-only builds identify unsupported HEIF before the generic JPEG/TIFF probe
or preparation can read the whole file; approved-input checks still use streaming
fingerprints before preparation reports unsupported.

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
Metadata-only retries also pass the checksum-gate fingerprint into the writer, closing the
interval between catalogue validation and the writer's read. Existing regular
files and links at candidate temporary paths remain untouched. Safe pre-exchange
failures remove only the uniquely owned prepared file. Concurrent edits preserve
the source and its catalogue checksum evidence, keep the durable rewrite marker,
and leave any ambiguously displaced entry at its reported sibling path. The
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

Before download discovery or catalog path reconciliation, Linux walks the
download tree through retained directory handles and recovers these journals.
The configured root may be a symlink alias; recovery resolves that trusted
anchor while refusing links beneath it. Reconciliation also recovers each
recorded source media and sidecar before planning, including old roots after
a directory change. Missing parent namespaces fail verification rather than
being treated as missing journal entries.
Directory reads require `/proc/self/fd`. Journal links are rejected. Dry-run and
filename-only download modes do not recover journals. Queued metadata work also checks its media
and sidecar journals before testing existence or checksums. Recovery restores
uncommitted originals or completes cleanup of committed replacements. Unknown
versions, malformed manifests, changed bytes, and conflicting destinations
retain the journal and fail the operation. Journal file locks prevent local
writers from recovering an active transaction. Terminal cleanup holds the lock
through manifest unlink, then closes all manifest handles before directory
removal. Only `ENOTEMPTY` from removal of a reverified empty, manifest-less
journal is deferred with a warning; publication still verifies the target and
removes only its matching prepared inode and bytes. Unknown entries, changed
bytes or namespaces, other removal errors, and directory fsync failures remain
errors. On NFS a journal entry unlinked while open stays as `.nfsXXXX`. Cleanup
accepts it only when its inode is one the cleanup just unlinked, waits briefly
for the client to retire it, then defers directory removal as for `ENOTEMPTY`.
`ESTALE` from the journal directory is retried a bounded number of times,
re-verifying each pass. Empty journal recovery applies the same bounded
cleanup, and historical prepared files are not swept by this path. NFS mounts
with `nolock` do not
provide cross-host locking; do not run kei on the same destination from
multiple NFS clients. Recovery does not authorize provider checkpoint progress
or downloaded-state finalization by itself.

## Capture revision

`METADATA_CAPTURE_REVISION` identifies the catalogue semantics produced by the
current binary. Schema v18 stores per-asset revisions and per-library active
and pending repair state. A normal sync hydrates stale downloaded rows in
bounded provider-lookup batches, independent of album, media, and date filters.
Only an identity that cannot be resolved from durable asset/master evidence
uses the bounded legacy hydration path. Unselected libraries keep separate
pending state and do not force work in selected libraries. The legacy missing-hash
fallback checks only the selected library. While revision repair has pending
work, it owns that backlog instead of forcing a second full enumeration.
Ambiguous provider children remain pending and block the affected checkpoint;
a matching rendition alone does not prove which child owns the metadata.
The `metadata_capture_ambiguity_counts_v1` diagnostic reports stored rendition,
matching child, and full-evidence matching child counts. It contains no IDs,
paths, checksums, or provider metadata. These counts do not authorize identity
selection, even when only one child matches every stored rendition.

## Unattributed legacy evidence

Schema v28 can preserve an unclaimed legacy master receipt with at least two historical child mappings without assigning it to a child. Preparation requires a complete current inventory with at least one live child; a sole survivor is not inferred to own the historical original. Historical siblings missing from that inventory do not establish ownership or authorize source deletion. Zero-current-child families and saved legacy owners remain excluded. Independently identified current children use the normal download and publication checks, without a legacy coverage certificate. `download/legacy_preservation.rs` owns preparation eligibility and file validation; `state/db/legacy_preservation.rs` owns immutable originals, protected paths and retained historical certificates. The original schema-28 migration created empty tables and protection triggers without classifying existing records. Retiring certification added no schema migration and rewrote no files. The subsequent fresh-start transition uses state schema 38. Older schema binaries still cannot open newer state; this change is not a downgrade guarantee.

Preparation protects the original catalogue rows, capture revisions, retry
receipts, relationship history, media paths and sidecar presence. It preserves
the old checkpoint. A valid original path already claimed by another catalogue
asset defers that family without creating preservation receipts or changing its
ownership, retries or checkpoint. Independent families in the same preparation
scan can still progress; malformed evidence remains an error. Before preparation,
unclaimed masters with multiple historical children already protect every
catalogued original path and its exact `.xmp` sidecar across libraries. Pending
metadata debt does not remove that protection. Ordinary writers, import adoption,
publication proofs, layout handovers and replacement recovery share this path
census; deferred originals retain their rewrite markers. A complete
unfiltered zone inventory must cover hidden children, all pages, an explicit
completion marker and a nonblank final cursor.
Each scan is limited to 10,000 pages and 1,000,000 observed records. It retains
compact latest identity/reference/deletion evidence for unrelated records and
full latest records for candidate masters and children, then hydrates those
families after the final page. Historical child mappings cannot replace complete
family discovery. Later updates and tombstones replace earlier evidence, including
references that enter or leave a candidate family.

The byte guards allow at most 64 MiB for one serialized response page and 256 MiB
for retained record representations, identity storage and pagination cursors.
Cumulative transferred bytes are counted separately and do not consume the
retained-evidence budget. These counts are representation bounds, not process RSS;
one response is decoded before its page-size guard. A cycle considers at most 64
new candidates. Exhausting a limit retains the hold. Sparse, malformed,
cross-library or incomplete evidence cannot establish preservation evidence. No diagnostic
count is ownership proof.

Failed preparation discovery records a versioned, library-scoped
`legacy_preservation_inventory_retry:` receipt in the metadata table. An unchanged
configuration and durable original-family evidence defer another preparation scan
for at most one hour across restart. Retry scheduling, capture cycle counters and
unchanged relation refresh timestamps do not reset that delay. Changed
configuration or original-family evidence permits
a new attempt; malformed, unsupported or out-of-range retry receipts never defer
work. Deferral supplies no provider evidence or checkpoint permission. Preparation and metadata-capture retry scheduling remain independent. Successful discovery clears its failure delay. Prepared records no longer require certification; unprepared discovery and attribution limits remain outside that retirement.

Preparation still requires its complete scan. It establishes protected historical evidence, not current work completion or checkpoint permission. Failure warnings expose only a fixed stage and category plus aggregate page, record, transferred-byte and retained-byte counts. Provider error text, record identities, paths and URLs remain private.

Prepared history is no longer revalidated or activated during normal sync. Missing certificates, changed historical dependencies or changed historical files alone do not force a full inventory, delta bridge, checkpoint hold or cycle failure. Existing `unattributed_legacy_proofs` rows and `active_generation` pointers remain unchanged history; normal sync no longer appends certificates or reactivates their old holds. Independently identified current work still uses its normal provider discovery, finalization, retry and checkpoint owners.

Native admission protects the actual state identity recorded in `unattributed_legacy`. An independently authenticated current child is not blocked solely because its provider master matches an ambiguous historical row. Current mapping and unfinished-generation guards still apply. Protected historical paths do not become adoptable by that child; path ownership checks retain their separate authority.

Current source provenance remains separate from historical certificates. Provider MMCS signatures identify generations and are never compared to local SHA-256. Historical download hashes, local-trust declarations and publication without durable source provenance do not establish verified source bytes. Current transfer, metadata, source-identity and state-write failures retain their existing obligations; retiring certification neither clears them nor enables a metadata writer.

Preparation fingerprints original media and sidecars through confined paths. Symlinks, pending temporary writes, replacement journals, shared paths and changed bytes reject preparation. Multiple hard links also reject it by default. Preparation warnings retain the aggregate `invalid_original_files` candidate count and expose `shared_file_links` as a subset, counting each candidate once at its first rejected media or sidecar. The TOML-only `[download].legacy_preservation_allow_hardlinks` flag defaults to false. True waives only the hard-link-count check for qualifying evidence paths. Actual use emits one redacted aggregate warning per library cycle, deduplicated by evidence path; enabling the flag without linked evidence emits no warning. Other confined-path, byte, receipt and current-work guards stay intact. The separate legacy policy hash retains its existing encoding for retained-checkpoint policy comparisons; certification retirement does not change the ordinary download or enumeration hashes.

The opt-in assumes external tools are coordinated so they do not change linked
contents, metadata, or kei-managed names during the operation. It does not prove
inode independence: distinct original and current paths can share an inode.
It adds no account-wide alias census or read-only enforcement. Existing
identical-file reuse can set timestamps through aliases even when metadata
outputs are disabled. Metadata repair, timestamp repair, pending-file handling
and replacement recovery remain unchanged. General hardlink support and stronger
preservation guarantees remain open under #884.

Shared-root replacement recovery checks protected paths from
all libraries before opening a journal, including unselected libraries. Missing
protected parents retain their path reservation through the nearest existing
ancestor; they do not abort unrelated recovery or clear preservation/checkpoint
holds. Other filesystem errors and existing protected journals still block
recovery. The preservation owner never moves, rewrites, adopts or deletes
original paths. With the hardlink opt-in, that path protection cannot prevent
changes made through another alias by an unchanged write path.
Normal downloads, metadata repair, local path reconciliation, imports and source
tombstones respect the protection before byte writes; SQL guards provide defense
in depth. `reconcile` reports drift in protected originals without converting
them into download retries. Verification and manifest export retain the original
identities, dates, paths and history; download status does not establish ownership.
Unprepared ambiguous originals use the same protection census, including supported
relative paths and their exact sidecars. A connection-local SQLite TEMP cache
indexes confined path keys; relevant identity and path changes invalidate it
transactionally, and external commits, schema changes and working-directory
changes require a fresh census. Ordinary writes and mapping timestamp refreshes
do not rebuild it. This cache does not change durable preservation evidence.
Source metadata recovery distinguishes protected legacy originals from healthy
managed primary-layout sources. Managed sources continue through their layout
owner; pending media or sidecar replacement journals still hold reconciliation.

Current checkpoint transactions retain their source cursor, configuration, sparse-identity and stale-plan fences. Failed enumeration, cancellation, unfinished current work without sufficient durable recovery proof, or a failed state transaction retain the affected checkpoint. Historical certificates are neither an extra prerequisite nor a substitute for these checks. The raw inbox, unprepared attribution, all-generation provider-work debt and expired-cursor recovery retain their existing policies and limits; certification retirement does not resolve every historical-state hold or establish recovery of the reports collected in #1044.

`status`, cycle reports and health JSON retain `unattributed_legacy_assets` and `unattributed_legacy_pending` with their existing names, types and scopes. The first counts preserved records with unknown attribution, not paths or renditions. The second counts records without a stored certificate at their recorded `active_generation`, including absent pointers. This is a presence lookup: it does not validate certificate versions, payloads, dependencies, configuration or files. Despite its historical name, it is not pending sync work. Status is per library, completed cycles total selected libraries, and skipped watch cycles report all-catalogue historical totals. Current capture `remaining`, `unresolved` and `deferred` counts, revision `processed` and `failed` tallies, and cycle metadata-capture observations keep their existing meanings.

Status reports unresolved historical attribution separately because current sync success does not establish historical completeness. Historical counters alone no longer fail one-shot sync, increment cycle or health failures, prevent health's `last_success_at` update, or wake an otherwise idle watch cycle. Current identity and selected-work failures keep their ordinary effects. Primary sparse-identity recovery still requires verified source provenance; stored local trust cannot replace it.

## Capture repair and retry

Automatic repair processes at most 500 stale assets per library in one sync
cycle. When a clean batch makes progress and work remains, watch and service
mode wait at most 60 seconds before the next cycle. A stalled or failed batch
uses the configured watch interval so persistent provider failures do not cause
rapid retries. A one-shot sync processes one batch. `kei status` reads durable
remaining counts, and `sync_report.json` reports the current cycle's refreshed,
failed, and remaining counts. Revision storage uses one row per distinct asset.
A synthetic one-million-asset database grew by 87.363 MiB, or 17.51 percent.
One library with one million stale assets needs 2,000 successful batches, which
adds about 33 hours of follow-up waits plus provider and cycle processing time.

The watch pre-check includes only libraries with pending capture work or
serviceable rewrite work, even when iCloud reports no provider changes. A
rewrite marker is serviceable only when a metadata writer is enabled. A capture
refresh preserves download status, paths, checksums, and media bytes. It stores
corrected catalogue metadata, the current revision, and any configured rewrite
marker before the library revision can become active. Lookup or state failures
leave the row stale, report the remaining work, and preserve the affected
provider checkpoint. A library promotes its active revision only when every
live downloaded row is current, excluding prepared historical originals without requiring a coverage certificate. Persisted rewrite markers may drain later
because they are durable recovery evidence.

Metadata failure markers must survive so a later run can retry metadata
without downloading the media again. Full enumeration, single-pass incremental,
and collecting incremental sync all commit changed catalogue metadata and its
configured rewrite marker together, before filtering, album routing, or
deciding whether unchanged media needs downloading. A failed commit preserves
the provider checkpoint. Queued rewrites drain once per cycle, after every
producer has finished, so a single writer owns each file. Read-only runs never
drain. Only that drain retires a marker on success, because it writes the file
from the same row it then clears. Bounded rewrite batches with enabled optional enrichment load current
album and people rows for their library-scoped asset IDs. A grouping-read failure
leaves the affected markers pending. A completed download writes the snapshot
it was planned from, so it records a marker on failure but never retires one.

With `metadata.include_albums` enabled, the first task payload includes its concrete pass album, even when the cycle's
grouping preload is empty. Provider membership changes update the
`asset_albums` read model and metadata retry markers in one transaction. The
projection uses child identity or the recorded legacy state owner, never a
sibling's master reference alone. Individual membership changes use indexed
identity lookups; container refreshes enumerate only that container. Schema v22
adds a reverse index for legacy owners without changing their recorded
identities. Completed snapshots can remove missing memberships; interrupted
snapshots preserve the prior live memberships. External grouping sources remain
unchanged. The end-of-cycle writer uses the accumulated memberships for each
tracked media path. Retry markers do not authorize writes when metadata outputs
are disabled.

## Per-path field ownership

`asset_metadata_paths` records each successfully finalized media path, its
provider rendition checksum, local fingerprints, and metadata retry receipts.
Registration and downloaded-state finalization share one transaction. The
catalogue still owns sync selection and keeps one path per rendition. Metadata
rewrites also visit the additional recorded paths that match the current
provider rendition. Each completion checks the selected path and fingerprints,
then clears only that path's debt. A failed copy does not block successful
copies or lose its retry evidence. Capture-repair receipts remain path-specific.
Missing paths retain retry evidence; old provider renditions remain recorded
but do not receive current-rendition metadata.

Schema migration seeds only the path already recorded in each catalogue row.
It does not scan for, infer ownership of, or authorize writes to older copies
that were never recorded. Additional paths enter the registry through verified
download finalization or explicit import adoption.

XMP sidecars record the exact properties that kei writes in the kei namespace.
A later rewrite deletes a cleared property only when that marker proves kei
owned the prior value. Unmarked standard properties and unrelated third-party
namespaces remain unchanged. An existing sidecar must be readable and parseable,
and its bytes must still match the writer's initial read at publication. A
failed check preserves the sidecar and its durable rewrite marker. When the
complete merged packet already matches the existing bytes, a retained regular
file snapshot rechecks identity, parent confinement and exact bytes before
reporting success. This avoids republishing an unchanged packet. Changed packets
use a new temporary path so retained ambiguous bytes cannot block a later retry.

## GPS provenance

Source GPS facts for sidecars are read through file-backed parsers. JPEG APP1,
TIFF-based RAW, PNG `eXIf`, and HEIF Exif items use checked seeks and
fixed-size TIFF fields. When a HEIF carries several Exif items, `pitm` and
`cdsc` select the one describing the primary image. HEIF atom and item-location
counts are streamed without allocating from provider-controlled lengths.
CloudKit is authoritative for the currently decoded coordinates, altitude, and
capture timestamps. The location decoder currently maps only `lat`, `lon`, and
`alt` from `locationEnc`, so source EXIF supplies GPS receiver time, speed,
speed units, and horizontal positioning error. Native horizontal accuracy is
emitted only when both validated native coordinates equal the provider latitude
and longitude after decoding to decimal degrees. There is no geographic
proximity tolerance: rounding differences can omit accuracy, but nearby or
edited locations do not establish that accuracy describes the same fix.
Matching coordinates alone are insufficient after GPS embedding: kei may have
inserted them beside an older native accuracy value. Sidecar planning also
requires the current file to match SHA-256 evidence from a verified download,
captured before any kei metadata write. Normal downloads pass that baseline to
the sidecar phase, even when embedding is disabled. Download finalization
atomically stores it in the path's `asset_metadata_paths.source_checksum`.
Queued rewrites use only that source checksum, never `local_checksum` or
`download_checksum`. Those older columns also support size validation and can
be populated from already-rewritten bytes during checksum recovery.

Schema v23 adds the nullable source checksum without backfilling historical
rows. Missing evidence stays unknown across embedded writes, failed state
updates, restart, and later retries. Adoption and local reconciliation do not
establish new source evidence. Existing evidence remains scoped to its path
and provider rendition; a different rendition invalidates it. Metadata-only
finalization preserves source evidence without changing it. This conservative
rule can omit valid accuracy from older downloads or after unrelated embedded
edits. It does not delete native metadata or add an output-revision sweep.

Missing, invalid, or different coordinates omit accuracy. A supported rewrite
removes the obsolete value only if the existing property marker proves kei
ownership. Unowned values remain unchanged. This rule applies to new sidecars
and already-supported rewrites; automatic catch-up for untouched sidecars is
separate work in #799. GPS receiver time and speed keep their existing policy.
Source I/O failures still publish current CloudKit metadata, preserve prior
kei-owned source GPS fields as unknown, and retain the metadata retry marker.
Readable unsupported or malformed metadata permits a CloudKit-only sidecar.
Source media is never opened for writing on this path.

## Stable-input and timestamp checks

A drain only rewrites a file whose bytes still match the checksum on its row,
because the alternative is embedding into damage and then vouching for it. A
file that no longer matches keeps its marker and its recorded checksum, so
`kei verify --checksums` and `kei reconcile` continue to report it. When a
rewrite does change the media, the new hash is stored before the marker
retires, and the pre-rewrite hash is kept as the provider download checksum so
reconcile can still tell an intentional rewrite from a truncated file. A
retired marker therefore implies the row describes the bytes on disk.

Capture timestamp repair records the current provider metadata hash as durable
intent. Before publishing changed media, it records the exact prepared output
checksum and size. If publication succeeds but state finalization is
interrupted, a later explicit repair can recognize that exact output and finish
the checksum transition without trusting unrelated bytes. Input bytes matching
neither the recorded local checksum nor the prepared receipt remain drifted.
Provider metadata updates that would invalidate a prepared receipt remain
retryable until the published bytes are finalised in state. Source-deletion
transitions follow the same rule so a tombstone cannot hide the only proof of
published repair bytes.
Capture-instant changes are guarded across every version of the asset, even
when the metadata hash is unchanged. Addition-date-only updates preserve the
prepared receipt. An upsert replacing the receipt's own rendition with a new
provider checksum may invalidate that receipt; a sibling replacement may not.
Tasks rejected by the state upsert are not dispatched or marked failed, so the
existing receipt and catalogue evidence remain intact and the state-write
failure keeps the checkpoint ineligible.
Provider-version or non-metadata file replacement invalidates the repair state;
byte-identical re-adoption preserves it. When replacement or adoption occurs
during the explicit repair run, downloaded-state finalization creates fresh
pending repair debt for the installed checksum.

A rewrite whose result could not be measured records the checksum as unknown
rather than leaving the superseded value in place, because a known-stale hash
would make the next pass read kei's own rewrite as damage and refuse it. A row
that never recorded a checksum is rewritten by ordinary metadata refresh but
claims no provider download hash, so reconcile keeps reporting a file that is
short of its provider size. Capture timestamp repair instead leaves that row
pending because it cannot prove file provenance.

## Movie preparation

XMP Toolkit preparation checks movie box boundaries with bounded file reads before
updating the temporary copy. Nested QuickTime `meta` boxes without a supported
FullBox prefix are rejected because the toolkit can corrupt their existing child
boxes. The optional four-byte zero terminator is accepted only at the end of a
`udta` child list. Rejection preserves the original media and leaves embedded
rewrite work pending through the existing retry owner.
