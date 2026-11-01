# OpenCode resume handoff: mixed provider fixture

Recorded 2026-11-01 18:47 UTC. User directed wind-down and requested WIP archival, coordinated by the integrator. Do not publish or integrate this unfinished patch independently.

## First action: inspect the existing build

On CachyOS via the established normal SSH connection, inspect the already queued build before doing anything else. At handoff PID 3510632 (bash -s) and its PID 3510637 (flock -x 9) were waiting on /home/robhooper/dev/kei-v027-sync/.scratch/beta-build.lock. This batch owns the queued job; it was not killed and might acquire the lock and compile after handoff. Do not start a duplicate or edit the frozen source while pending/running. Inspect process and lock status, queued-build-nd.sh, build-nd-cargo.log, candidate-nd-bin.sha256 and timestamps. Determine whether the queued build finished, failed, or has an uncertain outcome before using its artifact. At handoff candidate-nd-bin and build-nd-cargo.log are the OLDER pre-review build, not evidence for the current patch. Preserved copies are candidate-pre-review-nd-bin and build-pre-review-nd.log. The invocation log was empty when saved as queued-build-local-output-at-handoff.log; future cargo output goes to build-nd-cargo.log. No new job was started during wind-down.

## Source and WIP archival manifest

Worktree: /home/robhooper/dev/kei-mixed-fixture-fix. Detached HEAD and base: 9a0b5eb58c0a3735ba7bafa28cafc2de27bc9586. Main /home/robhooper/dev/kei-v027-sync was clean at that head when last inspected but is shared and can advance. /home/robhooper/dev/kei remains untouched.

Exact useful source for coordinated archival on an explicitly WIP branch, NOT beta integration:
- M src/sync_loop/runner/tests/mixed_provider_shapes.rs SHA-256 7eb871e1cd1de7746750351c72e6d0bde42a7778ae303ff9d90fd6ce1f778ec2
- M src/sync_loop/runner/tests/v027_baseline/raw_replay/two_library.rs SHA-256 7f2dbad5bbac5f9fc2e4d5eee3846c01fc858d7362fd988e476b5c9c027cf083
- ?? src/sync_loop/runner/tests/v027_baseline/raw_replay/two_library/fallback.rs SHA-256 385dca11888ee3a68b4a0ca5ed04dd076a91c229964aa0ef963ce75c6003c1b2

Complete tracked plus untracked diff: candidate.patch SHA-256 9ec60e1f5cd342b5504de049d4082c5d5711d19703a960182dc96b4f7f35d41c. This is TEST-ONLY, UNQUALIFIED, and may fail. No production/schema/generated/dependency changes. Preserve small handoff/review reports as appropriate; exclude binaries, private logs/databases, synthetic database contents, build caches, secrets and unrelated worktrees. Do not include speculative rank-race research in the source WIP branch, rejected into_lease work, other denied actions, sparse/crash/retry owners' work, or unrelated changes. No WIP here is expressly denied for archival, but no patch is safe to publish as a beta fix without the gates below. Integrator owns branch creation/push coordination. No commit/push/PR/merge/tag/release occurred in this batch.

## Implemented fixture and source review

Original mixed_library_provider_shapes models two six-cycle histories, identity/legacy separation, bytes/path/receipt stability, independent library progress, reopen, quiet tails and provider-encoding equivalence. Its intended incomplete changes page was unreachable: the provider treated only absent/page-* cursors as inventory but the actual cursor was before. The expected first-two-cycle hold was therefore not caused by the injected page. Merely changing the expected token was not justified.

The current candidate makes a typed BAD_REQUEST reachable at before for IncompleteInventory in cycles 0 and 1; invalid-token fallback sees an underreported zero count hint, a valid master/child prefix at rank 0, then malformed rank page at rank 1. The hint creates one ordered tail fetcher; offset 1 shows it processed the prefix master. Separate counters require the token, prefix and later failure. Rank queries require explicit startRank and indexing-state requests are separately classified. Existing checkpoint/identity/bytes/retry/quiet assertions are retained. This is a different fault path from the old intended missing-moreComing changes response: do not claim endpoint-specific coverage of the latter.

