# Local publication and recovery

Start here for the evidence needed to publish, retry, or adopt one local copy.

## Download and publication

`TaskPlanner::verified_downloaded_path` retains an existing recorded filename when the provider renames the same rendition within the current pass directory. Reuse still requires the same provider content generation, exclusive recorded ownership, an allowed destination and the recorded local checksum. A changed cloud filename alone does not create another local copy. Cross-directory template changes and missing-file destination recovery remain separate decisions.

For captured selection generations, `verified_selection_paths` returns the retained pass key with each published path. Projection keeps a same-pass, same-generation destination across folder-template changes within the configured root. New assets use the new template. An absent recorded destination remains the recovery target when ownership and protected-path checks permit it; publication still uses the no-overwrite path. Other passes' copies need the existing path-family match. This reuses retained generation evidence and adds no mapping table or serialized field.

`TaskPlanner::recorded_selection_path` owns that retained-path choice, alongside receipt reuse and retry-path resolution. Generation projection supplies authenticated scope/pass evidence and keeps authority over frozen decisions and admission. The planner preserves confinement, protected-path checks and local completion proof validation. Managed publication paths remain with the layout owner.

`TaskPlanner::retain_pending_destination` also owns the final recorded-path decision for ordinary pending retries. It keeps a durable reserved destination ahead of an older path, applies the existing content/ownership checks, selects authorized truncated replacement or a safe collision sibling, and retains the accepted claim. Retry orchestration supplies saved evidence and queues only returned tasks. A refused choice leaves the durable target unresolved. Sibling selection and claim retention are private planner operations; retry orchestration no longer mutates task paths independently.

Frozen-generation and authorized truncated-repair destinations use `TaskPlanner::bind_reserved_destination`. The planner claims the exact path before changing the task. Generation orchestration supplies its frozen destination and holds refused work; it no longer performs a separate task-path assignment after a claim.

`TaskPlanner::bind_pending_destination_proof` attaches the cross-directory finalization requirement for a confirmed pending task when current-content saved reservations require it. Ordinary full/incremental dispatch checks its pending snapshot before calling this owner; targeted retry calls it only for selected pending tasks. Both routes use the same library, asset, rendition, checksum and size lookup. Reservation lookup errors propagate before transfer dispatch.

Pending recovery and collected incremental execution share `url_refresh::refresh_expired_download_pass` for one exact-task refresh, execution and result merge. That owner admits retry only after URL expiry, below the transfer authentication threshold, without failed state writes or cancellation. Collecting execution checks its additional provider-authentication and planning-write failures before calling it. Pending recovery no longer has a separate admission branch. The shared owner carries prior authentication failures and fresh URL timestamps into the retry, starts a separate rate-limit counter and merges only the expired candidate results. Provider authentication failures return separately to checkpoint reporting.

Streaming cleanup uses the same exact-task lookup owner for ordinary transfer failures and expired URLs. It no longer re-enumerates albums or replans failed destinations. Refreshed records must match the selected rendition's content generation; the task retains its original path, metadata and publication authorization. Unmatched work remains pending. Cleanup still runs within the existing bounded pass and authentication/cancellation gates.

`url_refresh::refresh_failed_download_urls` is the direct refresh entry point for streaming cleanup, expired-task retry and stale-URL preflight. Captured selections supply their exact source evidence to that owner; ordinary tasks use their selected source identity. The former ordinary and incremental builder wrappers are removed. Cancellation returns the original tasks as unrefreshed without provider requests, retaining exact destination evidence in either case.

`CleanupRetryPlan::execute` consumes a refreshed plan once and hands its exact tasks and URL observation times to the shared transfer/publication pipeline. It starts an independent retry rate-limit counter and includes lookup observations in the returned pass result. Its failed-task list includes both transfer failures and tasks that could not be refreshed. Streaming cleanup consumes that list instead of reconstructing remaining work from the original candidates. Expired-task merging also consumes the returned failures, retaining failures outside that retry. Provider authentication evidence remains separate from transfer authentication counts; each caller retains its existing interruption and checkpoint handling.

Targeted pending recovery currently requires successful source work. A pending row alone does not prove that a destination is unattempted: expired transfers and incomplete managed publication can retain pending state. Removing this gate requires distinguishing that work before scheduling another attempt. The existing gate preserves bounded attempts; remaining pending work survives for ordinary retry after reopen.

The CLI rejects the retired `--retry-failed` flag before configuration, authentication or state startup. Ordinary sync owns automatic retry. The runner no longer resets failed rows or exits early through a separate retry-only startup path; regular dispatch retains its existing pending-work preparation.

`prepare_for_retry` is the state owner's retry-preparation operation, including library scoping, attempt reset and error retention. The retired `reset_failed` wrapper and trait entry have no remaining execution path.

The cycle owner has no retry-only argument or alternate eligibility gates. Inventory qualification, smart-folder path reconciliation, sparse-target preparation and zero-asset warnings all use ordinary sync rules. Pending failures do not reset or consume the provider cursor. Indexing qualification, materialization and retained-cursor replay still require their existing evidence before checkpoint promotion.

