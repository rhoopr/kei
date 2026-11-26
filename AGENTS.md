# AGENTS.md

kei syncs iCloud Photos media and metadata to local storage.

## Protect the photo library

- For changes to sync, state, or file writes, use the [owner map](docs/architecture.md#owners) and [safety contracts](docs/architecture.md#safety-contract-catalog).
- Verify media before publication, preserve existing files, and sync the parent directory before finalizing durable state. Replacing media or metadata requires authorization under the applicable contract.
- Unknown provider identity keeps work pending. A verified local file alone cannot authorize advancing a provider checkpoint.
- Keep credentials, Apple IDs, and unredacted provider identifiers out of logs and reports.
- When changing unsafe code, document the `SAFETY` invariant and update [UNSAFE.md](UNSAFE.md).

## Verification and permissions

- Choose [focused tests](tests/README.md#running). For durable behavior, follow the [state-transition proof](tests/README.md#state-transition-proof). Finish code changes with `cargo fmt --all --check` and `cargo clippy --all-targets --all-features -- -D warnings`.
- Live-provider and real host-service checks need explicit authorization and an isolated fixture.
- Confirm scope before removing supported behavior, weakening safety coverage, changing a public contract, or adding a dependency. An explicit request for that change is sufficient.
- Push or open a PR only when requested. Changes to `main` go through pull requests.
