# Provider checkpoints and enumeration

A saved cursor says where a zone resumes. The inbox retains provider records needed for replay. Destination receipts record local work. A successful empty poll can leave the cursor unchanged and create no inbox row. Check the accepted successor and outstanding work before interpreting a missing row as a lost checkpoint.

## Sync and provider checkpoints

```text
sync_loop::run_sync
  -> resolve configuration, credentials, libraries, and pass plans
  -> optional scoped changes/database pre-check
  -> sync_cycle::run_cycle
  -> download::download_photos_with_sync for each active library
  -> sync_cycle source checkpoint decision
  -> cycle reporting and watch control
```

Photo readiness is explicit provider evidence. `PhotoLibrary` accepts `FINISHED`
or `RUNNING`; missing and unknown states fail closed. Sync checks readiness before
watch shortcuts and again at the checkpoint boundary. Before downloading from
`RUNNING`, the cycle persists `indexing_reconciliation_required:<zone>` in the
existing metadata table. Presence is the obligation, including unknown future
payloads; no schema migration is needed. The previous trusted zone and database
checkpoints remain unchanged. Cross-zone album sources also contribute readiness.

While indexing, the download dispatcher uses full queries without incremental
replay, retained-work admission, metadata repair, retry retirement, or album
snapshot completion. Ordinary media still uses the existing verified publication
and durable finalization path. Managed primary layouts wait for `FINISHED`.
Watch cycles with a hold bypass the database no-change shortcut and rebuild
collection and album plans so omitted albums can reappear after indexing.

A retained hold forces full reconciliation after `FINISHED`. A scope refreshed
while readiness is `RUNNING` remains stale even if the cycle's later probe says
`FINISHED`; the next wake rebuilds it after readiness. Ordinary inventory recovers
failed work and discovers previously omitted assets. The held full-query path continues to suppress deletion-capable
maintenance, album snapshot completion, and the retained-token bridge even when
its initial probe reports `FINISHED`.

Complete natural EOF, successful local work, a fresh FINISHED scope, and a final
FINISHED probe qualify that inventory. One fenced transaction removes
`indexing_reconciliation_required:<zone>` and writes
`indexing_replay_required:<zone>` without moving the trusted source cursor or
database cursor. The later trusted cycle replays the old cursor and applies
source deletions. Replay presence, including an unknown future payload, bypasses
the watch database shortcut across reopen. Only normal checkpoint promotion
removes replay debt, including aggregate config-drift promotion. Rejection of
the retained cursor preserves the existing checkpoint gates.

Bounded coverage, interruption, failed transfers or
metadata, stale scope, missing proof, state-write failure, and readiness
regression cannot qualify an inventory or advance its cursor. Provisional
inventory cannot replace a comparable inventory anchor.

## Managed work and inventory evidence

Managed primary configuration, recorded bindings and pending operations retain
the RUNNING guard, including a later suffix configuration. Once FINISHED, the
held phase uses the same full-enumeration owner and token/EOF evidence, but
only consumes validated provider identities. It does not invoke local task
planning, journal recovery, publication, metadata drains, retry retirement or
snapshot pruning. A fresh FINISHED plan, unbounded selected scope, valid token
and final FINISHED observations from every source qualify that inventory.
The fenced transition additionally writes
`indexing_materialization_required:<zone>`. Presence, including unknown payloads,
forces a fresh full managed materialization through the existing journal and
publication owners before retained-cursor replay. It bypasses quiet watch
shortcuts and refreshes plans across restarts. An empty change feed therefore
cannot omit assets that inventory-only qualification never queued.

Managed materialization debt survives bounded runs, failed transfers,
metadata failure, interruption, regression and checkpoint-write failure. When an
old cursor exists, both full publication and its incremental bridge must qualify
before one checkpoint transaction removes materialization and replay debt.
Existing local-byte, collision, sidecar, ownership and interrupted-journal guards
still control every managed write. Readiness probes are observations, not an
atomic snapshot guarantee from Apple.

