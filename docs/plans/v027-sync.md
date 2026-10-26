# v0.27 baseline and incremental sync plan

Status: implementation and outcome validation remain incomplete. Do not release this branch.

Base: freshly fetched main cc3faff80d237aedcaa07a6d30e791550d1d9f07.
Branch: simplify/v027-baseline-incremental.
No unmerged changes or previous recovery patches are inputs.

## Intended behavior

The first ordinary run ignores all older-version checkpoints and compares current
selected iCloud content with local files. It resumes interrupted work. Once all
observed work is completed, explicitly excluded, or durably retryable, it saves
fresh scoped checkpoints. Subsequent cycles consume those checkpoints and run
independent retries even when the provider is quiet. Historical attribution,
optional metadata completeness and archive-layout convergence do not certify
provider progress.

Never rewrite existing media, metadata or timestamps, relocate recorded paths,
merge provider identities, wipe state, or use bulk downloads to discover matches.
Recover existing publication and layout journals through their current owners.

## Evidence and matching rules

The issue 853 comment at
https://github.com/rhoopr/kei/issues/853#issuecomment-6388935005 describes
41,805 preview paths and 41,318 existing unsuffixed counterparts. Those are
candidate counts, not proof of actual writes or per-file cross-zone identity.
The reported v0.25.2 duplicate writes and preserved baseline/new Shared keys
motivate mixed-version fixtures; they do not license universal zone aliasing.

- Reliably present: a valid receipt binds the current provider resource generation
  and selected rendition to unchanged, fully hashed local bytes. Validate receipt
  provenance, scope, ownership and confined path. A historical download_checksum
  must not silently become source_checksum.
- Missing: no usable file or unresolved candidate exists for selected content.
  Use existing transfer, publication, receipt and reservation primitives.
- Uncertain: plausible candidates lack sufficient evidence, disagree, changed
  locally, or cannot be read safely. Retain visible independent pending work.
  Neither mark them complete nor create a duplicate as a fallback.
- Filenames, suffixes, sizes, paths and old Primary/Shared labels locate candidates.
  They do not establish identity. Separate provider identities remain separate
  even when verified bytes coincide.
- A renamed or suffixed receipt-backed path should stay stable. Album/pass
  obligations must not be collapsed by removing current family checks wholesale.
- Each selected companion/rendition needs its own evidence. A verified still is
  not evidence that its motion component exists.
- Preview and execution must consume the same read-only decision. Execution alone
  admits state and transfers.

### Receipt coverage limits

Provider signatures are not ordinary SHA-256 checksums. Only the explicitly
validated resource-signature variant below supports local content comparison. The existing strict import verifier compares only a 64 KiB
prefix. Equal-size files with identical prefixes and different tails pass it.
The new characterization test enters the real verifier and proves that limit,
including a mismatching-prefix negative control and unchanged bytes/mtime.

The safe unchanged-byte subset requires source_checksum, recorded local SHA-256
and the freshly computed local SHA-256 to agree, with matching provider
rendition/signature/size and valid account/scope. Exclude managed layout-derived
source evidence. This supports mixed-label reuse without remote media.

The public v0.24.1/v0.25.2 writers demonstrate why source/local mismatches cannot
by themselves prove legitimate metadata edits: generic size-based adoption
could record unrelated bytes under the current identity and preserve an older
source hash. v0.26 corrected future writes but existing rows carry no writer
provenance. The synthetic historical SQL regression rejects that false positive
after reopening. Legitimately edited files with only those same columns are
also uncertain. Edited files need an independent receipt binding their identity,
generation, exact path and final bytes; pristine provider bytes are not required.

For a selected resource's fileChecksum, support strict standard Base64 decoding
to exactly 21 bytes with version byte 0x01. The remaining 20 bytes equal
SHA1("com.apple.XattrObjectSalt\0com.apple.DataObjectSalt\0" || complete plaintext).
This matches OpenBubbles/rustpush at commit
b3c96c23e1aae78d1229ebaccb12027624f1eaff, icloud/mmcs.rs prepare_put and
icloud/cloudkit.rs prepare_cloudkit_put. Prefix/length recognition must not be
applied to other MMCS fields, which can use the same prefix differently.

