# Tracing a change

Start with the decision owner in the [architecture map](../architecture.md), then trace its callers and the consumers of shared state.

## Change-impact traversal

Use these search anchors after finding the owner. The related owners are an
impact set, not a claim that every run calls each one in sequence. Read direct
callers and follow shared values through their consumers. Each row names the
flow section that owns its behavior and safety details; this table is not a
second call graph.

This table covers the highest-traffic changes only. Use the Owners table to
find the owning module, then this table to find related consumers. Use the
Change-impact checklist to choose verification.

| Change | Search anchors and related owners | Flow or invariant |
|--------|-----------------------------------|-------------------|
| Collection selection | `Selection` in `src/selection.rs`; `collection_libraries`, `pass_scope_for_zone`, and `resolve_passes_for_scope` in `src/commands/service.rs`; sync callers in `src/sync_loop/planning.rs` and import callers in `src/commands/import.rs` | [Sync and provider checkpoints](checkpoints.md#sync-and-provider-checkpoints), [Import-existing](publication.md#import-existing) |
| Provider progress | `check_changes_database` and `store_scoped_db_sync_token` in `src/sync_loop/precheck.rs`; cycle calls in `src/sync_loop/runner.rs`; `source_checkpoint_decision` in `src/sync_cycle.rs`; `commit_checkpoint_transition` in `src/state/db/checkpoints.rs` | [Checkpoint advancement](checkpoints.md#checkpoint-advancement), [Sync and provider checkpoints](checkpoints.md#sync-and-provider-checkpoints) |
| Enumeration proof | `download_photos_with_sync` in `src/download/orchestration/dispatch.rs`; full owner in `src/download/orchestration/full.rs`; incremental owner in `src/download/orchestration/incremental.rs`; `src/icloud/photos/album/enumeration.rs`, `src/icloud/photos/album/completion.rs`, and `src/icloud/photos/album/changes.rs` | [Full and incremental enumeration](checkpoints.md#full-and-incremental-enumeration) |
| File landing and state | `TaskPlanner` in `src/download/planner.rs`; `src/download/pipeline/producer.rs`, `consumer.rs`, `task.rs`, and `pass.rs`; `download_file_with_mode` in `src/download/file/transfer.rs`; `publish_part_to_final` in `src/download/file/publication.rs`; `finalize_downloaded_with_proof` in `src/download/finalize.rs`; `mark_downloaded` in `src/state/db/assets.rs` | [File landing](publication.md#file-landing), [Managed edited-primary layouts](publication.md#managed-edited-primary-layouts) |
| Durable retry | `src/download/orchestration/recovery.rs`; `build_pending_retry_download_tasks` in `src/download/retry.rs`; targeted lookup in `src/icloud/photos/album/lookup.rs`; `DownloadStateStore` pending-state methods in `src/state/db/contracts.rs` | [Durable pending retry](publication.md#durable-pending-retry) |
| Paths and import parity | `src/config/templates.rs`; `DownloadConfig::with_pass` in `src/download/orchestration/config.rs`; `src/download/paths.rs`; `expected_paths_for` in `src/download/filter/expected_paths.rs`; callers in `src/commands/import.rs`; local reconciliation in `src/download/orchestration/reconciliation.rs` | [Import-existing](publication.md#import-existing), [File landing](publication.md#file-landing) |
| Metadata refresh and writes | `Config::runtime.refresh_metadata` reads in `src/sync_loop/runner.rs` and `src/sync_cycle.rs`; capture repair in `src/download/orchestration/maintenance.rs`; immediate and queued owners in `src/download/metadata_rewrite/`; format and publication owners in `src/download/metadata/` | [Metadata](metadata.md#metadata) |
| Import adoption | `run_import_existing`, `import_assets`, and `StrictImportVerifier` in `src/commands/import.rs`; `ImportStateStore::import_adopt` and its SQLite implementation in `src/state/db/import.rs` | [Import-existing](publication.md#import-existing), [State and serialization](#state-and-serialization) |
| Service execution | dispatch in `src/lib.rs`; install, status, uninstall, and platform owners in `src/service/`; `src/service/run.rs` enters `src/sync_loop.rs::run_sync` | [Command dispatch](startup.md#command-dispatch), [Container startup](startup.md#container-startup) |

For schema, durable-key, sentinel, serialization, and shared-type changes,
follow [State and serialization](#state-and-serialization). Use
[the test guide](../../tests/README.md) to choose focused proof; a diagnostic count
or a source-only assertion does not replace production-path state-transition
proof.

## Change-impact checklist

This table chooses verification for a change. It assumes the owner is already
known; use [Change-impact traversal](#change-impact-traversal) to find the code
to read first.

| Change | Check |
|--------|-------|
| CLI command or flag | `src/cli.rs`, dispatch in `src/lib.rs`, help output, Docker, services, Homebrew, docs, CLI tests |
| TOML or runtime config | defaults, setup output, CLI precedence, hashes, persisted examples, docs |
| Selection or pass scope | list/sync/import parity, shared libraries, unknown names, unfiled scope, membership snapshots |
| Provider checkpoint | full and incremental proof, interruption, config drift, retry durability, scoped DB pre-check |
| SQLite schema/query | migration from every supported version, all readers/writers, status/report/manifest output, real SQLite tests |
| Provider record parsing | missing/malformed fields, shared zones, identity mapping, metadata capture, fixtures |
| File or path behavior | `.part`, checksum, no-overwrite publish, fsync, import compatibility, collision handling |
| Metadata writes | opt-in gate, pre-publish mutation, sidecars, retry markers, feature combinations |
| Service behavior | Linux, macOS, Windows, container defaults, status, install/uninstall renderers |
| Credentials or logging | secret wrappers, source lifetime, redaction, core-dump hardening, diagnostics, error paths |
| Machine output | JSON/CSV shape, redaction, reports, health, metrics, downstream compatibility |

## Tests

Use [the test guide](../../tests/README.md) to select a production-path test. See [CI qualification](../ci.md) for hosted and release checks.

## State and serialization

Asset capture and addition dates use fractional Unix seconds in the existing
non-STRICT SQLite INTEGER columns. Readers accept legacy whole seconds and
round fractional seconds to milliseconds without scaling the whole timestamp.
Invalid stored dates fail decoding. Metadata refresh commits these dates with
the metadata and rewrite markers; operational timestamps remain whole seconds.
This representation requires no schema migration or historical backfill.

Schema, primary-key, sentinel, durable-key, and serialization changes are
cross-cutting. Search every reader and writer, migrations, fixtures, reports,
status output, and round-trip tests before changing them.

## Rendition policy and configuration migration

`config/input.rs` requires top-level `config_version = 1` in existing files and
rejects retired rendition restrictions before startup can persist configuration
or open provider work. `config/resolve.rs` maps `[filters].exclude` to four
independent components: ordinary photos, ordinary videos, Live Photo stills and
Live Photo videos. Excluding all four is invalid. Persistence and setup emit the
same marker and exclusions.

`download/filter/versions.rs` selects available full-resolution originals,
current edits and alternatives for included components. Medium and thumbnail
resources do not replace a missing original. Live Photo still exclusion covers
original, edited and alternative stills; video exclusion covers original and
edited companions. URL deduplication prevents duplicate transfers. Shared
expected paths keep import and sync resource identities and filenames aligned.
Historical rendition keys remain readable for state and metadata recovery.
Expanded defaults intentionally change the enumeration safety hash; its old
policy encoding remains pinned for historical evidence.
