# Architecture

kei is a Rust CLI that transfers iCloud Photos media and metadata to local
storage. This guide identifies the owner of each major decision and the
boundaries that protect user data.

Use it as a starting map. Read the owning module and its direct callers before
changing behavior.

## Design rules

- User media and metadata must not be lost, corrupted, truncated, overwritten,
  or silently discarded.
- Local file and metadata rewrites are opt-in.
- Provider-specific parsing and identity rules stay in the iCloud adapter.
- Sync policy stays in the sync and download orchestration layers.
- Path rendering does not decide whether an asset should sync.
- SQLite transitions and provider checkpoints must survive interruption.
- Prefer the smallest complete implementation in the owning module.

## Owners

| Area | Owner | Boundary |
|------|-------|----------|
| Process startup and dispatch | `src/lib.rs` | Starts the runtime, resolves bootstrap paths, configures logging, dispatches commands, and maps exit codes. |
| CLI shape | `src/cli.rs` | Defines clap arguments and parsing. It does not execute command behavior. |
| Runtime configuration facade | `src/config.rs` | Preserves configuration types, entry points, and visibility. |
| Configuration input | `src/config/input.rs` | Owns the TOML schema and file loading. |
| Runtime policy types | `src/config/runtime.rs` | Owns resolved configuration types, media selection, and date-bound semantics. |
| Configuration resolution | `src/config/resolve.rs` | Resolves TOML, environment, and command inputs into runtime policy, including shared sync/import path fields. |
| Configuration paths | `src/config/paths.rs` | Resolves bootstrap, data, and credential paths and validates download directories. |
| Folder-template validation | `src/config/templates.rs` | Owns default category templates and validates token placement. |
| Configuration persistence | `src/config/persistence.rs` | Projects runtime policy to TOML and writes first-run configuration. |
| Selection grammar | `src/selection.rs` | Parses album, smart-folder, library, exclusion, and unfiled selectors. |
| Sync/watch facade and runner | `src/sync_loop.rs`, `src/sync_loop/runner.rs` | Preserves command-facing entry points; the runner composes startup, sync cycles, and watch control. |
| Sync session recovery | `src/sync_loop/session.rs` | Coordinates authentication, 2FA recovery, library initialization, and idle session-lock handoffs through the shared auth and command owners. |
| Database change pre-check | `src/sync_loop/precheck.rs` | Owns scoped database tokens, selected-zone checks, and inclusion of pending local work. It does not replace the per-zone checkpoint gate. |
| Sync plan refresh | `src/sync_loop/planning.rs` | Resolves and refreshes library pass plans through the command owner; reports shared-library notices and path warnings. |
| Local drift checks | `src/sync_loop/reconcile.rs` | Runs bounded local drift recovery and periodic read-only catalog diagnostics. |
| Watch policy | `src/sync_loop/watch.rs` | Owns watch cadence, metadata follow-up timing, and one-shot recovery rules. |
| Cycle ledger reporting | `src/sync_loop/reporting.rs` | Records cycle-run summaries and combines refresh-tail outcomes for reporting. |
| One sync cycle | `src/sync_cycle.rs` | Chooses source enumeration, reconciles config drift, dispatches each library, and advances or preserves provider checkpoints. |
| Library and pass planning | `src/commands/service.rs` | Resolves libraries, collection scope, album plans, smart folders, unfiled passes, and cross-zone hydration. |
| iCloud Photos adapter | `src/icloud/photos/` | Owns CloudKit records, queries, change streams, provider identity, albums, smart folders, and metadata decoding. |
| Album facade and counts | `src/icloud/photos/album.rs`, `src/icloud/photos/album/counts.rs` | Preserves album configuration and entry points; counts owns count queries and same-library batching. |
| Targeted provider lookup | `src/icloud/photos/album/lookup.rs` | Resolves requested records and merges identity evidence conservatively. |
| Provider enumeration | `src/icloud/photos/album/planning.rs`, `src/icloud/photos/album/enumeration.rs` | Planning selects rank ranges and page sizes; enumeration starts fetchers and delivers completion tokens. |
| Provider page fetching | `src/icloud/photos/album/fetch.rs` | Owns query construction, empty-page probes, record pairing, deduplication, and asset emission. |
| Enumeration completion | `src/icloud/photos/album/completion.rs` | Collects fetcher completion evidence and requires unanimous tokens before returning a checkpoint candidate. |
| Album hydration | `src/icloud/photos/album/hydration.rs` | Matches named-album members across source zones and hydrates durable asset or master identities. |
| Provider change streams | `src/icloud/photos/album/changes.rs` | Owns sequential change scans, delta buffering, and last-good-token delivery. |
| Download facade and dispatch | `src/download/mod.rs`, `src/download/orchestration/dispatch.rs` | Preserves download entry points and composes full, incremental, targeted-backfill, and durable-retry work. |
| Download models and configuration | `src/download/orchestration/models.rs`, `src/download/orchestration/config.rs` | Owns controls, results, per-zone checkpoint evidence, reporting projections, checkpoint reasons, coverage fingerprints, and configuration hashes. |
| Download context and selection | `src/download/orchestration/context.rs`, `src/download/orchestration/selection.rs` | Loads library-scoped state and identity evidence, derives pass configurations, and checks incremental routing eligibility. |
| Full enumeration | `src/download/orchestration/full.rs` | Owns bounded enumeration, album snapshots, pass counts, and token evidence. |
| Incremental enumeration | `src/download/orchestration/incremental.rs`, `src/download/orchestration/delta.rs` | Runs separate streaming and collecting strategies with one shared delta-state owner for provider-event bookkeeping and routing facts. |
| Download recovery | `src/download/orchestration/recovery.rs`, `src/download/orchestration/url_refresh.rs` | Runs durable pending recovery and refreshes exact download tasks with current provider URLs. |
| Local catalog maintenance | `src/download/orchestration/reconciliation.rs`, `src/download/orchestration/cleanup.rs`, `src/download/orchestration/maintenance.rs` | Reconciles catalog paths, removes durably owned stale temporary files, and repairs metadata-capture revisions. |
| Asset planning | `src/download/planner.rs` | Applies filters, derives tasks, records dispatched pending work, and persists membership and identity mappings. |
| Filter facade and configuration | `src/download/filter.rs`, `src/download/filter/config.rs` | Preserves filter entry points and shares path and filter settings between sync and import. |
| Asset eligibility | `src/download/filter/eligibility.rs` | Owns content, date, and filename filters and media classification, without path or filesystem decisions. |
| Rendition selection | `src/download/filter/versions.rs` | Owns RAW alignment, primary and companion selection, and selected-rendition metadata keys. |
| Task metadata | `src/download/filter/metadata.rs` | Builds metadata payloads with preloaded album and people groupings. |
| Expected asset paths | `src/download/filter/expected_paths.rs` | Derives paths shared by sync and import before collision resolution; uses the rendition-selection owner. |
| Collision planning | `src/download/filter/collisions.rs` | Owns normalized path claims and existing-file collision resolution. |
| Download task derivation | `src/download/filter/tasks.rs` | Composes eligibility, expected paths, collision results, and metadata into download tasks. Path rendering stays in `src/download/paths.rs`. |
| Download pipeline facade | `src/download/pipeline.rs`, `src/download/pipeline/streaming.rs` | Preserves entry points and composes run modes, producer execution, consumer execution, and pass finalization. |
| Local-file adoption | `src/download/pipeline/adoption.rs` | Validates current and pending local-file evidence before adoption or an on-disk skip. |
| Streaming producer | `src/download/pipeline/producer.rs` | Plans assets, forecasts disk space, records skip counts, and dispatches pending tasks. |
| Streaming consumer | `src/download/pipeline/consumer.rs` | Runs bounded workers and accumulates transfer results and deferred state writes. |
| Single-task execution | `src/download/pipeline/task.rs` | Coordinates one transfer, metadata completion, temporary-file ownership, and worker error classification. |
| Download pass and outcome | `src/download/pipeline/pass.rs`, `src/download/pipeline/outcome.rs` | Executes explicit task passes, finalizes streaming state, retries failed tasks, and aggregates sync outcomes. |
| Download progress and summary | `src/download/pipeline/progress.rs` | Formats durations and sync summaries and reports rate-limit pressure. |
| File facade and transfer | `src/download/file.rs`, `src/download/file/transfer.rs` | Preserves entry points and owns HTTP retry, resume, stream writes, and temporary-file cleanup. |
| File validation and fingerprints | `src/download/file/validation.rs`, `src/download/file/fingerprint.rs` | Validates response lengths and media, checks local size evidence, and captures same-read hashes and snapshots. |
| File publication and replacement | `src/download/file/publication.rs`, `src/download/file/replacement.rs` | Handles no-overwrite collisions and conditional replacement with displaced-file verification and restoration. |
| Confined local copies | `src/download/file/reconciliation.rs` | Retains verified files and parent-directory capabilities through local copy and state finalization. |
| File platform primitives | `src/download/file/platform.rs` | Owns platform rename, exchange, hard-link, and directory durability operations. |
| Linux replacement recovery | `src/download/file/replacement_recovery.rs` | Journals conditional replacement when atomic exchange is unsupported, and recovers interrupted publication without overwriting concurrent edits. |
| State finalization | `src/download/finalize.rs` | Persists downloaded or failed outcomes and retries deferred state writes. |
| Sparse identity retries | `src/download/orchestration/delta/sparse_identity.rs`, `src/state/db/sparse_identity.rs` | Selects bounded source retries and persists generation-fenced evidence; the cycle owner retains checkpoint authority. |
| Durable retry resolution | `src/download/retry.rs` | Revalidates pending provider identity and builds exact retry tasks. |
| Path rendering | `src/download/paths.rs` | Expands folder templates, normalizes names, and handles collision suffixes. |
| Metadata facade and values | `src/download/metadata.rs`, `src/download/metadata/values.rs`, `src/download/metadata/xmp_fields.rs` | Preserves writer entry points and shared values, with one owner for managed XMP properties, namespace initialization, and field encoding. |
| Metadata evidence | `src/download/metadata/probe.rs`, `src/download/metadata/source_gps.rs` | Probes existing EXIF/XMP fields and performs bounded source EXIF GPS decoding for write planning. |
| Embedded writer dispatch | `src/download/metadata/embedded.rs`, `src/download/metadata/formats.rs` | Validates approved input and selects a format writer using content and extension detection. |
| Format-specific metadata preparation | `src/download/metadata/heif_writer.rs`, `src/download/metadata/xmp_writer.rs`, `src/download/metadata/native_writer.rs` | Prepares HEIF-family, XMP Toolkit, or native-only EXIF output. Publication stays in the prepared-file owner. |
| Embedded replacement publication | `src/download/metadata/prepared.rs` | Owns exclusive temporary files, fingerprints, and stable-input publication through the file replacement primitives. |
| Sidecar ownership and publication | `src/download/metadata/sidecar.rs` | Prepares sidecars, validates ownership, and conditionally publishes writes, including reconciled sidecars. |
| HEIF facade and file-backed reads | `src/download/heif.rs`, `src/download/heif/file.rs` | Preserves HEIF entry points and typed errors; locates native Exif with bounded file reads without loading media payloads. |
| HEIF layout | `src/download/heif/boxes.rs`, `src/download/heif/items.rs` | Owns box boundaries, item tables, extent resolution, and item-location encoding. |
| HEIF metadata relationships | `src/download/heif/relationships.rs` | Resolves primary-image metadata ownership and encodes item references. Ambiguous ownership fails closed. |
| HEIF native Exif | `src/download/heif/exif.rs` | Extracts native Exif and repairs capture timestamps within the TIFF payload. |
| HEIF XMP | `src/download/heif/xmp_read.rs`, `src/download/heif/xmp_write.rs` | Reads, inserts, and replaces the selected XMP packet without re-encoding image payloads. |
| HEIF preservation checks | `src/download/heif/preservation.rs` | Validates non-XMP item payloads and opaque metadata after a rewrite. |
| Metadata rewrite facade and immediate execution | `src/download/metadata_rewrite.rs`, `src/download/metadata_rewrite/immediate.rs` | Preserves rewrite entry points and coordinates download-time metadata writes. The pipeline retains media publication and state finalization. |
| Metadata write planning | `src/download/metadata_rewrite/planning.rs` | Owns opt-in flags and embedded field decisions shared by immediate and queued execution. |
| Metadata write execution | `src/download/metadata_rewrite/embedded.rs`, `src/download/metadata_rewrite/sidecar.rs` | Coordinates fingerprint-guarded embedded writes and source-aware sidecar writes through the metadata owners. |
| Queued metadata retries | `src/download/metadata_rewrite/queued.rs` | Tags durable rewrite work, executes bounded retry pages, and retires completed markers. |
| Rewrite evidence and groupings | `src/download/metadata_rewrite/capture.rs`, `src/download/metadata_rewrite/grouping.rs` | Verifies capture-repair fingerprints and timestamps; loads current library-scoped groupings for queued work. |
| Account namespace | `src/account.rs` | Hashes exact configured login and provider realm without alias inference. |
| Account database boundary | `src/state/db/account.rs`, `src/commands/migrate_state.rs` | Validates an independent owner before state consumption, creates authenticated state, and explicitly adopts a preserved legacy snapshot. |
| SQLite facade and connection lifecycle | `src/state/mod.rs`, `src/state/db.rs` | Preserves state entry points, opens connections, and dispatches blocking SQLite work. Child owners retain their transaction boundaries. |
| Observed provider pages | `src/icloud/photos/inbox.rs`, `src/state/db/provider_inbox.rs` | Binds validated incremental observations to the account, retains source identities and original bytes atomically with a shadow receipt, and applies bounded backpressure. It does not authorize provider checkpoints. |
| Provider source catalog | `src/icloud/photos/projection.rs`, `src/state/db/provider_catalog.rs` | Validates replay provenance, derives versioned source facts, and atomically indexes records, references and unresolved evidence. Its receipt does not authorize queues or checkpoints. |
| Provider work admission | `src/icloud/photos/album/work.rs`, `src/download/orchestration/queue_projection.rs`, `src/state/db/provider_work.rs` | Confirms retained identities against current provider records, applies current task planning, and atomically admits guarded existing queue obligations and retained conflict debt. Checkpoint authority stays with the existing cycle. |
| SQLite schema | `src/state/schema.rs` | Owns schema versions and migrations. |
| State-store contracts | `src/state/db/contracts.rs` | Defines store roles and records exchanged with callers. |
| Asset state transitions | `src/state/db/assets.rs` | Owns asset lifecycle, download finalization, retry eligibility, and provider source-state transitions. |
| Shared SQLite writes and row decoding | `src/state/db/asset_writes.rs`, `src/state/db/rows.rs` | Shares asset writes within caller-owned transactions, column projections, date codecs, and row decoding. |
| Metadata capture retries | `src/state/db/metadata_capture_retry.rs` | Stores generation-fenced ambiguity evidence and bounded retry deadlines. The cycle owner retains checkpoint authority. |
| Durable provider identity | `src/state/db/identity.rs` | Stores library-scoped asset/master mappings and legacy state ownership. Provider record parsing stays in the iCloud adapter. |
| Durable membership | `src/state/db/membership.rs` | Stores album snapshots, provider relations, and grouping projections. |
| Durable metadata work | `src/state/db/metadata.rs` | Stores capture revisions, path-specific rewrite work, and repair receipts. It does not write media files. |
| Durable checkpoints | `src/state/db/checkpoints.rs` | Stores provider checkpoints, scoped database tokens, and enumeration progress. Sync policy decides when checkpoint proof permits a commit. |
| Temporary-file ownership | `src/state/db/temp_files.rs` | Stores losslessly encoded paths and durable temporary-file ownership. Filesystem cleanup stays in download orchestration. |
| Durable reconciliation | `src/state/db/reconciliation.rs` | Stores destination reservations and reads catalog evidence for reconciliation. |
| Durable import adoption | `src/state/db/import.rs` | Atomically adopts imported files into state and reads imported records. Matching policy stays in the import command. |
| State reports | `src/state/db/reports.rs` | Reads status, verification and manifest data, and records sync-run history. |
| Import-existing | `src/commands/import.rs` | Matches existing files to expected iCloud paths and adopts verified files into state. |
| Service integration | `src/service/` | Owns install, uninstall, status, service execution, and platform renderers. |
| Operator surfaces | `src/commands/status.rs`, `src/commands/doctor.rs`, `src/commands/manifest.rs` | Read local state for status, redacted diagnostics, and catalog export. |
| Reports and monitoring | `src/cycle_reporter.rs`, `src/report.rs`, `src/health.rs`, `src/metrics.rs`, `src/notifications.rs` | Converts cycle facts into reports, health, metrics, and notifications. |

