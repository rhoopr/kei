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
| Runtime configuration | `src/config.rs` | Resolves TOML, environment, and command inputs into runtime policy. |
| Selection grammar | `src/selection.rs` | Parses album, smart-folder, library, exclusion, and unfiled selectors. |
| Sync/watch loop | `src/sync_loop.rs` | Owns authentication recovery, watch cadence, library/pass refresh, database pre-checks, and cycle-level reporting. |
| One sync cycle | `src/sync_cycle.rs` | Chooses source enumeration, reconciles config drift, dispatches each library, and advances or preserves provider checkpoints. |
| Library and pass planning | `src/commands/service.rs` | Resolves libraries, collection scope, album plans, smart folders, unfiled passes, and cross-zone hydration. |
| iCloud Photos adapter | `src/icloud/photos/` | Owns CloudKit records, queries, change streams, provider identity, albums, smart folders, and metadata decoding. |
| Response capture | `src/icloud/photos/capture.rs`, `src/icloud/photos/session.rs` | Raw body storage; `src/sync_loop.rs` owns lifetime and failure propagation. |
| Download orchestration | `src/download/mod.rs` | Routes full, incremental, targeted-backfill, and durable-retry work and produces checkpoint evidence. |
| Asset planning | `src/download/planner.rs` | Applies filters, derives tasks, records dispatched pending work, and persists membership and identity mappings. |
| Streaming workers | `src/download/pipeline.rs` | Runs bounded producers and consumers, coordinates file transfer, metadata writes, adoption, and outcome aggregation. |
| File transfer | `src/download/file.rs` | Downloads, resumes, validates, and publishes one media file. |
| State finalization | `src/download/finalize.rs` | Persists downloaded or failed outcomes and retries deferred state writes. |
| Durable retry resolution | `src/download/retry.rs` | Revalidates pending provider identity and builds exact retry tasks. |
| Path rendering | `src/download/paths.rs` | Expands folder templates, normalizes names, and handles collision suffixes. |
| Metadata writing | `src/download/metadata.rs`, `src/download/heif.rs`, `src/download/metadata_rewrite.rs` | Probes and writes opt-in EXIF/XMP data and drains metadata-only retry markers. |
| SQLite state | `src/state/` | Owns schema migrations, role traits, asset state, membership snapshots, provider checkpoints, verification state, and sync runs. |
| Import-existing | `src/commands/import.rs` | Matches existing files to expected iCloud paths and adopts verified files into state. |
| Service integration | `src/service/` | Owns install, uninstall, status, service execution, and platform renderers. |
| Operator surfaces | `src/commands/status.rs`, `src/commands/doctor.rs`, `src/commands/manifest.rs` | Read local state for status, redacted diagnostics, and catalog export. |
| Reports and monitoring | `src/cycle_reporter.rs`, `src/report.rs`, `src/health.rs`, `src/metrics.rs`, `src/notifications.rs` | Converts cycle facts into reports, health, metrics, and notifications. |

## Main flows

### Command dispatch

```text
src/main.rs
  -> kei::main_inner
  -> src/lib.rs::run
  -> src/cli.rs
  -> command owner, service owner, or src/sync_loop.rs
```

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

### Full and incremental enumeration

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

Incremental enumeration consumes changes/zone events. It persists provider
identity mappings before applying created, soft-deleted, hard-deleted, or
hidden transitions. An asset-only `CPLAsset` creation hydrates its paired
master through `masterRef`, the durable mapping, or a targeted identity lookup
before routing; an inconclusive lookup preserves the prior zone checkpoint.
Album snapshots and smart folders may require targeted refresh work before or
alongside the incremental stream.

Downloadable photo records require non-blank `CPLMaster` and `CPLAsset` record names
and a usable `assetDate` before they enter filtering or path planning. Full
enumeration reports a malformed record as incomplete. Incremental enumeration
marks its zone token unsafe. Both routes preserve the prior checkpoint so an
unchanged provider record remains retryable, and neither route counts the
record as a policy, filename, or date skip.

Recent and date-bounded runs may advance only when the producer proves the
bound did not truncate the stream.

### Private CloudKit response capture

`--capture-icloud-responses` opts sync/service into invocation-scoped raw CloudKit bodies before parsing.
No auth traffic, headers, media, extra queries, or expanded fields; redirects stay same-origin.
Under the resolved data directory, `.diagnostics/<timestamp>-<uuid>/NNNNNN.body.part` is fsynced,
published as `.body` without overwrite, then parent-fsynced. Read-only modes permit only these extra writes.
Storage failures stop requests; partial transport bodies allow retries but fail capture. The sync owner
drains in-flight work and propagates failures; partial files may remain, and advanced checkpoints are not rolled back.

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
download and configured metadata writes pass, the file owner atomically
exchanges the verified `.part` file with that exact fingerprint. A changed
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
replaces the source only while an atomic exchange proves the displaced bytes
and prepared replacement still match their approved fingerprints. Metadata-only
retries also pass the checksum-gate fingerprint into the writer, closing the
interval between catalogue validation and the writer's read. Existing regular
files and links at candidate temporary paths remain untouched. Safe pre-exchange
failures remove only the uniquely owned prepared file. Concurrent edits preserve
the source and its catalogue checksum evidence, keep the durable rewrite marker,
and leave any ambiguously displaced entry at its reported sibling path. The
replacement file retains the source permissions.

