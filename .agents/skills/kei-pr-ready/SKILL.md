---
name: kei-pr-ready
description: Validate a kei branch before review without publishing or changing it. Use when asked whether a kei change is PR-ready, to run the repository gate, to check review readiness, to validate a branch or diff, or to identify missing tests, consumers, documentation, safety evidence, or user-flow checks before PR preparation.
---

# Validate a kei branch

Produce an evidence-backed readiness verdict. Keep the pass read-only unless the
user separately asks to fix a failure. Do not commit, push, open a pull request,
or change GitHub state.

## Preflight

1. Read repository-root `AGENTS.md`, then the relevant sections of
   `CONTRIBUTING.md`, `docs/architecture.md`, and `tests/README.md`.
2. Run `just agent-status`.
3. Resolve the remote default branch, falling back to `origin/main`, then run
   `just review-scope BASE=<resolved-base>`.
4. Record the exact base, merge base, and head. Inspect committed, staged,
   unstaged, and untracked changes. Return a not-ready verdict if unrelated
   work prevents an attributable review.
5. Create a coverage ledger for every changed `(status, path)` entry, including
   tests, docs, workflows, deletions, and renames. Mark each entry `reviewed` or
   `skipped` with a concrete reason. Keep unchanged callers and invariant owners
   in a separate context list. Never silently omit a changed entry.

## Classify impact

Map every changed behavior to the owner and direct consumers in
`docs/architecture.md`. Apply each matching row in its change-impact checklist.
At minimum, classify:

- provider identity, enumeration, checkpoint, or retry behavior
- SQLite schema, query, durable key, sentinel, or serialization
- file, path, publication, import, or metadata behavior
- CLI, configuration, machine output, service, or documentation
- tests, scripts, workflows, packaging, or release behavior

Trace shared types and literal consumers with `rg`. Identify the applicable
safety-contract IDs and focused scenario slices. Do not treat the round-trip
gate or a passing unit test as proof that all consumers were traced.

For file or path behavior, list every alternate byte-landing and
downloaded-state finalization route, including normal download, local path
reconciliation, import/adoption, pending recovery, explicit repair, and
metadata rewrite where applicable. Compare each reached route against
checksum, no-overwrite publication, metadata, fsync, durable state, retry,
checkpoint, and root-confinement invariants. A normal-download test does not
prove another route.

Record review depth by behavior and lens. Use separate coverage-ledger rows
for correctness, safety, liveness, performance, and user-visible metadata when
they apply. Do not label a complete owner or module "fully inspected" when the
review covered only one lens, such as scale.

## Validate

1. Run the smallest matching focused test or `just test scenario NAME`.
2. Apply the [contribution testing rules](../../../CONTRIBUTING.md#tests),
   including the changed command and its consumers for CLI or user-flow work.
3. Apply the [state-transition proof requirements](../../../tests/README.md#state-transition-proof).
   Report the required evidence or a concrete reason the proof does not apply.
4. For schema, primary-key, sentinel, durable-key, or serialization changes,
   search every old literal and prove migration and round-trip behavior.
5. Run `just gate`.
6. Investigate every failure. Use bounded output and
   `just agent-failure-summary` when a retained full-test log is relevant.

Treat gate, CI, and full-test results as proof only when the record shows that
the run stayed on one head and that head matches the reviewed head. Label
results from the same branch at another head as `STALE` and results from
another branch as `OTHER BRANCH`.

Do not run live tests unless the changed behavior requires them. Follow
`tests/README.md`, run live suites single-threaded, and preserve rate-limit and
shared-session constraints.

## Review and report

Self-review the complete end-state diff for unrelated scope, missed consumers,
weak evidence, accidental API changes, unnecessary abstractions, and stale
documentation.

Report:

- base, head, and reviewed diff scope
- merge base and validation provenance
- changed-file coverage ledger and separate context-file list
- impact classification, owners, safety contracts, and scenario slices
- behavior-specific review depth for correctness, safety, liveness,
  performance, and user-visible metadata
- state-transition proof or a concrete reason it does not apply
- exact validation commands and results
- unresolved failures, skipped checks, risks, and missing evidence
- final verdict: ready or not ready

Never report ready when a required check failed, was skipped without a
documented reason, could not be attributed to the reviewed head, or any changed
file is unaccounted for.