Album tests use `icloud::photos::album::<owner>::tests::<test_name>` instead
of `icloud::photos::album::tests::<test_name>`. The owners are `lookup`,
`counts`, `planning`, `enumeration`, `fetch`, `completion`, `hydration`, and
`changes`. All 88 original tests retain their names, attributes, and assertions,
including the ignored live lookup test. Shared fixtures stay in
`src/icloud/photos/album/test_support.rs`. Album tracing keeps the
`kei::icloud::photos::album` target.

Album-owner dependencies are one-way: hydration uses enumeration and change
scans; enumeration uses planning, fetching, and completion; fetching uses
planning types and completion evidence. Lookup, counts, planning, completion,
and changes do not call sibling owners. The facade retains stream routing and
public entry points. No album child owns durable checkpoint policy.

File tests use `download::file::<owner>::tests::<test_name>` instead of
`download::file::tests::<test_name>`. The owners are `transfer`, `validation`,
`fingerprint`, `publication`, `replacement`, and `reconciliation`. HTTP tests
retain their nested `wiremock_tests` module under `transfer::tests`. Test names
and assertions are unchanged; the Windows partial-replacement test remains
Windows-only. File tracing keeps the `kei::download::file` target.

File-owner dependencies are one-way: transfer calls validation and publication;
publication calls replacement, platform primitives, and fingerprinting;
replacement calls platform primitives and fingerprinting; reconciliation calls
platform primitives and fingerprinting; validation uses fingerprinting for
local-size evidence. Platform primitives do not decide replacement or retry
policy. In particular, Windows partial-exchange recovery stays in replacement.

