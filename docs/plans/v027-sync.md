# v0.27 baseline and incremental sync plan

Status: matching and conditional ledger adoption are in progress. Normal sync integration and full qualification remain outstanding.

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

The provider checksum is an opaque MMCS identifier, not a demonstrated local
content digest. The existing strict import verifier compares only a 64 KiB
prefix. Equal-size files with identical prefixes and different tails pass it.
The new characterization test enters the real verifier and proves that limit,
including a mismatching-prefix negative control and unchanged bytes/mtime.

Valid ordinary receipts bind the provider generation and selected rendition to
a verified source digest and a final local digest. Rechecking the final local
digest can preserve metadata-modified files without a remote download. Layout
receipts with source hashes inherited from older download_checksum fields are
not independent source evidence. Receipt-free candidates remain uncertain; the
reported archive's reusable coverage is not established by these fixtures.
Do not introduce a new certification framework, request reporter diagnostics,
access attachment 33785032, or download whole files to hide the evidence gap.

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
   Do not merge, publish beta artifacts, contact reporters or write a live account.

Initial cargo check passed. Further validation and implementation status must be
reported separately; preflight tests are not beta qualification.
