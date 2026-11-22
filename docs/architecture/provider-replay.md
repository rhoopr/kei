# Provider capture and destination replay

Follow retained observations from the provider inbox through the catalog to current destination work.

## Observed incremental shadow capture

After opening an authenticated, account-owned database, sync attaches capture to
its Photos service and libraries. Normal incremental `changes/zone` responses
are bounded and validated before `DeltaRecordBuffer` pairing or selection. The
adapter rejects ambiguous JSON keys and unusable source identity or scope for
the whole page. Original response bytes retain unknown fields, tombstones and
unresolved relationships, including numeric lexemes that a typed JSON re-encode
could alter. Cookies and authentication headers are not captured; resource URLs
remain transient payload, never durable identity.

Schema 29 adds `provider_shadow_pages` and ordinal source identities in `provider_shadow_records`. An immediate SQLite transaction rechecks the independent account owner and commits both together. The former per-scope pointer in `provider_shadow_receipts` has no runtime reader or writer; its schema and historical bytes remain preserved. Provenance includes the
account namespace and authenticated provider fingerprint, versioned realm,
CloudKit container and environment, private/shared database, validated zone and
returned owner when present, request cursor and successor. Unknown zone metadata
is retained in the payload without changing the scope key. Exact page replay is
idempotent; a changed payload remains a separate observation.

Original decoded response bytes are limited to 16 MiB per page. The inbox allows
512 MiB of charged payload and provenance bytes, including source identities.
This is a logical budget, not a physical SQLite/WAL file quota. An oversized
page, full inbox, failed write or unusable identity stops that page before
emission or successor acceptance. Such a refusal propagates through orchestration;
an uncaptured rank inventory cannot replace it and advance the checkpoint.
Existing typed invalid-token fallback and authentication handling remain intact.
Exact already-captured replay remains possible at capacity. Schema 43 releases completed payloads during incremental sync. A validated record-empty poll needs no inbox space. Previously projected empty polls are revalidated and removed during startup recovery or capacity recovery.

A nonempty page gives up its whole inbox charge only when every record has independent durable evidence. For a live photo record, the current published destination must have verified media and any enabled metadata, and its checked lookup confirmation must contain the matching record in the same source scope. A simple, explicitly typed private Primary deletion can use already-deleted local renditions if no family mapping makes the identity ambiguous. Unknown records, unresolved references without that proof, legacy work dependencies and mixed pages with unfinished records keep their bytes. Sync checks on projection, revisits earlier pages while reading the stream, and checks remaining candidates before returning an inbox-capacity error.

The release receipt and removal of the raw body commit together. Original hashes, identities and catalog facts still feed the existing change detector. Released pages retain their original `charged_bytes` for provenance; only pages without a `provider_finished_changes` receipt count against the live inbox. Replay still emits the current provider events. Cleanup does not advance the cursor, change media or shrink the SQLite file on demand. The catalog retains its own separate capacity limit.

SQLite WAL `NORMAL`
remains unchanged; a successful commit does not promise that every acknowledged
observation survives power loss. Network filesystems are not newly qualified.

Schema 36 adds a private-only canonical transport index. The provider adapter
recomputes evidence from the retained page after account validation. It
recognizes a complete `CPLMaster` `fields.resOriginalRes` `ASSETID`, a complete
`CPLMaster` `fields.resOriginalVidComplRes` `ASSETID` in changes pages (the
Live Photo motion resource), and a complete `CPLAsset` or `CPLMaster`
`fields.resComputeStateData` `ASSETID`. Each must have a nonblank `recordName`
and `recordChangeTag`, a numeric `size`, nonblank `fileChecksum`,
`referenceChecksum` and `wrappingKey`, and a validated download URL. Only the
whole validated `downloadURL` token is masked. If an eligible motion URL is
shared with another resource on the record, the page stays raw because URL
aliasing affects rendition selection. Record type, tag, descriptor bytes and
unknown bytes stay exact, and nested or arbitrary `downloadURL`
fields are not eligible. Each candidate is bound to the envelope that produced
its byte path: a changes candidate is resolved only against `zones[0].records`
and a lookup candidate only against the paired root `records`, so a lookalike
record at the same ordinal in another envelope can never lend eligibility or
mask an unknown-envelope URL. This is a provider-versioned asset identity, not a
verified content hash. The private compute path is conditional coverage: it
does not claim that observed reporter records satisfy this descriptor shape.
A scope that lacks the realm, database, zone name, or exact nonblank owner is a
raw observation with no coalescing proof, not a rejected capture. An invalid
private page, malformed scope, or mismatched index entry still fails validation.
The canonical key is an unambiguous hash of account namespace,
provider fingerprint, exact scope, request cursor, successor, `moreComing`,
classifier version and masked body digest, so different cursors or scopes remain
independent classes. Only pages that mask a motion URL use classifier version 4.
Older stored pages are not rescanned: the first rotated motion URL can require
space for one new representative. Pages that mask only original or compute-state
URLs retain their version 3 keys.
A hit reloads the representative through the checked page
loader, recomputes its key and masked bytes, and requires exact metadata and
source-identity equality. Unsupported scopes stay ordinary raw observations;
an invalid private page or a mismatched index entry fails validation. A replay
always reprojects the durable source, so an interruption after capture but before
projection recovers on the next equivalent observation.