`dispatch::finish_incremental_attempt` owns incremental-error fallback execution and rejected-token bookkeeping for ordinary and captured selections. It returns successful attempts unchanged, preserves the captured selection's state-error refusal and chooses its existing inventory executor. Authentication, transport and capture refusals propagate without starting an inventory. Ordinary fallback retains its full-enumeration reason. Guarded inventory reconciliation still owns its separate before/after cursor evidence and does not gain another fallback pass.

Download configuration no longer has a retry-only selection mode. Context loading and the producer do not load or filter against a known-ID set. Current selection can therefore recover pending destinations while discovering new assets. Historical skip diagnostics remain readable, and the inventory fingerprint retains `"retry_only": false` for ordinary-sync compatibility.

Private smart-folder passes use the same captured-generation destinations when their name, query types and filter exactly match the provider's built-in definitions. The Photos adapter validates those definitions; merely labeling a pass as smart does not qualify it. Incremental cycles still refresh smart membership through the current query before admitting work. Query absence does not authorize file deletion, and only the completed zone stream supplies an incremental successor. Unsupported, cross-zone and unowned selections retain their existing dispatch and debt gates.

Before a selection-work watch shortcut and before replaying a quiet delta, the generation owner checks completed selected paths for existence in pages of 256. Only `NotFound` reopens the affected destination in the existing generation queue. Existing files are not hashed or treated as damaged by this check. The state transaction clears that copy's active completion proof while retaining its recorded path, catalog receipts, provider facts and retry timing. Replay supplies the existing targeted confirmation and publication path; completed peers remain complete. Filesystem inspection and state-write errors propagate instead of certifying a quiet cycle.

```text
PhotoAsset
  -> planner::TaskPlanner
  -> pipeline::run_download_pass
  -> file::download_file
  -> optional metadata_rewrite
  -> verified .part publication
  -> finalize downloaded or failed state
```

Only producer-dispatched work becomes pending through `upsert_seen`. Filtered
or skipped assets must not be left as retryable work unless a dedicated state
transition owns that result.

Full enumeration divides the configured download-worker limit across concurrent
album passes. Each pass gets the same allocation, rounded down. The remainder
stays unused so replacement passes cannot exceed the total limit. Passes keep
their existing cancellation, publication, and state-finalization paths.

Asset dates used for filtering, path rendering, file metadata, and sidecars are
resolved from Apple's UTC instant plus the asset's `timeZoneOffset` when that
offset is usable. Missing or invalid offsets retain host-local rendering,
preserving the previous path and metadata behaviour. Date-only lower bounds
use a conservative UTC enumeration bound and then apply the exact resolved
calendar-date filter, so enumeration safety does not depend on the backup host
timezone.

Selection date inputs are fixed: `YYYY-MM-DD` means a capture-local calendar date, and `YYYY-MM-DDTHH:MM:SS` means a host-local datetime. `config/runtime.rs::parse_created_date_filter` owns these semantics. Relative `Nd` inputs are rejected with instructions to remove the limit or use a fixed boundary. Sync/service `--recent` and `--recent-scope`, and TOML `[filters].recent` / `recent_scope`, also fail with removal and fixed-date guidance before authentication or state/media changes. TOML rejection runs in `config/input.rs` before CLI precedence, so an override cannot hide a retired saved setting. `import-existing --recent N` uses its own positive-count parser and does not acquire sync date filtering. Fixed-date fingerprints and uncapped compatibility encodings remain unchanged.

Capture-local path rendering does not change the download config hash, so a
sync that sees no other drift stays incremental and never re-enumerates an
already-downloaded asset. Date-only created bounds change the eligibility hash
but not the path hash, so expanding a date window runs the required inventory
without treating existing media as path drift. Matching legacy hashes that
mixed date eligibility into path state migrate without reconciliation. When a
legacy hash is ambiguous, reconciliation uses the same durable path-family and
file-integrity proof as normal sync to discard the planned collision task
before copying or changing state, so an asset cannot collide with its own
recorded file.

For an asset carrying a usable offset, a path derived under host-local
rendering is not a current derived path, so a full sweep forwards that asset
and downloads it into its capture-local folder. The earlier copy stays where
it is: kei never deletes local media, and no command removes a file that no
longer matches a derived path. `import-existing` adopts through the same
derivation. Assets without a usable offset retain host-local paths and remain
compatible with icloudpd's date-folder layout.

Existing embedded and sidecar timestamps are repaired only through the
explicit `sync --refresh-metadata` flow, which is bounded by the same
no-overwrite probe gate as any other embedded write. An offset tag names the
zone of one specific timestamp, so the embed path attaches it only to a
timestamp it writes in the same pass, or to an existing one the probe proves
already renders the capture-local instant. When writing a timestamp into a
file that has orphaned datetime offsets, the writer clears those offsets before
installing the timestamp and its resolved offset. Attaching an offset to a
wall clock left by host-local rendering would assert an instant the asset never
had. `sync --refresh-metadata --repair-capture-timestamps` explicitly relaxes
the no-overwrite gate for state-recorded downloaded files. It requires embedded
datetime output, a usable Apple offset, and bytes that still match the recorded
checksum. Rows without that provenance, and files whose embedded format is not
supported by the active build, remain pending. The writer replaces the timestamp
and its offsets together through the stable-input publication path. For HEIF
media, an existing native Exif timestamp is patched in place with its paired
offset before XMP is written; ambiguous, shared, or unsupported native layouts
remain pending. Normal downloads and ordinary metadata refreshes still preserve
an existing timestamp. Capture repair is separate durable debt, so an ordinary
metadata drain cannot retire it. If ordinary embedded metadata would change the
same media while capture debt is pending, that embed waits for the explicit
repair; verified no-write and sidecar-only work may complete independently.

