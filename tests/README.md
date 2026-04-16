# Tests

## Quick Reference

```sh
# 1. Pre-commit (runs automatically on git commit)
cargo fmt -- --check
cargo clippy -- -D warnings
cargo test --bin kei --test cli --test behavioral

# 2. One-time auth setup (interactive, prompts for 2FA)
# fish:
env (cat .env | grep -v '^#') cargo run -- login --data-dir .test-cookies
# bash/zsh:
env $(grep -v '^#' .env | xargs) cargo run -- login --data-dir .test-cookies

# 3. Run all tests
./tests/run-all-tests.sh

# 4. Run a single test
cargo test --test sync list_albums_prints_album_names -- --ignored --test-threads=1
```

## Setup

1. Copy the example env file and fill in your credentials:
   ```sh
   cp .env.example .env
   ```
   ```
   ICLOUD_USERNAME=you@icloud.com
   ICLOUD_PASSWORD=your-app-specific-password
   ```

2. Authenticate (creates `.test-cookies/` with session files):
   ```sh
   # fish:
   env (cat .env | grep -v '^#') cargo run -- login --data-dir .test-cookies
   # bash/zsh:
   env $(grep -v '^#' .env | xargs) cargo run -- login --data-dir .test-cookies
   ```
   This prompts for a 2FA code. You only need to redo this when the session expires.

3. Create a test album in iCloud Photos (default name: `kei-test`) with these assets:

   | Asset | Purpose |
   |-------|---------|
   | Regular JPEG photo | Basic download, size comparison, EXIF tests |
   | Standalone video (MOV/MP4) | Skip-videos/skip-photos filter tests |
   | Live Photo (HEIC + MOV) | Skip-live-photos, MOV filename policy tests |
   | Apple ProRAW (.DNG) | RAW+JPEG pair for align-raw tests |
   | Photo with unicode filename | keep-unicode-in-filenames test |

   If your album has a different name, set `KEI_TEST_ALBUM=<name>` in your environment.

## Portability

Nothing account-specific is baked into the test code. Override these env vars to point the suite at your own account:

| Variable | Default | Purpose |
|----------|---------|---------|
| `ICLOUD_USERNAME` | (required) | Apple ID email |
| `ICLOUD_PASSWORD` | (required) | Apple ID password |
| `ICLOUD_TEST_COOKIE_DIR` | `./.test-cookies` | Pre-authenticated session directory |
| `KEI_TEST_ALBUM` | `kei-test` | Name of the test album in iCloud |
| `KEI_DOCKER_IMAGE` | `kei:latest` | Docker image used by `run-docker-live.sh` |

The shell scripts read these via `tests/lib.sh`. Rust tests read them via `tests/common/mod.rs` and (for the album name) `tests/sync.rs::album()`.

## Test Structure

| File | Auth? | Description |
|------|-------|-------------|
| `cli.rs` | No | CLI argument parsing |
| `behavioral.rs` | No | CLI behavior, state commands, config resolution |
| `sync.rs` | Yes | Sync, download, filtering against the test album (`#[ignore]`) |
| `state_auth.rs` | Yes | Status, reset-state, verify, import-existing, retry-failed (`#[ignore]`) |
| `common/mod.rs` | -- | Shared Rust helpers (`require_preauth`, `cookie_dir`, `walkdir`) |
| `lib.sh` | -- | Shared bash helpers for the run-*.sh scripts |

## Running Tests

### No-auth tests (no setup needed)

```sh
cargo test --bin kei                   # unit tests
cargo test --test cli                  # CLI parsing
cargo test --test behavioral           # CLI behavior + state commands
```

### Auth-required tests (need cookie dir with trusted session)

Auth tests must run single-threaded to avoid Apple API rate limits (503s).

```sh
cargo test --test sync -- --ignored --test-threads=1
cargo test --test state_auth -- --ignored --test-threads=1
```

### All Rust suites

```sh
./tests/run-all-tests.sh
```

Results are logged to `tests/results.log`.

### Live shell-script suites

```sh
./tests/run-gap-tests.sh           # Concurrent downloads, resume, partial-failure exit codes
./tests/run-deep-validation.sh     # Sync token + config hash invariants
./tests/run-docker-live.sh         # Docker container integration (13 checks)
```

Each sources `tests/lib.sh` to resolve cookie dir, DB path, and album name from the environment.

### Single test

```sh
cargo test --test sync list_albums_prints_album_names -- --ignored --test-threads=1
```

## Apple API Rate Limits

Apple returns HTTP 503 if you hit their API too fast. If you get 503s:

- Wait 10-15 minutes before retrying
- Always use `--test-threads=1` for auth tests
- Run test binaries sequentially (the script handles this)

## Files

| Path | Gitignored | Purpose |
|------|------------|---------|
| `.env` | Yes | Credentials |
| `.env.example` | No | Template for `.env` |
| `.test-cookies/` | Yes | Pre-auth session files (default location) |
| `tests/results.log` | Yes | Test run output |
| `tests/lib.sh` | No | Shared bash helpers |
| `tests/run-all-tests.sh` | No | Orchestrator for all Rust suites |
| `tests/run-gap-tests.sh` | No | Regression coverage for known gaps |
| `tests/run-deep-validation.sh` | No | Sync-token/config-hash invariants |
| `tests/run-docker-live.sh` | No | Docker integration tests |
| `tests/TESTS.md` | No | Detailed test reference |