Pipeline tests use `download::pipeline::<owner>::tests::<test_name>` instead
of `download::pipeline::tests::<test_name>`. The owners are `adoption`,
`consumer`, `outcome`, `pass`, `producer`, `progress`, `streaming`, and `task`.
Test names and assertions are unchanged. Shared test fixtures stay in
`src/download/pipeline/test_support.rs`.

## Main flows

### Command dispatch

```text
src/main.rs
  -> kei::main_inner
  -> src/lib.rs::run
  -> src/cli.rs
  -> command owner, service owner, or src/sync_loop.rs
```

`run` composes private phases in `src/lib.rs`: `load_startup_config`,
`resolve_startup_output`, `initialize_logging`, and `dispatch_command`.
Config loading retains the Docker fallback and passes parse errors to
`doctor` without blocking its diagnostic report. Output resolution keeps the
friendly-mode request separate from the mode allowed by the environment.
The log-writer guard stays in `run` until dispatch returns. Startup injects
the captured environment password before dispatch. Command dispatch retains
the setup wizard's one-shot sync path and the service owner's entry point.

The CLI requires a subcommand. `kei sync` enters the sync path. Commands such
as `status`, `doctor`, and `manifest` read local state without entering the
normal iCloud sync loop.

`main_inner` records whether stdin is interactive before it starts the async
runtime. Command owners use that single input mode for password and 2FA
prompts. A foreground command returns `TwoFactorRequired` to the exit
classifier, which reports auth exit code 3 and the `login get-code` and
`login submit-code` recovery flow. Only the sync owner may convert that error
into a durable wait, and only when the resolved configuration is in watch or
service mode.

Interactive login and `login get-code` may retry once from clean local auth
state when Apple rejects verification-code delivery with HTTP 403 and the
attempt loaded a persisted cookie jar or session. The failed session removes
only its cookie jar, session, and validation cache while it still holds the
per-account lock, then the shared auth owner creates one clean replacement
session. Password material and SQLite state remain unchanged. `reset session`
exposes the same locked cleanup explicitly. Cleanup advances a generation in
the existing lock file. A watch process that released its lock for idle sleep
detects a changed generation when it wakes and stops before its old in-memory
session can recreate reset credentials. Reauthentication handoffs carry their
pre-release generation into replacement-session creation, so a reset in that
gap also wins.

### Account ownership and legacy adoption

Configured login and realm determine collision-resistant names for state,
auth artifacts and encrypted credentials. Account-owner format version 1 is
independent of the application schema version and must be checked before its
migrations. Production connections require an owner expectation; unbound
connection helpers are restricted to tests. All state commands, including
read-only export and reset, enforce this boundary. Sync and import require a
usable authenticated provider identity and pin it independently of filenames.
Watch validation, replacement sessions and initialization retries retain that
principal pin. Changed or missing authenticated identity stops before further
provider or state work; it cannot become a recoverable quiet-watch warning.

Legacy discovery fails closed rather than creating fresh state over retained
debt. Explicit `migrate-state` requires verified operator ownership
confirmation and a fresh login under the account lock. SQLite backup retains
rows, unknown tables and committed WAL content, then binding, schema migration
and integrity validation occur in a private stage. Same-directory publication
refuses replacement and fsyncs the file and parent. The legacy source and
companions remain. Conflicting scoped provenance is rejected, not rewritten.
Legacy session and encrypted credential files are preserved and ignored;
keyring identity remains unchanged. See [account migration](account-state-migration.md)
for operator steps and compatibility limits.

### Observed incremental shadow capture

After opening an authenticated, account-owned database, sync attaches capture to
its Photos service and libraries. Normal incremental `changes/zone` responses
are bounded and validated before `DeltaRecordBuffer` pairing or selection. The
adapter rejects ambiguous JSON keys and unusable source identity or scope for
the whole page. Original response bytes retain unknown fields, tombstones and
unresolved relationships, including numeric lexemes that a typed JSON re-encode
could alter. Cookies and authentication headers are not captured; resource URLs
remain transient payload, never durable identity.

