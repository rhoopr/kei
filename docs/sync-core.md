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

The state lives in `<account>.sync.db` next to the account's old state file.

| Table | Holds |
|---|---|
| `zones` | One row per library zone: sync position, and whether a full listing has finished since the last reset. |
| `assets` | One row per `CPLAsset`: its master, the stored record, and the deleted, hidden and purged flags. A mark says it needs planning. |
| `masters` | One row per `CPLMaster`: the stored record. |
| `albums` | Album ID, current name, parent, whether it is a folder, deleted, and the folder fixed at first use. |
| `memberships` | One row per album relation: album, asset, present or departed. |
| `files` | One row per local copy: asset, rendition, album or unfiled, relative path, iCloud checksum and size, recorded SHA-256, state (pending, adopt, done, failed, modified), whether it is still wanted, attempts and last error. |
| `runs` | One row per cycle for status and reports. |
| `kv` | Small settings: the bound account and the planning config fingerprint. |

Stored records keep only the fields kei reads. Download URLs and wrapping keys are dropped; they expire, and the download step fetches current records instead.

Existing account-owner, temp-file ownership and migration-backup behavior is kept.

## Cycle

1. **Observe.** Read `changes/zone` pages from the saved position. Without one, `changes/zone` enumerates the whole zone, so a full listing uses the same path. When iCloud rejects the saved position, the zone forgets it and lists again in the same cycle. Each page is parsed in `src/icloud/photos/feed.rs`, then its records and the position after it are committed together. A page that fails validation stops that zone's observation and leaves the old position; owed downloads still run.
2. **Plan.** Once a zone has finished a full listing, compute the wanted copies of each marked asset from `assets`, `memberships`, the selection and the date range, and upsert `files`. A path is chosen once, with a stable collision suffix, and stored. A file already at a new copy's path with the expected size is adopted after hashing; anything else there is left alone. An asset whose master or album is not known yet stays marked. A config change marks every asset; it never forgets the sync position.
3. **Download.** Drain owed `files` in batches. Each batch looks up its current records for download URLs, then downloads with bounded concurrency through the existing publication path: `.part`, size and content checks, no-overwrite publish, parent-directory fsync, then the SHA-256 is recorded. A URL iCloud refuses gets one fresh lookup in the same cycle. Failures stay owed.
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

The upgrade establishes the new state, validates it, then removes the old history. Users report oversized state databases, so the upgrade must end with a compact database, not only a new schema.

1. **Build.** Create a new database file next to the old one. Read the old database (schema 49 or older) read-only and import, for each verified local copy, its asset, rendition, path, SHA-256 and size into `files` as done, plus account ownership and the config hash. Nothing else is carried over: no inbox, catalog, generations, journals or legacy evidence. The old file is not modified.
2. **Validate.** Before switching, check the new database: integrity check passes, the account owner matches, every imported copy has a unique path, the imported count matches the old database's verified copies, and each imported file exists with its recorded size. Files are not re-hashed; their recorded SHA-256 is trusted. A failed check discards the new file, keeps the old one in use and reports the reason.
3. **Switch.** Atomically rename the new file into place and keep the old one under a backup name. This is the rollback point.
4. **First cycle.** Run a full listing. Imported files are re-adopted by path and size, and anything missing is downloaded. Files the import could not match are left untouched and may coexist with new downloads.
5. **Clean up.** After the first cycle completes its full listing, delete the old database file and its WAL and backups, and report the space reclaimed. Until then, downgrading to the previous release works by restoring the backup. After cleanup it does not.

Because the new state lives in a fresh file, no `VACUUM` of the old database is needed, and the upgrade never needs more free space than the old file plus the new, smaller one.

## Budget

The sync core (state, cycle, plan, download driver) stays under 15,000 lines including its tests. One sync path, eight tables.

## Delivery

1. This design.
2. New state and cycle for the primary library: unfiled and albums, full and incremental, restart after a kill, a rejected sync position, an expired URL.
3. Shared libraries, smart folders, date range.
4. Upgrade from schema 49: build, validate, switch, first cycle and history cleanup, plus adoption of existing files.
5. Metadata pass.
6. Delete the old engine and its tests, and update `docs/architecture.md`.

Acceptance tests in `src/sync_core/tests.rs` drive whole cycles against a fake zone feed and a local HTTP server, reopen the state file between runs, and check files on disk, catalog rows and quiet later cycles. Process-level kill tests are still to come.

## Open items

- **Smart folders.** Derive each from asset flags where the fields allow it (Favorites from `isFavorite`, Videos from media type). Any folder that can't be derived is re-queried on its own each cycle. Each one needs checking.
- **Live verification.** A run against a real library needs the owner's machine and credentials.
- **Previews.** `--dry-run` and `--only-print-filenames` still use the previous engine. They move to the core before that engine is deleted.
- **Reporting.** Health files, metrics, notifications and the JSON report are not yet fed by the core; it logs a per-library summary and backup status.
- **Missing files.** Normal sync does not yet re-check that done copies still exist.
