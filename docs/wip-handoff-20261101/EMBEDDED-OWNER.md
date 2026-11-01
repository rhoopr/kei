# Kei beta: embedded metadata owner / private-XMP handoff

Status: STOPPED at user request. This is unqualified WIP, not an integration, release or publication candidate. The user wants to resume in OpenCode and asked an integrator to coordinate archival WIP branches. Do not independently push or merge this work; give that integrator the exact source and manifests below. No production changes were made to main during this work. Main `/home/robhooper/dev/kei-v027-sync` was clean at `9a0b5eb58c0a3735ba7bafa28cafc2de27bc9586` on the last check; verify again before any subsequent action.

## First actions for OpenCode

1. Read this file, repository `AGENTS.md` and applicable `.agents/skills`, then read main `.scratch/process-death-fixture-correction/owner-lifetime-e475-wip-v4/REJECT-ONLY-DESIGN.md`, `.scratch/physical-xmp-owner-wip/OWNER-CAPABILITY-REVIEW-v2.md`, its independent review, and `.scratch/embedded-rating-owner-v16/COORDINATION-CHECKPOINT.md`. Use standard existing `ssh -o BatchMode=yes cachyos` access through the authorized Mac; no Mac tests. Do not change SSH/network/host mount settings or use credentials, live accounts or private archives.
2. Inspect active jobs and current branch/worktree status before retrying anything. Verify manifests and source hashes, including untracked files. The shared build lock is `/home/robhooper/dev/kei-v027-sync/.scratch/beta-build.lock`; serialize shared-target compilation, Clippy and mutation through it. Use `TMPDIR=/home/robhooper/tmp`, 15 GiB minimum headroom and copied immutable exact-source binaries with before/after manifests and nonzero exact test counts. Do not kill unrelated jobs or remove locks. Never edit a source snapshot during its qualifying build.
3. Review the sealed no-write owner increment in v19 against the evidence and owner-only denial boundary. It is not compiled or approved for integration. Then take a selective, reviewed physical-XMP owner delta from v5 and the owner-private exchange primitive, not wholesale patches. Before any production activation implement and review the complete authenticated selected-operation lifecycle, exact lease/owner transaction and checkpoint classification described below. Do not present prior-candidate controls as validation of the changed candidate.
4. The team integrator, not this worker, owns any archival WIP branch/push. Coordinate exact base and migration dependencies and have source independently reviewed before any beta integration. Never commit/push private logs, database snapshots, binaries, credentials, private archives, or unreviewed lease-extraction code.

## Frozen v19 source

Worktree: `/home/robhooper/dev/kei-embedded-rating-owner-v19`, detached HEAD `983dd4963149193faebb3cc62b1de1a64caa0a97`. This is intentionally isolated and based on older published schema-42 checkpoint plus unpublished composed retry-43 and identity-44 work. It is NOT a clean patch on current main. Coordinate with integrator before rebasing; do not invent an empty migration 43 or independently occupy migration 45.

- Full tracked + untracked source manifest: `complete-source.manifest`, SHA-256 `57170d3f525e045d5f0f6f094beae84bf763bd232f8857ddb13936411d71acb5`. `complete-source-recheck.manifest` matched at handoff.
- Exact tracked diff: `tracked-wip.patch`, SHA-256 `79adbd6cba968529e28b8ed58c0a6e8cdccd4d121d1faa269eb65dc2da44cd91`. This does NOT contain untracked files; preserve live worktree plus manifest.
- Exact dirty status: `dirty-files.txt`, SHA-256 `f9ac88e3faa36be99e5baf26ecc6827988d310eba40e57956ccdd2c495994c73`; reproduced below. Directory entries marked `??` contain files named by the full manifest.
- Key file SHA-256: `src/state/db/metadata.rs` `9a8d1629b3e4db603b9edb08a23aaa8171ac388b6332cdd69a3c40f90a4b3a38`; `src/state/db/metadata_activity.rs` `959c3bec3ad54ca9615741e472cf66059fbe2fca993d1f44d1aebc880bf5f075`; `src/state/db/metadata_identity.rs` `ef37a9cdd1e185216dd1e320e5062e3b8d0a7bf09d1dd9b7c6985d2e01b1eab6`; `src/download/orchestration/queue_projection/tests.rs` `99f7bb49d447949ccdacf422d1f83d06a1c9c057783b103d504a848d7a0c6825`.

No source modification follows the manifest snapshot.