An independent review recomputed 15 fixed vectors using Python hashlib with a
different read partition; Rust tests passed empty, tiny, 64 KiB, 5 MiB and
100 MiB boundary cases plus all 256 version controls and encoding failures.
Bounded live samples from Primary and Shared confirmed this variant, including
tail-mutation negative controls. Neither observation licenses unknown variants.
The keyed v2 variant and metadata-edited bytes remain unsupported.

The new raw-file path can verify an indexed historical candidate, occupied requested/planned path, or checked
same-directory filename-family candidate without historical source receipts and without remote
media transfer. It computes the full local signature and a separate SHA-256,
then rechecks the held file and namespace inside adoption's transaction.
Current provider identity and rendition remain separate ledger obligations.
The provider path is enabled after independent source/vector review and passing
callback-failure, same-size mutation, inode-replacement and reopened-cycle tests.
Completely unindexed arbitrary renames or moves outside those filename families
and directories remain a discovery gap.
Range samples can reject candidates but cannot prove whole-file equivalence.
No bulk remote comparison, weaker identity assumption or permanent pending
bucket counts as resolution of that gap.
Do not request reporter diagnostics or access attachment 33785032.

## Owners and removal sequence

| Stage | Owners and intended simplification | Evidence needed first |
| --- | --- | --- |
| Matching | download/planner.rs; pipeline/producer.rs; orchestration/full/streaming.rs; commands/import.rs | Shared tri-state decision; eliminate existence-only completion and divergent preview shortcuts without weakening provenance |
| Fresh baseline | sync_cycle.rs; sync_loop/precheck.rs; state/db/checkpoints.rs; Photos enumeration/completion/changes | Version both database and zone checkpoint provenance; preserve old rows for rollback; resumable inventory and concurrent-change catch-up |
| Independent retry | download/retry.rs; orchestration/recovery.rs; state/db/assets.rs; provider_work.rs; sparse_identity.rs | Current complete/excluded/retryable admission survives reopen, newer observations, config drift and URL expiry |
| Remove historical completion gates | sync_cycle.rs LegacyCycle begin/certify/finish and LegacyPreservationIncomplete checkpoint override; orchestration/dispatch.rs metadata-backfill routing | Current observation admission must remain lossless; historical rows alone cannot force repeated full inventory |
| Remove optional completion gates | maintenance.rs and queued metadata consumers | Optional metadata failures retain their queues but do not hold an otherwise accounted-for provider token |
| Reduce redundant storage last | provider_inbox.rs; provider_catalog.rs; provider_selection.rs; provider_work.rs; provider_generations.rs; queue_projection.rs | Inventory every producer, consumer and retention/deletion owner; preserve unknown rows, active destinations and unfinished work before compaction/removal |

Keep replacement_recovery, pending_publication, temp_files, reconciliation
reservations and primary_layout journals until their own recovery completes.
Do not add a general event log or replace the queues. Do not redesign albums.

## Checkpoint and interruption contract

A token records accounted-for observed work, not historical archive certification.
Commit durable work before its token. If a current unknown record cannot be
retained losslessly under its account/library scope, preserve the affected token.
Never compare opaque token order. Record a baseline boundary and consume changes
covering inventory time before promotion, including concurrent edits/deletions.
Transfer failure must not require another full inventory after durable admission.
Old cursors and interrupted journals remain available for rollback; no destructive
migration may run before upgrade-interruption tests pass.

## Acceptance and commit sequence

1. Matching evidence tests first: mixed old versions, wrong same-name/same-size
   bytes, equal prefix/different tail, valid old receipts, changed local bytes,
   renamed/suffixed paths, ambiguity, shared distinct identities, missing
   companions and preview/execution parity.
2. Implement shared matching and durable uncertainty using existing owners.
   Assert bytes, recorded paths, metadata and timestamps stay unchanged.