## Durable pending retry

Failed and pending rows are not recovered by replaying the entire provider
inventory. The retry owner:

1. Removes only work already proven source-deleted.
2. Resolves current provider records in targeted batches.
3. Uses durable asset/master mappings and checksum/size evidence for legacy
   identities. A persisted legacy owner resolves matching siblings without
   changing the selected child.
4. Adopts an existing matching local file when safe.
5. Marks current filter exclusions as policy-excluded.
6. Persists unknown or transient verification state.
7. Queues exact unresolved asset/version/path tasks.

Unknown identity is not permission to delete or forget work.

A refused recorded destination stays pending rather than becoming policy-excluded. The retry selector tracks refusal separately from filter exclusions and retains a transient verification marker if no later pass can queue that rendition. A later accepted destination clears the marker. The recorded path, repair reason, attempt history and local bytes remain intact.

Ordinary targeted recovery defers a completed copy when a later currently selected pass still lacks that rendition's verified copy. It uses current pass selection and existing local completion evidence; historical reservations alone do not create work. Adoption first checks the selected task's durable destination. If that reserved destination cannot be adopted, an older catalog copy cannot complete it. The current-publication transaction keeps its ownership and concurrent-change guards.

Transfer admission does not consult lifetime attempt counts. Failed work remains eligible on later cycles regardless of prior attempts; bounded transfer retries and cycle failure gates still apply. The producer no longer loads an attempt-count snapshot or marks tasks exhausted. Recorded attempt counts remain diagnostic evidence. Explicit `[download.retry].per_asset` settings refuse during configuration loading with removal guidance. The retained generation-fingerprint `attempt_limit` and support-report `per_asset` fields are always zero for current execution.

Pending recovery can follow a partial source transfer failure when every reported failure maps to a distinct failed library/asset/rendition target. `SyncResult` carries those transient exclusions through result composition; the pending selector omits them for the rest of that cycle. Independent pending peers can progress without giving failed source tasks another attempt. State-write failures, interruption, authentication expiry, blocked checkpoints, incomplete identities or enumeration, and unmatched failure counts retain the recovery hold. Exclusions deliberately cover all destinations of the same rendition; they can defer another copy until the next cycle. They do not authorize provider checkpoints or change durable retry state.

A pending rendition can retain a valid prior publication after a later download
attempt failed. Ordinary sync and targeted retry recovery may reuse only its
current catalog path when prior
publication time, unchanged provider checksum and size, a matching path receipt,
equal recorded local/download hashes, the current filename family and positive
catalog/reservation ownership all agree. Recovery runs before fresh plan choices
are saved; recovered renditions and failed recovery writes stay out of transfer
and reservation dispatch. A healthy rendition about to receive a fresh choice
uses the same current-file proof so that a later failure can still recover it.
Existing durable choices keep their replay policy; saved different destinations
still retain debt.
The file is hashed beneath the current root without following links. A retained
file and namespace, plus the process's
destination guard, remain live through immediate conditional state finalization.
The state owner rechecks the row, receipt, identity, protected legacy evidence,
reservations and provider generations in one transaction. Recovery preserves the
original path, publication timestamp, all receipts, source provenance and metadata
debt; it does not mint provider progress from local bytes. Unknown identity or a
saved different destination remains pending. Cancellation before commit or a
failed state write cannot dispatch a fallback copy from that recovery attempt.
The destination guard coordinates participating writers in this process; it is
not an interprocess or network-filesystem lease. Managed selection keeps its
separate destination and source-provenance finalization.

Policy-excluded rows stay outside the actionable pending reader. After a
successful source pass, targeted revalidation checks their durable provider
identities only for explicit deletion. Present, omitted, malformed, and
transient responses retain the policy-excluded rows.

A provider checksum change makes the previous local path historical. Retry
adoption requires durable proof that the path holds the current provider
version and that its filename matches the recorded task filename.

## Import-existing

Writable sync calls `commands/import.rs::adopt_startup_archive` once for an empty state database, including recovery after state loss. It collects the resolved selected passes through the ordinary import matcher before starting downloads. Existing nonempty state skips this startup census. Missing or empty destinations need no provider scan. Adoption reads provider metadata and hashes local files; it makes no media requests and leaves media and sidecars unchanged. Ambiguous candidates remain unowned so normal sync can choose safe unique destinations.

The metadata key `startup_archive_adoption_v1` records `pending` before the census and `complete` after adoption. Cancellation, incomplete enumeration and local hash or state-write failures retain `pending`, including when earlier candidates were committed. A later process retries that census despite its now nonempty database. Completed startup adoption does not scan again on ordinary sync or watch cycles. Startup adoption errors are reported while independently identified current sync work proceeds; adoption does not advance provider checkpoints.

