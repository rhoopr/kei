# Module index

Use this index for a specific module. Start with the [architecture map](../architecture.md) for decisions that cross module boundaries.

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
| Local drift checks | `src/sync_loop/reconcile.rs` | Runs bounded missing-file recovery and optional periodic read-only catalog diagnostics. Present shorter files remain unchanged until explicit integrity reconciliation. |
| Watch policy | `src/sync_loop/watch.rs` | Owns watch cadence, metadata follow-up timing, and one-shot recovery rules. |
| Cycle ledger reporting | `src/sync_loop/reporting.rs` | Records cycle-run summaries and combines refresh-tail outcomes for reporting. |
| One sync cycle | `src/sync_cycle.rs` | Chooses source enumeration, reconciles config drift, dispatches each library, advances or preserves provider checkpoints, and fences retirement of current inventory requests. |
| Library and pass planning | `src/commands/service.rs` | Resolves libraries, collection scope, album plans, smart folders, unfiled passes, and cross-zone hydration. |
| iCloud Photos adapter | `src/icloud/photos/` | Owns CloudKit records, queries, change streams, provider identity, albums, smart folders, and metadata decoding. |
| Album facade and counts | `src/icloud/photos/album.rs`, `src/icloud/photos/album/counts.rs` | Preserves album configuration and entry points; counts owns count queries and same-library batching. |
| Targeted provider lookup | `src/icloud/photos/album/lookup.rs` | Resolves requested records and merges identity evidence conservatively. |
| Provider enumeration | `src/icloud/photos/album/planning.rs`, `src/icloud/photos/album/enumeration.rs` | Planning selects rank ranges and page sizes; enumeration starts fetchers and delivers completion tokens. |
| Provider page fetching | `src/icloud/photos/album/fetch.rs` | Owns query construction, empty-page probes, record pairing, deduplication, and asset emission. |
| Enumeration completion | `src/icloud/photos/album/completion.rs` | Collects fetcher completion evidence and requires unanimous tokens before returning a checkpoint candidate. |
| Album hydration | `src/icloud/photos/album/hydration.rs` | Matches named-album members across source zones and hydrates durable asset or master identities. |
| Provider change streams | `src/icloud/photos/album/changes.rs` | Owns sequential change scans, delta buffering, terminal raw-record presence/completion evidence, and last-good-token delivery. |
| Download facade and dispatch | `src/download/mod.rs`, `src/download/orchestration/dispatch.rs` | Preserves download entry points and composes full, incremental, targeted-backfill, durable-retry, and guarded inventory work. |
| Download models and configuration | `src/download/orchestration/models.rs`, `src/download/orchestration/config.rs` | Owns controls, results, per-zone checkpoint evidence, reporting projections, checkpoint reasons, coverage fingerprints, and configuration hashes. |
| Download context and selection | `src/download/orchestration/context.rs`, `src/download/orchestration/selection.rs` | Loads library-scoped state and identity evidence, derives pass configurations, and checks incremental routing eligibility. |
| Full enumeration | `src/download/orchestration/full.rs` | Owns bounded enumeration, album snapshots, pass counts, and token evidence. |
| Incremental enumeration | `src/download/orchestration/incremental.rs`, `src/download/orchestration/delta.rs` | Runs separate streaming and collecting strategies with one shared delta-state owner for provider-event bookkeeping and routing facts. |
| Download recovery | `src/download/orchestration/recovery.rs`, `src/download/orchestration/url_refresh.rs` | Runs durable pending recovery and refreshes exact download tasks with current provider URLs. |
| Legacy preservation preparation | `src/download/legacy_preservation.rs` | Validates unprepared historical families and files before storing protection evidence. It no longer certifies prepared history or supplies checkpoint permission. |
| Local catalog maintenance | `src/download/orchestration/reconciliation.rs`, `src/download/orchestration/cleanup.rs`, `src/download/orchestration/maintenance.rs` | Reconciles catalog paths, removes durably owned stale temporary files, and repairs metadata-capture revisions. |
| Asset planning | `src/download/planner.rs` | Applies filters, derives tasks, records dispatched pending work, and persists membership and identity mappings. |
| Filter facade and configuration | `src/download/filter.rs`, `src/download/filter/config.rs` | Preserves filter entry points and shares path and filter settings between sync and import. |
| Asset eligibility | `src/download/filter/eligibility.rs` | Owns content and date filters and media classification, without path or filesystem decisions. |
| Rendition selection | `src/download/filter/versions.rs` | Owns full-resolution original, current-edit and alternative selection for included components, provider rendition keys, and checksum-based historical resource recovery. |
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
| Staging identity | `src/download/file/staging.rs` | Coordinates destination guards, versioned staging names, and cleanup leases so retries and collisions keep independent staging files. |
| Managed layout file handover | `src/download/file/primary_layout.rs` | Snapshots layout fingerprints, copies verified members, publishes, recovers prepared links, and retires aliases for a confined managed layout. Transition policy stays in `src/download/orchestration/primary_layout.rs`. |
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
| HEIF native Exif | `src/download/heif/exif.rs` | Extracts native Exif and repairs capture timestamps, offsets, and paired subseconds within existing TIFF storage. Unsafe or insufficient storage leaves repair pending; native values are verified before publication. |
| HEIF XMP | `src/download/heif/stream.rs`, `src/download/heif/xmp_write.rs` | Reads the selected packet and plans bounded item-map replacements without re-encoding image payloads. The in-memory `xmp_read` remains a test and fuzz oracle. |
| HEIF streaming | `src/download/heif/stream.rs` | Reads bounded control data and metadata packets, streams unchanged file ranges, and compares protected payloads with fixed buffers. |
| HEIF preservation checks | `src/download/heif/stream.rs`, `src/download/heif/preservation.rs` | Streams production checks of non-XMP payloads and opaque metadata; the in-memory validator remains a test and fuzz oracle. |
| Metadata rewrite facade and immediate execution | `src/download/metadata_rewrite.rs`, `src/download/metadata_rewrite/immediate.rs` | Preserves rewrite entry points and coordinates download-time metadata writes. The pipeline retains media publication and state finalization. |
| Metadata write planning | `src/download/metadata_rewrite/planning.rs` | Owns opt-in flags and embedded field decisions shared by immediate and queued execution. |
| Original orientation | `src/icloud/photos/metadata.rs`, `src/download/metadata_rewrite/embedded.rs` | The provider adapter binds master orientation to the original JPEG/TIFF resource. The writer requires unchanged verified-download bytes and a proven orientation omission. The independent orientation flag and policy revision participate in frozen recipes. |
| Metadata enrichment policy | `src/download/filter/metadata.rs` | Adds album names and people only under their independent metadata options, including queued rewrites. Standard XMP excludes provider-internal fields. Policy revision and enrichment bits participate in selection and destination fingerprints. |
| Metadata write execution | `src/download/metadata_rewrite/embedded.rs`, `src/download/metadata_rewrite/sidecar.rs` | Coordinates fingerprint-guarded embedded writes and source-aware sidecar writes through the metadata owners. |
| Queued metadata retries | `src/download/metadata_rewrite/queued.rs` | Tags durable rewrite work, executes bounded retry pages, and retires completed markers. |
| Rewrite evidence and groupings | `src/download/metadata_rewrite/capture.rs`, `src/download/metadata_rewrite/grouping.rs` | Verifies capture-repair fingerprints and timestamps; loads current library-scoped groupings for queued work. |
| Account namespace | `src/account.rs` | Hashes exact configured login and provider realm without alias inference. |
| Account database boundary | `src/state/db/account.rs`, `src/commands/migrate_state.rs` | Validates an independent owner before state consumption, creates authenticated state, and explicitly adopts a preserved legacy snapshot. |
| SQLite facade and connection lifecycle | `src/state/mod.rs`, `src/state/db.rs` | Preserves state entry points, opens connections, and dispatches blocking SQLite work. Child owners retain their transaction boundaries. |
| Observed provider pages | `src/icloud/photos/inbox.rs`, `src/state/db/provider_inbox.rs` | Binds validated incremental observations to the account, retains source identities and original bytes atomically with a shadow receipt, and applies bounded backpressure. It does not authorize provider checkpoints. |
| Provider source catalog | `src/icloud/photos/projection.rs`, `src/state/db/provider_catalog.rs` | Validates replay provenance, derives versioned source facts, and atomically indexes records, references and unresolved evidence. Its receipt does not authorize queues or checkpoints. |
| Shared selection decisions | `src/state/db/provider_selection.rs` | Defines decisions and lossless native destinations used by active generations and publication. Historical shadow tables have no runtime writer or replay path. |
| Provider work admission | `src/icloud/photos/album/work.rs`, `src/download/orchestration/generation.rs`, `src/state/db/provider_generations.rs` | Confirms current provider identities and admits per-destination obligations. `provider_work.rs` retains guards for historical catalog admissions; its old scheduler and writer compile only in tests. Checkpoint authority stays with the existing cycle. |
| SQLite schema | `src/state/schema.rs` | Owns schema versions and migrations. |
| State-store contracts | `src/state/db/contracts.rs` | Defines store roles and records exchanged with callers. |
| Asset state transitions | `src/state/db/assets.rs` | Owns asset lifecycle, download finalization, retry eligibility, and provider source-state transitions. |
| Shared SQLite writes and row decoding | `src/state/db/asset_writes.rs`, `src/state/db/rows.rs` | Shares asset writes within caller-owned transactions, column projections, date codecs, and row decoding. |
| Metadata capture retries | `src/state/db/metadata_capture_retry.rs` | Stores generation-fenced ambiguity evidence and bounded retry deadlines. The cycle owner retains checkpoint authority. |
| Durable provider identity | `src/state/db/identity.rs` | Stores library-scoped asset/master mappings and legacy state ownership. Provider record parsing stays in the iCloud adapter. |
| Durable membership | `src/state/db/membership.rs` | Stores album snapshots, provider relations, and grouping projections. |
| Membership departure | `src/download/orchestration/primary_layout/membership.rs` | Retires unchanged owned album copies through the existing layout journal after current selected destinations complete and exact photo lookup confirms identity. Native copies require current publication authority; preserved-local copies and unowned sidecars stay in place. |
| Durable metadata work | `src/state/db/metadata.rs` | Stores capture revisions, path-specific rewrite work, and repair receipts. It does not write media files. |
| Embedded field ownership | `src/state/types.rs`, `src/download/metadata/`, `src/state/db/metadata.rs` | Writers measure changed properties and preserve exact prior values. Existing path and active-destination receipts retain prepared input/output checksums and field evidence; layout journals carry the same evidence through publication. Explicit source absence plus matching prior write proof authorizes clearing. Recipe completion alone never adopts a preexisting field. |
| Download field publication | `src/download/file/publication.rs`, `src/download/finalize.rs`, `src/state/db/assets.rs` | The file owner distinguishes installed output from identical-existing deduplication. Only installed output supplies measured field ownership to the downloaded-state transaction. Deferred writes carry the same evidence. An identical preexisting file cannot acquire ownership from discarded temporary output. |
| Current-selection metadata authority | `src/state/db/metadata.rs`, `src/download/orchestration/generation.rs` | Ordinary generation tails page through current exact destinations. Historical catalogue markers and older-generation writers cannot authorize a write or a checkpoint hold. Explicit capture repair keeps its library-scoped queue. |
| Durable checkpoints | `src/state/db/checkpoints.rs` | Stores provider checkpoints, scoped database tokens, and enumeration progress. Its fenced transition also commits deferred album snapshots with inventory-request retirement; sync policy decides when proof permits either operation. |
| Temporary-file ownership | `src/state/db/temp_files.rs` | Stores losslessly encoded paths and durable temporary-file ownership. Filesystem cleanup stays in download orchestration. |
| Durable reconciliation | `src/state/db/reconciliation.rs` | Stores destination reservations and reads catalog evidence for reconciliation. |
| Durable import adoption | `src/state/db/import.rs` | Atomically adopts imported files into state and reads imported records. Matching policy stays in the import command. |
| Durable layout journals | `src/state/db/primary_layout.rs`, `src/state/db/primary_layout/directory.rs` | Stores preservation-backed layout bindings, operations, members, path claims, immutable historical receipts, and typed exclusive-album directory moves. |
| Prior publication recovery | `src/state/db/pending_publication.rs` | Recovers a rendition's unchanged prior current publication and retains conditional recovery evidence. |
| Legacy preservation evidence | `src/state/db/legacy_preservation.rs` | Stores immutable originals and protected paths, retains historical coverage certificates without new certificate writes, and reads historical counters. It never assigns identity, writes files, or infers provider completion. |
| State reports | `src/state/db/reports.rs` | Reads status, verification and manifest data, and records sync-run history. |
| Import-existing | `src/commands/import.rs` | Matches existing files to expected iCloud paths and adopts verified files into state. |
| Service integration | `src/service/` | Owns install, uninstall, status, service execution, and platform renderers. |
| Support evidence | `src/support/`, `src/state/db/support.rs` | Allowlisted bounded runtime history and strictly offline immutable collection; see [support export](../support-export.md). |
| Operator surfaces | `src/commands/status.rs`, `src/commands/doctor.rs`, `src/commands/manifest.rs` | Read local state for status, redacted diagnostics, and catalog export. |
| Reports and monitoring | `src/cycle_reporter.rs`, `src/report.rs`, `src/health.rs`, `src/metrics.rs`, `src/notifications.rs` | Converts cycle facts into reports, health, metrics, and notifications. |

## Dependencies within the album and file owners

Album-owner dependencies are one-way: hydration uses enumeration, lookup and
change scans; enumeration uses planning, fetching, and completion; fetching uses
planning types and completion evidence. Lookup, counts, planning, completion,
and changes do not call sibling owners. The facade retains stream routing and
public entry points. No album child owns durable checkpoint policy.

File-owner dependencies are one-way: transfer calls validation and publication;
publication calls replacement, platform primitives, and fingerprinting;
replacement calls platform primitives and fingerprinting; reconciliation calls
platform primitives and fingerprinting; validation uses fingerprinting for
local-size evidence. Platform primitives do not decide replacement or retry
policy. In particular, Windows partial-exchange recovery stays in replacement.
Windows native move and backup-replacement calls share an absolute, lossless
UTF-16 path adapter. It resolves ordinary Win32 separators and components
without filesystem traversal, then supplies extended drive or UNC paths.
Existing verbatim namespaces and native units remain intact; embedded NUL is
rejected. Stored paths, confinement, no-overwrite flags, ownership checks and
replacement recovery do not change. This avoids MAX_PATH failures in nested
managed-primary staging without requiring a registry or manifest change.