Dirty files:

```text
 M src/auth/session.rs
 M src/download/file.rs
 M src/download/metadata_rewrite.rs
 M src/download/metadata_rewrite/planning/tests.rs
 M src/download/metadata_rewrite/queued.rs
 M src/download/mod.rs
 M src/download/orchestration/generation.rs
 M src/download/orchestration/incremental.rs
 M src/download/orchestration/models.rs
 M src/download/orchestration/primary_layout.rs
 M src/download/orchestration/queue_projection/tests.rs
 M src/download/orchestration/recovery.rs
 M src/download/orchestration/url_refresh.rs
 M src/download/orchestration/url_refresh/bounded_tests.rs
 M src/download/pipeline.rs
 M src/download/pipeline/consumer.rs
 M src/download/pipeline/outcome.rs
 M src/download/pipeline/outcome/tests.rs
 M src/download/pipeline/pass.rs
 M src/download/pipeline/pass/tests.rs
 M src/download/pipeline/producer.rs
 M src/download/pipeline/streaming.rs
 M src/download/pipeline/task.rs
 M src/download/pipeline/task/staging_tests.rs
 M src/download/pipeline/task/tests.rs
 M src/fs_util.rs
 M src/icloud/photos/album.rs
 M src/icloud/photos/album/changes.rs
 M src/icloud/photos/album/selection_capture.rs
 M src/icloud/photos/album/work.rs
 M src/icloud/photos/asset.rs
 M src/icloud/photos/cloudkit.rs
 M src/icloud/photos/mod.rs
 M src/icloud/photos/session.rs
 M src/state/db.rs
 M src/state/db/metadata.rs
 M src/state/db/metadata_activity.rs
 M src/state/db/provider_generations.rs
 M src/state/db/provider_generations/reference_refresh.rs
 M src/state/db/provider_generations/retirement.rs
 M src/state/error.rs
 M src/state/schema.rs
 M src/sync_cycle.rs
 M src/sync_loop/runner/tests/v027_baseline/fallback_publication.rs
?? src/download/orchestration/generation/
?? src/download/pipeline/retry_owners.rs
?? src/icloud/photos/source_causality.rs
?? src/state/db/metadata_identity.rs
?? src/state/db/provider_generations/compact_attestation.rs
?? src/state/db/provider_generations/immutable_epochs.rs
?? src/state/db/provider_generations/immutable_epochs.sql
?? src/state/db/provider_generations/refreshed_candidates.rs
?? src/state/db/provider_generations/refreshed_source.rs
?? src/state/db/provider_generations/refreshed_store.rs
?? src/state/db/provider_generations/refreshed_transfer.rs
?? src/state/db/provider_generations/refreshed_transfer.sql
?? src/state/db/provider_generations/transfer_wait.rs
?? src/state/db/retry_clock.rs
?? src/sync_loop/runner/tests/v027_baseline/fallback_publication/
```

## Implemented versus proposed

**Implemented in v19, untested after final edit:** the pre-existing selected explicit rating-only no-write producer now retains its live input identity through the real blocking SQLite owner transaction; exact selected receipt acceptance and exact admission retirement precede commit. After commit, `metadata.rs` constructs a private-field, non-Clone `CommittedLease` tied to an opaque live registry binding and operation ID. `metadata_identity::settled_with` consumes that seal and rejects wrong lease/operation; no production caller can construct the seal or obtain a lease-release accessor. `metadata.rs` contains a new test-only wrong-lease/wrong-operation control. The older unrestricted production `settled()` method was removed; test-only `test_settle` is `cfg(test)`. The physical publisher is NOT wired to this seal, and v19 has no production-complete cross-writer fence. An isolated ordinary source review found this no-write seal constructor occurs only after commit and is not returned; full code review and compile remain required.

**Do not integrate as a completed fix.** The actual embedded writer, selected authenticated private-stage producer, physical owner transaction, durable cleanup-debt classification and all route/checkpoint integrations are incomplete. Some earlier integration snapshots expose raw leases: v17 and v18 contain the test/draft `RetainedExchangeOutcome` with lease fields; preserve only as local evidence and do not publish/activate those draft interfaces. The formerly proposed `ExchangedPrivatePublication::into_lease` or equivalent accessor is explicitly excluded.

