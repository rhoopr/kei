# OpenCode handoff — recorded original sidecar preservation WIP

Status: **UNQUALIFIED; do not integrate, merge, tag, release, or represent as
a fix.** User requested wind-down; no further builds, tests, source changes,
or independent pushes were started after that request. The shared integrator
is coordinating archival WIP branches separately.

## First steps on resume

1. Read this handoff and the current checkout/agent instructions. Use only the
   established SSH route from the connected Mac to CachyOS; run no tests on the
   Mac. Do not touch the separate /home/robhooper/dev/kei checkout.
2. Check current processes, lock owner, logs and main/remote HEAD read-only
   before restarting anything. Our owned build batch has **terminated exit
   101**; no process from this lane remains. Do not kill other jobs, remove
   locks, or assume another lane is idle. Source was frozen during that batch.
3. Treat this patch as WIP with three observed AF failures and failing strict
   Clippy. Fix/review actual causes on a new isolated worktree, not main.
   Retain every original byte-preservation/ownership assertion. Do not treat
   a held operation as successful recovery.
4. Respect the denied database boundary below. Do not run the cross-binary
   consumer on that target, inspect it, or reach it indirectly. Ask the user
   for explicit authorization if access is still essential; an automatic
   approval rejection is not itself resolved by elapsed time or a new route.

## Source, base, and archival boundary

At handoff the published main checkout
/home/robhooper/dev/kei-v027-sync was clean at
9a0b5eb58c0a3735ba7bafa28cafc2de27bc9586 on
simplify/v027-baseline-incremental, matching its configured upstream status.
Recheck: other work may have advanced it.

The isolated worktree /home/robhooper/dev/kei-visible-archive-final is
detached at the same 9a0b5eb base. It has these modified tracked files:

- src/download/mod.rs
- src/download/orchestration/generation.rs
- src/download/orchestration/primary_layout.rs
- src/state/db/contracts.rs
- src/state/db/primary_layout.rs
- src/state/db/provider_generations.rs
- src/state/db/reconciliation.rs
- src/sync_loop/runner/tests/primary_layout.rs
- src/sync_loop/runner/tests/v027_baseline/indexing_layout.rs

There were no non-ignored untracked source files. Ignored scratch, target
artifacts, test roots and databases are not source and must not be committed.

Exact tested full diff:
unfiled-candidate.patch SHA-256
3a1d868c99282d9888a33f8a0c1aa00a64f87b5a548c706c82a8e08a080081f8.
unfiled-source-before.manifest and unfiled-source-at-handoff.manifest match;
each manifest SHA-256
c666f698cfc4e9f8274b961fc51fd6aa871d26783c566501ac47c344286ae098.
The original
primary_layout_preserves_sidecar_bytes_and_fences_old_metadata_queue test body
and its byte-exact visible archive assertion remain unchanged; an additive
owner-backed regression tests that safety obligation.

For the integrator's **archival branch only**, a source-only filtered patch is
archive-export/safe-wip.patch SHA-256
13a1736fe2b34a440841460fc1bc26633b45d571163989098365fc005ba045eb.
It is based on 9a0b5eb and passes git apply --cached --check against that base
index. It excludes the ignored historical consumer associated with the denied
database and has no added into_lease action. It contains no binaries, database,
or logs. **It is not byte-identical to the tested full patch and has not been
built or tested itself.** Integrator must inspect and label it unqualified WIP
on an archival branch; do not independently race pushes. Do not upload ignored
target binaries, SQLite files, WAL/SHM, private logs or retained synthetic
roots. This local handoff contains a path explicitly identified by auto-review;
redact that path and any private details from any remotely shared report.

## Implemented versus proposed