`src/commands/import.rs` shares configuration, selection, pass planning, and
path derivation with normal sync. Before matching or consulting prior imports,
it uses the download context's identity selector to install the same child or
guarded legacy master state ID as sync. It prepares selected rendition rows
and paths for every selected pass before adoption, then checks the complete
candidate census. Incomplete enumeration, fetcher panic, or cancellation before
completion leaves all adoption rows, mappings, and legacy owners unwritten.
The preflight uses memory proportional to selected renditions, all durable
catalogue paths and current receipts, and cached directory entries. Metadata
and receipt slices are shared; raw provider JSON is not retained. There is no
fixed memory cap or disk spool. Catalogue paths include historical and omitted
assets, so an unselected owner still prevents adoption of its file. Equivalent
relative and absolute paths use the same confined ownership key.
Import uses sync's collision-family owner for full-ID and ordinal filenames.
Different-size candidates remain distinguishable. An ambiguous short name,
sanitized full-ID collision, or multiple ordinal siblings is refused unless an
exact current-generation durable receipt and verified local SHA-256 identify
the file. Provider MMCS checksums are compared to provider checksums, never to
local SHA-256. It optionally compares remote prefix
bytes, hashes the local candidate, and calls `ImportStateStore::import_adopt`.
Adoption commits the provider child/master mapping and any compatible legacy
owner in the same transaction as the imported row. Failed adoption rolls back
both. Dry-run selects identity without persisting mappings or ownership.
An unclaimed legacy row completes adoption before it can skip later rehashing.
Adoption checks durable destination reservations in the same transaction as
catalog updates. A different asset, rendition, or provider content generation
cannot adopt a reserved path. Unknown legacy content remains occupied.
A refused adoption logs a warning and does not count as a match.
Size/mtime snapshots may skip later rehashing only while the current provider
checksum, path, size, mtime, and stored local checksum still match. Ambiguous
paths always require a local hash verification before that shortcut.

## File landing

The media publication sequence is:

```text
write or resume .part
  -> validate response and content length
  -> validate expected size
  -> validate SHA-256 checksum
  -> validate content type and sniffed bytes
  -> apply configured pre-publish metadata
  -> publish without replacing an existing final path
  -> fsync the parent directory
  -> finalize SQLite state
```

The transfer owner opens an existing resume file through a retained parent
handle with no-follow access and requires a regular file. It reads the Range
offset from that file handle and uses the same handle for append writes.
Resume probes reject symlinks. Identity checks reject a changed leaf before
body writes, content validation, and publication. A final identity or byte check
must pass before downloaded state can finalize. The task retains the file
through metadata work and publication; an opt-in metadata replacement must
match the writer's output fingerprint before it is accepted.
The durable temporary-path claim alone does not authorize an append.
Staging names use fixed-length SHA-256 digests of the lossless absolute
destination and provider checksum generation, encoded as
`kei-v1-<destination>-<generation><temp_suffix>`. MMCS checksums select a
provider generation; they do not verify downloaded bytes. Different
destinations have independent staging even when their bytes match.
`file::staging` acquires an in-memory destination guard before claiming or
inspecting a part. The task retains it through retries, resume, metadata,
publication and claim retirement. Different generations and suffixes for one
destination share that guard. Cancelled waiters cannot retire a peer's claim.
The weak registry upgrades and prunes entries under one short mutex; active
owned guards and waiting futures keep the shared destination mutex alive.
Completed and cancelled guards retire dead entries. No permanent lock files
are created. This coordination covers workers in one kei process, including
separate databases used inside that process. It does not coordinate independent
processes or hosts; existing cross-process limits remain, including network
filesystems whose advisory locking is disabled or unsupported.
Cleanup of versioned staging names takes the same destination guard through
claim retirement. A busy guard preserves both bytes and the claim. Legacy
checksum-only files are never adopted into a new destination's staging
identity. Existing exact-path ownership claims can still authorize their
stale orphan cleanup under the same age and confinement rules. Unclaimed
legacy files remain untouched; this change performs no migration or discard.
The streaming consumer cancels and drains after its authentication threshold,
so detached filesystem or metadata operations complete before guard release.
A resumed request rejected with HTTP 416 gets at most one fresh request per
attempt. The retained part stays intact until an acceptable response can
restart the transfer; a second 416 terminates that attempt. Transfer retry
pauses and bandwidth waits observe shutdown before another request or chunk
write. A cancelled bandwidth reservation is refunded, and interruption reports
the retained byte count without publishing or finalizing the file.
Before returning a body-stream error or interruption, the transfer owner flushes
pending filesystem writes. Retry offset reads and temporary-claim retirement
therefore follow write completion. A flush failure is a disk error rather than
a resumable transport error. Successful transfers also sync the data before
validation and publication.