Review-only design: main `.scratch/physical-xmp-owner-wip/OWNER-CAPABILITY-REVIEW-v2.md`, SHA-256 `7a4887ee4cd8d3b774b0825ae1adc9c595248d1364cf08fac0d689efae948802`. Independent review: `OWNER-CAPABILITY-INDEPENDENT-REVIEW.md`, SHA-256 `ebe934c78a0b367b1287280bb21ff4bf2b9508d79a01495a08c8c319839f47a2`. The proposal retains A continuously from authenticated admission, adds the actual post-toolkit B to that same lease, and never exposes the lease or release token to a caller. Actual selected receipt/pass/config and a fresh private directory are durably owner-bound before creation; toolkit I/O remains inside that directory, and the actual output inode is pinned only after toolkit close. Before public mutation the exact account/provider/generation/pass/destination/operation/transform, persisted prepared output and both parent/name/file capabilities are revalidated in the same SQLite immediate transaction that runs one confined exchange, verifies public B and private displaced A, writes exact receipt/checksum plus separate durable cleanup witness, and commits. Only the owner then settles the private lease. No async await inside that transaction. If filesystem exchange succeeds and DB fails, SQLite rollback does not roll back the filesystem: retain both names, pending state and reservation, report attention, do not reverse exchange or delete unknown entries.

Default all-features JPEG uses XMP toolkit. Parent's later scope decision permits the existing toolkit's pathname operations inside a fresh authenticated operation-owned private stage; do not implement a toolkit FFI rewrite or broad XMP hold by default. POSIX SDK derives temporary names as siblings of its staged file and may replace the staged-output inode. This protects normal cooperative same-process operation only; it is not a guarantee against arbitrary same-user filesystem mutation, a cross-process lock, or FS/DB atomicity. The private work-copy path and separate toolkit output temp are different files: a valid branch test must record the output temp identity immediately before toolkit and after close, not compare work-copy inode to output. Preserve public-path ownership safeguards, recorded paths, prior metadata and timestamps; no automatic existing-file metadata rewrites, unresolved sidecar retirement/recovery, or broad normal-update disable is authorized. Unsupported or uncertain affected work retains evidence and reports attention while independent files/scopes continue.

**Separate physical owner WIP, NOT an integration patch:** `/home/robhooper/dev/kei-physical-xmp-owner-v5`; packet main `.scratch/physical-xmp-owner-v5/`. Complete 671-file source manifest SHA-256 `212d332e0684b727245cb2eeeb276ea0e6c7b3bb34ff281386c189c6b1dbbb8d`; comparison manifest `b8e8e2c3d5e07e91bc8c1cc463a68753f4e852e473224b590b530ace2f701cfe`; WIP patch `ce722a6e3c6bddc72ed22dda01aec21e100532abb47a3f63fd111b7c7ab9ff58`. It is based on an older draft, includes 23 differing files and may reverse v19 changes; do not apply wholesale. It has unnumbered prepared-output/cleanup-witness drafts, private stage reservation, paginated classification WIP, and source-only controls. `finish_selected_physical_stage` deliberately fails closed pending `metadata.rs` transaction + sealed post-commit orchestration. No compile/runtime test/strict check has run. Its earlier copied registry settlement interface is not the reviewed v19 seal. If archiving any selected WIP from it, first inspect exact hunks and exclude unreviewed/raw lease exposure, `.orig` files and generated data.

**Owner-private confined exchange draft:** `/home/robhooper/dev/kei-confined-owner-private/src/state/db/metadata_activity/owner_exchange.rs`, SHA-256 `4e3b963777d89fc6acbf3d2942aa30b9d5b599f04ddcdc36485a77b89d927eba`. No standalone patch or tests of this exact visibility variant exist. It is a child private to the owner, accepts retained parent/file evidence, does one confined exchange without reverse exchange, deletion or fallback, syncs both parents and verifies A/B. It must be consumed only by the owner inside the exact transaction, not made a crate-wide mutation API. On error, preserve all evidence. Platform and cancellation behavior are unqualified.

**Earlier route evidence only:** v17/v18 and main `.scratch/identity-route-adapter/route-xmp-exchange-ownerproof.patch` SHA `12d9f9378e59071a96c756da09a06346416c036c8476ebd4f36aca7775409c57` expose lease fields in an unactivated enum and use a test-only fixture that manually derives its directory and has no pass-bound durable reservation. Do not publish or apply as-is, and do not convert its lease field to any equivalent extractor. Its real toolkit/route test results are useful limited prior-candidate evidence only.

## Tests, failures and unrun gates