3. Implement versioned baseline/checkpoints and independent retries. Test full
   baseline plus concurrent changes, expiry, second and third incremental cycles,
   quiet retries, selection changes, refreshed URLs and superseding observations.
4. Remove historical and optional-metadata prerequisites with production-cycle
   tests. Preserve scoped unknown current work. Then simplify duplicate storage.
5. Crash/reopen around partial admission, work commit, token save, publication,
   receipt and retirement. Prove stable collision reservations, no forgotten debt,
   repeated download or avoidable inventory. Include interrupted upgrade/rollback.
6. Independent design and final-diff review. Run focused production-path tests,
   formatting, clippy all targets/features, offline broad suites and feature
   combinations, and available platform checks on one stable head. Fix the stale
   branch_static migration-guide assertion minimally only if encountered.
7. Prepare a draft PR with plain-language delivered behavior and exact validation.
   Do not merge, publish beta artifacts, contact reporters or modify iCloud.
   Authorized live reads and bounded downloads use the existing saved session
   and disposable state/media, with no reauthentication or private output.

Initial cargo check passed. Further validation and implementation status must be
reported separately; preflight tests are not beta qualification.


## Current acceptance matrix

These results refer to the working tree, not a release or the last committed
head. Focused tests are evidence of individual outcomes, not readiness.

| Required outcome | Demonstrated | Remaining gap |
| --- | --- | --- |
| Intelligent reuse | Unchanged source receipts across mixed labels, renamed/suffixed paths, separate identities, wrong-content controls | Supported raw missing-receipt matching passes normal-cycle fixtures; activation is validated for the scoped raw-file route; edited/unknown signatures and unrecorded renamed candidates remain gaps |
| Visible uncertainty | Wrong-content/missing receipts remain pending with partial-failure reporting and no duplicate GET across reopen | Safe automatic resolution of the unverified subset |
| Fresh baseline and incremental | Normal-cycle fixtures ignore old tokens, catch concurrent changes, download missing content, then run second/third incremental cycles without inventory | Broader expiry/selection coverage; no unconditional completion under endless rank mutations |
| Independent retries | Failed transfers, quiet provider, changed templates, added/removed albums and changed filename filters preserve original destinations | Other selection combinations and expired-resource cases |
| Existing media unchanged | Bytes, paths, mtime and sidecars checked across reuse/reopen; newer renditions use stable separate destinations | Broader platform and historical metadata interactions |
| Crash and upgrade | SIGKILL at partial admission, work commit, publication, receipt, token transaction; schema36 WAL rollback snapshot | Completion-retirement, mixed legacy-root preservation, replay-cursor and starvation tests pass; old journal continuation guards tested |
| Live normal flow | Primary and Shared: bounded selected baseline and two reopened incremental cycles | This account does not establish reporter archive coverage |
| One sustainable ledger | New raw inbox/rank/parity writes and historical completion/layout planning removed; optional old metadata inputs frozen | Format2 completion and positively accounted sealed format1 roots retire; standalone catalog/inbox owners still need a bounded retirement decision |
| Preview parity | Shared matcher exists | Real streaming preview/execution fixture passes; all selected scope combinations still require final audit |
| Readiness | Independent review found concrete gaps and historical false-positive regression was fixed | Required lint/feature/platform suites, integrity regressions, fresh final review, reviewable commits |

The standalone Primary/Shared live sample tests establish transfer and reuse,
not complete selected-library baseline coverage or the reporter's mixed archive.
Unknown current records hold their affected token if durable work cannot account
for them. Full account/database/zone ownership and opaque token handling remain
mandatory.

Completed response retention had a fixed-capacity liveness problem. Format2
retirement now passes large-baseline, mixed-pending, reopen and process-death
tests. Sealed format1 completion handoff passes positive-accounting and
65-mixed-root starvation tests. These results do not authorize discarding
unresolved or unattributed historical evidence.
Compaction must transfer validated completion into compact per-file receipts,
retain native paths and pass obligations, preserve unresolved/publication/legacy
owners, and retire completed bodies inside a large baseline. Per-cycle markers
must also retire; moving the capacity cliff is not a solution. Historical
legacy-to-active receipt fallback must not launder unverified edited bytes.