The draft identifies a selected original already present as the same recorded
operation's claimed member.old. With explicit refresh and capture repair off,
it copies the original XMP packet exactly even if its old metadata decision
differs. A new tagged Copied outcome retains source/sidecar fingerprints and
previous decision. The owner checks the exact source/claim/receipt and
conservatively holds existing original metadata or capture-repair debt before
stage, on saves, before preservation and before media and XMP publication, and
at commit. Untagged selected files with historical Some(decision) retain the
ff3d producer's established settlement behavior; missing decisions hold. The
old decision is historical provenance, not new proof every component wrote.

Per the later reviewer adjudication, a Copied archive with member.old is held
rather than replaced. With member.old=None it uses existing physical
no-replace publication; a retry may accept only the exact recorded prepared
file identity. Copied settlement returns before reading or modifying selected
rows and does not advance child capture revision. No global alias scan/index,
schema migration, global filesystem ownership system or cross-process
filesystem atomicity claim was introduced. Physical no-replace does **not**
prove normalized path uniqueness on a case-sensitive filesystem.

Source review of this exact full diff found no additional definite source
blocker within that narrow physical no-replace scope; runtime and strict gates
below override any impression of readiness. Race after a check, crash before
installed receipt, general normalized alias uniqueness, capture-prepared debt,
and unprepared refresh of an existing binding are not proven.

## Qualification evidence

Pre-edit cargo check on 9a0b passed. Frozen AF and ND lib binaries built under
the shared build lock and coordinated CARGO_TARGET_DIR
/home/robhooper/dev/kei-v027-sync/target, TMPDIR under /home.

- AF binary SHA-256
  416cc57c7ba16f0189390d2b6dcca5945740fbd6cb53010327b7d6c059171cbe.
- ND binary SHA-256
  a877f3e7421b7dff1f9417fdd096165fc4eda072fbe6f3d880f97758905f35cb.
- AF test results: unfiled-af-results.tsv; per-test
  unfiled-test-<exact-name>.log. Every listed AF filter ran one test.
- AF strict Clippy failed: unfiled-clippy-af.log reports one library error
  (collapsible if at state/db/primary_layout.rs:547) and 21 lib-test errors,
  including cmp_owned and MutexGuard-across-await. No fixes were attempted.
  The batch exited 101. ND Clippy and ND focused tests were **not run**.
  XMP-specific runtime tests are not applicable without XMP.
- No full suite or release qualification was run.

AF passed (one exact test each):
- recorded_selected_original_existing_metadata_debt_blocks_preparation
  reached VISIBLE_ARCHIVE_EXISTING_DEBT_HELD.
- recorded_selected_original_debt_after_prepare_blocks_visible_publication
  reached VISIBLE_ARCHIVE_DEBT_AFTER_PREPARE_HELD.
- unrelated_fresh_owned_download_keeps_sidecar_and_source_receipt reached its
  fresh receipt/sidecar marker.
- explicit_refresh_selected_original_applies_owner_metadata_before_archive
  reached EXPLICIT_REFRESH_OWNER_WRITER_REACHED. This is owner-planned
  preparation plus prepared recovery, not unprepared refresh of an existing
  binding. The test's GET assertions cover its named synthetic media endpoints.
- copied_archive_refuses_late_foreign_media_and_xmp reached both xmp=false
  and xmp=true markers. The XMP case admits exact prepared media before the
  foreign sidecar refusal; it is not a concurrent syscall-race test.
- copied_metadata_evidence_is_new_and_old_reader_fails_closed.
- primary_layout_legacy_float_journal_reopens_saves_and_commits_exactly.
  This is an existing fresh synthetic state unit, NOT the denied cross-binary
  retained-journal consumer.

AF failed (one exact test each):
- recorded_selected_original_preserves_exact_sidecar_packet_through_recovery:
  VISIBLE_ARCHIVE_EXACT_STAGE_REACHED, so the scoped claimed source produced
  byte-exact staged XMP. The later normal closed-owner cycle returned
  failed_count=2, skipped.uncertain=2, full_enumeration_reason=
  EnumConfigHashDrift, where the test required zero failure. The recovery
  completion and later quiet assertions were not reached. Cause is unknown.