Schema 29 adds `provider_shadow_pages`, ordinal source identities in
`provider_shadow_records`, and a per-scope last-observed pointer in
`provider_shadow_receipts`. An immediate SQLite transaction rechecks the
independent account owner and commits all three together. Provenance includes the
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
Exact already-captured replay remains possible at capacity. No observations are automatically pruned. SQLite WAL `NORMAL`
remains unchanged; a successful commit does not promise that every acknowledged
observation survives power loss. Network filesystems are not newly qualified.

A shadow receipt is only the last observed page, and replay can move it back. It
is not a coverage frontier, materialization receipt or verified filesystem
progress. Existing `CheckpointEvidence`, sparse and legacy generation proofs,
recovery debt and filesystem guards retain checkpoint authority. Capturing a
tombstone cannot authorize local media deletion. Existing download and metadata
queues still consume the current stream; they do not replay this inbox yet.

Capture covers only incremental pages observed by existing sync in selected
scopes. Rank inventories, bootstrap, lookup and deletion-validation scans are
not represented as complete zone capture or historical completeness. Rank EOF
plus a delta bridge is not an atomic snapshot or proof of absence. New future
observations cannot retire old expired-epoch debt. Queue projection, capture-owned cursors, durable catalog selection and separate
progress remain later stages. Historical-version retention, unrecoverable-debt acknowledgment,
epoch recovery and compaction still require explicit policy and support proof.
The additive migration preserves existing rows and uses the migration owner's
savepoint. Re-entry validates the table columns, primary keys, replay key and
SQLite-assigned page identity required by source and receipt links. A conflicting
unknown table fails without partial schema or version changes.
Older binaries supporting only schema 29 must refuse the current database.

### Transactional source catalog and replay

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
work admission described below; source indexing alone cannot authorize it.

Derived page facts have a 16 MiB logical charge limit and a separate 512 MiB
catalog index budget. These are operational backpressure limits, not physical
SQLite/WAL quotas or a retention promise. A full index holds the checkpoint and
retains pending source observations; exact completed-page replay validates facts
and remains possible at capacity. Startup has no fixed latency guarantee. Nothing
is automatically pruned or discarded. WAL `NORMAL`, filesystem qualifications,
existing checkpoint evidence, publication receipts and metadata obligations are
unchanged. Historical intermediate retention and unrecoverable-debt acknowledgment
still require explicit policy decisions before dependent stages.

### Current provider work admission

Schema 31 adds `provider_work_receipts`, generation-bound
`provider_work_obligations`, and the `provider_work_scan` scheduling position.
Before normal dispatch, up to 64 retained, indexed CPLAsset observations per
cycle can trigger scoped child lookup followed by a complete current child/master
lookup. Original bounded confirmation bytes and source page/ordinal provenance
are retained. Duplicate keys, duplicate or omitted records, provider errors,
wrong owners, wrong zones and changed child/master relationships cannot confirm
work. Provider confirmation is an observation, not an atomic provider snapshot.

This first slice supports one private library-wide pass with an explicit default
owner, without recent caps, album exclusions, retry-only selection or metadata
backfill-only execution. Other selections retain observations and use their
existing pipeline; they receive no admission receipt from this stage. Current
media/date/filename filters and RAW, companion and path planning use the existing
owners. The work configuration fingerprint binds their frozen evidence separately
from enumeration and checkpoint hashes. Hidden or deleted current records retain
deferred evidence without new media work.

An immediate transaction independently validates authenticated account ownership,
source bytes, indexed identity, complete scope and current confirmation. Frozen
resource checksums, sizes and metadata must match the retained confirmation.
New rows, asset/master mappings, grouping/retry markers, work obligations,
deferred conflict evidence and the final receipt commit together. Any conflicting
existing generation, metadata, mapping or legacy identity defers unchanged.
Identical existing queue rows retain their retry and publication metadata.
Admitted-receipt replay verifies frozen obligations and never reconstructs an old
generation over a queue that has since advanced. Receipt state describes work
admission, not download completion, metadata publication or verified media.

Matching unfinished projected generations fence shared asset admission, identity
mapping and metadata refresh, including independent path publication receipts.
Completed generations do not indefinitely pin the mutable canonical queue.
A rotating bounded scan retries unresolved early sources without starving later
identities. Its position is scheduling evidence and cannot promote a cursor.
Work plans have a 16 MiB logical charge limit and work evidence has a separate
512 MiB logical budget. Nothing is automatically pruned; backpressure retains
source observations, unfinished work and the current checkpoint. These are not
physical database/WAL quotas or fixed startup latency promises.

Current versions under current configuration are the approved materialization
scope. Historical observations and all unfinished debt remain retained. Admission
of a current version does not prove historical completeness, acknowledge old
unrecoverable debt, authorize local tombstone deletion or select a historical
retention policy. Existing CheckpointEvidence, sparse/legacy proofs, opt-in
metadata writes, publication guards and WAL NORMAL qualification remain in force.

### Sync and provider checkpoints

```text
sync_loop::run_sync
  -> resolve configuration, credentials, libraries, and pass plans
  -> optional scoped changes/database pre-check
  -> sync_cycle::run_cycle
  -> download::download_photos_with_sync for each active library
  -> sync_cycle source checkpoint decision
  -> cycle reporting and watch control
```

CloudKit HTTP 401, 403, and 421 failures during enumeration or metadata
hydration return a session-expired outcome. The cycle preserves its provider
checkpoint and pending capture work. It does not treat these failures as a
reason to fall back to full enumeration or retry incomplete pass tokens.
Incremental producers pass session failures to the download pipeline before
stream completion. The pipeline records the interrupted run before status
commands read its outcome.
Before bounded mid-cycle reauthentication, the watch loop removes the cached
validation result and resets the HTTP connection pool. A recovered session
replays the retained checkpoint. Ambiguous provider identities remain pending;
authentication recovery does not authorize choosing a child or discarding work.

Legacy-master hydration scans all current visible and hidden children before
metadata-capture repair or pending-download recovery selects an owner. Hidden
children remain identity candidates; soft- and hard-deleted children do not.
Without a saved owner, multiple matching children remain unresolved, even
across visibility states.
This does not change download selection or permit checkpoint advancement
without the existing completion proof.

Hidden children can supply identity evidence without a valid `assetDate`.
Recovery counts all matching children before validating a selected child's
capture date. Missing, null, or out-of-range dates cannot authorize an owner
claim, metadata refresh, pending-file adoption, or download-policy exclusion.
Invalid selected dates retain repair or retry work and hold the checkpoint.
The display/path epoch fallback is not recovery evidence.

A new legacy-owner claim requires consistent, present per-rendition `added_at`
evidence matching the provider's `addedDate`, in addition to the existing
rendition checks. Missing provider dates do not use the epoch fallback as
identity evidence. Matching dates do not rank otherwise ambiguous children.
Whole-second legacy dates match within that second because older writers
discarded milliseconds. Fractional stored dates require an exact match.
Different stored rendition dates, including mixed whole-second and fractional
rows, remain unresolved at the atomic claim gate. Direct pending lookups also
require matching rendition evidence before they can persist a new owner.
Another historically mapped child blocks a new claim even when it is now
missing or has its own catalogue row. The state owner checks family history
and mixed rendition dates inside the claim transaction. Normal enumeration
and pending recovery use the same restrictions; read-only retry planning does
not persist a claim. Unresolved pending ownership holds the checkpoint.

