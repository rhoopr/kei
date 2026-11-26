# v0.30 sync core

This is the design for the v0.30 sync engine. It replaces the selection-generation, replay and checkpoint-debt engine with one loop over a small local catalog. It supersedes `implementation_plan.md` for engine work.

## Product decisions

These were decided on 2026-11-26 and are not reopened by implementation work.

1. **Paths are stable.** An album folder keeps the name it was created with, keyed by album ID. An iCloud rename never moves files.
2. **Sync never deletes.** A copy in an album the photo has left stays on disk and stops receiving updates. The catalog records the departure so status can report stale copies. Pruning, if added, is a separate explicit command.
3. **Replace only untouched files.** When iCloud publishes a new version of a rendition, kei replaces the local file only if it still matches the SHA-256 kei recorded when it wrote it. A file that differs is kept and reported as locally modified. No timestamp comparison.
4. **Metadata writing is opt-in and separate.** Off by default. When enabled, a pass after downloads writes metadata to kei-owned files from the catalog. It never affects the sync position or download completion.

## Engine rule

A zone's sync position advances in the same SQLite transaction that stores the records it covers. It does not wait for downloads. Unfinished downloads are pending rows in `files` and retry on later cycles. Backup completeness is a query over `files`, reported separately from the sync position.

## State

| Table | Holds |
|---|---|
| `zones` | One row per library zone: sync position, last full listing time, indexing state. |
| `assets` | One row per photo (CPLAsset with its CPLMaster): identity, capture date, flags (favorite, hidden, deleted, media type), and per-rendition resource fields (checksum, size, filename). |
| `albums` | Album ID, current name, parent, folder name fixed at first use. |
| `memberships` | Album ID, asset ID, present or departed. |
| `files` | One row per wanted local copy: asset, rendition, album or unfiled, relative path, recorded SHA-256 and size, state (pending, done, failed, locally modified), attempts, last error, and metadata-written marker. |
| `runs` | One row per cycle for status and reports. |
| `kv` | Small settings such as the account owner and the config hash. |

Existing account-owner, temp-file ownership and migration-backup behavior is kept.

## Cycle

1. **Observe.** With a sync position, read `changes/zone` pages. Without one, or when iCloud rejects it, run a full listing. Each page is parsed in `src/icloud/`, then its records and the new sync position are committed together. A page that fails validation stops that zone's cycle and leaves the old position.
2. **Plan.** Compute the wanted copies from `assets`, `memberships`, the selection and the date range, and upsert `files`. A path is chosen once, with a stable collision suffix, and stored. A config change only changes this step; it never purges the sync position.
3. **Download.** Drain pending `files` with bounded concurrency through the existing publication path: `.part`, SHA-256 verification, no-overwrite publish, parent-directory fsync, then mark done. Expired download URLs are refreshed by record lookup. Failures stay pending.
4. **Metadata.** If enabled, write metadata for done files whose metadata marker is stale.
5. **Report.** Record the run, completeness and failures.

A missing master, an unknown identity or an absent record keeps the row pending. Absence from a listing never deletes anything, on disk or in the catalog.

## Invariants

- Never overwrite a file kei did not write, or one that no longer matches its recorded SHA-256.
- Publish only verified bytes, sync the parent directory, then record the outcome.
- Advance a sync position only with the records it covers.
- Keep every unfinished copy as a pending row until it completes.
- Never delete local files during sync.

## Kept

Auth and session, CloudKit transport, record parsing and validation in `src/icloud/`, file publication in `src/download/file/` and `src/fs_util.rs`, path rendering, import and adoption, metadata writers, config, CLI, service and watch mode, health, reports and support export.

## Removed

The provider inbox and catalog, selection generations and decisions, rank pages, replay and accepted-replay coverage, checkpoint holds and bridges, provider work obligations, sparse identity tracking (replaced by pending rows), legacy trust and preservation, layout journals and album-directory moves, and the schema migrations they need.

## Upgrade

On first run, v0.30 reads a schema-49 or older database once and imports, for each verified local copy, its asset, rendition, path, SHA-256 and size into `files` as done. The old database is kept as a backup. The first cycle runs a full listing, which re-adopts imported files by path and checksum and downloads anything missing. Files the import cannot match are left untouched and may coexist with new downloads.

## Budget

The sync core (state, cycle, plan, download driver) stays under 15,000 lines including its tests. One sync path, seven tables.

## Delivery

1. This design.
2. New state and cycle for the primary library: unfiled and albums, full and incremental, restart after a kill, a rejected sync position, an expired URL.
3. Shared libraries, smart folders, date range.
4. Import from schema 49, and adoption of existing files.
5. Metadata pass.
6. Delete the old engine and its tests, and update `docs/architecture.md`.

Acceptance tests drive the real binary against a fake CloudKit server and check files on disk, catalog rows and a quiet second run.

## Open items

- **Smart folders.** Derive each from asset flags where the fields allow it (Favorites from `isFavorite`, Videos from media type). Any folder that can't be derived is re-queried on its own each cycle. Each one needs checking.
- **Live verification.** A run against a real library needs the owner's machine and credentials.
