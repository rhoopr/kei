# Contributing

Contributions are welcome. Small, isolated fixes can go straight to a pull
request. For anything that changes behavior, architecture, state, file writes,
or a public interface, open an issue first so we can agree on the approach.

## Before you start

For a nontrivial change, agree on the intended behavior and how to test it in the issue. Get agreement before removing supported behavior, weakening safety coverage, changing a public contract, or adding a dependency. An explicit request from a maintainer covers the change it names.

## Engineering expectations

### Work in the owning layer

The [architecture guide](docs/architecture.md) maps decisions to their owners. Update it when ownership, a documented flow, or an invariant changes. For public interfaces or persisted formats, trace their consumers; for format changes, verify migration from the old representation and round trips of the new one.

### Protect user data

Follow the architecture guide's
[data-safety invariants](docs/architecture.md#data-safety-invariants). Media and
metadata must never be lost, corrupted, truncated, overwritten, or silently
discarded. Local rewrites stay opt-in, provider behavior stays in its adapter,
and trust-boundary validation and data-loss guards stay intact.

Changes involving file writes, SQLite state, provider identity, or checkpoints
must cover interruption, retry, partial completion, and stale configuration.

### Keep the change narrow

- Prefer the smallest complete fix in the fewest owning files.
- Reuse or delete before adding a helper or abstraction.
- Prefer the standard library, then native platform/SQLite features, then
  existing dependencies.
- Add a dependency only when it is clearly better than the existing choices.
- Avoid one-implementation traits, factories, builders, speculative
  compatibility, and configuration without a current requirement.
- Prefer direct, readable code over clever or generalized code.

## Workflow

1. Fork the repository and create a branch from `main`.
2. Add focused tests for behavior changes and run the changed command or user flow when practical.
3. Run focused tests, formatting, and lint checks.
   Every PR runs the Linux baseline. Main pushes run the complete offline suites, platform tests and copies stress.
   Use `just gate` when diagnosing a failure or working without hosted CI.
   The [recipes](justfile) define the commands. See the
   [test entry points](tests/README.md#running) for focused checks.
   Live-provider and real host-service tests belong on an explicitly approved,
   isolated local fixture when the change needs them, not on every PR.
   Read [CI qualification](docs/ci.md) for the PR baseline and release prerequisites.
   Tool versions are pinned in [.github/workflows/checks.yml](.github/workflows/checks.yml).

4. Open a pull request against `main`.

All changes go through pull requests. Do not commit directly to `main`.

## Tests

- Every behavior change needs a focused test.
- Prefer a test that exercises the real production call graph.
- Put unit tests near the owning module and cross-module behavior in `tests/`.
- Assert concrete outcomes and edge cases, not only that a call succeeds.
- For CLI changes, run the changed command, check its help and docs, and verify any Docker, systemd, or Homebrew invocation that uses the changed interface.
- Do not dismiss a failing test as unrelated without investigating it.

Apply the [state-transition proof requirements](tests/README.md#state-transition-proof)
when the change affects durable behavior. That section defines applicability,
the required stages, and the evidence to report.

Some tests (`tests/sync.rs`, `tests/state_auth.rs`, and
`tests/import_existing_live.rs`) contact the live iCloud API and need real
credentials. They are `#[ignore]` by default. See
[tests/README.md](tests/README.md) for setup, then run:

```sh
just test live
```

Most changes do not need live tests. Every pull request runs the offline sync-safety suite.

## Pull requests and review

Describe the end state, not the sequence of edits. Include:

- The behavior changed and why
- The related issue, using `Fixes #123` or `Closes #123` when appropriate
- Affected safety-contract IDs or focused scenario slices, when applicable
- Exact commands and scenarios tested
- Data, state, filesystem, compatibility, or migration risks
- Tradeoffs or follow-up work that remains

For a readiness review, identify the commit you reviewed and any local edits it excludes. Credit CI only for the commit it tested.

Reviews prioritize:

- Correctness and user-data safety
- Ownership and layer boundaries
- Interruption, retry, and partial-state behavior
- Provider checkpoint, state, serialization, and config consistency
- Focused evidence through tests
- Unnecessary scope, abstraction, or configuration
- User-visible behavior and migration effects

## Rust style

- Rust 2024 edition with the minimum Rust version declared in `Cargo.toml`
- `cargo fmt` and `cargo clippy` must pass cleanly
- Warnings are errors in CI
- Use typed errors and `?`; do not use `unwrap` in production code
- Prefer borrowing to cloning and enums to boolean mode arguments
- Use newtypes for IDs, paths, units, tokens, and other easy-to-mix values
- Keep internal APIs `pub(crate)` unless a public surface or test-harness
  precedent requires `pub`

## Documentation

Use direct language and avoid em dashes.

### Wiki documentation

The wiki is a separate repository. Follow the [wiki maintenance checklist](docs/wiki-maintenance.md) for manual version-scoped command/configuration checks, canonical-guide ownership, URL compatibility and publication evidence. Repository tooling changes do not publish wiki content.

## Logs and config snippets

Code must not log Apple IDs, passwords, session cookies, bearer tokens, or
unredacted provider identifiers. Preserve `SecretString`, password redaction,
and other credential boundaries when changing logging or error paths.

Before posting logs or configuration, redact Apple IDs, passwords, session
cookies, bearer tokens, webhook URLs, and local paths you do not want public.
Keep enough surrounding text for the failure to remain readable.

## License

By contributing, you agree that your contributions are licensed under the
[MIT License](LICENSE).
