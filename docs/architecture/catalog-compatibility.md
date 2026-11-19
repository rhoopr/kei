# Historical catalog admission format

Use this reference to interpret evidence retained from the retired scheduler. [Current work admission](provider-replay.md#current-provider-work-admission) belongs to the destination owner.

The following describes the retired schema-31/32 scheduler and its retained evidence. These admission and wake operations no longer run in production.

Schema 31 adds `provider_work_receipts`, generation-bound
`provider_work_obligations`, and the `provider_work_scan` scheduling position.
Before normal dispatch, up to 64 retained, indexed CPLAsset observations per
cycle can trigger scoped child lookup followed by a complete current child/master
lookup. Original bounded confirmation bytes and source page/ordinal provenance
are retained. Duplicate keys, duplicate or omitted records, provider errors,
wrong owners, wrong zones and changed child/master relationships cannot confirm
work. Provider confirmation is an observation, not an atomic provider snapshot.

This slice supports one private library-wide pass with an explicit default owner, without album exclusions or metadata backfill-only execution. Default `PrimarySync` selection lazily discovers the private `/zones/list` response using the pinned authenticated session. Discovery requires bounded original JSON with unique keys, an explicit zones array, unique zone names, no zone errors and no unsupported continuation. Only a live exact `PrimarySync` entry with a nonblank string owner replaces the constructor's ownerless primary library after that exact selected scope returns a recognized indexing state. `RUNNING` retains provisional readiness and requires durable reconciliation; owner qualification alone never certifies a checkpoint. The complete discovered scope is preserved, including explicit owners other than `_defaultOwner`; lookup and inventory references must still match it. The complete validated private zone descriptors are cached separately from initialized libraries. Primary-only lookup initializes only the selected primary; unrelated private indexing failures do not block it. Private and shared library maps publish only after all requested library initialization succeeds. A failed full-map initialization preserves an already qualified primary and retries initialization using the validated private descriptors. Shared endpoint evidence cannot replace private primary ownership. Missing, deleted, blank or malformed primary owners preserve the ownerless constructor; no owner is inferred from a name. The current-work admission slice still requires `_defaultOwner` and is unchanged.
Default selection needs only the cached private listing and selected primary's
indexing success. Normal sync planning and import still request all libraries;
their existing full-map initialization contract is unchanged.

The discovered primary flows through default library selection, `get_library`,
`all_libraries`, `PhotoLibrary::all`, and the existing sole Unfiled admission
checks. Private-discovered non-primary photo zones (`SharedSync-*`) retain their
existing explicit-owner route. Attaching capture before or after discovery
preserves the owned DB context. Retained primary source pages with explicit
returned ownership already use the qualified scope and can confirm current work;
ownerless source pages remain separate retained observations. No source scope is
rewritten or history acknowledged. Shared-endpoint zones, ownerless zones and
different owners remain outside this slice. Other selections retain observations
and use their existing pipeline without an admission receipt. Current
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
Future confirmations that differ from a retained receipt only in complete
eligible Asset `downloadURL` tokens reuse that receipt's original raw bytes,
hash, scope, source and master as the charged representative. The current
authenticated confirmation and planned records are still validated first, and
the retained URL is never used for current download work. This requires exact
masked-byte equality, the same work source/config/scope/master and historical
source hash, and an existing receipt whose own hash still matches its bytes.
Non-URL bytes, unknown or incomplete descriptor shapes, and genuine checksum,
tag, metadata or size changes remain distinct raw confirmations. Reuse adds no
receipt row and does not clear conflict/debt, resurrect admitted work or merge
sources or configurations. Retained receipt confirmations stay historical,
finite evidence tied to their admitted obligation; reuse reads the existing
bytes and never rewrites or duplicates that stored confirmation set. Equivalent
replay therefore grows no further, while genuinely novel confirmations are
still admitted subject to the existing budget.

Matching unfinished projected generations fence shared asset admission, identity
mapping and metadata refresh, including independent path publication receipts.
Content and metadata fences permit safe collision-leaf replanning by the existing
path owners; the original receipt still retains its admission-time filename.
Completed generations do not indefinitely pin the mutable canonical queue.
Schema 32 adds source/config retry deadlines in `provider_work_retries`.
Deferred admission commits its attempt and deadline with the receipt and scan
bookkeeping; admission clears that retry schedule in the same transaction.
The first delay is one hour, doubles on repeated deferral and stops at 24 hours.
A new source or current configuration has its own immediately eligible work.
Each bounded admission selects the oldest due unadmitted source independently
of the historical scan position. A due early source can be retried while new
observations continue to arrive; a future retry does not consume the 64-attempt
budget or block fresh tail work. The SQL query can scan a mostly admitted
source prefix; the attempt limit bounds provider/planner work, not SQLite VM
steps or retained-catalog scan latency. Retry schedules never acknowledge debt
or promote a cursor.

The watch precheck includes due retained work for the same eligible selected
passes and current work fingerprint as admission. Matching unfinished
pending/failed download obligations also wake normal dispatch after restart.
The existing targeted recovery pass owns their publication. Metadata-only
markers keep the existing option-aware metadata precheck owner. Local wake-up
adds selected zones without inventing a provider database successor; provider
and zone checkpoints retain their existing gates. No due local work permits the
normal no-change shortcut. Deferred sources remain retained and become due at
their deadlines; an idle cycle does not imply materialization or completeness.
Work plans have a 16 MiB logical charge limit and work evidence has a separate
512 MiB logical budget. Nothing is automatically pruned; backpressure retains
source observations, unfinished work and the current checkpoint. These are not
physical database/WAL quotas or fixed startup latency promises.

Current versions under current configuration are the approved materialization
scope. Historical observations and all unfinished debt remain retained. Admission
of a current version does not prove historical completeness, acknowledge old
unrecoverable debt, authorize local tombstone deletion or select a historical
retention policy. Existing CheckpointEvidence, sparse proofs, opt-in
metadata writes, publication guards and WAL NORMAL qualification remain in force.