- copied_original_does_not_settle_real_admitted_metadata_selection:
  QueryReturnedNoRows at its exact active-selection query
  (tests/primary_layout.rs:12325 in this frozen source). Do not claim the
  selected archive destination was admitted or the accounting check passed.
  Selector redirect is a source-supported hypothesis, not an established cause.
- copied_original_occupied_archive_holds_and_exact_install_retries:
  COPIED_ARCHIVE_OCCUPIED_HELD and COPIED_ARCHIVE_INSTALLED_RECEIPT_REACHED
  were printed. The later reopen cycle returned failed_count=2,
  skipped.uncertain=2 and EnumConfigHashDrift, where zero was expected. The
  closed-owner retry completion was not proven. The receipt marker is after
  durable installed evidence; it does not cover a crash before receipt.

An earlier AF baseline on published ff3d plus test-only patch failed at the
byte-exact staged archive XMP. Its log/provenance are in
../visible-archive-baseline/REPORT.md. It does not prove recovery.

## Remaining concrete work

- First inspect each failing log and the retained source; determine whether
  changed edited=false→true selection config caused a fixture bridge or a real
  supported progress hold. Do not clear genuine debt or weaken assertions.
- Make the active-selection test reach a genuine owner-admitted archive row or
  report the precise natural projection/redirect that prevents it. Never seed
  SQL authority or turn an absent row into a pass.
- Complete and independently review authentic capture-repair debt,
  prepublication race/hold, and legacy-ambiguous journal controls as needed;
  preserve exact no-replace and component ownership checks.
- Fix the recorded AF Clippy diagnostics only after source review; then freeze
  full source and hash manifests, build each applicable mode once under the
  shared lock, copy immutable binaries, and execute focused tests outside it.
  Run strict both-mode checks and broader qualification only when warranted.
- The old cross-binary prepared-journal consumer remains blocked as below.
  Do not infer it passed from the independent legacy-float unit test.

## Approval and denial boundaries

Automatic approval review rejected a read-only SSH/Python inspection of
[redacted denied retained SQLite target].
The stated reason was possible exposure of private SQLite destination/path
metadata without explicit authorization; it explicitly prohibited bypass by
workaround or indirect execution. The attempted read did not execute. Do not
read that target, run the consumer against it, retry indirectly, upload it, or
use a copied representation. The separate no-replace, debt, refresh and
fresh-download tests create their own independent synthetic state and did not
access the denied target. The historical consumer in the full local WIP source
is excluded from the proposed archival patch. Other rejected actions,
including into_lease, are not authorized by this WIP.

No live account, user archive, commit, push, PR, merge, tag or release was
performed in this lane. The user authorized routine tests, and the latest
wind-down explicitly forbids starting more work now. A later explicit user
authorization may be needed for the specific denied target; do not bypass
automatic review.

## Existing evidence and dependencies

- Full frozen source/AF logs: this directory.
- Earlier baseline and real byte mismatch:
  ../visible-archive-baseline/REPORT.md.
- Owner and source trace: ../visible-archive-packet/DIAGNOSIS.md and
  ../visible-archive-packet/NARROW-OWNER-STATUS.md. Some older owner-dependency
  discussion proposed broader admission; the later explicit adjudication
  narrowed this batch to physical no-replace and unchanged selected rows.
- Source-reviewed debt, explicit refresh, selected-accounting and no-replace
  test proposals: ../visible-archive-negative-controls/,
  ../explicit-refresh-owner-control/, ../copied-active-selection/,
  ../copied-noreplace-controls/ and ../copied-foreign-controls/.
- Main task coordination owns schema/retry/crash work. Do not reuse schema
  numbers, modify another owner's active source, or assume this patch supplies
  the separately needed sidecar admission/settlement lifecycle.
