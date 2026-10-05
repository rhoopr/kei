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
  branch_static.rs             offline packaging, migration, and tooling checks
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
| `cargo test --test branch_static` | no | `just test offline`, selected scenario slices |
| `cargo test --all-features --lib icloud::photos::album::lookup::tests::live_targeted_record_lookup_distinguishes_present_and_missing -- --exact --ignored --test-threads=1` | yes | `just test live` |
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

The library lookup probe runs by exact name so the live route does not run
other ignored library tests, including the parent-controlled process-death
child. Full-test reaches this probe through `just test live`.

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
| `config-reconciliation` | Config-hash staging, local catalog path reconciliation, and repeat-cycle stability | Download config hashes, path reconciliation, config drift, or local reconciliation copies |
| `fulltest-harness` | Full-test phase reachability and rejection of stale or empty scenario filters | `just` dispatch, full-test orchestration, or scenario-runner helpers |
| `identity-recovery` | Malformed capture dates, ambiguous historical owners, deferred retries, restart, and eventual recovery | Metadata capture, pending recovery, identity evidence, or their checkpoint gates |
| `identity-deltas` | Incremental identity mapping, hard and soft deletion, selected relations, master-family transitions, and current catalog work admission | CloudKit change parsing, identity mapping, membership, or tombstone policy |
| `path-family` | Collision suffixes and primary, Live Photo, import, and pending-file family matching | Path rendering, collision handling, import matching, or on-disk adoption |
| `pending-recovery` | Durable pending hydration, policy-excluded deletion proof, ambiguous identity retention, and sibling recovery | Retry resolution, pending or policy-excluded state, provider identity, or targeted hydration |
| `service-health` | Health and metrics facts exposed by unattended operation | Health checks, metrics, cycle reporting, or service monitoring |
| `url-refresh` | Refresh of expired or aged download URLs without replaying stale deltas | Incremental downloads, URL freshness, album hydration, or retry downloads |

## Bounded identity recovery