Metadata-capture retry evidence includes each rendition's added date, at
millisecond precision. A changed date or an older fingerprint without dates
makes retained work eligible again. Conflicting or missing date evidence uses
the existing bounded retry queue. This does not delete or retire legacy rows,
replace saved owners, or restore dates overwritten by an earlier repair.
Recovery of those rows requires separately preserved historical evidence.

Ambiguous metadata-capture repairs retain a schema-27 retry row keyed by
library, catalogue asset ID, and target capture revision. An unchanged failure
retries after one hour, doubling up to 24 hours. Changed catalogue identity or
rendition evidence is eligible immediately. A durable generation rejects stale
attempts; its counter survives retry retirement. The state reader excludes
deferred candidates before applying the existing 500-identity batch limit.
Unresolved and deferred counts are separate from failed repair attempts.
Deferred work keeps the capture revision pending and its checkpoint blocked,
even when a cycle makes no repair attempt. The cycle checks each library
before committing inventory anchors or provider checkpoints, including
`--refresh-metadata` runs that bypass automatic capture repair. Idle health
and other-library success cannot turn retained ambiguity into a successful
backup. Completion retires retry rows only after the existing repair or
source-deletion rules remove the stale work. Retry scheduling does not select a child or authorize
metadata rewrites, deletion, or checkpoint advancement.
The download owners store these holds in `CheckpointEvidence`. Report counter
changes and result composition cannot clear them.

Unresolved asset-only delta hydration is incomplete work, even when no media
transfer fails. The producer records `unresolved_asset_identity:<zone> = 1` in
the existing metadata table before stream completion. The cycle owner clears
that zone's marker atomically with a proven incremental checkpoint, including
an inventory followed by a successful delta bridge. An inventory alone cannot
clear it. Another zone's success and process restart retain the marker.
Status aggregates all markers; cycle reporting does not advance health's last
success while any remain. Selected zones with markers bypass watch-mode
no-change shortcuts. Idle health also checks markers in unselected zones.
Intentional bounded checkpoint holds alone are not failures. The marker keys
remain in metadata; JSON report version 3 is unchanged.

The Photos adapter emits aggregate, fixed-label identity lookup diagnostics.
They distinguish omitted records, unexpected types, decode failures, invalid
master references, and record-level provider errors. Reference-zone context is
classified without logging provider identifiers or response bodies. These
observations do not authorize identity guesses or cross-zone retries.

Unpaired asset deltas retain typed sparse-share evidence from
`isSparsePrivateRecord` and the `linkedShare*` fields. Targeted record lookups
request these fields too; ordinary media enumeration keeps its existing field
projection. A structurally valid link remains unverified. Malformed links and
valid links without a usable master remain unresolved. Link identifiers are
opaque and their debug output is redacted. Lookup changes do not overwrite the
original delta evidence or authorize a target lookup.

Schema 26 adds `unresolved_sparse_identities`, keyed by library and source
record in the account-scoped database. `src/state/db/sparse_identity.rs` stores
the first observed link, latest delta link, separate lookup evidence, attempt
time, and retry deadline. The Photos adapter owns the versioned link encoding.
Observation and the existing unresolved marker commit in one transaction.
The `sparse_identity_generation` metadata counter assigns monotonically
increasing generations, including when a cleared source reappears.

`src/download/orchestration/delta/sparse_identity.rs` owns retry selection for both incremental
paths. A matching valid sparse lookup starts a one-hour delay. Repeated
matching results double the delay to a 24-hour maximum. Each library execution
selects at most 100 due, identified sparse source-only lookups, ordered by
retry deadline or last attempt, then generation and source key. Exact
source/master mappings bypass suppression and do not consume that budget. Malformed or changed
incoming evidence cannot inherit a cached negative result. Unclassified
sources still need an initial lookup before sparse retry policy can apply.
A deferred source remains unresolved and blocks the checkpoint. Retained
sources absent from replay re-enter normal hydration and media planning.

Authoritative source-only deletion lookups retain the completed delta token in
`last_outcome` as `["source_deleted_v1", token]`. This is provider evidence,
not a persisted claim that local processing completed. On restart or the next
batch, the same complete delta snapshot and unchanged source evidence can
reuse that result. The ordinary source-state transition runs again before a
current receipt is issued. A missing delta token, changed link, or authoritative
master mapping requires normal recovery.
For a different token, the Photos adapter scans the complete raw zone delta
since each saved deletion checkpoint. One scan validates a whole batch.
Any source change, including restoration with the same link, invalidates its
cached deletion. Sources absent from a complete scan retain their deletion
evidence at the current boundary with fresh generation fences. Missing tokens,
invalid pages, incomplete scans, and cancellation supply no reuse evidence.
Normal source lookups can still establish fresh results after a failed scan.
Validation does not advance the sync checkpoint or complete local processing.
This lets more than 100 deleted sources complete over bounded lookup batches
while unrelated zone records change, without relaxing checkpoint or state-write
guards. Existing schema-26 outcome labels remain readable.

Hydration, explicitly soft-deleted source `CPLAsset` deltas, and exact-source
hard-deletion tombstones supply generation-fenced receipts, not permission to
advance a checkpoint. Incremental results carry these receipts in
`CheckpointEvidence`, not `SyncStats`. Inventory and delta-bridge composition
retain the receipts in zone-local evidence. The cycle owner still requires
normal processing and checkpoint proof. The checkpoint transaction validates every retained source receipt
before clearing its row and zone marker. Missing, changed, or stale receipts
roll back the transition. Inventory alone, interruption, failed state writes,
and another library's success cannot clear the obligation. Configuration
reconciliation uses the same fence when publishing its staged checkpoints.
Status reports unresolved and deferred counts without provider identifiers.

A linked record name, missing/deleted shared zone, or history of removing a
Shared Photo Library is not a source deletion or master identity. No automatic
cross-zone recovery or checkpoint relaxation is added.

The per-zone provider checkpoint and the scoped database pre-check token have
different gates:

- A zone checkpoint may advance after a transfer failure when the exact retry
  work is durable and enumeration/token proof is complete.
- An exhausted album grouping write preserves the zone checkpoint. Replay
  retries the relationship before it skips media that is already downloaded,
  and the relationship insert is idempotent.
- The broader database pre-check token advances only after a clean aggregate
  cycle for the exact account, selection, filter, config, and selected-zone
  scope.

Eligibility-config drift preserves the active checkpoint while a complete
inventory and delta bridge build a replacement. Path-config drift preserves
provider checkpoints while local catalog paths are reconciled.

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

Local path reconciliation retains an unowned legacy master with multiple
historical children as unresolved work, even when targeted lookup returns one
usable sibling. It does not reserve that master's path or relax preservation
eligibility. Independently owned current children can still reconcile, and later
complete inventory can prepare preservation and release the checkpoint hold.

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

### Full and incremental enumeration

Debug builds can set `KEI_REQUEST_DUMP_DIR` to capture Photos session
bodies as `000001.req.<epoch-ms>.json` and `000001.res.<epoch-ms>.json`.
The counter pairs requests and responses; timestamps are captured separately.
Bodies are unredacted and private; request URLs and headers are not captured.
Use private local storage and redact bodies manually before sharing. This dump
is not a redacted doctor bundle.
HTTP error bodies retain the existing size bound and may contain non-JSON text.
Successful responses are serialized from parsed JSON without pretty-printing.
Writes are best-effort, with no run subfolder or cleanup.
Release builds without debug assertions do not include this diagnostic code.

