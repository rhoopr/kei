# kei tests

Everything under `tests/` is either a Rust integration target or a shell
script that exercises scenarios easier to set up from shell than from
Rust. The repo-root `justfile` is the entry point; the layout below
explains what runs where.

## Layout

```
tests/
  common/mod.rs       shared Rust helpers (require_preauth, walkdir, auth-retry)
  data/               fixtures (sample.heic, etc.)
  cli.rs              argument parsing and help output
  behavioral.rs       offline end-to-end behavior (pre-seeded DB, real binary)
  sync.rs             live sync flow against iCloud (#[ignore] live tests)
  state_auth.rs       live status / reset / verify / import commands
  shell/
    lib.sh            shared helpers: release-binary, preflight, check, scratch
    concurrency.sh    concurrency, resume, partial-failure exit code
    state-machine.sh  sync-token / config-hash lifecycle, corrupt recovery
    docker.sh         docker container scenarios
```

## Test catalog

| Target | Count | Network | Runs via |
|--------|------:|:-------:|----------|
| `cargo test --bin kei` | 1550 | no | `just test fast` |
| `cargo test --test cli` | 95 | no | `just test fast` |
| `cargo test --test behavioral` | 112 | no | `just test fast` |
| `cargo test --test sync` | 43 `#[ignore]` | yes | `just test live` |
| `cargo test --test state_auth` | 17 `#[ignore]` | yes | `just test live` |
| `tests/shell/concurrency.sh` | 8 | yes | `just test concurrency` |
| `tests/shell/state-machine.sh` | 20 | yes | `just test state` |
| `tests/shell/docker.sh` | 16 | yes | `just test docker` |

Counts are approximate and drift as tests are added.

## Running

```sh
just test fast        # offline trio (unit + cli + behavioral)
just test             # everything offline (cargo test --all-features)
just test live        # live sync + state_auth against iCloud
just test concurrency # shell: concurrent/resume/partial-fail
just test state       # shell: token + config-hash invariants
just test docker      # shell: docker container scenarios
just test PATTERN     # passes through to cargo test PATTERN
just gate             # full pre-push gate (what CI runs)
```

Without `just`, run the raw commands directly:

```sh
cargo test --bin kei --test cli --test behavioral
cargo test --test sync --test state_auth -- --ignored --test-threads=1
./tests/shell/concurrency.sh
```

## Fuzzing

Coverage-guided fuzz harnesses live under `fuzz/`, not `tests/`. They're
opt-in (nightly + cargo-fuzz), excluded from `just gate`, and run via
`just fuzz`. See [`fuzz/README.md`](../fuzz/README.md).

## Setup for live tests

1. Fill `.env` at the repo root (gitignored):

   ```
   ICLOUD_USERNAME=you@icloud.com
   ICLOUD_PASSWORD=your-app-specific-password
   ```

2. Authenticate once to seed the session directory:

   ```sh
   just dev login
   # or without just:
   cargo run -- login --data-dir ~/.config/kei
   ```

   This prompts for a 2FA code and writes session tokens. Redo only when
   the session expires (typically months).

3. Create a test album in iCloud Photos with at least:

   | Asset | Used by |
   |-------|---------|
   | Regular JPEG | Basic download, size comparison, EXIF tests |
   | Standalone video (MOV/MP4) | Skip-videos filter, Docker watch cycle |
   | Live Photo (HEIC + MOV) | Skip-live-photos, MOV naming policy, HEIC XMP embed |
   | Apple ProRAW (.DNG) | align-raw flag acceptance |
   | Photo with non-ASCII filename | keep-unicode-in-filenames |

   The default album name is `kei-test`. Override with `KEI_TEST_ALBUM`
   if your album is named differently.

## Portability

Every environment-specific value is read from an env var. No account
details are baked into test code.

| Variable | Default | Purpose |
|----------|---------|---------|
| `ICLOUD_USERNAME` | required | Apple ID email |
| `ICLOUD_PASSWORD` | required | Apple ID password |
| `ICLOUD_TEST_COOKIE_DIR` | `./.test-cookies` | Pre-authenticated session dir |
| `KEI_TEST_ALBUM` | `kei-test` | Test album name |
| `KEI_DOCKER_IMAGE` | `kei:latest` | Docker image under test |
| `KEI_TEST_SCRATCH_DIR` | `/tmp/kei-tests-$USER` | Base dir for shell-suite scratch |

`just test live` applies a few defaults on top (`ICLOUD_TEST_COOKIE_DIR=~/.config/kei`,
`KEI_TEST_ALBUM=icloudpd-test`) that match this repo's maintainer setup.
Override them in your environment to point at your own account.

## Rate limits

Apple returns HTTP 503 when its auth endpoint is hit too fast. If that
happens:

- Wait 10-15 minutes before retrying.
- Keep `--test-threads=1` for every auth suite.
- Don't run multiple live shell suites in parallel - they share the
  session lock at `~/.config/kei/<user>.lock` and will step on each
  other. `just test live` and the shell suites are intended to be
  invoked one at a time.

## What lives where

- **`cli.rs`** - pure clap parsing. No network, no binary invocation;
  just `Cli::try_parse_from(...)`.
- **`behavioral.rs`** - `assert_cmd`-driven end-to-end against the real
  binary with a pre-seeded state DB. Covers everything that doesn't need
  the network (status flags, reconcile routing, deprecation warnings,
  config resolution).
- **`sync.rs`** - live iCloud, `#[ignore]` gated. Covers the happy-path
  download flow, filters, EXIF/XMP write-through, HEIC embed, sidecars.
- **`state_auth.rs`** - live iCloud, `#[ignore]` gated. Covers status /
  reset-state / verify / import-existing / retry-failed.
- **`shell/concurrency.sh`** - things that need `kill -9` mid-process,
  `chmod 555` on a target dir, direct sqlite3 assertions on the state
  DB mid-test. Hard to do cleanly from Rust.
- **`shell/state-machine.sh`** - token + config-hash invariants across
  multiple kei invocations with DB mutation in between.
- **`shell/docker.sh`** - anything that requires `docker run`, watch
  mode + SIGTERM, healthcheck probes inside the container.
