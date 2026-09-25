# Contributing

Contributions are welcome. Small, isolated fixes can go straight to a pull
request. For anything that changes behavior, architecture, state, file writes,
or a public interface, open an issue first so we can agree on the approach.

## Before you start

Use the owner table in [the architecture guide](docs/architecture.md) before
changing code, then read the relevant flow, invariant, and change-impact
sections.

For a nontrivial change, post a short plan on the issue before implementation:

- What behavior should change, and what are the acceptance criteria?
- Which module owns the decision, and which direct callers are involved?
- Can the change affect SQLite state, provider checkpoints, configuration,
  reports, or local files?
- What happens after interruption, retry, partial failure, or config drift?
- Which safety contract or focused scenario slice applies, and which tests
  will prove the behavior?
- Which user-facing surfaces need matching changes?

Discuss the approach before deleting production code or tests, changing a
public CLI/API contract, or adding a dependency or broad configuration option.

## Engineering expectations

### Work in the owning layer

1. Find the relevant area in [the architecture guide](docs/architecture.md).
2. Search for the exact symbol, command, flag, SQL name, config key, or error.
3. Read the owner module and its direct callers before editing.
4. Trace shared types, traits, schema, serialization, tokens, and paths through
   all consumers.
5. Update the architecture guide when ownership or a documented flow changes.

Keep policy in its owner. For example, path formatting must not decide sync
policy, and the download pipeline must not own CloudKit response parsing.

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
2. Run `cargo check` before code work.
3. Make the smallest complete change in the owning module.
4. Add focused tests for behavior changes.
5. Run the changed command or user flow when practical.
6. Review the diff for unrelated changes, unnecessary complexity, missed
   consumers, and incomplete docs.
7. Run the pre-push gate:

   ```sh
   just gate
   ```

   The [gate recipes](justfile) define the checks and commands. They include
   static checks and offline tests with all features and without default
   features. The gate stops on the first failure. Read
   [the test guide](tests/README.md#running) for other test entry points.
   CI tool versions are pinned in [.github/workflows/ci.yml](.github/workflows/ci.yml).

8. Open a pull request against `main`.

All changes go through pull requests. Do not commit directly to `main`.

## Tests

- Every behavior change needs a focused test.
- Prefer a test that exercises the real production call graph.
- Put unit tests near the owning module and cross-module behavior in `tests/`.
- Assert concrete outcomes and edge cases, not only that a call succeeds.
- For CLI changes, run the changed command and check Docker, systemd, Homebrew,
  and documentation surfaces where applicable.
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

Most changes do not need live tests. CI runs the offline suite on every pull
request.

## Pull requests and review

Describe the end state, not the sequence of edits. Include:

- The behavior changed and why
- The related issue, using `Fixes #123` or `Closes #123` when appropriate
- Affected safety-contract IDs or focused scenario slices, when applicable
- Exact commands and scenarios tested
- Data, state, filesystem, compatibility, or migration risks
- Tradeoffs or follow-up work that remains

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