Before now enters the paginated changes snapshot. Counters require a continuation cursor for PageCuts/DuplicateEmpty and a continuing empty page for DuplicateEmpty. They aggregate libraries/cycles: they prove at least one such response per replay, not every library or recovery cycle; duplicates are adjacent within a page, not cross-page. Healthy before now returns snapshot observations, so the exact original six-cycle test must pass before claiming encoding equivalence.

Separate fallback.rs controls use authenticated synthetic owner, ShadowCapture, selected private libraries, scoped receipts and actual media bytes. They cover complete/incomplete rank fallback, independent peer progress, byte-for-byte prior descriptor retention on incomplete work, source/local checksum, path/receipt assertions, reopen and later ordinary delta without repeated rank inventory. After a bare completed inventory, no active selected-observation descriptor is intentional conservative behavior; independent review confirmed missing descriptor alone is not a bug. Direct wiremock startup avoids a silent bind skip.

Independent read-only reviewer /root/sparse_review reviewed ordered-prefix modeling, pagination reachability, authenticated scope/receipts/checksums and descriptor retention; no source-level blocker subject to reached tests. The reviewer verified applied source matched reviewed candidates apart from formatting. It did not run tests or issue a final qualification disposition. No reviewer agent remains running.

## Exact result status and continuation

Initial cargo check --locked --offline passed on clean 9a0 base; see initial-check.log. rustfmt and git diff --check passed before freezing. Current patch 9ec60e1f has NOT run any tests in either mode; its queued ND build is only --lib --no-run. Both feature-mode executions, strict Clippy/static/boundary gates and independent final review remain open. Old ff3d diagnostic results cannot qualify this patch. Do not infer a pass from build/list or a skipped bind.

1. Determine queued build outcome and binary provenance, then run from immutable exact ND binary: original mixed_library_provider_shapes across both histories and all three encodings, plus both new fallback tests. Confirm nonzero test counts, no bind skip, reached prefix/failure/continuation/empty counters, and existing receipt/byte/checkpoint/reopen/quiet behavior. If failure, use one bounded KEI_MIXED_REPLAY trace and trace the actual branch; do not alter expectations to pass.
2. When source is stable and ND passes, compile once in all-features mode under shared lock, exact disk guard and copy/hash the executable while still holding the lock; run the same exact focused tests. Run repository-required strict Clippy both modes, fmt, diff and relevant static/boundary checks. No duplicate full suite.
3. Obtain independent final review of the exact patch and evidence. Apply/rebase only after checking current main and coordinating other owners. Integrator coordinates WIP archive; integration requires separate qualification and authorization.

## Research, dependencies and restrictions

Prior causal evidence: /home/robhooper/dev/kei-v027-sync/.scratch/provider-progress-5105/STATUS.md. Separate exploratory worktree /home/robhooper/dev/kei-mixed-fallback-owned at ff3d and packet /home/robhooper/dev/kei-v027-sync/.scratch/mixed-fallback-owned/NEXT-CYCLE-REPORT.md retain a conditional mid-rank insertion race. It fails only under a post-insertion query-token timing assumption not established for the actual provider. docs/architecture.md:1570 treats complete full-pass tokens as authoritative. Do not change production policy or integrate that speculative race on this evidence. Sparse V6 is published in base 9a0; leave its packet and other active crash/legacy/retry work alone.

All testing and source work on CachyOS via established normal SSH; no Mac tests, live provider, private archives, or unapproved access bypass. Shared compile/Clippy/target mutations require main .scratch/beta-build.lock and exact df -Pk >= 15728640 KiB guard, TMPDIR under /home. No source edits during qualifying build, no removed locks/evidence, no killing shared/unowned jobs, no duplicate full suites. Use isolated immutable binaries. Routine tests and safe source edits were already authorized; avoid repetitive approvals. No outstanding user denial relevant to this batch; normal require_escalated SSH was approved after sandbox network blocked initial attempt. Respect actual denials and check uncertain outcomes.