Schema v19 records the exact temporary path before a state-backed download can
write or resume it. Normal completion and graceful interruption retire that
claim. A process crash leaves the claim as durable cleanup authority. Later
cleanup inspects only claimed paths, rejects every symlink in the path, and
holds verified directory or file handles through removal so an ancestor swap
cannot redirect deletion. It retires the claim after deletion or when the path
is missing or no longer the claimed stale file. A suffix and file age alone
never authorize deletion.

Explicit integrity repair uses `reconcile --inspect-path PATH --confirm-damage SHA256:SIZE`. Inspection alone grants no replacement authority. Schema 42 stores the confirmation on the existing `provider_active_destinations` row, bound to its selection manifest and inspected bytes. It reopens only that destination's completion state and retains historical receipts. The current selected pass attaches the fingerprint to its conditional publication. If the file changes again, inspection can replace the pending confirmation after the operator confirms the new fingerprint. The transaction checks that the previous confirmation still matches. Source unavailability retains damaged bytes and retry work. The file owner uses atomic exchange or its existing replacement journal. The former `sync --repair-truncated` entry point refuses with per-destination confirmation guidance.

Managed primary destinations stay with the primary-layout journal. The planner pins the confirmed damaged input, clears its source provenance, and retains healthy sibling members without rewriting them. The layout owner prepares verified replacement bytes and durably preserves the damaged input before publication. Ordinary landed-repair recovery uses existing independent publication receipts; a fingerprint change by itself cannot complete repair.

When no-overwrite publication finds an existing destination, the file owner
compares it with the verified `.part` file. Identical bytes are deduplicated.
Different or unverifiable bytes return a typed collision and retain the
`.part` file. The pipeline records the task as failed before it can write a
sidecar or finalize downloaded state.

A file may be safe on disk while its state write is still a cycle failure. Do
not weaken deferred state-write handling or infer that a visible final file
means the database transition succeeded.

## Destination reconciliation

Schema 25 stores local reconciliation destination choices in
`reconciliation_paths`. The planner reserves current and historical catalog
paths and previous choices across all libraries, with library-qualified asset
and rendition ownership. Unselected renditions keep their paths reserved when
live resolution changes. Before it copies any media, it commits the new choices
in one SQLite transaction. A failed
reservation write prevents publication. Retries reuse the reserved leaf even
when provider lookup order or availability changes. The recorded source path
stays unchanged until file and metadata validation and state finalization pass.
Each choice is keyed by the provider checksum and byte size. A changed rendition
gets a separate destination; retries of the same content reuse its choice.
Migration retains schema-24 choices with unknown content as occupied paths. It
does not infer their content from a catalog row that may already describe a newer
version. Reconciliation can create a new sibling for these legacy choices.
Choices remain reserved after completion and config drift because old copies
remain on disk. This ledger does not authorize overwrite or deletion.
Full downloads, incremental downloads, and pending retries load this ledger
and the catalog paths before planning. They reuse their own reserved
destinations and cannot download into or adopt a path owned by another asset
or rendition. When the ledger is active, downloads commit new destinations
before publication. Pending retries commit the final choice after recorded-path
overrides. A failed reservation write prevents download dispatch. After a
failed finalization, retries can adopt a verified reserved sibling even when
the catalog still records the previous source. Dry runs and filename listings
do not commit choices. Recorded retry paths follow the same ownership checks. Explicit truncated-file repair can still use its own path
when the existing fingerprint and repair authorization pass.

Durable pending retries can record a destination in another directory after a
recorded-path override. Only pending retry planning replays that exact choice
when its library, asset, rendition, checksum, size and path ownership match and
the destination is beneath the current download root without parent traversal.
It preserves the root spelling and validates the saved destination key. Ordinary
downloads and local reconciliation retain the requested-directory replay check.
Retries stop planning another pass once every pending rendition for that asset
is matched or safely adopted; outstanding companions still require planning.
This does not migrate or discard reservations, alter selection completeness, or
authorize promotion of a pending path configuration with smart-folder selection.

Cross-parent pending adoption requires an independently durable local SHA-256
for the exact destination and current provider generation. It checks every saved
alias before allowing a pass to use ordinary adoption. MMCS is a generation
signature, not a local SHA-256; size and media headers cannot supply this proof.
The file owner retains no-follow directory capabilities and the regular-file
handle, revalidates identity and bytes before the immediate state write, and
retains those capabilities through that write. Failed adoption writes retain
debt and later attempts must reopen and reverify; no hash-only deferred adoption
write is queued. Both ordinary and selection enumeration inspect all matching
aliases before metadata actions, skip adoption, task adoption or touched-state
promotion. A refusal holds that asset's debt while other enumerated assets may
continue; it cannot enter the size-only fallback.

Fresh downloads to an affected reserved pending destination retain the original
publication inode and current-root no-follow namespace through immediate and
bounded deferred finalization. Each attempt revalidates both identity and the
publication fingerprint. Changes observed at revalidation retain the media and
pending debt instead of committing stale verified provenance. Retained ownership
and revalidation do not atomically exclude arbitrary Unix changes after the last
validation while a database write awaits execution. Targeted cleanup and
collecting incremental dispatch preserve the affected task's proof requirement;
cleanup also requires the original failed generation's checksum and size.
Equivalent root spelling is rebased component-wise using platform ownership keys;
this does not relax filesystem no-follow traversal or mutate the saved ledger.