Run `just test scenario identity-recovery` for the bounded recovery phases of
[#862](https://github.com/rhoopr/kei/issues/862). It composes existing production
owner tests instead of adding a simulation framework. The complete offline
suites execute these same tests; the gate also validates the scenario filters.

| Evidence | Tests selected by the slice |
|----------|-----------------------------|
| Missing, null, out-of-range, and valid epoch capture dates; one child or a matching visible/hidden sibling | `hidden_invalid_capture_date` selects both metadata capture and pending recovery |
| Conflicting or missing historical dates/owners and failed retry persistence | `run_cycle_legacy_owner_guard_preserves_dates_and_checkpoint`, `bounded_full_sync_hydrates_live_legacy_pending_master` |
| Deferred work, changed authoritative evidence, bounded recovery, and no repeated completed work | `run_cycle_metadata_capture_retry_preserves_durable_checkpoint_until_repaired`, `ambiguous_capture_repair_keeps_identity_and_checkpoint_without_full_backfill` |
| Schema-28 preservation with one or multiple current children | `run_cycle_single_survivor_preserved`, `run_cycle_ambiguous_children_preserved_independently` |
| Changed rendition evidence and stale retry attempts | `metadata_capture_retry_changed_evidence_is_due_and_old_attempt_cannot_delay_it` |
| Other-library success cannot clear unresolved work; streaming and collecting paths retain failed work | `unresolved_identity_survives_restart_and_other_zone_success_then_recovers`, `sparse_retry_omitted_source_preserves_failed_work_then_recovers_media` |

The two capture-date matrices seed file-backed SQLite and nonempty media
directories. Each logs its provider date, sibling visibility, cycle, and operation
sequence on failure. They release production DB handles and reopen SQLite before
asserting durable dates, ownership, statuses, retry evidence, capture revisions,
checkpoints, and unchanged media as applicable. Assertions use fixture facts,
not the production candidate-selection algorithm. A valid Unix epoch must make
progress; an invalid date must never become an epoch through a fallback.
Matching malformed siblings remain ambiguity evidence.

The capture matrix changes invalid provider evidence to a valid date, injects a
SQLite refresh failure, removes the trigger, and requires recovery on the next
cycle plus an unchanged follow-up. A valid owner claim may survive a failed
metadata refresh, but the catalogue date, capture receipt, and checkpoint must
remain unchanged until refresh commits. Malformed mixed-child cases remain unresolved;
separate existing tests supply authoritative ownership and prove finite recovery.
Fully valid ambiguous families exercise schema-28 preservation in their existing
regressions. The capture epoch control isolates a single child's valid date.
No media rewrite is enabled. Pending recovery verifies the exact downloaded
bytes, preserves unrelated media, and permits only one HTTP download.

Historical pre-fix revisions have not been rerun for this slice. The #861
counterexamples are retained and strengthened on current main; this is not a
claim that historical/refactor parity proves the independent oracle.

The initial slice in #873 ran 11 tests in 25.7 seconds on Linux with a warm default-feature
build cache. That is below the proposed additional 2-5 minute PR budget. Cold
compilation is excluded; complete-suite timings remain separate gate evidence.

Broad exploration, semantic fuzzing, composed filesystem recovery, and physical
NFS/NAS qualification remain later phases. The bounded process-death matrix
below adds actual Linux termination at selected durable transitions.
SQLite reopen and injected transaction failure do not model abrupt process death
or power-loss durability.

### Seeded recovery sequences

The bounded next phase adds four fixed seeds: 862, 873, 869, and 870.
The expanded 15-test slice took 27.4 seconds with a warm default-feature cache;
the four sequence tests took 0.94 seconds.
Each runs eleven production cycles against its own file-backed SQLite database
and nonempty media directory. This targets evidence that regresses after a valid
owner survives a failed refresh, beyond the initial invalid-to-valid matrices.

Every trace begins with ambiguous evidence, a provider-only improvement during
backoff, then an explicitly due retry whose valid owner claim survives an injected refresh
failure. Five seeded events reorder missing, null, and out-of-range provider
dates, cancellation during lookup, and another refresh failure. The tail always
supplies a new valid date, then runs two unchanged cycles. Recovery must complete
on that first fault-free cycle; seeded ordering cannot postpone the liveness
deadline. Each cycle releases production DB handles and reopens SQLite before
checking dates, owner, capture receipt, retry row, checkpoint, statuses, media
and an unrelated sidecar. The quiet tail permits normal delta polling but no
repeat metadata hydration, inventory scan, repair, or download.

Replay one schedule with
`cargo test --lib capture_recovery_sequence_seed_862 -- --nocapture`
(substitute any listed seed). Each step prints the seed, event index, event and
complete expanded trace. The fixed seeds use the existing locked rand dependency;
retain the expanded trace when comparing different dependency revisions.

The deadline mutation is confined to the fixture, and cancellation is cooperative.
This does not qualify abrupt process termination, filesystem recovery or NAS
durability. The bounded generated histories below extend these fixed schedules.

### Reconstructed and generated histories

`tests/data/recovery-histories.json` contains three **reconstructed** histories
based on public descriptions in [#861](https://github.com/rhoopr/kei/issues/861),
[#765](https://github.com/rhoopr/kei/issues/765),
[#853](https://github.com/rhoopr/kei/issues/853), and
[#862](https://github.com/rhoopr/kei/issues/862). These are synthetic compositions,
not captured reporter databases, photos, provider traffic, or exact incident
replays. Each entry records its provenance and relationships among the provider
family, SQLite evidence, files, and configuration. The runner instantiates those
relationships using the existing synthetic recovery fixture.

Eight default seeds draw 1-18 faults with replacement from nine events. They vary
length, repetition, missing child evidence, invalid dates, visibility, temporary
recent-item filtering, interrupted lookup, and failed refresh. Every history has
the same required ambiguity/deferred/failed-refresh prefix and valid/two-quiet
cycle tail. The prefix establishes a known durable owner; generated faults test
that retained owner's recovery, not arbitrary attribution of unknown families.
The original four fixed schedules remain separately runnable. Together, the six
tests execute 192 production cycles. The initial all-feature run took 7.02 seconds
with a warm build cache; compilation is excluded.

All histories enter the production cycle and reopen SQLite after every operation.
The oracle uses fixture facts to check exact ownership, status, dates, checksums,
retry/revision state, checkpoints, original bytes, unrelated sidecar bytes, and
bounded work. Recovery must complete on the first valid tail cycle. Two further
cycles must not repeat metadata hydration, repair, or download. Configuration
changes here cover recent-item selection only; metadata writers remain disabled.

Run the focused slice with `just test scenario identity-recovery`. Replay a seed:

```sh
KEI_RECOVERY_SEED=765 cargo test --lib capture_generated_histories -- --nocapture
```

On failure, at most 24 fresh fixture replays try deleting fault events while
preserving the prefix, mandatory tail, and original panic message. The original
failure is always rethrown. The reported reduced trace is a bounded reduction,
not a claim of global minimality. A temporary byte-loss mutation was detected and
reduced from four faults to `MissingChild` in six replays; that mutation is not
part of the committed tests. Replay a fault array directly (no dependence
on the RNG version):

```sh
KEI_RECOVERY_FAULTS='["MissingChild","FailRefresh"]' cargo test --lib capture_generated_histories -- --nocapture
```

Save the printed seed, full original trace, reduced trace, and assertion before
changing code. Promote any demonstrated product defect to its own deterministic
regression. These tests do not exercise arbitrary library shapes, real-account
state, abrupt process death, or power-loss durability.

### Mixed library provider response equivalence

`mixed_library_provider_shapes` composes long-lived synthetic families in two
zones with reused raw IDs, distinct dates and checksums, independent current
children, unattributed legacy files, and numbered Live Photo publications in
two zone-local albums. Two fixed histories each run six production cycles through
`run_cycle`, reopening real SQLite after every cycle. One withholds the final
inventory while another zone progresses; the other changes still-only selection
to both renditions and changes album selection and order during identity debt.
Both retain complete-evidence recovery and two quiet cycles. The recovery
cycle makes any synthetic discovery retry eligible by changing only its due
timestamp; it retains the retry signature, identity debt and all receipts.
This models elapsed scheduling time without sleeping or bypassing proof gates.

Each history runs through baseline responses, independent-family reordering and
alternate page cuts, then those same cuts with exact duplicate observations and
an empty continuation page. These are equivalent snapshots, not reordered causal
updates or tombstones. Normalized state comparisons include identity, ownership,
publication receipts, membership, retry and deletion state, and active and staged
zone checkpoints. Independent oracles check legacy bytes, existing numbered paths,
new-child ownership and subsequent stability, request bounds, and quiet hydration,
download, metadata-repair and file counts. New collision suffix spelling is not
part of response equivalence. Mock-server bind failures fail the test.

The cap is 36 production cycles per feature configuration. Replay JSON selects
one complete six-cycle history and encoding; a non-baseline encoding also runs its
baseline, for at most 12 cycles. It preserves prerequisites and recovery/quiet
tails; no fault-deleting reducer is applied to these fixed histories:

```sh
KEI_MIXED_REPLAY='{"history":"SelectionChange","encoding":"DuplicateEmpty"}' \
  cargo test --all-features --lib mixed_library_provider_shapes -- --nocapture
```

`mixed_numbered_album_mode_transition_repro` reduces the discovered receipt-reuse
bug to two production cycles, one zone, two album publications, and no legacy or
identity debt. `mixed_legacy_reservation_blocks_preparation_repro` reduces the
second bug to one zone and one reconciliation call: missing sibling evidence
must not create a master reservation, claim an owner, or disqualify later
preservation. The full history proves subsequent recovery and quiet cycles.
The existing pagination, tombstone, hidden-sibling, configuration
reactivation, single-survivor, multi-album and restarted Live Photo proofs remain
separate lower-level coverage. This small composition matrix does not qualify
whole-library inventory memory or request cost at production library scale.

### Abrupt process-death recovery

On Linux, `process_death_recovery_matrix` kills six dedicated subprocesses with
SIGKILL: fallback-journal displacement, installation and commit; media publication
before state finalization; verified state before checkpoint completion; and
checkpoint completion. The parent checks signal 9 and independently reopens
SQLite and reads media before recovery. A worker that exits early or fails to
reach its marker fails the proof with its synthetic log. Each rendezvous has a
20-second bound; mock-server bind failures fail normally.

Journal cases start with a successful production cycle, damage only an owned
synthetic file, and enable the existing explicit truncated-file repair. A test-only
hook selects the existing journal publication lane after its fingerprint guards;
existing seccomp coverage separately proves unsupported-rename routing. Other
cases start from pending media. Every case preserves an independently owned
same-ID asset and private sidecar in an unselected zone.

Restart promotes interrupted runs through the same owner used by production
startup, then enters `run_cycle` with reopened SQLite. The first valid cycle must
restore correct media, exact publication receipts and ownership, metadata retry
state, and the source checkpoint. A committed replacement can retain a recent
prepared-file claim under the existing cleanup grace period. The fixture verifies
those bytes, then ages only that exact claim and file; the next production cycle
must retire them through normal cleanup. Two further cycles must perform no
hydration, download or metadata repair and leave durable state, file bytes,
identities and modification times stable. Identity/time checks also detect
metadata rewrites that reproduce identical bytes.
The matrix has at most 28 production cycles including prerequisites, six killed
cycles, recovery, eligible cleanup and quiet tails. Warm execution initially took 4.7 seconds with all features and 4.5 seconds
without default features; compilation is excluded.

Replay one complete case, retaining its prerequisite and recovery/quiet tail:

```sh
KEI_PROCESS_DEATH_REPLAY=journal-committed \
  cargo test --all-features --lib process_death_recovery_matrix -- --nocapture
```

Temporary synthetic byte-corruption and premature-checkpoint probes both failed
at the independent pre-recovery assertions; neither probe is committed.
The accepted points are the six names printed by the matrix; unknown names fail.
The ignored worker is invoked only by its exact parent-controlled test name and
is excluded from the live-test route. Existing lower-level post-publication kill,
returned-error journal interruptions, cancellation and SQLite-reopen tests remain
separate coverage. This slice proves process termination at those transitions;
it does not qualify power-loss durability, arbitrary crash points or real mounts.

## Tactical external-state recovery

The next bounded [#862](https://github.com/rhoopr/kei/issues/862) slice extends
`just test scenario config-reconciliation` with existing fixtures and production
owners. It adds these specific interactions to coverage already present:

| Existing evidence | Added interaction |
|-------------------|-------------------|
| Confined copy races, hardlinked destination refusal, and full-cycle symlink rejection | Full-cycle hardlink and same-size conflict refusal, externally relocated destination directory, SQLite restart, valid same-byte source replacement with altered mtime, local recovery and quiet follow-up |
| Reconciled sidecar conflicts and failed catalogue finalization | Source packet edited externally after media and sidecar publication but before successful catalogue finalization; restarted retries preserve both packets, then explicit fixture conflict resolution copies the edited packet |
| Ordinary sidecar publication with failed marker clear | File-backed failure, reopened recovery and quiet follow-up with independent parsed XMP and durable publication/debt assertions |
| Released-schema upgrade preservation and failed-download recovery | Schema-25 metadata debt composed with disabled/enabled sidecar policy, library selection reactivation, a hidden active item, and a deleted sibling |

The full-cycle external-entry cases preserve source bytes, source mtime,
unrelated metadata, the previous catalogue path, destination reservations and
provider/config checkpoints through two refusals. They assert the corresponding
failure diagnostic and block normal source dispatch. After only fixture
conflicts are removed, a replaced source inode with identical bytes and a changed
mtime must reconcile locally. The destination receives capture mtime; the source
retains the external timestamp. A restarted unchanged cycle must preserve both
historical and current publication receipts without another media copy.

The external-sidecar case begins with a published media copy and packet whose
catalogue finalization failed. An edited source packet conflicts with that prior
packet on two reopened retries. Neither packet may be overwritten or discarded.
The fixture preserves the prior destination packet under a separate filename
before retrying, then requires the edited custom packet, source media/date
preservation, and a reopened quiet tail. This is explicit fixture resolution,
not automatic conflict resolution or new metadata policy.

Ordinary metadata rewrite debt does not by itself hold a fresh provider
checkpoint. The incremental-owner test checks the returned checkpoint candidate
and durable debt together; that owner does not commit the token. Its persisted
previous token remains unchanged. The released-state test enters the production
refresh tail, which also does not advance provider checkpoints. It checks exact
library-scoped identities, dates, hashes, statuses, capture revisions, publication
receipts, deleted metadata, media bytes/mtime and completed sidecar bytes/mtime.

The schema fixture was emitted by official v0.24.0; all catalogue rows and
operation histories here are synthetic. These tests do not rerun historical
provider sync or claim to reproduce a reporter's database. XMP-only scenarios
run with XMP enabled; the external media cases also run without default features.
The existing `released_v0240_failed_history_recovers_after_upgrade_and_restart`
still supplies the separate production-cycle failed-download upgrade proof.

The initial focused scenario passed 71 test invocations in 67.5 seconds,
including recompilation with a warm dependency cache. The new external media
cases took 0.34 seconds together; the released-state case took 0.27 seconds and
the ordinary sidecar lifecycle took 0.25 seconds, excluding compilation.
Reversible controls removed the hardlink alias guard and ignored the rewrite
library scope separately. The hardlink test detected falsely completed
reconciliation in cycle zero; the released-state test detected premature debt
retirement in the unselected library. Both production files were restored
exactly. Historical pre-fix revisions were not rerun for these new compositions.

Hardlinks retain current refusal behavior. This slice does not implement #884,
change product policy, qualify NFS/NAS or power loss, or duplicate the separate
subprocess-death phase.

## Terminal replacement cleanup

`terminal_cleanup_subprocess_regressions` scopes synthetic faults to disposable
Linux subprocess fixtures. It detects manifest handles still open at directory
removal, injects `ENOTEMPTY` on an empty terminal directory, and covers normal
publication, committed restart recovery and empty-manifest initialization.
Unknown entries, changed directory identity or target bytes, other removal
errors and directory sync failures must retain failure classification and the
prepared path. Existing late-original-edit, active-lock, malformed-manifest,
symlink and interrupted-publication proofs remain separate.

With XMP enabled, `terminal_sidecar_cleanup_retires_marker_across_restarts`
seeds a downloaded media row and first sidecar, changes only provider metadata,
and runs the production queued rewrite owner. Under both descriptor-sensitive
and deterministic empty-directory faults, the marker must retire and the
operation's prepared file must disappear. Two reopened SQLite/startup-recovery
cycles must preserve sidecar bytes, inode, timestamp and link count, media
checksum, and temporary-entry count. A historical prepared file stays intact.
These tests model terminal failures on local storage. They do not prove an NFS
silly-rename mechanism or physical NAS behavior.

## Strict changes/zone pages

`normal_changes_rejects_entire_malformed_page_before_emission` exercises missing,
null and non-array records, zone cardinality and owner scope, mixed nested record
errors, and unusable tokens. Continuation-cycle and valid empty/unknown-record
controls run beside it. Both incremental consumer strategies reject a malformed
later page through their production entry point.

`run_cycle_rejected_zone_page_preserves_cursor_debt_and_media_across_restart`
seeds file-backed SQLite with a checkpoint, downloaded media and pending retry
work. It rejects a later wrongly scoped page, reopens the database between cycles,
checks a quiet provider delta and the watch local-work bypass, resolves the retry
with explicit provider deletion evidence, then checks an unchanged cycle. Existing
media bytes remain intact and no cycle publishes another file. The synthetic
fallback cannot provide authoritative completion during the malformed cycle.

## Observed incremental shadow capture

The shadow fixtures open account-owned, file-backed SQLite and retain exact
original JSON bytes through two reopens, including an unknown record, tombstone,
unresolved cross-zone relationship and a numeric lexeme changed by typed
re-encoding. Stored source ordinal, type, deletion flag and account/provider/scope
facts are independent oracles. Replay through the legacy stream preserves its
paired events and does not alter seeded media, downloaded rows or old-epoch debt.
Private/shared scopes remain separate; unknown zone metadata does not create a
new stable identity. Changed payload revisions remain separate observations.

A receipt-write trigger fails after page and source inserts and proves complete
rollback. Reopen, remove only that fault, refuse an exhausted inbox, recover and
replay idempotently at capacity. A changed revision at capacity is refused
without overwriting existing observations. Malformed identities, duplicate JSON
keys and record-scope mismatches refuse the entire page. Account provenance is
rechecked even for an existing page. A delayed provider response after receiver
cancellation captures nothing and retains the legacy cursor before restart.
Actual HTTP tests preserve original bytes and enforce both declared-length and
chunked page limits.

An actual CLI matrix starts without pending retry debt, offers a valid rank EOF
and replacement anchor, and injects a receipt-write failure, exhausted logical
budget, malformed source identity or duplicate JSON key. Two process attempts
retain the cursor, existing media and observations without issuing rank queries.
Removing only the injected fault permits recovery and two quiet repeats. The
capacity case charges a real previously captured page; it does not simulate a
physical disk-full condition.

`run_cycle_shadow_transaction_preserves_cursor_debt_and_media_across_restart`
seeds historical media, mapping, downloaded state and pending debt. Four
production cycles across reopen cover capture failure, captured observations
with unresolved checkpoint evidence, recovery under the existing gate and a
quiet repeat. Media remains intact and no cycle publishes another file. The
actual CLI startup control captures an unknown source through normal library
resolution without manually attaching an album. The schema-28 migration fault
control retains source cursors, debt and an unknown BLOB in a conflicting table
with matching columns but no durable key. It rolls back partial DDL, then retries
and reopens the current schema twice. Payload replay also retains existing observations
through synthetic migration re-entry. Owned migration controls also reject TEXT,
inline descending integer and WITHOUT ROWID page keys on two reopens, preserving
schema 28, source cursor, old debt, conflicting DDL and unknown BLOB bytes.
Released-schema history fixtures also
retain their independent row, media and checkpoint assertions.

Reversible negative controls replace original bytes with typed JSON, remove the
capture transaction and bypass owner validation, the rowid guard or the refusal
classification. Each corresponding assertion
must fail before source restoration. Bounded exact-resource URL refresh tests,
including RAW/JPEG provider rendition and per-file retry isolation, remain part
of qualification. These synthetic controls do not qualify power-loss durability,
full-zone history, expired-epoch completeness or inbox-driven materialization.

## Transactional source catalog replay

Owned file-backed fixtures replay schema-29 captured sources through migration
and two quiet reopens. Independent expected facts include masters, assets,
albums, membership, unknown records, typeless tombstones and incomplete links.
Literal references retain escaped source field paths and cross-zone provenance;
original JSON bytes and numeric lexemes remain unchanged. Distinct revisions and
private/shared scopes stay separate; replaying an old page cannot overwrite facts.

A receipt trigger aborts after real record, reference and unresolved-evidence
writes. The index rolls back completely while the original capture survives.
An actual CLI fixture starts with a schema-29 backlog, fails projection in two
processes, removes only the injected trigger, recovers and repeats quietly twice.
Existing media, downloaded receipts, metadata revisions, pending retry and old
scope sparse debt remain intact. That retained debt keeps the overall CLI cycle
incomplete even after indexing recovers; projection cannot acknowledge it. Only the existing current stream can advance its token;
the old captured successor is never installed as the checkpoint.

Controls reject owner, provider, hash, scope, source ordinal and page-metadata
mismatches, including a source changed after planning but before the transaction.
Completed replay refuses a corrupted receipt, facts or unsupported projector
version without rewriting the retained evidence.
A conflicting schema-30 table preserves schema 29, captured bytes and unknown
BLOB history through two failures and recovers after removing only the injected
fixture conflict. Logical index exhaustion retains pending sources, allows exact
completed replay at capacity, and recovers with available capacity. A page that
fits the raw limit but expands derived reference paths beyond the per-page budget
is refused without partial indexing. Cancelled replay leaves work for restart.

Reversible bypass controls remove the catalog transaction, raw/source equality,
transaction source recheck or startup replay. Each fails its intended runtime
assertion before source restoration; the complete positive matrix then passes.

These fixtures qualify additive source indexing, not inbox-driven queue
materialization, selection generations, snapshot completeness, historical debt
acknowledgment, local deletion or stronger power-loss durability.

## Current catalog work admission

The `queue_projection` slice enters the actual current lookup, task planner and
SQLite admission owners with an owned file-backed database, historical captured
facts, pre-seeded cursor/debt, unknown BLOB history and nonempty media plus an
unrelated sidecar. Older source payloads confirm the current provider generation;
its checksum, size and metadata bind the admitted queue obligation. Two reopens
then perform no repeated lookup or admission. Current configuration changes
replan filtered observations without replacing retained historical source facts.

Pending/failed content and metadata conflicts retain retry text, attempts and
prepared publication evidence. Replaying an admitted source never restores an
old queue generation; shared asset, mapping and metadata writers preserve
unfinished projected work. A real conflicting file forces safe current-generation
leaf replanning through the shared planner/upsert without stranding the queue.
Fixtures qualify default `PrimarySync` through actual `PhotosService::new` and
`resolve_libraries` before and after capture attachment. Authenticated private
zone discovery supplies an explicit owner; missing, deleted and different-owner
entries never infer one, and shared endpoint evidence cannot qualify primary.
Malformed, duplicate, errored and unsupported-continuation listings fail before
cache publication, then valid discovery recovers and remains cached. A retained
explicit-owner source triggers current admission on the default route. A real
receipt-write fault rolls back all queue, mapping, obligation and scan writes;
recovery and two reopened quiet cycles preserve prior checkpoints, debt, original
ownerless observations, nonempty media, sidecars and unknown BLOB history.
Scope/account/provider, changed-source and corrupted confirmation controls refuse
admission. Unsupported selection and read-only
modes perform no new work publication. The bounded scan advances past unresolved
identities and returns to them without inferring coverage.

A receipt trigger fails after real queue, mapping, obligation and scan writes.
The complete transaction rolls back with source/catalog/checkpoint and media
intact. Normal dispatch encounters the same fault without a rank fallback.
On Linux, an isolated parent kills the test child while the production admission
transaction is paused immediately before its receipt. Another connection sees
only committed source history; restart rolls back all work publication, then
recovers and reopens quietly twice. This proves process-interruption recovery,
not power-loss survival. The private test child is ignored unless its parent
explicitly invokes it with an isolated marker and disk-backed temporary root.

Schema-30 fixtures upgrade to 31 through two reopens. A conflicting unknown work
table fails migration without changing the schema version, captured sources or
unknown bytes. Work receipt and derived metadata byte budgets retain unresolved
sources and existing obligations at capacity.

These tests qualify bounded work admission into the existing queues. They do not
claim media materialization/verification, recent-cap semantics, named-album or
shared-zone admission, cursor ownership, snapshot absence, pruning, historical
retention or acknowledgment of unrecoverable debt.

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

Account-bound migration qualification uses real file-backed SQLite with a
live WAL, historical downloaded and pending rows, retained source cursor and
pending token, sparse identity debt and an unknown future table. It enters
the production adoption owner, checks source/companion preservation and
reopens the published state twice. Negative controls cover colliding logins,
realm and provider-pin mismatches, unowned and interrupted empty files,
conflicting legacy provenance, missing authentication, occupied destinations,
and reset/export against a misplaced owned database. Authentication fixtures
prove that legacy files are ignored and a fresh adoption constructor clears
only new-namespace auth under its lock. Released-schema CLI history first
proves legacy refusal, then uses a synthetic owner fixture to test application
schema upgrades; that fixture is not attributed to real authentication or
adoption. No provider credentials or reporter data are used.

The overlapping-account CLI matrix seeds distinct catalogues, retries,
publication receipts and checkpoints under equal zone/asset IDs. It exercises
status, manifest, verify, reconcile, doctor, import-existing, sync and both
state/token resets with correct owners and a deliberately misbound database.
Each correct-owner command preserves the other account's rows, database bytes
and media. Equal provider pins do not implicitly merge different login strings.
A separate adopted, owned database runs two production quiet sync cycles across
reopen with its original media, cursor, master mapping and receipt intact.

On Linux, the real adoption owner pauses a disposable subprocess before backup,
after the SQLite copy, after stage fsync, after publication and after directory
fsync. The parent sends SIGKILL, checks committed source WAL and unknown rows,
then retries unpublished copies or reopens the complete published target twice.
An interrupted private stage is preserved; after publication it shares the
canonical database inode. This proves process-interruption recovery, not
power-loss durability or automatic orphan cleanup. Other platforms run the
owner, CLI and quiet-cycle tests without the Linux SIGKILL matrix.

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

3. Keep at least one eligible asset in the primary library. General live
   tests use `tests/data/live-selection.toml`: albums `none`, unfiled enabled,
   primary library, and the 10 most recent assets. Change the bound only there.
   No named album, filename, date, or media format is required. A read-only
   preflight reports the eligible filename count and fails if it is zero.
   When the bound truncates the inventory, live tests require checkpoint
   suppression and a no-download repeat full pass. Positive incremental live
   checks apply only when this bounded selection proves EOF. Deterministic
   production-path tests cover both cases and config reconciliation.

Import runners also pass the shared count as `--recent`, because
`import-existing` does not use the TOML recent count.

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
  Content policies and metadata use bundled production-path tests under
  `src/download/orchestration/fixture_tests/`.
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
  bounded primary-library selection.

`just full-test` can run the Linux user-service lifecycle with
`KEI_FULL_TEST_REAL_SERVICE=1`. It refuses to run if `kei.service` already
exists, and it uninstalls the service before returning. The installer may
temporarily enable user linger; uninstall restores the prior linger state when
that state was recorded during install.

## Sparse-share identity regression

`tests/data/sparse_shared_asset.json` is synthetic, with invented identifiers.
It reproduces the reported sparse record structure, not an authenticated Apple
fixture. Parser and lookup tests preserve opaque share evidence and check
malformed fields, redacted output, requested fields, and no linked-target
requests. The full-cycle unresolved-identity test also runs with this record
pointing to a zone absent from the selected libraries. It reopens SQLite,
checks another zone's success, preserves seeded media, and proves explicit
source-deletion recovery through lookups, typed soft-deletion deltas, and
exact-source hard-deletion tombstones,
then an unchanged follow-up cycle. It covers malformed
and changed links, rejects unrelated durable identity mappings, and injects an
unresolved-marker write failure before replay. A source-deletion write failure
retains the sparse row and checkpoint until the next successful cycle. The source checkpoint stays
unchanged until recovery. A DEBUG-level capture checks that changed-link
diagnostics contain no record, zone, or owner identifiers. Exact-master
recovery tests also run with sparse linkage present. These tests do not prove
a safe remapping rule for photos retained after shared-library removal.

Durable retry tests cover schema-25 migration, restart, preserved original and
separate lookup evidence, capped backoff, fair batches, and stale or missing
completion receipts. The checkpoint test injects transaction failure and checks
that token, marker, and retained rows roll back together. Full-cycle tests run
both streaming and collecting paths. They check a deferred replay, other-zone
success, new valid media downloads, and exact source recovery when replay omits
the source. Recovery retains seeded files and retries after interruption or a
state-write failure before an unchanged follow-up cycle. Parser tests round-trip
the versioned link encoding and reject corrupt or unsupported evidence.

The sparse-deletion batch regression seeds 101 retained sources and local
media, then runs streaming and collecting cycles against explicit source
`UNKNOWN_ITEM` responses. It reopens SQLite between cycles, tests replayed
and omitted source deltas, injects a cached-deletion state-write failure, and
checks that recovery uses 101 total lookups and stops querying after completion.
The same test changes the zone snapshot between batches with unrelated records.
Additional streaming and collecting cycles restore a source with the same link,
fail a validation tail page, or cancel validation. Each case retains unresolved
work and the old checkpoint. Provider tests check raw source IDs across pages,
including paired assets, and reject malformed, wrong-zone, and cyclic responses.
Failed or cancelled validation is followed by recovery and an unchanged cycle
with no repeated source lookups.
Snapshot-token, changed-link, stale-generation, redaction, and versioned
completion-evidence roundtrip tests prevent reuse of invalid deletion evidence.

## Released schema upgrade history

`behavioral::released_upgrade::released_v0240_history_preserves_durable_evidence_through_upgrade`
starts from the SQL schema emitted by the official v0.24.0 Linux x86_64
binary (schema 25), then executes the current production CLI through schema 31.
The fixture contains no captured user data. SQL explicitly reconstructs a
synthetic history with original and edited renditions, repeated IDs in two
libraries, a local removal, metadata debt, an unfinished sync ledger row,
partial bytes, master mappings, and committed versus pending checkpoints.

The test reopens SQLite after commands, checks preserved bytes and identity
evidence, changes the destination config, requires missing-file reconciliation
to leave durable failed work, and repeats reconciliation and verification twice.
Restoring a file alone must not falsely finalize its failed row. The companion library test
`released_v0240_failed_history_recovers_after_upgrade_and_restart` starts from
the same schema fixture (or actual released-binary-created schema), retains an
explicit authoritative source/master mapping and failed media row, and runs
three production cycles. The first injects a state-write failure and must retain
the checkpoint and a pending retry row (retry routing resets failed to
pending before the injected write). After reopening SQLite, authoritative synthetic
provider evidence and a local HTTP JPEG must finish recovery on the second
cycle. The third must retain the recovered path and bytes without another
lookup or download. Unrelated original and sidecar bytes remain unchanged.
This companion test uses real JPEG framing and provider checksum encoding. Text byte fixtures
test preservation and SHA-256 checks; they do not qualify image decoders.

The normal gate runs this test without downloads or a historical executable.
To additionally qualify the actual released binary on Linux, obtain these
official assets outside the test gate:

- [v0.24.0 archive](https://github.com/rhoopr/kei/releases/download/v0.24.0/kei-linux-x86_64.tar.gz)
- [v0.24.0 checksums](https://github.com/rhoopr/kei/releases/download/v0.24.0/SHA256SUMS.txt)

Then run:

```sh
CARGO_TARGET_DIR=/home/example/kei-cache python3 scripts/qualify-released-upgrade.py \
  --assets /home/example/kei-releases/v0.24.0 \
  --scratch /home/example/kei-upgrade-qualification
```

The qualifier checks the archive against both the pinned GitHub asset digest
`0402df3eff13904ca1417d5b52758ccbe98a2d7f5360b84cb04cde5972a5c4f3`
and the release checksum file. It extracts only the regular binary, checks
`kei 0.24.0`, and requires `unshare -Urn` to run both replays with only loopback networking enabled. No external
network interfaces or routes are available; local HTTP serves synthetic bytes. The release tag resolves to
`aec13c42ce476edd8f8e0019bf58da1fd5d1b879`; archive verification establishes
artifact identity, not a reproducible-build attestation.

In qualification mode, the old binary creates schema 25 and its emitted schema
must exactly match the checked-in SQL. The SQL fixture was generated with
Python sqlite3 Connection.iterdump() after the released binary ran verify
against an empty database file; PRAGMA user_version = 25 was appended because
iterdump does not emit that pragma. It contains the complete released schema
and no asset rows. The equality check compares all non-internal sqlite_master
objects, so regeneration cannot silently substitute the current schema. It verifies four synthetic files, detects
an external edit, and preserves durable state before the current binary runs.
Fixtures are still seeded by explicit SQL, not by historical iCloud sync.
All child CLIs clear inherited environment and use explicit fixture config and
data paths. Cargo uses `--offline`, so dependencies must already be cached.
The qualifier fails if user/network namespaces are unavailable; it does not
silently fall back to a externally network-enabled run. The cycle test uses a strict wiremock bind
so unavailable loopback fails qualification instead of skipping the proof.

## Bounded incremental URL refresh

Synthetic Photos-session and loopback-CDN fixtures reproduce tokenless refresh
scans and persistent HTTP 410 cancellation of a delayed healthy peer on the
unchanged implementation. The fixed production collecting and explicit-pass
paths use bounded child/master lookup and preserve exact selected task evidence.
File-backed SQLite tests seed pending work and unrelated media, reopen after
refusal, repair the provider fixture, download exact bytes, and check an unchanged
tail without repeated completed provider work. Negative controls change master,
child, zone/owner, rendition, checksum, size and lookup presence. RAW/JPEG
preference swaps are exercised in both aged preflight and expiry recovery,
with the original provider rendition pinned independently of logical task keys. Authentication
and stalled-lookup cancellation retain debt, and cancellation during an active
transfer accounts for both in-flight and queued tasks after reopen. These are
synthetic runtime proofs; they do not validate reporter data or resolve sparse
Shared Library ownership.

Primary discovery availability is covered by
`primary_discovery_default_selection_does_not_initialize_unselected_private_libraries`:
default selection with an unrelated private zone still indexing succeeds with
explicit primary ownership, while a full-map request fails without publishing a
partial map. The qualified primary survives that failure, and a later full-map
request recovers when indexing finishes. The selected-primary indexing failure
test separately proves that cached valid descriptors cannot publish ownership
before the selected scope passes its own check. Malformed-list tests prove that
invalid complete-list evidence is never cached. Existing queue admission tests
cover capture attachment, durable receipts, rollback, replay and unchanged follow-up.
