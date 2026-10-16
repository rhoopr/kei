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