**Recovery limitation:** an existing cross-parent reserved file without a trusted
matching local hash remains untouched and pending. This includes a crash after
publication but before any durable receipt commits. These bytes cannot be
automatically adopted, replaced or discarded; this slice does not fetch fresh
provider bytes to establish an independent comparison.

Download planning also loads current-content publication receipts from
`asset_metadata_paths`. An additional album copy can satisfy a download only
in that pass's destination and filename family. The planner rejects foreign
catalog ownership, opens the file without following links, and checks its
local SHA-256. Old provider generations cannot satisfy current downloads.
These checks do not move the catalog's current path or change retry receipts.
Without reconciliation reservations, an unsafe historical path disables optional
receipt reuse for that pass instead of blocking ordinary downloads. Reserved
paths retain strict validation. Live Photo planning applies filters before file
checks and does not hash the still when same-size current companions satisfy
the existing skip rules. New companions use a verified still filename. Existing
companions with older numbered still stems remain usable after hash verification;
Kei does not rename or delete them.

Historical smart-folder membership cannot complete local path reconciliation. It reports separate conditional evidence when the fresh smart query is its only remaining dependency: album snapshots and catalog identity must be complete, with no deferred missing file or failed local work. The cycle owner can discharge that dependency only after successful current smart enumeration and durable work. During pending path drift, lower-date-bounded selections drain ordered provider queries to EOF while forwarding only their configured selection to the existing download planner. Every selected pass must report a usable, unanimous token. This costs a complete query on path drift, with streaming tail filtering. Cancellation stops the raw drain.

Fresh query evidence certifies selected local work separately from source checkpoint coverage. An excluded date tail still holds zone and database checkpoints. Stale plans, interruption, unresolved identity, failed enumeration, local work or state transactions retain the pending path hash. Every selected library must finish. Atomic promotion compares the active path hash and exact pending target before publishing the new hash and removing its marker. Eligibility inventory-and-bridge promotion remains separate. Unchanged cycles skip catalog reconciliation and the exhaustive path proof; ordinary bounded smart-folder refreshes may still query the provider.

Local path reconciliation retains an unowned legacy master with multiple
historical children as unresolved work, even when targeted lookup returns one
usable sibling. It does not reserve that master's path or relax preservation
eligibility. Independently owned current children can still reconcile, and later
complete inventory can prepare preservation and release the checkpoint hold.

A historical child media receipt at a protected original is not a current claim
when the exact mapped child and rendition have a separate ready, source-backed
current file. A missing current file remains unsupported by this historical
exception; historical rows do not authorize initial materialization. The state owner snapshots the family dependencies and rederives
that exception inside preservation preparation. The file owner retains confined
handles, verifies their bytes and namespace immediately before commit, and
refuses cancellation or drift. Historical receipts, originals and owner facts
remain unchanged. Explicit local-byte acceptance does not satisfy this exception.
Ordinary adoption and skip decisions cannot reuse a protected original merely
because a child has a historical receipt there.

Local path reconciliation also loads these publication receipts. A verified
copy in the selected pass satisfies that rendition even when the catalogue's
current path belongs to another album. Reconciliation preserves a verified
numbered still filename when planning its companion, so changing Live Photo
selection or album order does not create another copy of an owned pair.

Local path reconciliation opens source and destination leaf entries without
following symlinks. It hashes the opened file and rechecks its identity before
accepting a destination, including entries that appear during publication.
Temporary copies use new unique names, verified bytes, and no-overwrite
publication. On Unix, they start with owner-only permissions, so a failed copy
remains private. A completed copy receives the source permissions.
An unsafe or replaced entry preserves the previous catalogue path
and leaves reconciliation incomplete. A blocking reconciliation failure skips
that library's normal source/download pass, so adoption cannot bypass the
rejection. Its provider checkpoint and the aggregate database pre-check token
remain unchanged. Ambiguous temporary entries remain for
inspection.

Reconciliation retains directory capabilities through state finalization.
Configured roots, recorded sources, destinations, and temporary siblings must
not contain `..` components. Reconciliation rejects these paths before lexical
normalization or filesystem writes, because a linked preceding component can
change which directory `..` selects. Ordinary relative roots remain supported.
Descendant directories are opened without following symlinks. Source reads,
temporary creation, validation, and publication use these retained handles;
namespace replacement leaves reconciliation incomplete. Windows directory
handles deny rename and delete sharing while the receipt is live. Unix checks
that the retained parent still occupies the planned namespace before accepting
the result. Publication and temporary-file checks remain descriptor-relative.
When the configured root changes, the recorded old source is opened beneath
the common lexical ancestor of the old path and new root, with no-follow
traversal below that anchor. This allows root moves without trusting linked
source directories. Destination writes stay beneath the new configured root.
Relative paths are normalized inside the filesystem owner, not in durable
catalogue keys.

