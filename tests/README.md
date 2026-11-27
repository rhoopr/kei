# kei tests

## Test workflow

Local iteration uses focused tests. Every PR runs the Linux baseline; pushes to main run complete offline and platform tests. Run live
provider and real host-service checks only when relevant and authorized.
`KEI_FULL_TEST_REAL_SERVICE=1 just release-local` collects the local release
prerequisites without replaying hosted offline checks. `just full-test` remains
available for an explicit complete local audit. See [CI qualification](../docs/ci.md).

Everything under `tests/` is either a Rust integration target or a shell
script that exercises scenarios easier to set up from shell than from
Rust. The repo-root `justfile` is the entry point; the layout below
explains what runs where. Follow the
[contribution and review rules](../CONTRIBUTING.md#tests) for test placement,
behavior changes, and failure investigation. This guide owns the
[state-transition proof](#state-transition-proof) and suite-specific setup.

On Unix, `cargo test --test cli setup_enter_default` drives the real wizard through a Python 3 pseudo-terminal with a temporary home and a deferred password file. It checks cancellation before saving and the default first-sync choice without contacting Apple or using a keyring.

## Layout

```
tests/
  common/mod.rs       shared Rust helpers (require_preauth, walkdir, auth-retry)
  data/               media fixtures (see "Media fixtures" below)
  cli.rs                       argument parsing and help output
  behavioral.rs                offline end-to-end behavior (pre-seeded DB, real binary)
  branch_static.rs             offline packaging, migration, and tooling checks
  sync.rs                      live sync flow against iCloud (#[ignore] live tests)
  state_auth.rs                live status / reset / verify / import commands
  import_existing_live.rs      live import-existing scenarios (#[ignore] live tests)
  shell/
    lib.sh            shared helpers: release-binary, preflight, check, scratch
    concurrency.sh    concurrency, resume, partial-failure exit code
    state-machine.sh  saved position / planning fingerprint lifecycle, rejected-position recovery
    docker.sh         docker container scenarios
```

## Test catalog

| Target | Network | Runs via |
|--------|:-------:|----------|
| `cargo test --lib` | no | `just test fast` |
| `cargo test --test cli` | no | `just test fast` |
| `cargo test --test behavioral` | no | `just test fast` |
| `cargo test --test branch_static` | no | `just test offline`, selected scenario slices |
| `cargo test --all-features --test sync -- --ignored --test-threads=1` | yes | `just test live` |
| `cargo test --all-features --test state_auth -- --ignored --test-threads=1` | yes | `just test live` |
| `cargo test --all-features --test import_existing_live -- --ignored --test-threads=1` | yes | `just test live` |
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

`branch_static` belongs to the complete offline routes, including `just gate`,
rather than `just test fast`. Run it directly or through a relevant scenario
slice when changing packaging, migration guidance, or validation tooling.

The three live Rust integration targets also contain offline tests. Without `--ignored`,
they run shared helper tests; `import_existing_live` also checks fixture
isolation, command construction, and rejection of a removed CLI flag. These
offline tests run in `just test offline` and `just gate` without credentials.

## Focused scenario slices

Named scenario slices group existing Rust tests by risk so a change can run
the smallest relevant proof before the broader gate. All slices are offline
and require no credentials. Run one with `just test scenario NAME`; run the
full set with `just test scenarios`.

| Slice | Protects | Run for changes to |
|-------|----------|--------------------|
| `auth-session` | Persisted-session reuse, validation caching, 2FA push state, and reauthentication routing | Authentication recovery, session validation, or watch-mode reauthentication |
| `config-docs` | Supported configuration stays aligned with examples, migration guidance, and contributor commands | Config keys, defaults, examples, migration docs, or gate commands |
| `config-reconciliation` | Config resolution and stable stored paths when planning settings change | Path settings, config resolution, or replanning |
| `fulltest-harness` | Full-test phase reachability and rejection of stale or empty scenario filters | `just` dispatch, full-test orchestration, or scenario-runner helpers |
| `identity-deltas` | Change-feed observations, masters, memberships, Shared Library stand-ins and smart folders | CloudKit change parsing, membership, or planning inputs |
| `path-family` | Collision suffixes, icloudpd layouts and adoption of existing files | Path rendering, collision handling, import matching, or on-disk adoption |
| `pending-recovery` | Interrupted listings, rejected positions, the position-with-records rule, confirmed damage and publication receipts | Sync positions, owed copies, or replacement |
| `service-health` | Health and metrics facts exposed by unattended operation | Health checks, metrics, cycle reporting, or service monitoring |
| `state-upgrade` | Upgrade from a previous release's state, ownership refusal and cleanup | The upgrade, account binding, or state access |
| `url-refresh` | Fresh URLs from lookups and one refresh of an expired URL | Downloads, lookups, or URL freshness |

## Sync core acceptance tests

`cargo test --lib sync_core::` drives whole cycles of the sync engine. `src/sync_core/tests.rs` serves a fake zone change feed, record lookups and smart-folder queries through `PhotosSession`, and file bytes from a local HTTP server. Each run opens the real state file again, so restart behavior is part of every test. Tests check files on disk, state rows and quiet later cycles: a first full listing, a new photo that needs only its own work, resuming an interrupted listing, a rejected sync position, an expired URL, replacement only of untouched files, stable album paths, departed copies that stay, photos waiting for their master, adoption of existing files, Shared Library stand-ins, smart folders, date ranges, the upgrade from a previous release's state and confirmed damage.

`metadata_pass` checks the opt-in metadata pass: embedded writes after download that later cycles leave alone until iCloud changes the photo, sidecars with album names, locally edited copies left untouched, and kei's own bytes recognised after a crash between publication and recording. `a_replacement_published_before_a_crash_is_not_downloaded_again` checks the same receipt for downloads.

`icloudpd_layout` stages libraries with icloudpd's own fixture names and sizes and runs the adopt-only cycle `import-existing` uses. A divergence from icloudpd's layout leaves a copy owed. Collisions under the default policy get `<stem>-<size><ext>` first, as icloudpd and earlier kei did.

`src/sync_core/state.rs` and `src/sync_core/access.rs` unit tests cover the commit-with-position rule, plan application and account binding. `download::file::replacement_recovery::tests::unsupported_rename_flags_exercise_the_production_publication_routes` reruns the replacement tests with `renameat2` flags refused, so the journaled fallback is exercised.

## State-transition proof

Changes to durable configuration, filesystem paths, media publication,
metadata, SQLite state, retry work, or sync positions require a
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
   and sync-position results. An empty destination proves only the empty case.
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
| Optional local offline gate | `just gate` | Static checks, all-feature and no-default suites, and focused-filter catalog checks |
| Optional combined local battery | `just full-test` | The offline gate's checks plus nightly tools, package, Docker, live provider, release-binary, and service phases |

`just test offline` runs the behavior and catalog portion of `just gate`.
It clears inherited account, credential and config-path inputs in its child process so offline fixtures cannot consume the live account loaded by `just full-test`. The parent keeps those inputs for the later live phases.
`just test` runs only the all-feature suite. Existing focused, packaging,
Docker, service, and live commands remain available individually.

### Commands

```sh
just test fast                  # fast offline unit + key integration targets
just test                       # all-feature offline tests
just test offline               # all-feature/no-default suites + scenario catalog checks
just test scenario NAME         # focused behavior slice from scripts/test-scenarios/
just test scenarios             # every focused offline scenario slice
just test live                  # live lookup + sync + state_auth + import-existing against iCloud
just test live-shell            # discovered live shell suites
just test live-smoke            # release-binary live CLI/import smokes
just test concurrency           # shell: concurrent/resume/partial-fail
just test state                 # shell: token + config-hash invariants
just test docker                # shell: Docker container scenarios
just test packaging             # release build + archive smoke
just test docker-full           # Docker build, PUID, multiarch, CLI/default-command smokes
just test service               # local service-smoke wrapper
just test PATTERN               # passes through to cargo test PATTERN
just static-checks              # fmt, clippy, docs, dependency policy, lint, contracts, typos
just gate                       # static checks + offline behavior tests
just release-smoke              # fast offline release regression smoke
just release-local              # authorized local release provider/host prerequisites
just full-test                  # optional combined battery with logs, live skips, metrics
```

Without `just`, run the raw commands directly:

```sh
cargo test --lib --test cli --test behavioral
KEI_LIVE_SKIP_CREATED_BEFORE=2025-01-01 cargo test --all-features --no-fail-fast --test sync --test state_auth -- --ignored --test-threads=1
./tests/shell/concurrency.sh
```

## Media fixtures

The [fixture corpus guide](data/README.md) records the selected formats,
provenance, sanitization, independent encoders, SHA-256 manifest, size budget,
production-path tests, and extracted-source-package checks.

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

## HEIF streaming allocation regression

`cargo test --lib heif_sparse_ -- --nocapture` exercises the production probe,
preparation, native Exif repair, XMP insertion/replacement, temporary validation,
and guarded publication on generated 1 MiB and 128 MiB sparse HEIF containers.
A test-only `System` allocator wrapper measures per-thread net live Rust heap
growth and the largest actual allocation. Both remain below fixed byte limits,
and the larger input may add at most 64 KiB of peak heap growth. This proof has no
wall-clock threshold and does not claim RSS or C++ toolkit allocation coverage.
Streaming fingerprint checks and markers across the copy-buffer boundary, hole,
and tail independently verify retained media ranges. A stale approved source
must preserve the changed source and leave no temporary metadata file.
`run_pending_heif_metadata_limit_retains_debt_then_recovers_across_reopens`
checks that a sparse 9 MiB native Exif operation refusal cannot complete an empty metadata
plan. Two reopened failures preserve retry debt, checksums, hashes and media;
a changed source stays held until explicit download admission, then recovery
and two quiet reopens preserve the accepted source and leave no temporary file.
`heif_valid_avif_` derives independently encoded AV1 data and properties from the
public libavif fixture, moves the coded item into `idat` with construction method
1, and adds 1 MiB/16 MiB sparse `idat` or opaque storage. These valid large-`meta`
containers exercise production probe, insertion, replacement, validation, and
publication with bounded allocation growth. Independent hashes verify the codec,
image properties, and all opaque payload bytes. Late file corruption must be
rejected even after both layouts were read. A separate case covers more than
65,536 top-level boxes without a growing inventory. Set
`KEI_HEIF_STREAM_INSPECT_DIR` to an existing task output directory to retain large
inputs and outputs for an independent decoder audit.
The earlier sparse HEIC containers validate metadata and ISO-BMFF preservation,
not codec playback or authenticated account behavior. The tests run in the ordinary XMP
library suite and need no credentials or stored media fixtures.

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

3. Choose an explicit fixed lower capture date containing at least one eligible primary-library asset. Set `KEI_LIVE_SKIP_CREATED_BEFORE` to that `YYYY-MM-DD` before running an authorized general live sync route, for example `KEI_LIVE_SKIP_CREATED_BEFORE=2025-01-01 just test live`. Replace the example date with your own narrowly chosen bound; all selected dates at or after it can download, so this is not a ten-item cap. An unset, invalid or rolling value fails closed before provider preflight.

A first sync lists the whole library whatever the date bounds: about six minutes on a 40k-record account. So each Rust live test binary, and each shell suite, lists the primary library once into scratch state with a selection that matches no photo, then gives each test its own copy of that catalog (`require_preauth`, `kei_seed_listed_state`). A test's first sync reads only the changes since and plans its own selection; its download folder is new to the copy, so nothing is treated as moved. Tests of first-listing behavior use `require_preauth_fresh` or fresh state and pay the full listing. The shared session directory never holds sync state. Live commands time out after 30 minutes. Passwords reach `kei` through `--password-file` or the environment, never the command line, so a failed command never prints them.

General live tests read the selectors in `tests/data/live-selection.toml`: albums `none`, unfiled enabled and the primary library. Rust `tests/common/live_selection.rs` and shell `tests/shell/lib.sh` add `[filters].skip_created_before` from `KEI_LIVE_SKIP_CREATED_BEFORE` to each generated config. Set optional `KEI_LIVE_SKIP_CREATED_AFTER` to an inclusive fixed upper capture date when the approved fixture is a closed interval. Missing upper dates retain lower-bound-only selection; empty, malformed or inverted dates fail before live authentication. Rust scenarios may narrow the configured interval but cannot widen it. Both bounds reach shell, cross-zone and release-binary sync configs. No named album, filename or media format is required. The listing step is also the preflight: a filename-only run against a copy of the listed state reports the eligible count and fails on an empty selection without downloading media. It uses isolated config/state paths and can reuse or refresh cached authentication.

Exact content checks use the [bundled corpus](data/README.md). The
[migration map](data/live-migration.md) names each deterministic replacement
and the live responsibilities that remain. The separate cross-zone scenario
still needs its explicit opt-in fixture.

## Portability

Every environment-specific value is read from an env var. No account
details are baked into test code.

| Variable | Default | Purpose |
|----------|---------|---------|
| `ICLOUD_USERNAME` | required | Apple ID email |
| `ICLOUD_PASSWORD` | required | Apple ID password |
| `ICLOUD_TEST_COOKIE_DIR` | `./.test-cookies` | Pre-authenticated session dir |
| `KEI_LIVE_SKIP_CREATED_BEFORE` | required for live sync fixtures | Explicit fixed `YYYY-MM-DD` lower capture-date bound, written as `[filters].skip_created_before`; no rolling or count fallback |
| `KEI_LIVE_SKIP_CREATED_AFTER` | unset | Optional inclusive fixed `YYYY-MM-DD` upper capture-date bound, written as `[filters].skip_created_after`; must not precede the lower date |
| `KEI_DOCKER_IMAGE` | `kei:latest` | Docker image under test |
| `CARGO_TARGET_DIR` | `./target` | Cargo build directory. Full-test packaging, shell, live, service, and metrics phases use release artifacts from this directory. |
| `KEI_FULL_TEST_TMPDIR` | `/tmp/codex/kei/full-test/tmp` | Temporary directory for full-test child processes and shell-suite scratch data. |
| `KEI_TEST_SCRATCH_DIR` | `/tmp/codex/kei/shell-tests-$USER` | Base dir for standalone shell-suite scratch; `just full-test` overrides this to `$KEI_FULL_TEST_TMPDIR/shell` or `/tmp/codex/kei/full-test/tmp/shell` |
| `KEI_IMPORT_FIXTURE_DIR` | `/tmp/codex/kei/import-fixture` | Parent directory for isolated import fixture runs retained for failure inspection |
| `KEI_FULL_TEST_CROSS_ZONE_ALBUM` | unset | Optional full-test album fixture for cross-zone hydration. The album must include at least one asset from a non-primary source zone. |
| `KEI_FULL_TEST_CROSS_ZONE_MIN_FILES` | `1` | Minimum non-primary downloaded asset rows required when `KEI_FULL_TEST_CROSS_ZONE_ALBUM` is set. |
| `KEI_FULL_TEST_REAL_SERVICE` | unset | Set to `1` to let `just full-test` install, start, status-check, and uninstall a real Linux user systemd service |
| `KEI_FULL_TEST_EXPECT_VERSION` | unset | Optional exact Cargo package version expected by the release-archive smoke |

`just test live` uses the shared bounded selection. Cookie dir falls through
to the harness default (`./.test-cookies`); set `ICLOUD_TEST_COOKIE_DIR` to
override it. Test state and download directories are isolated.

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
  bounded download flow, state, watch, reports, and filesystem recovery.
  Content policies are covered offline by the sync core acceptance tests.
- **`state_auth.rs`** - live iCloud, `#[ignore]` gated. Covers status /
  reset state / verify / import-existing / automatic retry through ordinary sync.
- **`import_existing_live.rs`** - live iCloud, `#[ignore]` gated.
  `import-existing` against real CloudKit record shapes. The offline
  icloudpd layout cases live in `src/sync_core/tests.rs` (`icloudpd_layout`).
- **`scripts/test-scenarios/`** - focused offline behavior slices named by
  risk class, not release history. Current slices cover URL refresh,
  identity/tombstone deltas, pending recovery, path-family collisions,
  auth/session flows, service health, config docs, and full-test harness
  reachability. Each named filter must resolve to at least one current test.
  Use these before a broad gate when touching the matching owner path.
- **`shell/concurrency.sh`** - things that need `kill -9` mid-download
  and direct sqlite3 assertions on the state DB (every file on disk is a
  saved copy, no duplicate paths). Hard to do cleanly from Rust.
- **`shell/state-machine.sh`** - saved-position and planning-fingerprint
  invariants across kei invocations with DB mutation in between: a config
  change plans again without listing, `reset sync-token` and a rejected
  position list from the start without downloading again, and a deleted
  file returns through `kei reconcile`.
- **`shell/docker.sh`** - anything that requires `docker run`, watch
  mode + SIGTERM, healthcheck probes inside the container.
- **`scripts/full-test/run_release_archive_smoke.sh`** - packages the host
  release binary into a temp archive, extracts it, and runs basic CLI/config
  probes against the extracted binary. `just test packaging` runs the host
  release build plus this smoke.
- **`scripts/full-test/run_release_regression_smoke.sh`** - fast offline
  patch-release gate for the saved position, retry backoff, adoption of
  files already on disk, interrupted and rejected listings, media
  validation, and config paths. Run it before releases that touch those
  areas.
- **`scripts/full-test/run_docker_puid_smoke.sh`** - offline Docker entrypoint
  checks for PUID/PGID drop, UID/GID repair and opt-out, symlink safety,
  `MALLOC_ARENA_MAX=2`, root-default behavior, and invalid env rejection. `just test docker-full` runs this with
  Docker build, multiarch, and CLI/default-command smokes.
- **`scripts/full-test/run_docker_packaging_smoke.sh`** - offline checks of real
  Docker build-context filtering and the image healthcheck at default/custom
  ports, including HTTP failure. Requires host port 9090 to be free.
- **`scripts/full-test/run_live_import_rehearsal.sh`** - live mini rehearsal
  for the TOML-first import path: seed a tiny real tree, import it into
  separate state, and verify that the import records the seed's exact
  copies, a repeat dry-run finds them recorded, and a following sync neither
  downloads nor adopts. `just test live-smoke` runs this after the
  release-binary live CLI smokes.
- **`scripts/full-test/run_cross_zone_album_hydration.sh`** - opt-in live
  release-binary check for accounts with a prepared cross-zone album fixture.
  It selects the named album with `libraries = ["all"]` and fails unless the
  sync records at least one saved copy from a non-primary source zone.
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
  bounded primary-library selection.

`just full-test` can run the Linux user-service lifecycle with
`KEI_FULL_TEST_REAL_SERVICE=1`. It refuses to run if `kei.service` already
exists, and it uninstalls the service before returning. The installer may
temporarily enable user linger; uninstall restores the prior linger state when
that state was recorded during install.
