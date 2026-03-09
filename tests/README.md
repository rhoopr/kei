# Tests

## Quick Reference

```sh
# 1. Pre-commit (runs automatically on git commit)
cargo fmt -- --check
cargo clippy -- -D warnings
cargo test --bin icloudpd-rs --test cli --test state

# 2. One-time auth setup (interactive, prompts for 2FA)
# fish:
env (cat .env | grep -v '^#') cargo run -- sync --auth-only --cookie-directory .test-cookies
# bash/zsh:
env $(grep -v '^#' .env | xargs) cargo run -- sync --auth-only --cookie-directory .test-cookies

# 3. Run all tests
./tests/run-all-tests.sh

# 4. Run a single test
cargo test --test sync list_albums_prints_album_names -- --test-threads=1
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
env (cat .env | grep -v '^#') cargo run -- sync --auth-only --cookie-directory .test-cookies
# bash/zsh:
env $(grep -v '^#' .env | xargs) cargo run -- sync --auth-only --cookie-directory .test-cookies
   ```
   This prompts for a 2FA code. You only need to redo this when the session expires.

3. Verify the session works:
   ```sh
   cargo test --test setup_auth -- --ignored
   ```

4. Create an `icloudpd-test` album in iCloud Photos with these assets:

   | Asset | Purpose |
   |-------|---------|
   | Regular JPEG photo | Basic download, size comparison, EXIF tests |
   | Standalone video (MOV/MP4) | Skip-videos/skip-photos filter tests |
   | Live Photo (HEIC + MOV) | Skip-live-photos, MOV filename policy tests |
   | Apple ProRAW (.DNG) | RAW+JPEG pair for align-raw tests |
   | Photo with unicode filename | keep-unicode-in-filenames test |

   The sync tests target this album for deterministic, behavioral assertions.

## Test Structure

| File | Auth? | Description |
|------|-------|-------------|
| `cli.rs` | No | CLI argument parsing — no network |
| `state.rs` | No | State commands against absent DB — no network |
| `sync.rs` | Yes | Sync, download, filtering — targets `icloudpd-test` album |
| `state_auth.rs` | Yes | Status, reset-state, verify, import-existing, retry-failed |
| `setup_auth.rs` | Yes | Verifies pre-auth session is valid (ignored by default) |
| `common/mod.rs` | — | Shared helpers |

## Running Tests

### No-auth tests (no setup needed)

```sh
cargo test --bin icloudpd-rs          # unit tests
cargo test --test cli                  # CLI parsing
cargo test --test state                # state commands (no DB)
```

### Auth-required tests (need .test-cookies/)

Auth tests must run single-threaded to avoid Apple API rate limits (503s).

```sh
cargo test --test sync -- --test-threads=1
cargo test --test state_auth -- --test-threads=1
```

### All tests

```sh
./tests/run-all-tests.sh
```

Results are logged to `tests/results.log`.

### Single test

```sh
cargo test --test sync list_albums_prints_album_names -- --test-threads=1
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
| `.test-cookies/` | Yes | Pre-auth session files |
| `tests/results.log` | Yes | Test run output |
| `tests/run-all-tests.sh` | No | Runs all tests sequentially |
| `tests/TESTS.md` | No | Detailed test reference |