A signed URL expiry stops the current full-download batch. The pipeline retains
failed tasks, including queued tasks cancelled by that expiry, and makes one
cleanup attempt through targeted asset and master lookups. User shutdown stops
cleanup lookup and download dispatch. It does not enumerate albums again to
refresh those URLs or rebuild unfiled exclusions. Cleanup updates only the URL
on an already-selected task, preserving its destination and publication
authorization. The library, child identity, rendition, checksum, and size must
still match. Changed resources, missing lookups, a second expiry, and cancelled
tasks retain durable retry work. Successful
cleanup does not convert incomplete enumeration into checkpoint proof.
Authentication failures during refresh stop cleanup and return a session-expired
outcome. Refresh rate-limit observations contribute to the cycle count even
when provider retries are exhausted. The `expired_url_refresh_failed` diagnostic
reports failure, authentication, and rate-limit counts without provider details.

Collecting incremental preflight and expiry recovery use the same bounded
child/master lookup owner. The authenticated session and exact requested zone
scope each lookup. Explicit zone/owner evidence and child/master references
must agree; incremental recovery also pins the originally selected master and
provider rendition. The selection owner translates virtual RAW/JPEG task keys
back to provider keys once, before refresh; later provider policy changes cannot
retarget the selected resource.
Refresh changes only the URL when child, master, rendition, provider checksum
and size still agree. Unmatched resources retain durable failure work; expiry
and lookup absence never authorize deletion. No zone enumeration, path
replanning, or cross-instance mapping import is used for URL refresh.

Explicit-task passes isolate CDN expiry to the failed resource so healthy
peers finish, including the single cleanup retry. Authentication thresholds,
user shutdown and fatal state-write failures still stop dispatch. Interrupted
and queued unfinished tasks remain in the returned failure debt and are
persisted before checkpoint decisions. Only successful exact tasks retire retry
debt. A collecting pass makes at most one expiry refresh and retry after its
optional aged-URL preflight. Failed resources stay durable for the next cycle.

Refresh logs record phase start/completion, counts and elapsed time. Refreshed
resources carry their lookup-observation time into worker dispatch for a DEBUG
age measurement. The first enumeration observation is labelled separately;
it does not measure a refreshed URL's age. Neither observation establishes
provider issuance time or expiry, which remain unknown.

Full enumeration streams records/query results and gathers a provider token
from every active pass. Natural stream completion and usable, unanimous pass
tokens are the authoritative proof. Count probes and pagination differences
are diagnostics. Recoverable pass-token gaps can be retried in the same cycle.
Download orchestration normalizes each child to its `CPLAsset.recordName`
before streaming or collecting paths plan it, so page boundaries and sibling
order cannot change its state ID. It retains a legacy master-keyed state ID
only when the provider checksum matches and durable identity history permits
that child to adopt it. Before a download pass adopts a legacy master-keyed
row, it atomically records that child as the durable owner. Later sibling
mappings cannot change the owner. Print-only and dry-run paths select from
existing state without claiming an owner. Cleanup retries carry the child
record name and reapply the state ID selected by the first pass before they
rebuild download tasks.

The iCloud adapter validates a complete changes/zone page before pairing records,
invoking hydration callbacks, or accepting its successor. A page must contain one
zone matching the requested name and any requested owner, an explicit records
array without record errors, and a nonblank token. Continuing pages must use a
new cursor. An empty terminal page may return the request cursor. Rejection
emits no records from that page; earlier accepted pages remain replayable from
the durable checkpoint. These checks also protect raw hydration and source
change validation. Source change validation retains its stricter record identity
and cancellation requirements. Diagnostics omit provider cursors and identifiers.

Incremental enumeration consumes changes/zone events. It persists provider
identity mappings before applying created, soft-deleted, hard-deleted, or
hidden transitions. An asset-only `CPLAsset` creation hydrates its paired
master through `masterRef`, the durable mapping, or a targeted identity lookup
before routing; an inconclusive lookup preserves the prior zone checkpoint.
Album snapshots and smart folders may require targeted refresh work before or
alongside the incremental stream.

Both strategies use `IncrementalDeltaState` in `orchestration/delta.rs` for
identity mappings, source-state transitions, album and relation bookkeeping,
event counts, and completion tokens. The streaming strategy emits created
assets through a bounded channel and defers album, relation, and unpaired-asset
work. The collecting strategy observes all events before state writes, hydrates
missing identities, and applies relations before routing created assets.
These orders preserve each strategy's token-safety and selected-album behavior.

Downloadable photo records require non-blank `CPLMaster` and `CPLAsset` record names
and a usable `assetDate` before they enter filtering or path planning. Full
enumeration reports a malformed record as incomplete. Incremental enumeration
marks its zone token unsafe. Both routes preserve the prior checkpoint so an
unchanged provider record remains retryable, and neither route counts the
record as a policy, filename, or date skip.

Recent and date-bounded runs may advance only when the producer proves the
bound did not truncate the stream.

### Download and publication

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

### Durable pending retry

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

Policy-excluded rows stay outside the actionable pending reader. After a
successful source pass, targeted revalidation checks their durable provider
identities only for explicit deletion. Present, omitted, malformed, and
transient responses retain the policy-excluded rows.

A provider checksum change makes the previous local path historical. Retry
adoption requires durable proof that the path holds the current provider
version and that its filename matches the recorded task filename.

### Import-existing

`src/commands/import.rs` shares configuration, selection, pass planning, and
path derivation with normal sync. It optionally compares remote prefix bytes,
hashes the local candidate, and calls `ImportStateStore::import_adopt`.
Adoption checks durable destination reservations in the same transaction as
catalog updates. A different asset, rendition, or provider content generation
cannot adopt a reserved path. Unknown legacy content remains occupied.
A refused adoption logs a warning and does not count as a match.
Size/mtime snapshots may skip later rehashing only while path, size, and mtime
still match.

## Data-safety invariants

### File landing

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

`kei sync --repair-truncated` is the only media-replacement path. Pending
retry planning requires the durable reconcile truncation marker, confirms the
recorded file is still truncated, and fingerprints its bytes. After the new
download and configured metadata writes pass, the file owner replaces only
that exact fingerprint. It uses atomic exchange where supported and the
journaled Linux fallback described below otherwise. A changed
target is restored or retained and the state row stays failed. All other
downloads keep no-overwrite publication.

When no-overwrite publication finds an existing destination, the file owner
compares it with the verified `.part` file. Identical bytes are deduplicated.
Different or unverifiable bytes return a typed collision and retain the
`.part` file. The pipeline records the task as failed before it can write a
sidecar or finalize downloaded state.

A file may be safe on disk while its state write is still a cycle failure. Do
not weaken deferred state-write handling or infer that a visible final file
means the database transition succeeded.

### Checkpoint advancement

`SyncResult.checkpoint` carries the per-zone completion, interruption, state
durability, identity, and token-block evidence. The source-checkpoint classifier
reads this evidence and the result's source token and outcome. It does not read
the reporting snapshot in `SyncResult.stats`.

Full enumeration obtains evidence directly from pipeline finalization. Attached
pending recovery and metadata-capture repair also supply execution evidence.
Same-zone composition combines this evidence once, then projects its counters
into the report. Recovery pass and record lists stay in the per-zone evidence;
cycle reports do not accumulate them. Report fields and reason values keep
their existing JSON representation.