`METADATA_CAPTURE_REVISION` identifies the catalogue semantics produced by the
current binary. Schema v18 stores per-asset revisions and per-library active
and pending repair state. A normal sync hydrates stale downloaded rows in
bounded provider-lookup batches, independent of album, media, and date filters.
Only an identity that cannot be resolved from durable asset/master evidence
uses the bounded legacy hydration path. Unselected libraries keep separate
pending state and do not force work in selected libraries.

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
speed units, and horizontal positioning error. No coordinate matching or
tolerance is applied for minor Photos location edits.
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

Schema, primary-key, sentinel, durable-key, and serialization changes are
cross-cutting. Search every reader and writer, migrations, fixtures, reports,
status output, and round-trip tests before changing them.

### Secrets and diagnostics

Passwords stay behind `SecretString` and password-source boundaries. Logging
uses a redacting writer as a backstop, but code must not send Apple IDs,
passwords, session cookies, bearer tokens, or unredacted provider identifiers
to logs or machine output. Preserve process hardening that limits credential
exposure through core dumps.

Opt-in raw capture is private, not log output: bodies may contain personal metadata,
identifiers, and credential-bearing URLs. Never share unredacted. Filenames use only generated values.
The resolved data directory is trusted; `.diagnostics` must be current-user-owned, private, and not a symlink.
Capture is Unix-only with `0700` directories and `0600` files; other platforms fail closed.

`password set` prompts only in interactive input mode. Headless callers must
use a password file or password command. The command resolves the secret and
passes it directly to the credential store without printing it.

## Safety contract catalog

Stable IDs connect safety rules to production owners and focused tests.
`scripts/check-contracts` rejects missing links in that chain.

| Contract | Owner | Required behavior |
|----------|-------|-------------------|
| `FILE_PUBLISH_NO_OVERWRITE` | `src/download/file.rs`, `src/download/pipeline.rs` | Publishing a completed `.part` file never replaces an existing final file unless `--repair-truncated` carries exact durable path and fingerprint authorization. A no-replace collision succeeds only when the verified `.part` and destination bytes are identical. Different or unverifiable bytes retain retry evidence and cannot reach metadata writes or downloaded finalization. |
| `TEMP_FILE_DELETE_REQUIRES_DURABLE_OWNERSHIP` | `src/download/mod.rs`, `src/download/pipeline.rs`, `src/fs_util.rs`, `src/state/db.rs` | Orphan cleanup deletes only an exact stale path claimed in durable state. It retains verified filesystem handles through removal and never follows a directory or file symlink. Normal completion and graceful interruption retire the claim. |
| `SYNC_TOKEN_ADVANCE_REQUIRES_CLEAN_CYCLE` | `src/sync_cycle.rs` | The database pre-check token advances only after a successful non-dry-run cycle with a current pass plan. |
| `SOURCE_CHECKPOINT_REQUIRES_DURABLE_RECOVERY` | `src/sync_cycle.rs`, `src/download/mod.rs` | A zone checkpoint advances only with complete token evidence and durable recovery for unfinished work. |
| `MALFORMED_REQUIRED_ASSET_FIELDS_BLOCK_CHECKPOINT` | `src/icloud/photos/asset.rs`, `src/icloud/photos/album.rs`, `src/download/mod.rs`, `src/sync_cycle.rs` | A live asset with a missing or invalid required identity or capture date blocks its zone checkpoint before filtering or path planning. |
| `UNKNOWN_PROVIDER_IDENTITY_REMAINS_PENDING` | `src/download/retry.rs` | Inconclusive provider identity retains the pending row and records verification evidence. |
| `POLICY_EXCLUDED_REQUIRES_EXPLICIT_SOURCE_DELETION` | `src/download/retry.rs`, `src/state/db.rs` | Policy-excluded rows become source-deleted only after targeted provider deletion evidence. Present or inconclusive responses retain them outside actionable pending work. |
| `METADATA_WRITES_REQUIRE_OPT_IN` | `src/download/metadata_rewrite.rs` | Media and sidecar metadata writes run only for explicitly enabled metadata flags. |
| `METADATA_EMBED_REWRITE_REQUIRES_STABLE_INPUT` | `src/download/metadata.rs`, `src/download/file.rs`, `src/download/metadata_rewrite.rs` | Every embedded metadata rewrite prepares a uniquely owned sibling and replaces the media only while both the destination and prepared bytes match their approved fingerprints. Failure preserves concurrent edits and durable retry evidence. |
| `HEIF_EMBED_REWRITE_REQUIRES_STABLE_INPUT` | `src/download/heif.rs`, `src/download/metadata.rs`, `src/download/file.rs`, `src/download/metadata_rewrite.rs` | A HEIF-family embedded rewrite accepts tone-map insertion only when `dimg` and primary Exif `cdsc` relationships prove the exact target and no existing XMP owns that tone map, prepares a uniquely owned sibling, and replaces the media only while both the destination and prepared bytes match their approved fingerprints. Failure preserves concurrent edits and durable retry evidence. |
| `XMP_SIDECAR_REWRITE_REQUIRES_STABLE_INPUT` | `src/download/metadata.rs`, `src/download/metadata_rewrite.rs` | An existing XMP sidecar is replaced only when it parses and its bytes still match the writer's initial read. Failure preserves the sidecar and durable rewrite marker. |
| `METADATA_CAPTURE_REVISION_REPAIR_IS_DURABLE` | `src/download/mod.rs`, `src/state/db.rs` | Revision repair updates catalogue metadata and configured rewrite evidence before promotion, stays library-scoped, and preserves the provider checkpoint on unresolved work. |

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
