# kei tests

Everything under `tests/` is either a Rust integration target or a shell
script that exercises scenarios easier to set up from shell than from
Rust. The repo-root `justfile` is the entry point; the layout below
explains what runs where. Follow the
[contribution and review rules](../CONTRIBUTING.md#tests) for test placement,
behavior changes, and failure investigation. This guide owns the
[state-transition proof](#state-transition-proof) and suite-specific setup.

## Layout

```
tests/
  common/mod.rs       shared Rust helpers (require_preauth, walkdir, auth-retry)
  data/               media fixtures (see "Media fixtures" below)
  cli.rs                       argument parsing and help output
  behavioral.rs                offline end-to-end behavior (pre-seeded DB, real binary)
  sync.rs                      live sync flow against iCloud (#[ignore] live tests)
  state_auth.rs                live status / reset / verify / import commands
  import_existing_live.rs      live import-existing scenarios (#[ignore] live tests)
  shell/
    lib.sh            shared helpers: release-binary, preflight, check, scratch
    concurrency.sh    concurrency, resume, partial-failure exit code
    state-machine.sh  sync-token / config-hash lifecycle, corrupt recovery
    docker.sh         docker container scenarios
```

## Test catalog

| Target | Network | Runs via |
|--------|:-------:|----------|
| `cargo test --lib` | no | `just test fast` |
| `cargo test --test cli` | no | `just test fast` |
| `cargo test --test behavioral` | no | `just test fast` |
| `cargo test --all-features --test sync` | yes | `just test live` |
| `cargo test --all-features --test state_auth` | yes | `just test live` |
| `cargo test --all-features --test import_existing_live` | yes | `just test live` |
| `tests/shell/concurrency.sh` | yes | `just test concurrency` |
| `tests/shell/state-machine.sh` | yes | `just test state` |
| `tests/shell/docker.sh` | yes | `just test docker` |
| `scripts/full-test/run_live_import_rehearsal.sh` | yes | `just full-test` |
| `scripts/full-test/run_cross_zone_album_hydration.sh` | yes | `just full-test` when `KEI_FULL_TEST_CROSS_ZONE_ALBUM` is set |
| `scripts/full-test/run_docker_puid_smoke.sh` | no | `just full-test` |
| `scripts/full-test/run_release_archive_smoke.sh` | no | `just full-test` |
| `scripts/test-scenarios/*.sh` | no | `just test scenario NAME` or `just test scenarios` |
| `scripts/full-test/run_release_regression_smoke.sh` | no | `just release-smoke` |
| `.github/workflows/service-smoke.yml` | no | `just service-smoke` (linux/macOS) |

## Focused scenario slices

Named scenario slices group existing Rust tests by risk so a change can run
the smallest relevant proof before the broader gate. All slices are offline
and require no credentials. Run one with `just test scenario NAME`; run the
full set with `just test scenarios`.

| Slice | Protects | Run for changes to |
|-------|----------|--------------------|
| `auth-session` | Persisted-session reuse, validation caching, 2FA push state, and reauthentication routing | Authentication recovery, session validation, or watch-mode reauthentication |
| `config-docs` | Supported configuration stays aligned with examples, migration guidance, and contributor commands | Config keys, defaults, examples, migration docs, or gate commands |
| `config-reconciliation` | Config-hash staging, local catalog path reconciliation, and repeat-cycle stability | Download config hashes, path reconciliation, config drift, or local reconciliation copies |
| `fulltest-harness` | Full-test phase reachability and rejection of stale or empty scenario filters | `just` dispatch, full-test orchestration, or scenario-runner helpers |
| `identity-deltas` | Incremental identity mapping, hard and soft deletion, selected relations, and master-family transitions | CloudKit change parsing, identity mapping, membership, or tombstone policy |
| `path-family` | Collision suffixes and primary, Live Photo, import, and pending-file family matching | Path rendering, collision handling, import matching, or on-disk adoption |
| `pending-recovery` | Durable pending hydration, policy-excluded deletion proof, ambiguous identity retention, and sibling recovery | Retry resolution, pending or policy-excluded state, provider identity, or targeted hydration |
| `service-health` | Health and metrics facts exposed by unattended operation | Health checks, metrics, cycle reporting, or service monitoring |
| `url-refresh` | Refresh of expired or aged download URLs without replaying stale deltas | Incremental downloads, URL freshness, album hydration, or retry downloads |

## State-transition proof

Changes to durable configuration, filesystem paths, media publication,
metadata, SQLite state, retry work, or provider checkpoints require a
state-transition proof through the production call graph. The test must cover
these stages:

1. **Initial durable state:** Use real SQLite and a `TempDir` where practical.
   Create the exact state and files that exist before the transition.
2. **Controlled mutation:** Change one config field, provider fact, local
   entry, or injected failure.
3. **Production cycle:** Enter through the normal production owner and its
   direct callers. Do not duplicate planning or finalization policy in the
   test.
4. **Durable outcome:** Assert concrete filesystem, SQLite, metadata, retry,
   and checkpoint results. An empty destination proves only the empty case.
5. **Steady-state cycle:** Run the same configuration again. Assert no repeat
   reconciliation, duplicate file, orphaned state path, or repeated provider
   work.

Cover each applicable dimension: album, smart-folder, and unfiled passes;
single and companion media; absent, regular, conflicting, leaf-symlink, and
parent-symlink entries; metadata disabled and enabled; interruption; and
state-write failure. Keep the matrix proportional to the changed behavior.

`just gate`, line coverage, fuzzing, and fresh-workspace live tests are
supporting evidence. They do not prove a multi-cycle transition unless they
exercise the initial state, mutation, durable outcome, and steady-state cycle.
Reports must state whether the destination was empty, whether durable state
was pre-seeded, what changed between cycles, and whether an unchanged cycle
was checked.

## Running

Choose the route by purpose:

| Purpose | Command | Proof |
|---------|---------|-------|
| Focused iteration | `just test scenario NAME`, `just test fast`, or `just test PATTERN` | Relevant tests with default features; not a complete gate |
| Complete offline gate | `just gate` | Static checks, all-feature and no-default suites, and focused-filter catalog checks |
| Release and live validation | `just full-test` | The offline gate's checks plus nightly tools, package, Docker, live provider, release-binary, and service phases |

`just test offline` runs the behavior and catalog portion of `just gate`.
`just test` runs only the all-feature suite. Existing focused, packaging,
Docker, service, and live commands remain available individually.

### Route coverage

The change from the `8f3467c` route removes replay, not distinct test setups:

| Route | Before | Now |
|-------|--------|-----|
| `just gate` | Static checks; all-feature suite; no-default suite | Static checks; `just test offline` |
| `just test offline` | All-feature suite; default-feature drift targets; no-default suite; default-feature scope-matrix filter | All-feature suite; no-default suite; default-feature scenario catalogs |
| `just full-test` offline phases | Static checks; offline route; every scenario list and test invocation | Static checks; offline route, with no scenario execution replay |
| Focused scenarios | Default-feature library and `branch_static` filters, listed before execution | Unchanged; empty filters now also fail |
| Nightly tools | Optional fuzz build; required nightly unused-dependency check | Unchanged |
| Package and Docker | Release archive; extracted binary; container build, PUID, multiarch, CLI, default command | Unchanged |
| Live and service | Provider tests; shell suites; release-binary CLI and import rehearsal; service smoke | Unchanged, including opt-in cross-zone and real host-service checks |

Both complete Rust suites include every integration target and the library
scope matrix. At this baseline, the drift-target loop adds no targets.
The no-default suite remains separate: it exercises disabled-XMP behavior
and native EXIF paths.

Default features enable `xmp`. All features also enable `__fuzz_internals`.
The latter adds parser wrappers, re-exports, and extra HEIF preservation
assertions. Its conditional sites do not replace default-feature production
paths or remove default tests. This source-level comparison, together with
the target and environment inventory, supports using the all-feature suite
for the scenario and scope-matrix proof. Matching test names alone is not
that argument. Revisit this equivalence if feature gates change.

Scenarios add no fixtures, environment, or isolation beyond their underlying
tests. `scripts/test-scenarios/check.sh` sources the same runners and checks
each filter against cached default-feature catalogs, once per target. It
fails on empty filters, stale filters, failed listings, or an empty catalog
of scenarios. These catalog checks do not execute tests and are not counted
as passing tests. Focused commands still list and execute each filter.

Offline suites retain Cargo's test scheduling. `just test fast` retains its
single-threaded library run. No repository-wide `RUST_TEST_THREADS` setting
was found; caller overrides still apply. Live provider tests remain
single-threaded. Full-test retains its temporary-directory setup, live skip
and rate-limit records, child-failure handling, and start/end Git provenance.
Finalization rejects missing or skipped required offline phases and failed
phase records. Live and platform skips remain explicit, not test coverage.

Linux tooling tests in `branch_static` require `just` on `PATH`; CI installs
its pinned version before both complete Rust suites and coverage runs. Other
platforms retain their existing Rust test coverage; executable dispatcher
fixtures are Linux-only.

### Commands

```sh
just test fast                  # fast offline unit + key integration targets
just test                       # all-feature offline tests
just test offline               # all-feature/no-default suites + scenario catalog checks
just test scenario NAME         # focused behavior slice from scripts/test-scenarios/
just test scenarios             # every focused offline scenario slice
just test live                  # live sync + state_auth + import-existing against iCloud
just test live-shell            # discovered live shell suites
just test live-smoke            # release-binary live CLI/import smokes
just test concurrency           # shell: concurrent/resume/partial-fail
just test state                 # shell: token + config-hash invariants
just test docker                # shell: Docker container scenarios
just test packaging             # release build + archive smoke
just test docker-full           # Docker build, PUID, multiarch, CLI/default-command smokes
just test service               # local service-smoke wrapper
just test PATTERN               # passes through to cargo test PATTERN
just static-checks              # fmt, clippy, docs, audit, lint, contracts, typos
just gate                       # static checks + offline behavior tests
just release-smoke              # fast offline release regression smoke
just full-test                  # grouped full battery with logs, live skips, metrics
```

Without `just`, run the raw commands directly:

```sh
cargo test --lib --test cli --test behavioral
cargo test --all-features --test sync --test state_auth -- --ignored --test-threads=1
./tests/shell/concurrency.sh
```

## Media fixtures

`tests/data/` holds real camera and encoder outputs, not hand-built containers,
so metadata writers are exercised against independently produced item maps.

| Fixture | Origin | Licence | Covers |
|---------|--------|---------|--------|
| `sample.heic` | iPhone capture | repository fixture | single `hvc1` primary, Exif item, thumbnail, no XMP |
| `apple-hdr-gainmap.heic` | [`johncf/apple-hdr-heic`](https://github.com/johncf/apple-hdr-heic/blob/master/tests/data/hdr-sample.heic), iOS 17.6.1 | MIT, reproduced in `tests/data/LICENSE-apple-hdr-gainmap` | `grid` primary over six tiles, HDR gain map, `idat`, and two XMP items |
| `white_1x1.avif` | [`libavif` encoder output](https://github.com/AOMediaCodec/libavif/blob/cbb391c194ee15cf9607e517c269ed99e6bf0197/tests/data/white_1x1.avif) | BSD 2-Clause, reproduced in `tests/data/LICENSE-libavif-white-1x1` | primary `av01` item with no XMP, exercising AVIF insertion |

`apple-hdr-gainmap.heic` is the multi-image case. Item 9 is the XMP describing
the primary image and item 11 describes the gain map, so a writer that picks
the wrong packet moves the photograph's rating and keywords onto the gain map.
Its primary is a derived `grid`, which also keeps the tiled layout covered.

Hand-built containers still appear in unit tests for layouts no real file
provides, such as construction method 2, item IDs above `u16`, and deliberately
malformed boxes.

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
   KEI_DATA_DIR=~/.config/kei cargo run -- login
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
| `CARGO_TARGET_DIR` | `./target` | Cargo build directory. Full-test packaging, shell, live, service, and metrics phases use release artifacts from this directory. |
| `KEI_FULL_TEST_TMPDIR` | `/tmp/codex/kei/full-test/tmp` | Temporary directory for full-test child processes and shell-suite scratch data. |
| `KEI_TEST_SCRATCH_DIR` | `/tmp/codex/kei/shell-tests-$USER` | Base dir for standalone shell-suite scratch; `just full-test` overrides this to `$KEI_FULL_TEST_TMPDIR/shell` or `/tmp/codex/kei/full-test/tmp/shell` |
| `KEI_IMPORT_FIXTURE_DIR` | `/tmp/codex/kei/import-fixture` | Where `import_existing_live.rs` caches its `--recent 100` sync fixture across runs |
| `KEI_FULL_TEST_CROSS_ZONE_ALBUM` | unset | Optional full-test album fixture for cross-zone hydration. The album must include at least one asset from a non-primary source zone. |
| `KEI_FULL_TEST_CROSS_ZONE_MIN_FILES` | `1` | Minimum non-primary downloaded asset rows required when `KEI_FULL_TEST_CROSS_ZONE_ALBUM` is set. |
| `KEI_FULL_TEST_REAL_SERVICE` | unset | Set to `1` to let `just full-test` install, start, status-check, and uninstall a real Linux user systemd service |
| `KEI_FULL_TEST_EXPECT_VERSION` | unset | Optional exact Cargo package version expected by the release-archive smoke |

`just test live` applies `KEI_TEST_ALBUM=kei-test` to match this
repo's maintainer setup. Override in your environment to point at your
own account. Cookie dir falls through to the harness default
(`./.test-cookies`); set `ICLOUD_TEST_COOKIE_DIR` to override.

### Loopback-bound tests

Some offline unit tests bind `127.0.0.1` for wiremock or the metrics HTTP
server. Those tests probe loopback binding first and return early only when the
host rejects the bind with a permission error. Normal CI hosts still run the
tests strictly; restricted sandboxes get an explicit skip line instead of a
false bind failure.

### Cross-zone album fixture

`just full-test` skips cross-zone album hydration by default. To enable it,
prepare a named album that includes at least one asset whose source library is
not `PrimarySync`, then run:

```sh
KEI_FULL_TEST_CROSS_ZONE_ALBUM="album-name" just full-test
```

The phase syncs that album with `libraries = ["all"]` using the release
binary. It then checks the fresh state DB for downloaded album rows where
`assets.library <> 'PrimarySync'`. Set
`KEI_FULL_TEST_CROSS_ZONE_MIN_FILES` if the fixture should prove more than one
non-primary asset.

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
  the network (status flags, reconcile routing, config resolution).
- **`sync.rs`** - live iCloud, `#[ignore]` gated. Covers the happy-path
  download flow, filters, EXIF/XMP write-through, HEIC embed, sidecars.
- **`state_auth.rs`** - live iCloud, `#[ignore]` gated. Covers status /
  reset state / verify / import-existing / sync --retry-failed.
- **`import_existing_live.rs`** - live iCloud, `#[ignore]` gated.
  Comprehensive `import-existing` scenarios: matches a real-sync fixture,
  dry-run, idempotency, `--recent` cap, `--recent Nd` rejection, truncated
  / missing files producing unmatched entries,
  TOML-only resolution. Companion to the wiremock unit tests in
  `src/commands/import.rs::wiremock_tests` -- live verifies real Apple
  CloudKit shapes work end-to-end; wiremock covers the policy matrix
  exhaustively.
- **`src/commands/import/wiremock_tests/icloudpd_compat.rs`** - icloudpd
  compat baseline. Each test stages an on-disk layout using fixture data
  (filenames, folder structure, sizes) lifted verbatim from the
  `icloud_photos_downloader` test suite, then runs kei's `import_assets`
  loop and asserts every file matches. Acts as a regression guard against
  layout divergence across kei refactors. Source mirroring:
  `test_download_photos.py`, `test_download_photos_id.py`,
  `test_download_live_photos.py`, `test_download_live_photos_id.py`,
  `test_download_videos.py`, `test_folder_structure.py`. Runs as part of
  `cargo test --lib` in `just test fast` and `just gate`. Includes
  `dedup_size_suffix_collision`, which exercises the
  `<stem>-<size><ext>` collision shape (icloudpd's filename-conflict
  resolution): `import_assets` falls back to the size-suffixed path when
  the bare name doesn't match, so libraries with collisions still
  match cleanly.
- **`scripts/test-scenarios/`** - focused offline behavior slices named by
  risk class, not release history. Current slices cover URL refresh,
  identity/tombstone deltas, pending recovery, path-family collisions,
  auth/session flows, service health, config docs, and full-test harness
  reachability. Each named filter must resolve to at least one current test.
  Use these before a broad gate when touching the matching owner path.
- **`shell/concurrency.sh`** - things that need `kill -9` mid-process,
  `chmod 555` on a target dir, direct sqlite3 assertions on the state
  DB mid-test. Hard to do cleanly from Rust.
- **`shell/state-machine.sh`** - token + config-hash invariants across
  multiple kei invocations with DB mutation in between.
- **`shell/docker.sh`** - anything that requires `docker run`, watch
  mode + SIGTERM, healthcheck probes inside the container.
- **`scripts/full-test/run_release_archive_smoke.sh`** - packages the host
  release binary into a temp archive, extracts it, and runs basic CLI/config
  probes against the extracted binary. `just test packaging` runs the host
  release build plus this smoke.
- **`scripts/full-test/run_release_regression_smoke.sh`** - fast offline
  patch-release gate for sync tokens, retry and pending adoption, pagination
  diagnostics, media validation, pass templates, and config paths. Run it
  before releases that touch those areas.
- **`scripts/full-test/run_docker_puid_smoke.sh`** - offline Docker entrypoint
  checks for PUID/PGID drop, volume chown, `MALLOC_ARENA_MAX=2`, root-default
  behavior, and invalid env rejection. `just test docker-full` runs this with
  Docker build, multiarch, and CLI/default-command smokes.
- **`scripts/full-test/run_live_import_rehearsal.sh`** - live mini rehearsal
  for the TOML-first import path: seed a tiny real tree, import it into a fresh
  DB, and verify a repeat dry-run stays matched. `just test live-smoke` runs
  this after the release-binary live CLI smokes.
- **`scripts/full-test/run_cross_zone_album_hydration.sh`** - opt-in live
  release-binary check for accounts with a prepared cross-zone album fixture.
  It selects the named album with `libraries = ["all"]` and fails unless the
  sync records at least one downloaded asset from a non-primary source zone.
- **`service-smoke` workflow** - per-platform CI smoke for `kei install`
  / `kei uninstall`. Builds the release binary on
  `ubuntu-latest`/`macos-latest`/`windows-latest`, runs `kei install
  --dry-run` to print the platform artifact (systemd unit, launchd
  plist, or Windows SCM preview) without writing files or invoking the
  service manager, validates the artifact or no-op contract with
  platform-native checks (`systemd-analyze verify`, `plutil -lint`, or
  `Get-Service`), then runs `kei uninstall` against a clean host.
  Catches renderer, dry-run, and default install regressions; doesn't
  exercise the actual service-manager handoff.

## Service testing contract

Automated coverage:

- CLI parse/help for `kei install`, `kei uninstall`, and `kei service`.
- Pure systemd, launchd, Windows SCM renderers and status formatters.
- `kei install --dry-run` prints the service artifact and writes nothing.
- Linux/macOS/Windows smoke validates dry-run output syntax and clean
  uninstall behavior.
- Docker packaging defaults to `kei service run` so containers keep the
  24-hour watch fallback when `[watch].interval` is unset.

Manual real-install coverage:

- `systemctl enable --now` and systemd restart behavior.
- `launchctl bootstrap` / `bootout` against a live GUI domain.
- Windows SCM `CreateServiceW`, account password handoff, and service
  control dispatcher startup.
- Boot/reboot persistence and a real long-running sync against the
  `kei-test` album.

`just full-test` can run the Linux user-service lifecycle with
`KEI_FULL_TEST_REAL_SERVICE=1`. It refuses to run if `kei.service` already
exists, and it uninstalls the service before returning. The installer may
temporarily enable user linger; uninstall restores the prior linger state when
that state was recorded during install.