- Last before-seal selected-owner source: v16 `owner-v3-source.manifest`, SHA-256 `2a97b314bd444b4371f398f9dfd061fc34916c50173a4f85dfc0b630960773cd`. All-features binary SHA `c6b9e58e9ab30441ddc9ba8060993b339de85fceee049e483e9198b33df908d8`: five exact nonzero controls passed, including real selected-owner cancelled waiter at SQLite precommit, independent path debt, producer/reopen, rollback/reopen and atomic identity pair. No-default binary SHA `a6eb96f76fec86aafece82cc3f2ee1db8d1e011cfcf6cc679419f6a4f72bdf98`: four analogous exact controls passed. Both modes separately passed eight `metadata_identity` registry tests. Exact logs/binaries are main `.scratch/embedded-rating-owner-v16/owner-v3-*`; sources matched before/after. These binaries do NOT contain the final v19 owner seal.
- Earlier first owner build failed six `Send` lifetime errors in a test that used `tokio::spawn`; corrected test drops the actual locally pinned non-Send future. Failure log is `.scratch/embedded-rating-owner-v16/owner-build-af.log`, corrected prior-candidate logs are `owner-v2-*`.
- Separate route source manifest SHA `4827a73be580e17f8a23712f297d4b07ba8865d106f95ae0d906861a5b17f100`; all-features immutable binary SHA `3eed11130db2571438cc483d6774d7ddff2f7da6acbe8dabd8812200496b04f7`. Five exact tests passed: real toolkit output plus synthetic authenticated account, cross-parent retained exchange, target/stage substitution holds, unsupported exchange with no mutation, cancelled exchange waiter. Packet main `.scratch/embedded-rating-owner-v18/`. Fixture is not pass-bound or durably owner-reserved and this binary does not validate production dispatch or owner settlement.
- Current v19 `git diff --check` passed after the seal test edit. V19 has NOT been compiled or run in either mode; the seal negative test and entire physical producer, checkpoint integration, migration and strict Clippy have NOT passed. V14 prior strict AF Clippy failed with 137 diagnostics (including five test-only lifetime findings subsequently fixed in v15); do not waive remaining diagnostics or call them v19 results.
- Final candidate must run actual authenticated pass-bound XMP toolkit publication, before/after toolkit output-temp inode observation, real no-delete exchange and transaction, independent files/scope, wrong operation/lease/account/pass/payload, same-byte substituted inode, cancellation during actual blocking toolkit and owner commit, DB failure after exchange/rollback, retained cleanup debt and reopened status, missing/foreign/valid-JSON tampered witness, >256 settled-debt operations, completed-root retirement/reopen, same-scope successor with older pending activity, ordinary no-write/reopen and unchanged paths/bytes/mtime; both modes where applicable, strict Clippy/fmt/diff/boundary checks and independent code review. No Mac tests; do not claim untested Windows/macOS behavior.

## Outstanding checkpoint, migration and state risks

The uncompiled physical owner WIP has known defects in its earlier checkpoint draft: foreign/misbound rows can be omitted by account-filtered scans after an all-library count; selection=None rows may be skipped without proving independence; an unscoped generic token setter can reject valid settled cleanup debt; library `LIMIT 257` before filtering settled records can block unrelated work after 256 successes; retired generations may leave retained activity pointing at missing roots; and a length-64 output hash alone is not an independently authenticated witness. The v5 worker reported changes intended to page settled records, hold foreign evidence and same-scope unresolved successors, but its latest source has not compiled or been independently verified. Inspect the real source, not the report, and ensure exact immutable output binding, accountable foreign rows, scope-specific transaction guards and compact/retirement behavior. A differing seal does not alone prove old same-scope pending work irrelevant.

Published main schema was 42 when checked; v19 contains actual unpublished retry migration 43 and identity-index migration 44. A separate retry retirement effort may claim 45. The new XMP prepared-output/settlement witness schema is unnumbered and unapproved; choose the next migration only after integrating the real predecessor chain and coordinating with the integrator. Verify old-reader rejection, transaction rollback, owner auth, reset/retention and migration from real released schemas. Avoid unconditional deletion triggers that would block legitimate future reset/cleanup without reviewed semantics. Durable active+prepared+witness triplets may classify content settled plus cleanup debt only if exact authenticated bindings agree; a receipt alone is not settlement or cleanup authority.

## Approval and denial boundaries