Required broad lint, feature combinations and available platform checks are
unfinished. Existing broad failures need classification as obsolete contract,
fixture drift or genuine integrity regressions. Do not restore removed machinery
just to satisfy obsolete assertions. No push, PR or beta artifact has occurred.


Remaining legacy response-retention gap: standalone selection projections,
catalog records, and canonical inbox bodies are not yet retired. Positively
accounted sealed format1 roots now compact completed decisions and retire the
root and rank pages only when every relevant observation is accounted for.
Mixed roots retain rank pages required by unfinished work. Existing transport compaction removes equivalent duplicate
bodies, not all evidence whose work has finished. Format2 completion retirement
does not cover these legacy owners. Reclamation must first preserve unresolved
media, frozen/prepared metadata, destinations, catalog debt, and provider-work
obligations, then remove only validated unreferenced sources in dependency order.
A retry that is not currently due is not proof of completion. The independent
pre-upgrade rollback database remains intentionally retained.

### Current checkpoint and finite blockers

The resume retry slice separates generic asset retries from exact active-ledger
owners. Existing transfer policy still permits one initial and one bounded
cleanup attempt; a quiet reopen under backoff makes no attempt. A due older
observation whose current confirmation differs remains visible, durable retry
work without vetoing unrelated current enumeration. Authentication failures,
new-observation refusal and failed durable writes still block progress.

Startup no longer projects all raw history. Selected-scope preflight considers
only due recorded work with the matching configuration. Wrong scope, changed
configuration and future backoff do not consume or fail that retained source.
A malformed due source preserves its cursor and retry across reopen.

The resumed working-tree focused v027 run passed 103 tests with four explicit
probes ignored. Scope/configuration/backoff controls, independent retry and
failed-deferral-write controls passed. All-target/all-feature compilation,
formatting and diff checks passed. This validation includes preserved
uncommitted compaction/adoption edits, so it is not clean final-head evidence.
Live validation was not repeated because this slice changed local retry
accounting rather than provider transport or content comparison.

The subsequent two-library normal-cycle proof passes in both processing orders
through a baseline and two reopened incremental cycles. A malformed due source
preserves its exact retry, raw bytes and zone cursor; the healthy library
downloads once, advances its own zone cursor, and performs no later inventory
or media transfer. Existing file bytes and timestamps stay unchanged. The
database-wide cursor remains conservatively held while one library is blocked.
The three adjacent raw-replay tests pass. This proof required no runtime behavior
change; a test-only export supplies the real retained-work configuration hash.

Remaining evidence and implementation gaps:
- Independently review the now-tested optional legacy-compaction isolation and
  cross-parent adoption simplification at the final candidate head.
- Qualify remaining pending-count scaling in an optimized build; the path-specific
  lookup now avoids full validation of unrelated provider obligations but still
  hashes their saved manifests.
- Finish obsolete state-API removal and required lint/feature/platform checks;
  classify relevant broad failures without restoring removed contracts.
- Checkpoint a clean final candidate and perform exact-head acceptance and full
  changed-file review. An actual PR review still requires a PR and its exact head.

Historical edited files without independent provenance, unsupported signatures
and arbitrary unindexed moves remain explicit evidence/discovery limits.
Reporter resolution has not been established. No branch push, PR creation or
beta publication has occurred.


### Bounded compaction and adoption qualification

Current-format budget pressure now reclaims current-format bodies only. Legacy
completion cleanup uses one optional bounded batch inside a savepoint; failure
rolls back that cleanup while retaining old evidence. Required unfinished-owner
freezing remains outside that optional rollback boundary.

