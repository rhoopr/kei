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

## Rollout

Keep the existing PR workload until the replacement qualification route,
publication guards, and nightly run have passed. Activate lighter PR routing
only after that evidence exists. Record cold and warm timings separately.

Release publication must consume the artifacts that passed qualification.
GitHub release assets, GHCR version/latest tags, and Homebrew updates all depend
on that result. Nightly and manual rehearsals have no publishing permissions.

## Local release prerequisites

A release request authorizes planning, not unattended use of credentials or
changes to an existing service. Confirm the isolated test account, selected
fixtures, and host-service prerequisites before running live checks. Never use
an existing media/state tree as disposable test data.

1. Check out the final release commit with a clean worktree. Do not change it
   after qualification starts.
2. Run `KEI_FULL_TEST_REAL_SERVICE=1 just full-test` on the prepared Linux host.
   This requires working credentials, Docker, the pinned tools, and an unused
   user-service slot. Retain the finalized report and private logs.
3. Run `python3 scripts/ci/local_release.py record --repo rhoopr/kei --sha COMMIT --report REPORT`.
   The validator rejects missing, skipped, rate-limited, failed, or dirty-head
   evidence. All discovered shell suites and the real host-service phase must
   pass. Cross-zone testing remains an explicit fixture-dependent extension.
4. Post only the redacted summary and report digest to the release PR. Do not
   upload private logs, credentials, provider identifiers, or local account paths.
5. Repeat the record command with `--publish --evidence-url COMMENT_URL` after
   approval to publish the attestation. The release gate accepts only the latest
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
