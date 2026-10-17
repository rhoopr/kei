# CI qualification

PR checks gate ordinary correctness. Release qualification gates publication.
Nightly qualification detects regressions before release day. All results
refer to one exact commit. A skipped check is not a passing check.

## Coverage contract

| Check | Ordinary PR | Relevant PR or full override | Nightly and release |
|-------|-------------|------------------------------|---------------------|
| Format, Clippy, rustdoc, roundtrip, security, lockfile | Required for applicable changes | Required | Required |
| Script/workflow lint, workflow hardening, typos | Required | Required | Required |
| Linux all-feature and no-default offline suites | Required for code changes | Required | Required |
| Scenario catalogs and safety-contract checks | Required for applicable changes | Required | Required |
| Windows/macOS builds and platform smoke | Required for code changes | Required | Required |
| Full Windows/macOS offline suites | Not required | Required for platform-sensitive changes | Required |
| Compact copies/reseed safety fixture | Part of ordinary suite | Required | Required |
| 300-writer copies stress | Not required | Required for scale-sensitive changes | Required |
| Instrumented coverage | Not merge-critical | Available through full qualification | Required report, not a percentage threshold |
| Nightly unused-dependency analysis | Not required | Full qualification | Required |
| Fuzz build and bounded seeded fuzz runs | Not required | Full qualification | Required |
| Optimized offline regression tests | Not required | Full qualification | Required |
| Five release archives and extracted-binary smoke | Not required | Full qualification | Required |
| Both container architectures, entrypoint and service smoke | Relevant smoke only | Full qualification | Required |
| Live provider, live shell, release-binary import, real host service | Never on untrusted PRs | Protected maintainer qualification only | Separate exact-commit evidence for releases |

Full qualification must enumerate ignored tests. Live tests remain opt-in and
single-threaded. Do not substitute an unfiltered `--ignored` run for this
inventory. Cross-zone fixtures and unavailable host platforms need an explicit
applicability decision, not a silent skip.

The local `just gate` and `just full-test` commands retain their existing
contracts. CI reuses their owners and test routes; it does not claim that an
offline workflow performed live provider or real host-service validation.

## Local work

Before opening a PR, run focused tests for the changed behavior plus formatting
and lint checks. Do not replay the complete offline gate locally. PR CI owns
that evidence. Use local live-provider and real service checks only when the
change needs them and the isolated fixtures are approved. `just gate` remains
available for diagnosis or work without hosted CI.

## PR routing

Draft PR events skip routing, quality checks, qualification, and direct Service
smoke builds, including when labeled `ci:full`. The required `CI` job still runs
as a small failing marker: skipped required checks count as success on GitHub.
Marking the PR ready for review starts validation for that event's exact merge
commit, even if the head has not changed. A new head starts a new run.

Converting a ready PR back to draft starts the failing marker and uses the
existing PR concurrency group to supersede the older run. Marking it ready again
requires a fresh successful `CI`; draft skips never satisfy that check. Service
smoke retains its path filters and also handles both draft transitions. Main
pushes, manual qualification, nightly runs, and releases keep their existing
routes. The repository does not currently enable a merge queue.
GitHub does not guarantee concurrency ordering during rapid transitions. If a
ready run is superseded, retrigger validation for the current head; cancelled or
missing work cannot pass the required check.

The required check stays `CI`. A failed, cancelled, missing, or unexpectedly
skipped required job fails that check. The `ci:full` label runs full
qualification. Workflow, script, Cargo, build, fuzz, container-build, and unknown
input changes also select full qualification. Ordinary code changes keep Linux
all-feature and no-default tests, with macOS/Windows build and CLI/media/service
smoke. Filesystem, state, configuration, and service changes select complete
platform tests. The large copies stress test follows its runtime owners:
download, state, Photos provider, sync, configuration, retry, selection,
filesystem/shared types, command service types, media fixtures, and its own
queue-projection fixture. CLI entrypoints, unrelated commands, authentication,
and other unit-test modules do not select it by themselves. The compact
copies/reseed fixtures remain in ordinary offline tests.