Synthetic acceptance covers an open baseline exceeding a 64 KiB evidence budget,
a blocked companion retained through 12 reopened successor cycles and 120 new
publications, quiet completion, library separation, changed-template pass
ownership, valid and invalid legacy seals, and process death across receipt,
manifest and root retirement. The repeated-cycle fixture keeps at most two
generation roots and preserves the completed sibling's bytes and timestamp.
These are ledger/publication fixtures, not a new full provider-loop or live run.

An initial measured run reclaimed 39 bodies during 40 current publications,
retained 41,227 bytes of bulky manifest evidence, and performed zero historical
root validations during current budget pressure. The optional successor batch
validated the 65-item historical root exactly once. The 40-publication loop
took 1.407 seconds; the 120-publication reopened loop took 4.878 seconds on the
CachyOS debug build with other tests running. These are observations, not
production throughput or 40,000-file estimates.

The preserved adoption change removes an unused cross-parent finalization branch
and its unsupported unequal-hash inference. Destination validation still checks
confinement and ownership; prepared publication recovery and the atomic matcher
own content proof. The cross-parent raw-signature/stale-history control and four
reservation-integrity tests pass, including foreign ownership and failed durable
reservation writes. Old unscoped prepared lineage remains protected, not cleared
to make explicit repair succeed.

A separate measurement exposed remaining work: ten existing reservation-reader
calls took 879 microseconds with zero pending items and 2,791,898 microseconds
with 100 pending items. The compact reuse transaction calls the same helper,
which loads and validates every pending manifest. This measures that helper
through its public reader, not full matching throughput. A 1,000-item case was
stopped during fixture setup after the smaller case established the issue;
no 1,000-item result or extrapolated release timing is claimed. The retained
regression fixture uses 0/10/100 items without a flaky timing assertion.

Next bounded option: select potential conflicting destinations before loading
their manifests, while preserving canonical/native path collision semantics and
transactional ownership checks. Do not simply filter on the serialized path key
or cache away the commit-time guard. The following checkpoint implements this conservative path-specific filtering.
Full final lint, feature/platform gates and exact-head independent review remain
outstanding. No live validation or publication was repeated in this unit.


### Path-specific reservation lookup

Compact receipt reuse now asks for possible owners of its destination inside the
existing transaction. The filter checks each pending manifest's saved hash,
extracts its path declarations without decoding unrelated provider response byte
arrays, and checks those declarations against the destination table. Unknown,
malformed, noncanonical or contradictory path evidence fails closed. Exclusion
does not certify unrelated provider content, complete work or retire evidence.
Possible collisions still use the original full root, provider, row and scope
validation.

The filter uses the same native/case/AM-PM collision key as the final ownership
check. It deliberately does not filter by library or configuration: an older
configuration or another library may still own the physical destination. No
schema, durable index, queue or cache was added. Existing all-reservation readers
remain unchanged. Tests cover case, dot and AM/PM aliases, non-UTF-8 paths,
contradictory destination rows, damaged manifest hashes, malformed matching
provider evidence, and ownership after a configuration change and reopen.

Paired timings exercise both readers against the same 0/10/100-item fixture in
the same debug binary. They measure helper work, not full sync or production
latency. There is no cached release build, so optimized-build qualification is
deferred rather than launching an unrelated full release build. The algorithm
still scans and hashes pending manifests per reuse; this bounded change does not
remove that scaling term. Before considering an authenticated path summary or a
cache with transaction invalidation, measure the optimized build and review
whether those larger changes are justified.

Final standalone paired debug measurements (ten calls each):
- 0 pending: full reader 1.368 ms; path-specific reader 1.055 ms.
- 10 pending: full reader 264.252 ms; path-specific reader 157.972 ms.
- 100 pending: full reader 2,849.692 ms; path-specific reader 1,428.930 ms.

The affected v027 acceptance slice passed 110 tests with four explicit probes or
subprocess workers ignored. Formatting and diff checks passed. This is focused
qualification, not final broad lint/platform validation or independent PR review.

### Optimized lookup and normal-orchestration measurement

