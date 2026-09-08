# AGENTS.md

kei is a Rust CLI that syncs iCloud Photos media to local storage.

Read by scope:

- Code work: workflow and Rust style in `CONTRIBUTING.md`.
- Ownership, sync, state, filesystem, provider, or cross-cutting work: relevant
  owner, flow, invariant, and impact sections in `docs/architecture.md`.
- Test changes or failure attribution: relevant sections of `tests/README.md`.

## Discovery

Before editing code:

1. Run `cargo check`.
2. Locate the owner in `docs/architecture.md`.
3. Use `rg` for the exact symbol, command, flag, SQL name, config key, error,
   durable key, or serialization shape.
4. Read the owner and direct callers.
5. Trace every consumer of shared types, CLI/API surfaces, schema, primary
   keys, sentinels, tokens, paths, and serialized values before changing them.

Resolve material ambiguity before implementation. State assumptions in the
plan when they affect behavior, safety, or a public interface.

Keep policy in its owner. Path rendering does not decide sync policy; the
download pipeline does not parse CloudKit records.

## Safety

- Never lose, corrupt, truncate, overwrite, or silently discard user media or
  metadata.
- Preserve `.part` writes, SHA-256 verification, no-overwrite publication,
  parent-directory fsync, and durable state finalization.
- Local media or metadata rewrites require an explicit user option.
- Preserve provider checkpoint gates across interruption, retry, partial work,
  config drift, and stale planning.
- Unknown provider identity is durable retry evidence, not permission to delete
  or forget work.
- Keep provider quirks and record parsing in `src/icloud/`.
- Keep trust-boundary validation and data-loss guards intact.
- Never log passwords, cookies, bearer tokens, Apple IDs, or unredacted
  provider identifiers. Preserve secret wrappers and redaction.
- Exception: with debug assertions and `KEI_REQUEST_DUMP_DIR` explicitly enabled, unredacted Photos request/response bodies may be dumped to that private local directory; headers, URLs, normal logs, and builds without debug assertions are excluded.
- Keep `unsafe` local. Document each block with a concrete `SAFETY` invariant
  and update `UNSAFE.md` when unsafe code changes.

## Implementation

- Do not block the async runtime; use async I/O or `spawn_blocking`.
- Profile before performance-only changes. Bounded concurrency must preserve
  file, state, retry, and checkpoint invariants.
- Add `#[must_use]` when ignoring a result can lose state, safety, or a
  user-visible decision.
- Keep provider, state, filesystem, and policy layers separate.
- Every changed line must serve the requested behavior, required tests or docs,
  or cleanup made necessary by the change. Report unrelated cleanup separately.

## Tests and completion

- Follow the testing contract in `CONTRIBUTING.md`; investigate every failure
  before changing product code or calling it unrelated.
- Run focused tests while iterating and `just gate` for PR-ready work.
- Finish with `cargo fmt --all --check` and
  `cargo clippy --all-targets --all-features -- -D warnings`.
- Run live tests single-threaded only when needed; follow `tests/README.md`.
- CLI changes: run the command and inspect help, docs, Docker CMD, systemd
  `ExecStart`, and Homebrew paths where applicable.
- Schema, primary-key, sentinel, durable-key, or serialization changes: search
  every old literal and prove migration and round-trip behavior.
- Update `docs/architecture.md` when ownership, a documented flow, or an
  invariant changes.

## Restrictions

Get explicit approval before deleting production code or tests, changing a
public CLI/API contract, adding a dependency, pushing, opening a pull request,
or changing `main`.

Never use star imports, unexplained `#[allow(...)]`, `git add -A`, `git add .`,
`git commit --amend`, or `sudo`.

Documentation uses direct language and no em dashes.