Inventory-drop diagnostics use a complete count measurement from the initial
full-query invocation, captured before activity and delta-bridge composition.
The download owner records natural completion, usable counts, and canonical
resolved pass/query/filter coverage. The cycle compares only equivalent complete
scopes after the existing success, interruption, cancellation and stale-plan
vetoes. Configured bounds, missing counts, targeted subsets and retries cannot
replace a complete anchor. A complete selected smart-folder-only query can qualify;
source replay preserves that query's measurement without adding delta activity.
Mixed refresh subsets cannot supply a complete anchor.
Public `api_total_at_start` remains additive activity telemetry.

Diagnostic anchors use `inventory_anchor_v1:` metadata keys scoped by enumeration
hash, zone and coverage digest, with version and exact canonical coverage in the
payload. Legacy `inventory_anchor:` rows remain untouched and are never certified
retroactively. Malformed or unsupported evidence is ignored; the next eligible
inventory seeds a fresh baseline without an upgrade-time drop warning. Inventory
evidence never authorizes source checkpoints or proves retained deletion history.
The same accepted drop facts feed JSON reports and the durable cycle ledger.

## Lookup and capture recovery

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

Incremental asset-identity lookups, paired-asset lookups, and selected-album
relation hydration in `src/download/orchestration/delta.rs` cancel provider reads
on shutdown. Started state writes finish outside these cancellable waits.
`support_hydration_wait_v1` reports a fixed phase and started, waiting, returned,
or cancelled outcome at INFO, with a waiting observation every 30 seconds.
Returned means the read returned, including errors; it does not certify replay,
identity, or a checkpoint. Interrupted hydration retains the existing checkpoint
and pending configuration. These waits have no new overall time limit and do
not establish a universal shutdown deadline.

Selected relation targets use bounded child lookups followed by complete-pair
validation before a full-zone fallback scan in each configured source.
Omitted, deleted, changed-pair or foreign-scope results remain unresolved;
lookup absence does not authorize deletion. Authentication and request failures
propagate to the checkpoint hold. Sources retain their existing priority.
Within one library sync, relation targets from album passes share a read only
when the request context, endpoint, full zone/owner and ordered fallback sources
match. Targets are combined before reading; no absence cache survives an error,
interruption or cycle boundary. A remaining unresolved target still blocks the
checkpoint through the existing relation gate. A fallback scan can still take
multiple pages and has no new overall deadline.

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
Status aggregates all markers. Selected zones with markers bypass watch-mode
no-change shortcuts. Retry debt remains visible as incomplete backup work but
does not fail process health, including during skipped watch cycles.
Intentional bounded checkpoint holds alone are not failures. The marker keys
remain in metadata; JSON report version 3 is unchanged.

Current destination debt is reported separately from checkpoint permission. The generation owner reads its existing selected-work debt after execution; the cycle owner makes one-shot sync fail while that work remains, including retries whose deadline has not arrived. Reports and notifications retain the partial-failure outcome. `cycle_reporter.rs` independently reports operational health: transfer and provider retry debt stay healthy, while state-access, storage or authentication blockers make `/healthz` fail immediately. Storage errors in transfer workers and metadata-only rewrites stop further file work for that cycle, retain unfinished tasks and markers, and leave watch mode running. These observations add no durable queue or checkpoint permission.

The service starts its HTTP listener after initial authentication and library initialization. During initial authentication, a readiness probe cannot connect; once the listener is running, a failed authentication cycle returns HTTP 503. Watch uses the configured interval after each cycle, including cycles that progress a bounded metadata batch. Explicit capture repair keeps its saved progress for the next run.

The Photos adapter emits aggregate, fixed-label identity lookup diagnostics.
They distinguish omitted records, unexpected types, decode failures, invalid
master references, and record-level provider errors. Reference-zone context is
classified without logging provider identifiers or response bodies. These
observations do not authorize identity guesses or cross-zone retries.
`exact_lookup_rejection_v1` describes a rejected lookup request with fixed
scope/pairing/presence labels and an expected-owner category. It applies the
existing scope matcher; the older reference-zone JSON-equality classification
alone does not establish rejection. Missing or malformed master references and
record errors are distinguished from an explicit different-master reference.
No owner or record value is emitted.