Routing uses filenames, not PR descriptions or semantic diff parsing. Changes
inside shared runtime files still select stress even when a particular change
is diagnostic. For example, #984 retains stress because it changes Photos
hydration and session code; #985's reset command and unrelated test modules
skip stress while keeping the full platform tests selected by its entrypoints.
New files within stress runtime owners select stress conservatively. Renames
consider both old and new paths. Full/heavy overrides, unknown top-level
inputs, main pushes, and scheduled qualification retain stress. Docs-only PRs
retain workflow/script lint and typos.

## Rollout

Keep the existing PR workload until the replacement qualification route,
publication guards, and nightly run have passed. Activate lighter PR routing
only after that evidence exists. `CI_LIGHT_PR_ENABLED` defaults to unset, which
retains the broad PR checks. After merge, run Nightly qualification on main,
verify its complete artifact receipt, then enable the variable with value
`true`. Observe the first scheduled run and freshness check. Record cold and
warm timings separately. Clear the variable to restore broad PR checks.

Release publication must consume the artifacts that passed qualification.
GitHub release assets, GHCR version/latest tags, and Homebrew updates all depend
on that result. Nightly and manual rehearsals have no publishing permissions.

## Local release prerequisites

When a maintainer explicitly requests a release, run these local prerequisites
before starting release CI. Verify the configured isolated test account, selected
fixtures, and an unused host-service slot. Ask for input if those prerequisites
are unavailable or ambiguous. Never use an existing media/state tree as
disposable test data, and never replace an existing service. An ordinary PR or
CI rehearsal does not authorize credential-bearing checks.

1. Check out the final release commit with a clean worktree. Do not change it
   after qualification starts.
2. Run `KEI_FULL_TEST_REAL_SERVICE=1 just release-local` on the prepared Linux host.
   This builds the live CLI/container fixtures, then runs provider, shell,
   release-binary, import, and real service checks. It does not repeat the
   hosted offline suite. It requires working credentials, Docker, and an unused
   user-service slot. Retain the finalized report and private logs.
3. Run `python3 scripts/ci/local_release.py record --repo rhoopr/kei --sha COMMIT --report REPORT`.
   The validator rejects missing, skipped, rate-limited, failed, or dirty-head
   evidence. All discovered shell suites and the real host-service phase must
   pass. Cross-zone testing remains an explicit fixture-dependent extension.
4. Post only the redacted summary and report digest to the release PR. Do not
   upload private logs, credentials, provider identifiers, or local account paths.
5. As part of the authorized release, repeat the record command with
   `--publish --evidence-url COMMENT_URL`. The release gate accepts only the latest
   repository-owner status for that exact commit, no older than seven days.
6. Start the release only after those prerequisites pass. A tag starts heavy
   qualification; manual dispatch of Release rehearses it without publication.

The attestation is a maintainer trust boundary, not independent proof that local
commands ran. Keep the underlying evidence. A new commit requires a new run.
Nightly results do not replace this requirement.

## Nightly operation

`Nightly qualification` runs on main at 07:23 UTC each day (03:23 EDT or
02:23 EST). It runs even when the commit is unchanged, since dependencies and
runner images can change. It has no publication permission. Manual dispatch on
main uses the same route; manual Heavy qualification can test a feature branch
without publishing or posting notifications.

`Nightly freshness` checks at 19:23 UTC. A missing, failed, cancelled, or stale
completed run opens one incident issue. Repeated failures do not add comments.
Recovery adds a comment and closes the incident. A completed run must have
started within 36 hours. Scheduled Actions can be delayed or disabled; this
second schedule detects missing qualification runs but is not an external
availability monitor for GitHub itself.

Reports and candidate artifacts are retained for 14 days. PR, nightly, and
release concurrency groups are separate. Nightly work cannot cancel a release.