A reconciled media copy receives the same capture mtime as a normal download.
If the retained source and destination paths resolve to the same pathname,
reconciliation only updates the catalogue path spelling. It preserves media
mtime and existing or absent sidecars, then validates media before finalization.
This permits changes between relative and absolute download roots.
Before changing timestamps, reconciliation rejects a distinct destination that
shares the source file's identity, including a hard link. The source mtime and
catalogue path stay unchanged until the conflicting entry is resolved.
When XMP sidecars are enabled, an existing source packet is validated and
copied byte-for-byte, including custom properties and ownership markers. Path
migration does not upgrade or regenerate an existing packet. If no source
sidecar exists, the normal metadata planner generates one from the current
payload and source GPS facts. A source read failure stops before publication;
it cannot leave an incomplete packet that blocks the next attempt. Generated
packets do not infer native accuracy provenance from a local checksum.

Reconciliation reserves recorded and planned paths by asset and rendition. Keys
use the filesystem owner's checked absolute paths, so equivalent relative and
absolute roots share ownership. These keys do not change task or catalogue path
spellings. Invalid reservation paths leave reconciliation incomplete and block
file finalization. A collision with another asset or rendition selects a stable
identity-suffixed path. Each rendition can reuse its own reservation across
passes and retries, including after a
partial state-write failure. Existing files alone do not select another name;
the confined copy owner checks their bytes and rejects conflicts.

Sidecar publication uses the same retained directory capabilities and refuses
conflicting destination bytes. Reconciliation records the new catalogue path
only after capture mtime, sidecar work, and final input checks succeed. It
retains the old files and keeps failed work pending. Reconciliation planning
includes existing media destinations so a retry after a metadata or state-write
failure can finish without downloading or creating a second media copy.
Reconciliation retries the selected destination and leaves conflicting media
or sidecars untouched; it does not allocate another collision filename on each
attempt. Ordinary download collision naming and on-disk skip rules are unchanged.
Ordinary download planning also treats a state-protected legacy original or exact
sidecar as occupied when the directory cache cannot see that spelling, and selects
an independent identity suffix instead of targeting the protected identity.

Legacy preservation inventory failures retain the existing reason and budgets.
`legacy_inventory_failure_v2` adds fixed phase/subreason labels, whether a valid
terminal marker was observed, and the failing child reference's known family
context and soft-deletion flag. A missing master reference leaves family
membership unknown. Context describes the failed validation, not counts for
all records. EOF observation is not successful hydration or checkpoint proof.
These fields exclude identifiers, tokens, URLs, paths and provider error text.

## Managed edited-primary layouts

### Album path transitions

Schema 40 adds a nullable `directory_binding` to `album_containers`. `download/orchestration/album_directory.rs` binds authenticated container identity to its current path under an unchanged root. It supports a component-aligned `{album}` token with static or `{library}` ancestors and dated descendants. Sanitized hierarchy components remain relative to that root. Initial foreign-directory collisions choose a distinct sibling. Existing per-file receipts retain their paths through an explicitly unclaimed binding; they never establish exclusive directory ownership. Mixed selections also remain unclaimed.

The existing layout operation and member tables store `album-directory-v1` headers and exact per-file evidence. A same-parent move requires exclusive ownership, unchanged managed bytes and sidecars, completed work, stable provider container scope except its name, and unchanged directory identity. The file owner moves the directory without replacement, including user files, verifies its identity, and syncs both retained parents. An occupied destination gets a unique sibling; a racing collision cancels only an operation whose original source identity is still present. Recovery reuses the frozen target after a move has landed.

Recovery checks stable authenticated container identity before its unchanged-name shortcut. If the provider renames the container again during interruption, including back to its original name, preparation finishes the recorded transition before processing the latest name and template. It does not require discovery to reproduce the intermediate name.

An anchored binding remembers additional templates and their confined download root when future files can land outside its original directory. Rename planning combines the anchor census with exact selected external copies. External parent routes reuse the existing operation and member journal and force independent-file fallback. All destination directories are installed before eligible anchor user entries move. External user trees remain in place. Pending route collisions retain the journal; recovery does not cancel only the anchor after another destination has landed. Nested destination routes that leaf collision suffixes cannot separate fail before publication.

Replay rechecks managed bytes before moving a planned source. Local edits cancel the unmoved intent so the next census selects independent-file fallback. A byte mismatch found after movement restores the source without replacement before cancellation. The source census uses validated current native publication or current managed claims. Historical path receipts cannot supply ownership or veto current work.

Independent-file fallback creates its empty directory at a private operation-specific staging path, then journals that identity before publishing the final directory name without replacement. A crash before journaling can leave an unclaimed staging directory, but does not occupy the selected destination or grow its collision suffix. Replay after journaling recognizes the staged or installed identity. Foreign final-directory arrivals cancel only a still-unmoved staging intent and retain the original source.

Publication appends exact current-path compatibility receipts where paths are representable, and successor managed bindings while retaining old manifests, layout bindings, members, and path receipts. Old slots remain fenced. Missing-file recovery does not revive a moved historical slot. Current selection owns scheduling and checkpoint debt. Historical manifests need no directory-specific successor certificate; unfinished physical operations retain their journals and path fences.

