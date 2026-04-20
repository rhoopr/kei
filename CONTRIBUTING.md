# Contributing

Contributions are welcome. For anything beyond a small fix, open an issue first so we can talk through the approach before you write the code.

## Workflow

1. Fork the repo and create a branch from `main`.
2. Make your changes.
3. Run the pre-push gate:
   ```sh
   just gate
   ```
   This runs the same checks CI does: `cargo fmt --check`, `cargo clippy -D warnings`, the offline test trio (unit + CLI + behavioral), doc lints, lockfile check, `cargo audit`, and typos. Fail-fast.

   Without `just` installed, run the raw commands (see `justfile` for the full list):
   ```sh
   cargo fmt --all --check
   cargo clippy --all-targets --all-features -- -D warnings
   cargo test --bin kei --test cli --test behavioral
   ```
4. Open a pull request against `main`.

All changes go through PRs - no direct commits to `main`.

## Auth tests

Some tests (`tests/sync.rs`, `tests/state_auth.rs`) hit the live iCloud API and need real credentials. These are `#[ignore]` by default. See [tests/README.md](tests/README.md) for setup, then:

```sh
just test live
```

You don't need to run auth tests for most changes. CI runs the offline suite (unit, CLI, behavioral) on every PR.

## Code style

- Rust 2021 edition, stable toolchain
- `cargo fmt` and `cargo clippy` must pass clean
- Warnings are errors in CI (`RUSTFLAGS="-Dwarnings"`)

## License

By contributing, you agree that your contributions are licensed under the [MIT License](LICENSE.md).
