# Test Suite Reference

## Overview

| File | Tests | Auth Required | Network |
|------|------:|:---:|:---:|
| Unit tests (`src/`) | 1018 | No | No |
| `behavioral.rs` | 101 | No | No |
| `cli.rs` | 93 | No | No |
| `state_auth.rs` | 17 (ignored) | Yes | Yes |
| `sync.rs` | 31 (ignored) | Yes | Yes |
| `setup_auth.rs` | 1 (ignored) | Yes | Yes |
| **Total** | **1261** | | |

## Running Tests

```sh
# Pre-commit safe (no auth, no network)
cargo test --bin kei --test cli --test behavioral

# Live iCloud tests (requires pre-auth session + icloudpd-test album)
cargo test --test sync --test state_auth -- --ignored --test-threads=1

# Full suite (requires pre-auth session + icloudpd-test album)
./tests/run-all-tests.sh

# Single test
cargo test --test sync sync_dry_run_downloads_nothing -- --ignored --test-threads=1
```

See `tests/README.md` for setup instructions.

---

## Unit Tests (`cargo test --bin kei`)

1018 tests across source modules. All offline, no credentials needed. Covers
CLI parsing, config, download pipeline, path resolution, EXIF, iCloud API
client, session management, SRP auth, state DB, retry logic, and shutdown.

---

## Behavioral Tests (`tests/behavioral.rs`)

101 tests. No credentials or network needed. Tests state subcommands, metadata
operations, config serialization, and end-to-end behavioral scenarios against
absent/fresh databases and mock data.

---

## CLI Tests (`tests/cli.rs`)

93 tests. Pure argument parsing - no network, no credentials. Validates that
every subcommand, flag, and enum value is accepted or rejected correctly.

Subcommands covered: `sync`, `login` (get-code, submit-code), `list` (albums,
libraries), `password` (set, clear, backend), `reset` (state, sync-token),
`config` (show, setup), `status`, `verify`, `import-existing`.

Categories: help output, invalid input, global flags, short flag aliases, enum
validation, numeric validation, flag acceptance, subcommand-specific,
cross-subcommand flags, exit codes, env var overrides, deprecated flag/alias
acceptance.

---

## State Auth Tests (`tests/state_auth.rs`)

17 tests, all `#[ignore]`. Require pre-authenticated session. Run with:

```sh
cargo test --test state_auth -- --ignored --test-threads=1
```

Covers: status after sync, reset-state (with/without --yes), verify (existence,
checksums, missing files, corruption detection), import-existing (nonexistent
dir, matched files, empty dir, custom folder structure), retry-failed
(after success, with no DB).

---

## Sync Tests (`tests/sync.rs`)

31 tests, all `#[ignore]`. Uses the `icloudpd-test` album for deterministic
behavioral assertions. Require pre-authenticated session. Run with:

```sh
cargo test --test sync -- --ignored --test-threads=1
```

Categories: metadata (list albums/libraries), core download (all asset types,
dry run, idempotent re-run), filter flags (skip-videos, skip-photos,
skip-live-photos, skip-all, date filters), size and naming (medium, force-size,
name-id7, folder structure, unicode), EXIF, RAW alignment, live photo MOV
policy, misc flags (temp suffix, threads, notification script, PID file), bare
invocation, error paths (missing directory, nonexistent album/library, bad
credentials).

---

## Setup Auth (`tests/setup_auth.rs`)

1 test, `#[ignore]` by default. Verifies a pre-auth session is still valid.

```sh
cargo test --test setup_auth -- --ignored --test-threads=1
```