A paired lookup can retain an inconclusive changed-master hint from a live,
scoped child, including when the old master explicitly returns `NOT_FOUND` or
`UNKNOWN_ITEM`. An old-master tombstone or any other record error does not
authorize a hint. Incremental asset hydration makes at most one additional paired
lookup in that same scope during a normal download run. Dry-run and filename-only
modes retain their prior held behavior. Only a complete current pair may update the mapping
through the existing state owner. A missing or deleted redirected master,
foreign scope, another relationship change, cancellation or failed write holds
the checkpoint. Other lookup consumers keep the hint unresolved. Sparse share
links do not authorize this same-scope retry. Selected sparse-target recovery
uses the separate strict qualification described below.

## Sparse identities and source deletion

Unpaired asset deltas retain typed sparse-share evidence from
`isSparsePrivateRecord` and the `linkedShare*` fields. The marker accepts exact
`INT64` and `NUMBER_INT64` type spellings, or an absent type, with integer
value `1`; this does not broaden deletion-field validation. Targeted record
lookups request these fields too; ordinary media enumeration keeps its existing field
projection. A structurally valid link remains unverified. Malformed links and
valid links without a usable master remain unresolved. Link identifiers are
opaque and their debug output is redacted. Lookup changes do not overwrite the
original delta evidence. A link alone does not authorize a target lookup.

Selected sparse-target recovery is owned by
`src/download/sparse_target.rs`. A fresh source lookup must agree with its retained link. The Photos adapter checks the target owner, zone and child/master pair. An unrestricted full-library plan can materialize missing renditions through the ordinary download pipeline.

For a scoped Shared selection, the cycle runs the target library before PrimarySync. A due retained link requests a fresh scan if the target would otherwise replay an older selection. Only a clean scan of the complete current plan and a successful target checkpoint can supply the selection generation used for recovery. The source checks the current child/master pair against the destinations that generation admitted, including overlapping album paths. The checkpoint transaction rechecks those publication receipts and held files. A partial query, an absent member or conflicting identities leave the source checkpoint in place. Absence does not establish deletion.

Album membership can queue a metadata rewrite even when metadata writing is off. In that case the rewrite stays queued and does not block an independently verified media copy. Enabled metadata writers require their publication receipts, and a prepared capture repair always blocks recovery.

A target receipt is useful only with exact current identity, metadata, source
provenance and independent protected-path-safe files. Trusted Legacy File
acceptance does not satisfy this source proof. The source checkpoint transaction
revalidates the source generation, target catalogue/configuration snapshot and
held target files. Partial downloads retain the source obligation. Existing
bounded source retry selection remains authoritative; no general retry budget,
checkpoint reset or debt-backed advancement is introduced. Recovery decisions
use fixed allowlisted labels in retained support history; a target-ready or
prepared historical file is partial work, not a complete backup.

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

Sparse observation and retry writes acquire an immediate SQLite transaction
before reading generation or stale-attempt evidence. Independent WAL writers
therefore serialize before those reads, preserving generation fences without
a deferred snapshot upgrade.

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

## Cursor rejection and advancement

A complete inventory followed by an explicitly rejected retained cursor creates
`retained_checkpoint_hold:<zone>`. The versioned receipt contains a cursor
fingerprint, requested enumeration/preservation policy hashes, an attempt count,
and the next attempt deadline; it contains no provider identity or cursor value. Unchanged
held zones defer provider, inventory and legacy preparation work for one hour.
The initial rejection and two automatic retries form a finite budget of three
attempts per boundary and policy. Exhausted holds perform no further provider
work. Due attempts reserve their count and next deadline durably before requests,
so interrupted or incomplete attempts consume a reservation. A failed reservation
performs no provider work. Version-one receipts without a count retain one spent
attempt. Malformed or unsupported receipts remain an actionable hold with no
provider work. Genuine policy or active cursor changes permit bounded re-evaluation. Original assets, owner/date facts, legacy
generations, transfer retries and sparse/deletion evidence remain intact.