The user authorized safe embedded explicit-refresh lifecycle implementation/testing, source/test edits on trusted CachyOS, synthetic tests and the private-XMP staging scope. No fresh approval is needed for routine authorized work. User separately asked to wind down and let the integrator coordinate archival WIP pushes; that instruction supersedes continuing this worker now. No live accounts, credentials, private archives, unrelated worktrees, automatic rewrites, sidecar-retirement activation, merge, tags or release.

Automatic approval review rejected the escalated `ssh -o BatchMode=yes cachyos 'bash -s'` write to `/home/robhooper/dev/kei-selected-xmp-exchange-fixture/src/exchanged_publication.rs`, which proposed `ExchangedPrivatePublication::into_lease` and would also declare the module in `src/lib.rs`. Review reason: allowing callers to extract/release the owner lease could bypass owner-only settlement. It was confirmed NOT executed; module/declaration absent. Do not retry that action, expose an equivalent accessor elsewhere or work around the denial. The newer owner-private seal is a distinct normal-reviewed action, not permission to bypass the rejection. Any further concrete protected action must use normal approval review; if denied stop that action and report exact target/reason/outcome.

## Jobs and safety at wind-down

This worker started no build after the wind-down instruction. The routes worker's own earlier queued no-default primitive build (`flock` PID 3509916, SSH session 49364) was safely canceled while still waiting: session exited 130, no compiler or build log existed, and no other process or lock was touched. It targeted an older crate-visible exchange primitive, not the owner-private variant; it has no result. At snapshot, unrelated shared archive qualification was running under `flock` PID 3506089 with cargo PID 3511624 and rustc PID 3511635; another unowned worker was queued. Do not kill, remove its lock, or assume it still runs later—inspect process/log state first. All original evidence, logs and retained synthetic databases are preserved; do not clean them up.

Key additional evidence: main `.scratch/embedded-rating-owner-v16/COORDINATION-CHECKPOINT.md`; `.scratch/process-death-fixture-correction/` including original fixture baseline, retained replay/authenticated-control evidence and independent reviews; `.scratch/remaining-qualification-groups/f0-process-death-diagnostic.md`; `.scratch/five-bars/qual-f0bdc218-20261030/HANDOFF.md`. Preserve prior snapshots and do not conflate their source/binary hashes with a final integrated candidate.

## Archival source inventory for the coordinating integrator

The exact candidate source set is the union of `changed-tracked-files.txt` (SHA-256 `2f46d27fea67fdf3d9f3e1f4ac23135ebcd36e42151dca8fd14600c6b1d62acd`) and `untracked-files.txt` (SHA-256 `3b1f14da97afca223bc1e6a621c5529cf69d4f372664ae0324154c36c024e8f2`), with per-file hashes in `complete-source.manifest`. The tracked patch alone omits these exact 15 new source files:

```text
src/download/orchestration/generation/compact_policy.rs
src/download/pipeline/retry_owners.rs
src/icloud/photos/source_causality.rs
src/state/db/metadata_identity.rs
src/state/db/provider_generations/compact_attestation.rs
src/state/db/provider_generations/immutable_epochs.rs
src/state/db/provider_generations/immutable_epochs.sql
src/state/db/provider_generations/refreshed_candidates.rs
src/state/db/provider_generations/refreshed_source.rs
src/state/db/provider_generations/refreshed_store.rs
src/state/db/provider_generations/refreshed_transfer.rs
src/state/db/provider_generations/refreshed_transfer.sql
src/state/db/provider_generations/transfer_wait.rs
src/state/db/retry_clock.rs
src/sync_loop/runner/tests/v027_baseline/fallback_publication/issue845_wait.rs
```

The candidate is unqualified and contains composed retry-43 + identity-44 dependencies; archive only on an explicitly labelled WIP branch based on `983dd4963149193faebb3cc62b1de1a64caa0a97`, after verifying the manifest and coordination with the retry owner. Include small human-readable handoff/review reports only if the integrator chooses their exact intended repository location; do not blindly add `.scratch`, generated manifests/logs, synthetic databases, retained files, binaries, credentials, private logs or archives. The v17/v18 lease-exposing draft interfaces, rejected `into_lease` action, v5 wholesale comparison patch with reverse hunks/`.orig`, build targets and the unreviewed old crate-visible mutation primitive must not be published as accepted code. The owner-private exchange draft is separate source evidence, not included in v19; review and archive only a separately labelled non-activating safe version if the integrator determines it is useful. No commit or push was performed by this worker.