Incremental result assemblers still use `SyncResult::from_execution` to capture
their execution statistics at the result boundary. This is the boundary of the
pilot, not a second authority after result assembly. Later composition must use
the captured evidence, never reconstruct it from a reporting snapshot. Query
token holds, inventory/delta bridges, and unresolved identities update the
evidence in their existing policy owners.

Preserve the zone checkpoint on:

- Dry-run
- Stale pass planning
- Session expiry or interruption
- Incomplete enumeration
- Non-durable state
- Missing, blank, mismatched, or otherwise blocked token proof

Transfer and metadata failures may use a newer checkpoint only when the
recovery work needed after that checkpoint is durable.

### Metadata

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
replacement file retains the source permissions.

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
errors. Empty journal recovery applies the same bounded cleanup. Historical
prepared files are not swept by this path. NFS mounts with `nolock` do not
provide cross-host locking; do not run kei on the same destination from
multiple NFS clients. Recovery does not authorize provider checkpoint progress
or downloaded-state finalization by itself.

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

Schema v28 can preserve an unclaimed legacy master receipt with at least two
historical child mappings without assigning it to a child. A complete current
inventory must contain at least one live child. A sole survivor must satisfy the
same independent current-child receipt and file checks as a multi-child family;
historical siblings missing from that inventory do not establish ownership or
authorize source deletion. Zero-current-child families and saved legacy owners
remain excluded. `download/legacy_preservation.rs` owns eligibility and
file validation; `state/db/legacy_preservation.rs` owns immutable evidence and
generation-fenced receipts. Migration creates empty tables and protection
triggers only. It does not classify or rewrite existing records. Schema-27
binaries reject a migrated database; read-only reports require the current
schema and never migrate it.

Preparation protects the original catalogue rows, capture revisions, retry
receipts, relationship history, media paths and sidecar presence. It preserves
the old checkpoint. A complete unfiltered zone inventory must cover hidden
children, all pages, an explicit completion marker and a nonblank final cursor.
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
cross-library or incomplete evidence cannot activate preservation. No diagnostic
count is ownership proof.

Failed preparation discovery records a versioned, library-scoped
`legacy_preservation_inventory_retry:` receipt in the metadata table. An unchanged
configuration and durable original-family evidence defer another preparation scan
for at most one hour across restart. Retry scheduling, capture cycle counters and
unchanged relation refresh timestamps do not reset that delay. Changed
configuration or original-family evidence permits
a new attempt; malformed, unsupported or out-of-range retry receipts never defer
work. Deferral supplies no provider evidence or checkpoint permission. Prepared
records still require fresh certification, and metadata-capture retry scheduling
remains independent. Successful discovery clears its failure delay.

Preparation and certification use separate complete scans; a preparation result
cannot certify work performed later. Failure warnings expose only a fixed stage
and category plus aggregate page, record, transferred-byte and retained-byte
counts. Provider error text, record identities, paths and URLs remain private.

Activation requires a normal completed current inventory and, when a retained
cursor exists, a successful delta bridge. Independently identified current
children need separate, finalized receipts and verified files for every selected
rendition. A file published before a state-write crash may qualify by exact
SHA-256 equality with the current provider checksum; filename and size are not
sufficient. Modified media needs its verified pre-metadata checksum. Pending
rewrite, capture-repair or path-reconciliation debt keeps this conservative
qualification blocked. Retained reconciliation destination choices are naming
evidence, not pending work, and remain part of the dependency comparison. No metadata writer is enabled automatically. A replay
that creates debt holds the cursor; a later cycle may qualify after configured
current-child work completes.

Original media and sidecars are fingerprinted through confined paths; links,
pending temporary writes, replacement journals, shared paths and changed bytes
reject qualification. Preparation warnings retain the aggregate
`invalid_original_files` candidate count and expose `shared_file_links` as a subset,
counting each candidate once at its first rejected media or sidecar. Typed shared-link
errors also expose only the fixed reason `shared_file_links` at certification and
checkpoint stages. Original and current-child files retain the same independent-object
requirement; a hash recheck does not authorize shared inode writes.
Shared-root replacement recovery checks protected paths from
all libraries before opening a journal, including unselected libraries. Current child files and sidecar presence are also bound
into the receipt. Original files are never moved, rewritten, adopted or deleted.
Normal downloads, metadata repair, local path reconciliation, imports and source
tombstones respect the protection before byte writes; SQL guards provide defense
in depth. `reconcile` reports drift in protected originals without converting
them into download retries. Verification and manifest export retain the original
identities, dates, paths and history; download status does not establish ownership.

The final SQLite transaction compares the original snapshot, current child and
family evidence, receipt generation, configuration metadata and prior cursor.
It appends the proof and commits the cursor together. Staged config-reconciliation
cursors use the same transaction. Invalid or stale proof, incomplete current work,
interruption and write failure retain the prior cursor. Existing current-provider
identity and checkpoint guards remain independent and are never cleared to permit
preservation. An unchanged activated receipt can support ordinary incremental
cycles without another full inventory. Changed family, configuration, file or
receipt evidence reactivates the hold while retaining every original snapshot and
prior proof. Reactivation does not authorize historical attribution or rewriting.

`status`, cycle reports and health JSON expose `unattributed_legacy_assets` and
`unattributed_legacy_pending` separately from download failures. The first counts
legacy records still needing attribution; the second counts records awaiting
valid current-work preservation proof. Original capture revision and retry history
remain stored. Advancing current sync does not claim a complete historical backup;
backup safety remains false while attribution is unresolved.

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
live downloaded row is current. Persisted rewrite markers may drain later
because they are durable recovery evidence.

Metadata failure markers must survive so a later run can retry metadata
without downloading the media again. Full enumeration, single-pass incremental,
and collecting incremental sync all commit changed catalogue metadata and its
configured rewrite marker together, before filtering, album routing, or
deciding whether unchanged media needs downloading. A failed commit preserves
the provider checkpoint. Queued rewrites drain once per cycle, after every
producer has finished, so a single writer owns each file. Read-only runs never
drain. Only that drain retires a marker on success, because it writes the file
from the same row it then clears. Each bounded rewrite batch loads current
album and people rows for its library-scoped asset IDs. A grouping-read failure
leaves the affected markers pending. A completed download writes the snapshot
it was planned from, so it records a marker on failure but never retires one.

The first task payload includes its concrete pass album, even when the cycle's
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
failed check preserves the sidecar and its durable rewrite marker. Each attempt
uses a new temporary path so retained ambiguous bytes cannot block a later
retry.

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

### State and serialization

Asset capture and addition dates use fractional Unix seconds in the existing
non-STRICT SQLite INTEGER columns. Readers accept legacy whole seconds and
round fractional seconds to milliseconds without scaling the whole timestamp.
Invalid stored dates fail decoding. Metadata refresh commits these dates with
the metadata and rewrite markers; operational timestamps remain whole seconds.
This representation requires no schema migration or historical backfill.

Schema, primary-key, sentinel, durable-key, and serialization changes are
cross-cutting. Search every reader and writer, migrations, fixtures, reports,
status output, and round-trip tests before changing them.

### Secrets and diagnostics

Passwords stay behind `SecretString` and password-source boundaries. Logging
uses a redacting writer as a backstop, but code must not send Apple IDs,
passwords, session cookies, bearer tokens, or unredacted provider identifiers
to logs or machine output. Preserve process hardening that limits credential
exposure through core dumps.