A fresh inventory and its counts cannot establish lost historical deletion
facts or resolve this hold. Permanently rejected boundaries remain actionable
holds; this protocol does not rebootstrap them. A successful validated replay
of the retained boundary removes the hold in the same transaction that stages
or activates its successor. Transaction-time metadata comparisons fence both
hold staging and checkpoint promotion against changes to the planned cursor
and enumeration configuration, including the exact staged candidate read during
aggregate promotion. Existing sparse generation validation
still applies. Global configuration activation remains atomic across selected
zones. Zone-local hold aggregation keeps status and backup safety truthful even
when another zone succeeds.

Ordinary incremental producers carry validated changes/zone completion separately
from query enumeration. Smart-folder-only passes also replay the source zone;
that delta has no media destinations, and the selected folder query still owns
media admission. Its query token never substitutes for the source successor, and
a missing query token retains the selection-proof hold and same-cycle query
recovery policy. Mixed unfiled and smart-folder refreshes retain the actual
delta successor and every identity, partial-scan, cancellation and durable-write
veto. A successful smart query does not certify retained-boundary replay. Typed
provider cursor rejection is distinct from malformed or missing successor data.

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

Support history records when replay from the preserved checkpoint starts. If
replay returns an error before producing a result, history records a fixed
reason instead of the error text. A start without a later result or failure
means no outcome was recorded; it does not identify the cause of the hold.

## Full and incremental enumeration

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
`exact_refresh_outcomes_v1` separates requested/refreshed/remaining task keys
from planned unique child identities, observed child results, planned paired
identities and observed paired results. These are identity counts, not HTTP
request counts. Planned counts may include work cancelled before a result.
Fixed child/task rejection categories distinguish selected-master, rendition,
checksum and size mismatches. Task rejection counts deduplicate durable task
keys; categories are observations, not permission to change selection or retry.
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
Live album names must decode to a nonblank provider name. The Photos adapter accepts a valid plain-name fallback, but never substitutes a record identifier. A live delta without a usable name retains a checkpoint hold and leaves the stored container name unchanged. ID-only tombstones still identify deleted containers. Album discovery fails before returning its name-keyed map if a name is unusable or collides after sanitization, including collisions with smart folders. This prevents silent container loss; allocating independent destinations for colliding albums remains separate work.

Album snapshots and smart folders may require targeted refresh work before or
alongside the incremental stream.

Both strategies use `IncrementalDeltaState` in `orchestration/delta.rs` for
identity mappings, source-state transitions, album and relation bookkeeping,
event counts, and completion tokens. The streaming strategy emits created
assets through a bounded channel and defers album, relation, and unpaired-asset
work. The collecting strategy observes all events before state writes, hydrates
missing identities, and applies relations before routing created assets.
Before reserving paths, collecting coalesces equivalent consecutive observations
for each library, source identity and selected pass, using the later signed URL.
It compares parsed dates, metadata, resources and URL alias relationships; a
different observation remains admitted. Raw event and checkpoint evidence is
never coalesced. Distinct metadata revisions in one batch still follow the
existing first-observation metadata refresh behavior.
These orders preserve each strategy's token-safety and selected-album behavior.
Shutdown races provider-event and successor waits in both strategies; the owned
receiver is dropped before the successor wait to release provider backpressure.
Receiver closure cancels pending changes/zone requests and retry delays, but does
not interrupt an already-started durable shadow capture. Cancellation during
either consumer wait supplies no replay-completion proof.