The transport-index schema step creates and validates its tables without compacting historical bodies. Schema 38 retires historical pages before that step; the legacy compaction algorithm, dependency-validation checks and telemetry event path have been removed. Historical diagnostic fields remain readable. New cycles record `not_observed`, zero event counts and unknown byte-reclaim totals. Current capture still uses the canonical index to avoid repeated equivalent observations.

Current page loading has one raw-byte path: it validates the stored owner, continuation flag, size bound, SHA-256 and ordinal identities. Current capture verifies exact or provider-classified equivalent raw observations against that loaded page. Historical alias records remain in the live upgrade snapshots. Their original response bytes are retired after [upgrade verification](startup.md#owned-schema-upgrade-backups). Neither has a runtime replay consumer. The empty legacy-alias table remains in the schema for compatibility with existing schema shapes.

A shadow receipt is only the last observed page, and replay can move it back. It
is not a coverage frontier, materialization receipt or verified filesystem
progress. Existing `CheckpointEvidence`, sparse generation proofs,
recovery debt and filesystem guards retain checkpoint authority. Capturing a
tombstone cannot authorize local media deletion. Existing download and metadata
queues still consume the current stream. Capture alone cannot admit queue work;
the bounded retained-source admission in
[Current provider work admission](#current-provider-work-admission) is a
separately validated owner.

Capture covers only incremental pages observed by existing sync in selected
scopes. Rank inventories, bootstrap, lookup and deletion-validation scans are
not represented as complete zone capture or historical completeness. Rank EOF
plus a delta bridge is not an atomic snapshot or proof of absence. New future
observations cannot retire old expired-epoch debt. Complete queue projection,
capture-owned cursors, durable catalog selection and separate progress remain
later stages; the bounded current-generation slice in
[Current provider work admission](#current-provider-work-admission) does not
complete them. Historical-version retention, unrecoverable-debt acknowledgment,
epoch recovery and compaction still require explicit policy and support proof.
The additive migration preserves existing rows and uses the migration owner's
savepoint. Re-entry validates the table columns, primary keys, replay key and
SQLite-assigned page identity required by source and receipt links. A conflicting
unknown table fails without partial schema or version changes.
Older binaries supporting only schema 30 must refuse the current database.

## Transactional source catalog and replay

Schema 30 adds `provider_catalog_records`, literal source references in
`provider_catalog_references`, unresolved indexing evidence in
`provider_catalog_debt`, and versioned per-page receipts in
`provider_catalog_pages`. Records refer to the original captured page and source
ordinal. All payload versions remain separate observations; replay order and
opaque provider cursors do not establish a current version. Original inbox bytes
retain fields that the catalog does not interpret.

Startup replays pending captured pages before planning libraries, one page at a
time with cancellation between transactions. Live capture indexes its committed
page before lossy pairing. The provider adapter revalidates account-bound scope,
original hash, page metadata and source identities, then derives facts on the
blocking pool. Read transactions provide a coherent source snapshot; the immediate
write transaction rechecks ownership and reloads the source before inserting all
records, reference facts, unresolved evidence and the final receipt together. A
failed projection retains the captured page and existing checkpoint for restart.
It cannot trigger an uncaptured rank fallback.

Unknown kinds, typeless tombstones, malformed references and incomplete asset or
container links retain unresolved evidence. A literal target name is not proof of
that target's identity or materialization. No tombstone authorizes local deletion.
Receipt version 1 means source facts were indexed, including unresolved evidence;
it does not mean selected, downloaded, rewritten, verified, historically complete
or acknowledged recovery debt. Existing queues also accept the bounded current
work admission in
[Current provider work admission](#current-provider-work-admission); source
indexing alone cannot authorize it.

Derived page facts have a 16 MiB logical charge limit and a separate 512 MiB
catalog index budget. These are operational backpressure limits, not physical
SQLite/WAL quotas or a retention promise. A full index holds the checkpoint and
retains pending source observations; exact completed-page replay validates facts
and remains possible at capacity. Startup has no fixed latency guarantee. Nothing
is automatically pruned or discarded. WAL `NORMAL`, filesystem qualifications,
existing checkpoint evidence, publication receipts and metadata obligations are
unchanged. Historical intermediate retention and unrecoverable-debt acknowledgment
still require explicit policy decisions before dependent stages.

## Current provider work admission

Provider-discovered work is admitted by the [current selection and destination owner](#private-selection-and-destination-replay). Historical catalog receipts, retry deadlines, and raw evidence remain readable. They cannot admit work or wake the watch loop.

## Selection generation shadow

The schema-33 tables `provider_selection_generations`, `provider_selection_sources`, `provider_selection_decisions` and `provider_selection_destinations` remain as historical evidence. Schema 38 archives prior rows at the fresh-start boundary. Their observational writer, manifest replay and capacity gate have been removed. Current provider confirmation and task planning feed active destination admission. The later catalog admission writer `project_provider_work` also compiles only in historical-database tests.

`provider_selection.rs` retains the shared decision, outcome, destination and lossless native-path types used by active generations. Their serialization is unchanged. Active destination publication, work retry, source checkpoint gates and raw-inbox capture keep their existing owners. Support history retains the selection storage field for compatibility; current cycles leave it unobserved.


## Private selection and destination replay

Schema 34 adds active generations in `provider_active_generations`, original
validated rank pages and record indexes in `provider_selection_rank_pages` and
`provider_selection_rank_records`, bounded decisions in
`provider_active_decisions`, source links in `provider_active_sources`, and exact
physical obligations in `provider_active_destinations`. These tables supplement
the schema-33 shadow store and the existing source inbox and catalog.
`icloud/photos/album/selection_capture.rs` validates complete bounded query
responses before the existing pairing and filtering stream consumes them.
`download/orchestration/generation.rs` composes existing selection policy,
current confirmation, transactional queue projection and destination replay.
`state/db/provider_generations.rs` owns stored declaration, provenance,
admission, frozen metadata intent and completion validation.

Activation requires an authenticated account-owned database and one exact
private or shared provider scope with an explicit nonblank owner. The exact discovered owner remains part of request, response and durable scope validation; `_defaultOwner` is not a separate eligibility rule. Every selected pass must
be a supported named album, Unfiled query or provider-defined smart query
using the same concrete store and membership scope. Named albums may bind authenticated foreign media sources in that store. Unsupported
ordinary writable profiles are refused before dispatch. Both database scopes use the same validated source admission and per-destination completion owner. Database identity remains part of the durable scope; a receipt in one database cannot complete work in the other. Metadata backfill and read-only execution
keep their existing owners. Ordinary
explicit metadata refresh with preservation can use active destinations.

Dispatch no longer runs legacy catalog admission. Inventory or replay admits and completes work per selected destination. Watch precheck uses that owner's wake decision directly; legacy catalog work cannot add a wakeup or veto it.

An Unfiled pass label can render a destination path but cannot establish album membership. Destination admission excludes that label from both membership insertion and grouping-conflict checks; a current path decision is not provider membership evidence.

Album discovery retains raw provider ancestry in `PhotoAlbum::selector_components()`. Qualified selectors match the full ancestry; leaf selectors can select same-named containers. Ambiguous qualified selectors and incomplete discovery fail before planning. Path rendering sanitizes each component independently. Album and smart-folder names retain separate selection identities.

Discovery walks `CPLAlbumByPositionLive` breadth-first in the authenticated library zone. Explicit `albumType=3` records supply child queries through string `parentId`; type 0 records supply album passes. Root sentinels are skipped. Repeated identities or cursors, mismatched parents, unknown explicit types, and traversal limits fail discovery. Missing type retains leaf-only compatibility. The field/query contract is documented in [the iCloud Photos Sync API reference](https://icps.steiler.dev/dev/api/#albums-and-folders); [the icloud-docker nested-album fixture](https://github.com/mandarons/icloud-docker/commit/9374bad0eba38d86e0791efc22b5e99024ad9b6d) independently records string parent references.

`BoundAlbumSelection` carries prior published selector-to-container evidence through startup, watch and indexing refresh. Explicit same-zone container tombstones can recover a previously bound missing selector. Missing discovery and configuration deselection never authorize retirement. Zero-pass recovery uses authenticated exact photo lookup and an empty existing selection generation; it neither enumerates ranks nor advances the provider cursor. Historical headers validate immutable bindings, while current execution rechecks durable deletion status.

Completed membership deltas publish required selected destinations before retiring obsolete owned copies. Whole-photo deletion takes precedence, and unknown identity keeps recovery pending. Native retirement seeds the existing layout journal only at departure, using admitted current-publication receipts with exact source, path and hashes. Preserved-local dispositions and shared-path claims cannot authorize native retirement. Modified files and adjacent unowned sidecars remain in place. Retirement clears only the exact native publication authority in the layout commit transaction and retains historical verification receipts.

The shared filesystem retirement owner checks media and sidecar disposition together, including an expected absent sidecar, before each move and after the guarded no-replace rename. Journal recovery applies the same checks to visible and already-retired entries. A late sidecar, changed owned sidecar or namespace change triggers no-overwrite restoration through retained directory capabilities. Media is restored before metadata when possible; a later writer is never replaced. Failed or partial restoration retains saved entries, claims and the pending layout journal. A preserved member receipt is revalidated on resume and does not bypass the retirement checks.

Ordinary writable dispatch now requires a qualifying current-selection context for every nonempty pass set and validates the database's authenticated pin before recovery or cleanup. Unsupported query definitions and missing scope return an actionable error without dispatcher writes or provider requests. Provisional/indexing execution and previews retain their separate contracts. Empty pass sets return without work or a candidate checkpoint. The rendition-wide tail is now invoked only for read-only planning; writable startup resets and pending-to-failed promotion have been removed from dispatch.

Current-selection attempts and due times remain with the destination owner; unrelated historical rendition failures are preserved. Dry-run and filename previews do not reset retry state even when a caller supplies an existing store. Provisional/indexing inventory retains its incomplete-evidence restrictions.

Managed-layout completion does not re-enter legacy catalog admission. Destination completion cannot turn unrelated historical pending rows into failures. Read-only legacy retry planning still reports planned retries and unresolved identities without running that promotion.

Managed inventory projects selected destinations before publishing files. After collection freezes membership and native metadata intent, the existing bounded destination replay runs the managed journal with current stored groupings. Native acknowledgement requires the exact binding, metadata recipe and verified media/sidecar fingerprints under destination locks. A journal commit alone cannot complete native metadata before its intent exists. Preserved unselected originals retain their journal protection without acquiring a current selection obligation.

Ordinary authenticated current selection reads historical metadata-capture summaries for reporting without scheduling historical repair or using unresolved historical counts to block its checkpoint. Selected identity, publication and metadata obligations still gate advancement. Explicit refresh and compatibility execution retain their capture-repair policy. Historical rows, retry evidence and ownership records remain durable.

Selection replay reuses independently verified destination publications before task execution. A remaining destination task bypasses rendition-wide on-disk adoption and uses normal transfer/publication finalization. This lets a landed file recover after a failed finalization transaction without marking only the aggregate asset row complete and stranding its destination obligation. No-overwrite publication and exact-path completion guards remain active.

Local replacement policy is per destination. An established copy whose bytes still match its verified Kei receipt can receive the current provider resource without treating Kei's filesystem write time as a local edit. A changed copy requires a strictly newer, resource-specific provider edit time. Equal, newer-local and unknown times preserve the local copy. The Photos adapter decodes `adjustmentTimestampEnc` as a bounded plist Date only for an adjusted still supplied by CPLAsset. Originals, alternatives and video resources have no proven edit clock. CloudKit record modification time never authorizes media replacement.

Schema 39 adds nullable typed `preserved_local` evidence to the existing native destination row. Its intent binds the generation, configuration, pass, child, rendition and path; its observation records the local checksum, size and optional filesystem mtime. Preservation resolves media policy while leaving `verified_media` false and `source_checksum` absent. Preserved copies cannot supply current-provider source reuse. Metadata completion retains this disposition and the original local mtime. Missing copies reopen destination work. Managed copies use their existing layout journal and preservation-backed publication owner.

Timestamp-authorized publication validates displaced bytes and mtime before accepting replacement. On Linux filesystems without atomic exchange, the existing replacement journal checks both from the same confined handle after displacement and before its commit marker. Interrupted uncommitted journals restore the displaced file and its actual timestamps. The clock belongs to the active attempt; recovery never infers timestamp authorization. Windows uses its existing replacement backup and the common displaced-file mtime check. Metadata writes remain retryable if the original local mtime cannot be retained.

The unchanged-output exemption requires `current_publication` authority on the existing native destination row, including checked metadata output. Schema 39 adds this marker with an unknown-by-default migration and a unique partial index per library and lossless path. The destination owner retires prior authority and assigns the checked output in the publication transaction. Historical verification remains intact. Receipt reuse and generation creation time cannot establish authority. Raw SHA-256 recovery can finalize an unfinished exact destination after publication lost its state transaction. Restoring an older published file counts as a local edit. Managed copies also retain their current layout binding, and independent peers cannot borrow a catalogue-wide receipt. Legacy metadata completion without a validated native receipt invalidates obsolete native authority.

Schema 41 adds nullable embedded-field receipts to existing path and native-destination rows. Migration grants no field ownership. Initial downloads commit measured fields with installed-output finalization; identical-existing publication retains only independently valid current ownership. Metadata rewrites freeze input/output evidence before publication and complete it in the existing metadata transaction. Directory members and managed bindings carry that evidence through checked moves and copies. Checked preserved-local metadata completion uses the same current-publication marker to retain the latest physical output and field ownership across selection changes. It retains `verified_media=0`, no source checksum and its preserved-local disposition. Provider-source reuse, unchanged-provider replacement exemptions, directory handoffs and retirement require non-preserved verified media. Selection alone cannot promote a preserved copy; changed local bytes revoke its previous metadata authority.

Downloaded-state completion requires parent-directory durability. `download/finalize.rs` calls the strict async wrapper around `fs_util::fsync_parent_dir` before both immediate receipt writes and deferred retries. Native destination reuse and pending-publication adoption apply the same boundary after their existing confined-file checks and before state acknowledgement. A parent sync failure retains landed bytes and unfinished debt. Exact-byte recovery retries the sync before acknowledging those bytes, without another media transfer when existing provenance guards permit reuse. Identical `.part` collisions do not bypass this boundary. The wrapper runs on the blocking pool and preserves the synchronous helper's platform policy, including its non-Unix no-op.

Selected task planning can replay an exact durable reservation in another folder beneath the current root. It uses the existing retry reservation checks for owner, rendition, provider content and path confinement, while retaining protected-identity relocation. A reservation preserves a destination choice only after current selection admits the asset; it does not create selected membership or authorize adoption of unproven bytes.

A sealed, checkpoint-ready selection with no decisions can survive a delta containing only applied deletions of known assets. Dependency comparison ignores those tombstone-only pages for this empty-selection reuse, without changing the original generation, seal or retained provider evidence. Unknown tombstones, typed membership deletions and mixed/live pages retain their dependencies and require revalidation. Nonempty selections retain their existing invalidation rules.

Before selected planning, reserved cross-folder recovery uses the existing retained-file adoption proof. It requires the recorded local hash at that exact reserved path, rechecks ownership and the retained file, and preserves any original download hash across metadata-altered size. A failed write or changed file holds the selected work. Native destination reuse can acknowledge a prior adoption without inventing download provenance; a prior metadata publication requires matching canonical and per-path download hashes. Equivalent root spellings use the same component-normalized receipt path.

A provider revert can reactivate an earlier native publication at its original path. If the canonical row is pending for that version, reuse restores it only when an independent path source receipt matches the provider checksum, size and local fingerprint. The canonical transition and native destination acknowledgement share a transaction. Import-only receipts cannot create download provenance through this path.

For destination-owned selection, HTTP 410 cancels the transfer batch through a child cancellation token. Enumeration continues to project current membership and run deferred unfiled passes. Cancelled transfers enter the existing exact URL-refresh cleanup with their destination bindings intact. Checkpoint release still requires complete query coverage and completed destinations. User shutdown, authentication failure and state-write circuit failure cancel both enumeration and transfers.

Authenticated current-selection previews do not append the aggregate pending-retry preview. They count or print current selected paths once and preserve historical failures without treating those failures as current preview debt. Preview sessions without authenticated selection scope retain the compatibility path.

CLI dry-run and filename-only modes share read-only startup. They skip configuration creation, destination writability probes, PID files, credential saving, support history, health/report files, and notification scripts. Authentication loads existing session data without a writable account guard; response cookies and session headers remain in memory. Corrupt authentication files remain intact and cannot supply cached validation. Expired authentication stops the preview before recovery can delete caches or strip routing state. Writable authentication retains its lock, generation checks, and persistence path.

Compatibility dry-run and filename planning read existing retry evidence without changing the supplied store. Targeted lookup and legacy hydration may resolve preview paths, but planning does not backfill mappings, claim owners, adopt local files, persist reservations, revalidate policy-excluded deletions, or update verification and retry rows. Full previews preserve enumeration-interruption markers; incremental previews skip startup mapping backfill. Unknown and deleted lookup results leave durable evidence intact.

After confirming a current provider selection, the producer can recover a pending or failed aggregate row whose exact prior publication still matches its source, local hash, size and ownership. It uses the existing `pending_publication` transaction before destination planning, preserving the original publication timestamp, receipts and retry count. The selected destination still needs its independent completion proof. Healthy downloaded rows and managed layout journals stay with their existing replay owners. A failed recovery transaction blocks that asset without transferring comparison media; conflicting reserved paths retain an enumeration hold.

If a raw file lands before its destination finalization transaction fails, destination reuse can finish it without another transfer. The current admitted path, provider SHA-256, local SHA-256, size and asset facts must match. This recovery is limited to non-imported pending or failed rows whose canonical publication is absent or still points to another path, or downloaded rows with a recorded publication at a different path. The destination must have no prepared output. Matching raw media does not complete metadata intent; selected sidecar and embed obligations remain with the metadata owner. Same-resource root drift can retain the old downloaded row when new-path finalization fails. Recovery commits downloaded state, the new path source receipt and native destination verification together through the existing publication guards, preserving the old path receipt. Opaque provider checksums and metadata-rewritten media still require their existing receipts. A failed recovery transaction retains the checkpoint hold and does not download comparison media.

Download context loads both pending and failed rows as unfinished identity and path evidence. Current selection still owns admission. Loading failed evidence neither resets retry counts nor changes durable status; it preserves master/child ownership and exact prior publications after failure.

After selection binds a task to its final destination, an explicit truncation-repair request can attach the existing replacement fingerprint. The saved path, provider checksum and size must match that selected task. The existing repair owner checks the durable truncation reason, prior publication, regular-file fingerprint and shortened size. A changed provider version or another destination receives no replacement authority. Publication still rechecks the fingerprint before replacing bytes.

Before freezing a selected repair destination, the planner retains the recorded path through its existing reservation guards. If provider content changed, it chooses a safe sibling and leaves the damaged historical file untouched. Managed layout journals keep their own path and replacement authority. A persisted exact child/master owner survives provider checksum changes; claiming an unowned legacy master still requires matching content and identity evidence.

Native selection can retain an established one-child master mapping through the existing legacy owner claim transaction. The selector requires matching current content and added-date evidence, rejects protected state and competing historical children, and keeps the recorded retry identity and reserved destination. An unmapped historical master alone cannot authorize a native claim. Persisted owner receipts continue to govern later content changes.

Destination metadata completion settles the canonical path's local checksum through the existing prepared-output settlement owner in the same transaction. The update requires the exact prepared output, path, metadata generation and prior local checksum. Original download provenance stays unchanged; a different path or concurrently changed canonical row is not rewritten.

Destination reuse also accepts an exact durable import whose canonical row and path receipt agree with the independently measured local checksum, imported size and current provider version. Its local pre-rewrite input becomes the destination's source fingerprint. The canonical download checksum remains absent. Changed bytes, another path or another provider version cannot use this import proof, and reuse fetches no comparison media.

Explicit capture-local timestamp repair also uses current selection for destination scheduling. The existing metadata writer receives the requested repair policy. A selected destination with its own exact prepared-output receipt uses that publication owner; it does not require a second legacy capture-repair receipt. Rows with a legacy repair receipt retain that receipt's validation and finalization. Timestamp repair still requires a usable provider offset and verified local bytes, and ordinary sync preserves existing timestamps.

Provider confirmation resolves a selected observation against its bound media source. Owner-library rank observations inherit the authenticated query scope; cross-zone hydrated observations carry an explicit source zone. The media source must share the selection owner's authenticated account and concrete state store. Paired lookup validation remains in `icloud/photos/album/work.rs`. Cross-zone albums refresh current inventory on incremental cycles because the membership zone's delta does not cover foreign media changes. This reuses the smart-folder refresh path.

If a rendition changes during the first inventory, kei defers that asset. On retry, the inventory and a fresh lookup must agree on the current rendition. Admission can then use that revision if the retained observations all identify the same child and master in the selection's own zone, the generation basis is unchanged, and the zone has neither a saved cursor nor a reconciliation request. This resolves only the asset; the normal completion checks still govern the checkpoint.

New selected decisions store an explicit `media_source` scope and zone from the authenticated provider binding. Retained decisions omit that field and retain their exact encoding and single-zone meaning. Replaying a retained decision preserves its original binding representation. Durable admission accepts a foreign media source only when the pass profile declares that exact binding, its account context matches, and all selection sources are live membership relations for the selected child. Confirmation, mapping guards, destination rows and publication receipts use the media zone. Query evidence keeps the membership zone. Profile construction uses the same provider-owned account, concrete-store and realm validation as confirmation. Sources retain the complete provider zone and are sorted by scope; duplicate zone names are refused. Empty source lists remain omitted to preserve existing single-zone configuration fingerprints.

The generation source store also accepts original album-owner membership scan pages for a named-album pass. The provider adapter validates changes-page scope, record shape and continuation before capture, then captures before emitting records. Hydration task clones retain this capture context. Rank and membership pages share request/response retention and ordinal indexing. Membership requests must match the generation's owner zone and named-album pass; they are not checkpoint authority. Source linking resolves live relations to their child IDs only for the selected container. Wrong-album and deleted relations do not select a child. Original record names, response bytes and ordinals remain unchanged. Membership-backed decisions use the existing admission and replay validators.

Failed confirmation preserves the observed child's authenticated media binding in the deferred decision. Retained-generation recovery can look up that exact bound identity when membership evidence contains no reconstructable media facts. Resolving it requires an unchanged generation basis, the same media binding, validated original membership links and fresh paired confirmation matching the proposed decision. Missing or mismatched proof leaves the original decision intact. Cancellation and authentication failure stop recovery; neither completes the identity.

A generation binds account and provider ownership, full zone identity, selected passes and queries, source dependencies and selection, rendition, path and metadata configuration. Lossless native destination paths are independently anchored. A rank page records its exact request and original response bytes, including unknown fields and tombstones. Rank pagination remains an observation window. EOF plus a delta bridge does not establish a snapshot or prove absence. Current paired confirmation determines resource identity; observed order and opaque rank or source tokens do not select versions. Ambiguous or incompatible facts remain deferred. Existing fixed-date, exclusion, RAW, companion and naming owners decide eligibility. Sync has no newest-N frontier or per-filter count cap.

An immediate projection transaction commits the validated decision, source
links, per-pass/rendition/path obligations, existing queue records, mappings and
grouping changes together. Unfinished old versions and frozen grouping/writer
intent fence incompatible newer work. Independent healthy work can proceed.
A changed grouping alone does not block a missing media download. Under the
destination lock, a selected metadata writer can take over an older intent
when a confirmed album membership explains the entire grouping change and
both intents identify the same resource, path and metadata settings. The old
row retains its frozen hash and media receipt. Only the writer's normal
completion settles the replacement. A display name, an unfiled observation
or an unexplained people change cannot authorize the transfer. A prepared
output stays with the writer that prepared it.
Current query coverage is sealed only from that generation's own evidence;
replaying a newer inventory cannot seal an interrupted old window. Configuration
or source changes create a distinct generation or require existing validated
reseed. They cannot acknowledge omitted eligibility or old unresolved debt.

For an unfiled Shared rendition with no established destination, a collision
suffix can conceal an ambiguous migration match. If the unsuffixed path is
occupied, admission checks downloaded Primary catalog rows at that exact path.
A matching rendition, provider checksum and size without a verified Primary
destination defers the Shared decision as `ambiguous_primary_archive_path`.
It neither claims the original nor transfers a suffixed copy. Other children
can finish, and an eligible retry checks the collision again.

Named albums and smart folders keep independent physical copies even when their templates render the same directory. Current selection qualifies colliding filenames with the stable pass key and retains that exact destination on replay. Planning keeps each selected rendition eligible until its own destination is verified; adoption cannot skip it using another selected album or smart folder's path. Same-pass template drift and managed bindings retain their established paths. The immediate admission transaction checks competing album and smart-folder ownership before every transition to admitted, including confirmed selected declarations deferred by an earlier conflict. A conflict retains the frozen declaration; a racing producer with an unfrozen decision can replan once through the existing sibling guards. Historical shared-path declarations remain unchanged and trigger a successor inventory. Their original receipts and metadata work retain their existing owners. The declaration's `independent_album_copies` marker distinguishes current destination policy from historical replay without changing the user configuration fingerprint or adding a ledger. Unfiled behavior is unchanged.

Destination media completion requires an actual confined file fingerprint and
independently verified selected source checksum. An exact active physical
receipt remains reusable as source evidence and for compatible historical aliases after current selection and file verification; a new album destination retains its own obligation. Opaque provider
signatures remain independent from source and local SHA-256. Fresh or legacy
fallback publication requires a separately verified per-path source receipt;
validated native reuse relies on its own lossless generation/destination receipt,
including operation-owned prepared metadata output. Imported generic
local checksums cannot establish source provenance. Metadata completion is
separate and uses the existing option-aware rewrite owner with the exact native
path, frozen grouping and writer flags. Disabled flags cannot finish older
writer debt or enable writes implicitly. Before an embedded replacement, the
active receipt stores the measured prepared output tied to the exact intent,
input and source. Reopen can recognize only that operation-owned output;
arbitrary damage cannot become publication proof. Filesystem publication and
receipt finalization retain their existing compare-and-swap guards.

Legacy TEXT paths remain compatibility projections. A non-UTF-8 receipt cannot
update a checksum or clear markers through a lossy text path. Both legacy
metadata queues retain ambiguous native-path markers without dispatching that
text path to a sibling file; exact native work uses its active receipt. These
markers are retained evidence, not an acknowledgment of another physical path.
XMPFiles retains its existing UTF-8 filename requirement. Native-capable HEIF
and sidecar owners use native paths without granting additional rewrite options.

Startup and watch can wake unfinished selected work even when the canonical
asset row already has one downloaded destination. An unchanged completed
coverage window reuses exact selection and publication evidence. Historical
scheduling visits one due old root per cycle, at most 64 decisions and 16 MiB of
serialized manifests, with a persisted continuation and retry deadlines. Within
that transaction, each unique original rank page is strictly decoded and its
complete ordered record index audited once. Compact page proofs are discarded
when the operation ends; new reads use a stable read transaction. Each replay
validation operation can read up to 512 MiB of unique serialized input, without retaining all
parsed pages. Original-fact reconstruction separately limits retained serialized
page inputs to 64 MiB; unavailable bounded evidence remains deferred debt.
Album source discovery checks candidate pages one at a time in the same read transaction and retains only matching ordinals. Unrelated membership pages do not spend a child's decision-validation allowance.
The current generation receives a bounded priority metadata lane; remaining writers
share the existing physical writer budget and persisted general continuation.
HTTP 401, 403 and 421 current-confirmation failures stop additional hydration in
the selected run and preserve the existing bounded reauthentication route.
Non-authentication proof failures retain their own deferred debt. Active cleanup
reconstructs original failed tasks' child/master and provider-rendition binding
from their validated declarations, then uses the existing exact URL-refresh
owner. Historical RAW original/alternative relabeling is resolved by a unique
checksum-and-size match in the frozen provider response, independently of the
current configuration. Generation replay restores historical logical RAW keys
from the frozen destinations before projecting records or dispatching tasks.
Pending-record metadata follows the resource's provider key. New selection
never swaps original and alternative keys. Ambiguous siblings remain unresolved; other rendition
keys require an exact key match. A refreshed URL cannot replace frozen selection, metadata, path or
publication authorization. A transient per-invocation registry composes failed
historical and current runs; it is not durable identity or a separate memory quota.

The active store grows with the selected library. Existing evidence does not consume the allowance for the next write, including after an interrupted sync. Each individual charge is limited to 512 MiB; pages and manifests retain their 16 MiB limits. A failed write rolls back the whole observation or projection. No partial page can authorize progress. Logical charges do not bound SQLite file or WAL size.

During writable sync, the generation owner verifies the seal of each checkpoint-ready generation in scope before retiring its raw rank responses. Deferred identities, unfinished destinations or metadata, prepared outputs, and confirmed repairs keep the original responses. For a settled generation, the transaction replaces each response with its ordered selected-child list. A receipt binds that list and the complete ordered record index to the original page ID. Source checks and seal verification use this receipt on reopen. The original confirmation inside each decision still supplies asset and rendition evidence for later repair. Checkpoints, files, and publication receipts are untouched.

A later sealed, checkpoint-ready generation can supersede an unverified planned path when it has completed the same pass, child, master and rendition at a different path. Scope, configuration, provider checksum, size, metadata and dates must match. The earlier row stays admitted and unverified as historical evidence; it is neither deleted nor marked published. That row no longer creates retained replay work, asset-write conflicts or checkpoint debt. Prepared metadata outputs, incomplete current publications and deferred identities retain their guards. Scheduling validates the scope's retained decision proofs in the same read transaction before using this rule.

A changed managed rendition can also retire an older decision's admission after fresh authenticated confirmation of the same account, store, source, pass, child and master. Every old destination must match a cancelled pre-publication layout operation and a current selected rendition. The layout owner verifies retained media and sidecar fingerprints before projection. The projection transaction admits the successor, records its manifest and cancelled-operation hashes in the old decision's admission reason, and withdraws the old destinations' admission together. Any admission failure rolls back both changes. The original manifests, paths, retry evidence, journal members and publication receipts remain intact; retirement never marks media or metadata complete. The shared decision validator checks this proof on reopen, and the existing admitted-destination filters retain ownership of scheduling, grouping, metadata and checkpoint debt. Publishing journals, native prepared metadata outputs, unknown provenance and unrelated identities remain fenced. Managed current collection precedes historical replay so obsolete work cannot publish before this decision.

Captured observations, admitted decisions, verified media and verified metadata
are separate durable facts. CLI download counts retain the existing task
semantics and do not claim unique-file counts or full-zone completion. Active
progress cannot replace `CheckpointEvidence`, sparse generation proofs,
filesystem guards or source checkpoint policy. Selection checkpoint debt uses
the same authenticated scope and configuration as retained recovery. Unresolved
decisions from other configurations remain evidence without independently
vetoing the current successor. Unsettled prepared metadata outputs at overlapping current
destinations retain their veto across configuration changes. Admission,
publication and journal guards retain their cross-configuration protection.
Local wake-up cannot invent a provider cursor. Tombstones do not
authorize local media deletion. WAL `NORMAL`, filesystem support and power-loss
limits remain unchanged. Capture-owned cursor promotion, historical intermediate
retention, unrecoverable-debt acknowledgment, expired-epoch replacement,
full-scan absence and compaction require separate decisions and qualification.