`password set` prompts only in interactive input mode. Headless callers must
use a password file or password command. The command resolves the secret and
passes it directly to the credential store without printing it.

## Safety contract catalog

Stable IDs connect safety rules to production owners and focused tests.
`scripts/check-contracts` rejects missing links in that chain.

| Contract | Owner | Required behavior |
|----------|-------|-------------------|
| `FILE_PUBLISH_NO_OVERWRITE` | `src/download/file.rs`, `src/download/pipeline.rs` | Publishing a completed `.part` file never replaces an existing final file unless `--repair-truncated` carries exact durable path and fingerprint authorization. A no-replace collision succeeds only when the verified `.part` and destination bytes are identical. Different or unverifiable bytes retain retry evidence and cannot reach metadata writes or downloaded finalization. |
| `TEMP_FILE_DELETE_REQUIRES_DURABLE_OWNERSHIP` | `src/download/orchestration/cleanup.rs`, `src/download/pipeline.rs`, `src/fs_util.rs`, `src/state/db.rs` | Orphan cleanup deletes only an exact stale path claimed in durable state. It retains verified filesystem handles through removal and never follows a directory or file symlink. Normal completion and graceful interruption retire the claim. |
| `SYNC_TOKEN_ADVANCE_REQUIRES_CLEAN_CYCLE` | `src/sync_cycle.rs` | The database pre-check token advances only after a successful non-dry-run cycle with a current pass plan. |
| `SOURCE_CHECKPOINT_REQUIRES_DURABLE_RECOVERY` | `src/sync_cycle.rs`, `src/download/orchestration/` | A zone checkpoint advances only with complete token evidence and durable recovery for unfinished work. |
| `MALFORMED_REQUIRED_ASSET_FIELDS_BLOCK_CHECKPOINT` | `src/icloud/photos/asset.rs`, `src/icloud/photos/album/fetch.rs`, `src/icloud/photos/album/lookup.rs`, `src/download/orchestration/incremental.rs`, `src/sync_cycle.rs` | A live asset with a missing or invalid required identity or capture date blocks its zone checkpoint before filtering or path planning. |
| `UNKNOWN_PROVIDER_IDENTITY_REMAINS_PENDING` | `src/download/retry.rs` | Inconclusive provider identity retains the pending row and records verification evidence. |
| `POLICY_EXCLUDED_REQUIRES_EXPLICIT_SOURCE_DELETION` | `src/download/retry.rs`, `src/state/db.rs` | Policy-excluded rows become source-deleted only after targeted provider deletion evidence. Present or inconclusive responses retain them outside actionable pending work. |
| `METADATA_WRITES_REQUIRE_OPT_IN` | `src/download/metadata_rewrite.rs` | Media and sidecar metadata writes run only for explicitly enabled metadata flags. |
| `METADATA_EMBED_REWRITE_REQUIRES_STABLE_INPUT` | `src/download/metadata.rs`, `src/download/file.rs`, `src/download/metadata_rewrite.rs` | Every embedded metadata rewrite prepares a uniquely owned sibling and replaces the media only while both the destination and prepared bytes match their approved fingerprints. Failure preserves concurrent edits and durable retry evidence. |
| `HEIF_EMBED_REWRITE_REQUIRES_STABLE_INPUT` | `src/download/heif.rs`, `src/download/metadata.rs`, `src/download/file.rs`, `src/download/metadata_rewrite.rs` | A HEIF-family embedded rewrite accepts tone-map insertion only when `dimg` and primary Exif `cdsc` relationships prove the exact target and no existing XMP owns that tone map, prepares a uniquely owned sibling, and replaces the media only while both the destination and prepared bytes match their approved fingerprints. Failure preserves concurrent edits and durable retry evidence. |
| `XMP_SIDECAR_REWRITE_REQUIRES_STABLE_INPUT` | `src/download/metadata.rs`, `src/download/metadata_rewrite.rs` | An existing XMP sidecar is replaced only when it parses and its bytes still match the writer's initial read. Failure preserves the sidecar and durable rewrite marker. |
| `XMP_GPS_ACCURACY_REQUIRES_MATCHING_LOCATION` | `src/download/metadata.rs`, `src/download/metadata_rewrite.rs`, `src/state/db.rs` | Native horizontal accuracy requires valid native latitude and longitude equal to the exported provider coordinates, plus a matching verified-download source checksum. Recovered local or download checksums cannot establish native provenance. Readable missing, invalid, or mismatched evidence omits accuracy and clears only obsolete kei-owned values. Source I/O failure preserves unknown fields and durable retry evidence. |
| `METADATA_CAPTURE_REVISION_REPAIR_IS_DURABLE` | `src/download/orchestration/maintenance.rs`, `src/state/db.rs` | Revision repair updates catalogue metadata and configured rewrite evidence before promotion, stays library-scoped, and preserves the provider checkpoint on unresolved work. |

## Change-impact checklist

| Change | Check |
|--------|-------|
| CLI command or flag | `src/cli.rs`, dispatch in `src/lib.rs`, help output, Docker, services, Homebrew, docs, CLI tests |
| TOML or runtime config | defaults, setup output, CLI precedence, hashes, persisted examples, docs |
| Selection or pass scope | list/sync/import parity, shared libraries, unknown names, unfiled scope, membership snapshots |
| Provider checkpoint | full and incremental proof, interruption, config drift, retry durability, scoped DB pre-check |
| SQLite schema/query | migration from every supported version, all readers/writers, status/report/manifest output, real SQLite tests |
| Provider record parsing | missing/malformed fields, shared zones, identity mapping, metadata capture, fixtures |
| File or path behavior | `.part`, checksum, no-overwrite publish, fsync, import compatibility, collision handling |
| Metadata writes | opt-in gate, pre-publish mutation, sidecars, retry markers, feature combinations |
| Service behavior | Linux, macOS, Windows, container defaults, status, install/uninstall renderers |
| Credentials or logging | secret wrappers, source lifetime, redaction, core-dump hardening, diagnostics, error paths |
| Machine output | JSON/CSV shape, redaction, reports, health, metrics, downstream compatibility |

## Tests

- Unit tests live near their owner module.
- Cross-module and binary behavior lives under `tests/`.
- Live iCloud tests are ignored by default and run single-threaded.
- `tests/data/media-manifest.json` owns the bundled media inventory and size
  budget. `download::orchestration::fixture_tests` exercises these bytes
  through enumeration, planning, download, publication, and reopened SQLite.
  Its selection, naming, metadata, and recovery modules replace content-dependent
  live assertions. `tests/data/live-migration.md` records the mapping. General
  live entry points share `tests/data/live-selection.toml` and reject an empty
  eligible selection before the suite.
  `scripts/fixtures/` owns maintainer-only generation and sanitization and
  the optimized extracted-source-package check. Tests do not fetch media.
- Shell suites cover crash, concurrency, state-machine, and container behavior.
- Fuzz targets cover parser and metadata trust boundaries.
- `justfile` owns local script and workflow lint commands. Protected CI runs
  the same shellcheck, shfmt, ruff, and actionlint checks with pinned versions.

See [the test guide](../tests/README.md) for the current suites and commands.

## Maintaining this guide

Update this file in the same pull request when a change:

- Moves an owning decision to another module
- Adds a command or cross-cutting state transition
- Changes file publication or metadata mutation
- Changes provider checkpoint or retry evidence
- Changes schema, durable keys, or serialization
- Changes the best test for an invariant

Keep the guide focused on stable ownership and safety. Source code remains the
final authority.