For native copies, the directory member freezes the exact source publication identity before movement. The durable move transaction clears that old path's marker and commits the new container binding. The committed operation then represents a landed move, so recovery never restores it over later file writes. Its unsettled native member supplies bounded actual-publication evidence to the existing local policy and finalization owners. Matching current content receives a native receipt only after independent target fingerprinting; changed provider content uses ordinary replacement policy. Actual replacement, metadata publication or preserved-local completion consumes the handoff transactionally. Consumed members retain their evidence but cannot authorize restored historical bytes. Unselected historical renditions do not become checkpoint debt.

Modified or missing media, modified managed sidecars, preserved-local copies, retained edit history, unclaimed directories, mixed selections, and changed hierarchy parents use the journal's independent-file fallback. It freezes the trusted old receipts separately from observed local bytes, creates one stable destination directory, and fences relinquished source slots. Source media remains in place as unmanaged local copies; the ordinary file owner independently establishes the selected destinations. User-added entries move without replacement only from an exclusively owned source directory. Unknown trees and private history remain at their original paths. Current publication and metadata obligations remain with the existing selection owner.

Managed-primary missing-file repair uses the current binding and confirmed rendition. Its existing member journal records optional missing-file evidence while retaining old encodings when that field is absent. The replacement is published without overwrite authority for the absent media. Any surviving owned sidecar is independently preserved before conditional replacement. An unexpected media arrival or changed sidecar remains protected. Recovery never recreates a fenced historical directory. Membership transitions use stable authenticated container identity and the current binding, preserve outstanding publication, and independently complete each destination before withdrawing source admission.

Established managed families resolve their recorded directory from same-pass binding and journal evidence before deriving destinations. Folder-template changes therefore affect new families rather than duplicating existing ones. The same resolution is used by queue planning, previews, processing and interrupted-operation recovery. Recorded rendition filenames remain stable when the resource is unchanged or the file extension stays the same; format-extension transitions retain the layout owner's preserving publication behavior. Historical candidates outside the configured root, or multiple candidates that cannot identify one family, are not guessed into a binding.

`download/orchestration/primary_layout.rs` owns managed-layout transitions. For new families, `photos.edited_as_primary` selects primary naming when true and suffix naming when false (default). Existing bindings retain their recorded naming policy. Pending operations take precedence over committed bindings so interrupted publication finishes under its journaled policy. Processing, queue planning and read-only previews resolve this policy in the layout owner before deriving paths or comparing decisions. The shared selected-rendition renderer assigns current/original-archive roles without changing rendition keys, RAW selection or URL deduplication. The layout owner confirms current provider resources through the Photos adapter, prepares all selected members, preserves displaced media and sidecars as independent exact copies, then conditionally publishes and retires verified obsolete aliases. Default suffix families retain ordinary append-only behavior.

Schema 35 adds independently addressable layout bindings, operations, members, path claims and immutable preservation receipts under `state/db/primary_layout.rs`. Scoped native paths, stable child and pass identity, source/config decisions and a checked binding generation pin each operation. Durable phases are planned, prepared, preserved, publishing and committed; cancellation retains its journal and private prepared files. Recovery runs after filesystem replacement recovery and before ordinary source dispatch. A started handover finishes its recorded generation before new source/config work may plan another operation. Config/path reconciliation performs this recovery before reading catalogue paths, including operations at a previous root or template. First-binding migration inventories each scoped per-path rendition receipt and its own recorded placement independently of current selection or the latest catalogue filename; exact local bytes and the complete path-owner census remain required before preservation or retirement.

Managed recovery stops at the first unresolved operation; later independent
operations in that recovery pass remain pending. Reconciliation retains its
checkpoint hold. A separately completed independent family does not establish
continuation past a blocked operation in the coordinator.

The claims fence ordinary transfer, finalization, adoption and queued metadata. Current-publication retry recovery leaves managed plans with the layout owner and rechecks slot generation and exact local-byte claims in its state transaction, including claims created after the reader snapshot. Metadata for selected renditions is prepared off the visible path; it commits with the new binding and matching provider selection receipts. Ordinary whole-asset metadata refresh resolves ownership across every binding and pending claim for the scoped provider child, including when the caller has no concrete pass key; it does not queue competing writes for managed paths. Explicit metadata refresh uses the preserving owner and records its intent in the operation; pending operations recover before unchanged-binding shortcuts. Retained-source reuse carries the SHA-verified fingerprint through candidate receipt capture and independent copying; a second snapshot must equal the trusted first snapshot. Local path reconciliation uses this same layout owner for managed families. Current/archived media and sidecars must match their pinned fingerprints. Unknown ownership or local changes hold completion and protect existing contents. Real album membership is persisted before managed reconciliation commits matching metadata; an unfiled path label is never recorded as album membership. Original archives retain their prior metadata and retry evidence outside the ordinary writer queue; prepared capture-repair evidence stays visible to source-deletion guards. Archive receipt and whole-asset ownership lookups use scoped library/child indexes. Historical catalogue receipts remain evidence, not permission to write a superseded slot.

Files in a Live Photo family are reserved and prepared together. Publication across separate files is recoverable rather than simultaneously atomic to external readers. Filesystems without the required atomic conditional publication/retirement operations leave durable work pending. See [edited primary usage](../edited-primary.md) for storage, exclusion and downgrade guidance.