The ignored synthetic workload
`v027_optimized_pending_lookup_and_normal_cycles` runs alone with one test thread
in the default-feature release test binary. Build command:
`cargo test --release --lib v027_optimized_pending_lookup_and_normal_cycles -- --ignored --nocapture --test-threads=1`.
The initial optimized build took 3m18s; the final fixture build reused those
dependencies and took 2m26s. No index, cache, schema or runtime policy changed in
this measurement checkpoint; counters exist only under cfg(test).

Each case uses one downloaded JPEG fulfilling two album passes, plus 0, 100 or
1,000 unrelated pending renditions with approximately 4 KiB synthetic response
padding apiece. Setup reuses one planner instead of rebuilding a full saved-path
planner for every admission. It still uses real admission APIs and their SQLite
checks; setup is excluded from all workload timings. It takes 0.001/0.435/10.275s
in the selected-ledger cases and 0.001/0.499/10.346s in fallback cases. This is not
a claim of linear setup complexity.

Observed final release measurements, seconds:

| Pending | Full reader, 10 calls | Path reader, 10 calls | Selected baseline | Selected quiet, two cycles | Fallback baseline | Fallback quiet, two cycles |
| --- | ---: | ---: | ---: | --- | ---: | --- |
| 0 | 0.000449 | 0.000142 | 0.028525 | 0.005393 / 0.005507 | 0.018539 | 0.002878 / 0.002665 |
| 100 | 0.700566 | 0.131390 | 0.164780 | 0.007302 / 0.006625 | 0.366401 | 0.043271 / 0.043294 |
| 1,000 | 5.536973 | 1.446369 | 1.169534 | 0.013783 / 0.014032 | 2.543877 | 0.503372 / 0.503912 |

Both reader variants run against the same database in each case. The table uses
the selected-ledger paired lookup timings; the fallback paired 1,000-item timings
were similar (5.559374s full, 1.451884s path). Workload timings exclude initial
download/setup. They include ordinary download orchestration, synthetic provider
responses and localhost transport where needed, not a complete account/CLI run.

Observed call counts explain why helper timing alone overstated the common path:
- Selected-ledger baseline: two full reads, zero path-specific reads.
- Fallback baseline: three full reads, two path-specific reads for two passes.
- Both quiet cycles in both modes: zero of either reader.

Source tracing agrees: only a committed RecordedPresent match invokes
reuse_bound_download and its path-specific ownership lookup, once per rendition.
Existing selected-ledger completion can bypass that route. Fallback publication
(for example cross-zone albums) reaches it; the fixture selects that production
mode through the existing album source API using synthetic records, not a live
shared account. Quiet fallback time still grows in this fixture even with zero
lookup calls, so that cost must not be attributed to this helper.

All six cases preserve the original bytes/mtime, retain every pending destination,
download the one original only once and perform no later media GET or quiet rank
inventory. The six-case workload passed in 43s. These are small fixed-output
workloads with increasing pending state, not large-library throughput tests.

Recommendation: another bounded fix for repeated fallback ownership scanning is
justified before beta. At the measured 1,000-pending cost, 40,000 such commits
would add about 97 minutes of path lookup alone if per-call costs remained
similar. That is an arithmetic projection (not observed runtime, not an estimate
of reporter scope); at 100 pending, the analogous projection is about 9 minutes.
The common selected-ledger path did not make these calls. Do not replace the
persistence design based on these measurements. Review a narrow way to avoid
revalidating all unrelated pending evidence per fallback commit while retaining
transactional collision safety, then rerun this exact optimized workload.

The same optimized binary subsequently passed the 110-test affected v027
acceptance slice in 21.51s, with five explicit probes/workers ignored (including
the measurement workload already run separately). Formatting and diff checks
passed. Final broad lint/features/platform checks and independent exact-head
review are still outstanding.

### Bounded fallback candidate-cache change

The fallback receipt commit now reuses a connection-local TEMP candidate index,
following the repository's existing protected-path cache protocol. It is derived
from the original hash-checked manifest path declarations; it is not a second
durable ledger, receipt owner or completion proof. Every possible collision still
runs full current manifest/root/scope validation inside the caller transaction.

