# Integration Tests

## Structure

| File | Tests | Description |
|------|-------|-------------|
| `cli.rs` | 66 | Pure CLI-parsing tests — no network, no credentials. Validates subcommands, flags, enum variants, short aliases, and error cases. |
| `auth.rs` | 8 | Authentication tests against Apple's real servers. Exercises `sync --auth-only` and `submit-code`. |
| `sync.rs` | 34 | End-to-end sync tests. Covers download, filtering, folder structure, dry-run, idempotent re-sync, and flag combinations. |
| `state.rs` | 17 | State management tests for `status`, `reset-state`, `verify`, `import-existing`, and `retry-failed` subcommands. |
| `common/mod.rs` | — | Shared helpers: `cmd()` builds an `assert_cmd::Command`, `creds_or_skip()` skips tests when credentials are absent. |

## Running Tests

```sh
# All integration tests (cli tests run without credentials; others skip gracefully)
cargo test

# Individual test targets
cargo test --test cli
cargo test --test auth
cargo test --test sync
cargo test --test state

# Single test
cargo test --test cli help_flag_succeeds
```

## Credentials

Tests in `auth.rs`, `sync.rs`, and `state.rs` require valid iCloud credentials. Without them, those tests print `SKIP: no credentials` and pass.

### Setup

1. Copy the example env file:
   ```sh
   cp .env.example .env
   ```

2. Fill in your credentials:
   ```
   ICLOUD_USERNAME=you@icloud.com
   ICLOUD_PASSWORD=your-app-specific-password
   ```

The `.env` file is gitignored. `dotenvy` loads it automatically in the test harness.

## How It Works

- Each file is a separate `[[test]]` target in `Cargo.toml`, compiled as its own binary.
- `common::cmd()` builds an `assert_cmd::Command` pointing at the `icloudpd-rs` binary.
- `common::creds_or_skip()` returns `Some((username, password))` or `None`, letting credential-dependent tests skip without failure.
- CLI-only tests append `--help` to argument lists to validate parsing without executing commands.
- `tempfile::tempdir()` provides isolated directories for cookie stores and downloads.
- `predicates` crate is used for composable stdout/stderr assertions.