Downloadable photo records require non-blank `CPLMaster` and `CPLAsset` record names
and a usable `assetDate` before they enter filtering or path planning. Full
enumeration reports a malformed record as incomplete. Incremental enumeration
marks its zone token unsafe. Both routes preserve the prior checkpoint so an
unchanged provider record remains retryable, and neither route counts the
record as a policy, filename, or date skip.

Date-bounded runs may advance only when the producer proves the lower bound did not truncate the stream. Sync count selection, global-frontier construction, per-filter caps and the recent-only recovery engine are retired. Import sampling remains separate. Enumeration, coverage and selection-generation encodings retain their historical uncapped count/scope slots so unchanged uncapped configurations and retained generations do not drift just because the input surface was removed.

## Guarded current inventory after count retirement

Schema 37 seeds `inventory_reconciliation_required:<zone>` once from the existence of each exact `recent_selection_recovery:<zone>` key. It never parses or certifies the old value. Original keys and exact value bytes, including malformed, non-UTF-8 and future-version payloads, remain untouched. An existing inventory request is not replaced. The schema version prevents completed requests from being reseeded on quiet reopen. The new request is presence-only current work, not another historical receipt ledger.

`sync_loop/precheck.rs` includes a selected zone with a nonempty pass plan whenever its request exists, even if changes/database reports no changed zones. `sync_cycle.rs` snapshots the request and current cursor/configuration fences, then passes the obligation to `download/orchestration/dispatch.rs`. With retained cursor P, `reconcile_inventory` performs one guarded attempt per zone per cycle:

1. Replay strict changes/zone from P through terminal successor S. The provider adapter reports terminal completion separately from whether any raw records were observed. Qualification requires a completed replay; cancellation or an incomplete result returns a hold. Identity or state failures can still allow healthy inventory work but veto qualification.
2. Run the ordinary current inventory and publication owner for the configured selection. Supported private selections use `generation::inventory`; other scopes use `full.rs`. Queries drain to natural EOF with usable token evidence, including a filtered tail when fixed dates limit selected work. Query tokens establish enumeration completion, never replace P or S, and never establish an atomic snapshot.
3. Only after successful current work and complete selected coverage, replay changes/zone from S to a terminal successor. Every raw records array in this replay must be empty. An empty emitted-event stream is insufficient: unknown records, tombstones or rank displacement still make the interval nonquiet. A changed interval retains the request and trusted cursor for a later cycle; there is no same-cycle catch-up loop.

Unresolved initial delta identity can still allow independently valid inventory jobs to finish, but vetoes qualification. Failed transfers, metadata, state writes, incomplete enumeration, cancellation, stale plans and readiness regression retain the request. The private generation delta path does not add an implicit inventory beside this attempt. Missing cursors use the ordinary first-run path and retain the request until a later guarded attempt is possible. Healthy completed files and all original historical evidence survive retries.

An explicitly rejected cursor or supported static incremental fallback can still run the ordinary inventory, but cannot qualify the request without the guarded replay evidence. Completed inventory can feed the existing retained-checkpoint hold. A fallback error on the terminal replay does not start a second inventory or substitute a query token. Session, transient and durable-capture refusals keep their existing error routes.

`sync_cycle.rs` retires qualified current work only after required enumeration/path configuration activation and a final FINISHED readiness check. `state/db/checkpoints.rs::commit_checkpoint_transition` compares the exact request value, accepted retained/offered cursor, active configuration hashes, absent pending hashes, and indexing/identity holds. The same transaction completes the inventory's deferred album-membership snapshots and removes the request. Failed comparison or transaction preserves both the request and prior membership; a rejected attempt cannot infer membership absence.

Request retirement writes no source checkpoint. A fixed-date selection can complete this current obligation while its excluded tail still holds the zone/database checkpoint. Sparse identity, expired-cursor and retained provider-work gates remain independent; a qualified request can retire while another hold remains. Legacy coverage certification is retired and cannot supply or veto checkpoint permission. This flow proves neither a provider snapshot nor complete historical deletion coverage, and never authorizes local media deletion.

## Checkpoint advancement

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