TEMP triggers invalidate on any insert, update or delete in the three active
generation/decision/destination tables. Their invalidation rolls back with the
underlying write. External commits, main-schema changes and working-directory
changes also invalidate; reopening has no cached state. Query-only connections
use the uncached path. Candidate reads outside a caller transaction now take one
read transaction so exclusion and authoritative validation share a snapshot.
Routine asset/master mapping updates do not invalidate pending path evidence;
their identity checks remain live in the existing commit path.

The focused controls cover cached absence followed by local and external
admission, malformed evidence with rollback, external completion, schema drift,
query-only use, configuration changes, case/dot/AM-PM/native aliases and reopen.
No persistent schema or general cache API was introduced. Stable pending work
can reuse the index; real changes to pending evidence deliberately rebuild it.
Performance with a constantly changing pending set is not claimed constant.

Population terminology: this lookup considers distinct current-format decisions
with an admitted, unverified destination, not total scanned files or all old
recovery obligations. Completed destinations leave that set during baseline;
completed bulky evidence also drains. No historical reporter counter establishes
40,000 simultaneous pending candidates. The earlier 97-minute arithmetic used
40,000 calls at a fixed 1,000-pending population. A hypothetical linear projection
of the old 145ms/call to 40,000 pending gives 5.8s/call and about 64 hours for 40,000
calls with that population held constant. Those are unobserved arithmetic
scenarios; a growing/draining population has a different cumulative cost, and
the disposable index changes repeated-call behavior when pending evidence is
stable.

Same optimized six-case workload after the disposable candidate index:

| Pending | Ten path lookups before | Ten path lookups after | Fallback two-pass baseline before | Fallback two-pass baseline after |
| --- | ---: | ---: | ---: | ---: |
| 0 | 0.000320s | 0.001106s | 0.018539s | 0.018579s |
| 100 | 0.138566s | 0.022067s | 0.366401s | 0.307232s |
| 1,000 | 1.451884s | 0.163807s | 2.543877s | 2.303657s |

The ten-call after measurement includes the initial candidate rebuild plus
subsequent cached queries; it is not a separately measured warm-call latency.
Do not multiply its average by 40,000 to produce a new ETA. Stable repeated
lookups no longer rehash unrelated manifests on every call. Real pending changes
still invalidate that index. The two-pass fallback still performs three
unchanged full-reader startup calls, explaining its smaller total improvement.
Selected-ledger baseline and quiet timings remain similar (1,000 pending:
1.180s baseline; 15-16ms quiet). Fallback quiet remains about 512ms at that size
and makes neither reservation reader call.

The workload passed with media/no-repeat-transfer and pending-conservation
assertions. The same optimized binary passed 111 affected v027 tests with five
explicit probes/workers ignored. Focused cached-absence invalidation tests,
formatting and diff checks passed. No 40,000-item experiment, live account call,
schema migration or publication occurred. Further broad qualification and
independent review remain required; this checkpoint addresses the demonstrated
stable-pending fallback rescan rather than expanding into other startup costs.

### Retired historical certification test contract

Ordinary sync no longer creates historical attribution certificates, relocates
old files into a new archive layout, or waits for that work before saving current
progress. The tests that required those operations and their exclusive fixture
have been removed together. This does not remove interrupted-operation recovery
or requested metadata completion.

Their applicable safety assertions are covered by current normal-flow fixtures:
shared historical path claims, mappings, revisions and retries remain unchanged;
external media and sidecar hardlinks retain their bytes, timestamps and inodes;
and missing or blank baseline bridge successors cannot save a token or observation
proof. Same-selection reopen must recover before later configuration expansion.
Existing selected Hidden/album/companion, publication/checkpoint interruption,
conservative signature matching, prepared-write and quiet-retry tests retain
those independent contracts. These synthetic checks do not certify any reporter's
archive or replace supported-version migration and platform qualification.
