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
| `assets` | One row per `CPLAsset`: its master, the stored record, the deleted, hidden and purged flags, and for a Shared Library stand-in, the photo it links to. A mark says it needs planning. |
| `masters` | One row per `CPLMaster`: the stored record. |
| `albums` | Album ID, current name, parent, whether it is a folder, deleted, and the folder fixed at first use. |
| `memberships` | One row per album relation: album, asset, present or departed. |
| `smart_members` | Members of the queried smart folders, replaced each cycle. |
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

## Libraries and folders

**Shared Library.** A `SharedSync-*` zone uses the same feed on the private database. When a photo moves from the personal library to the Shared Library, the personal library keeps a stand-in record that links to it. The stand-in has no copy of its own; its album memberships place the linked photo. Every selected zone is observed before any is planned, so those memberships are known first. If the Shared Library is not selected, photos that live there are not saved, and each cycle warns with their count and the `[filters] libraries` setting that adds it.

**Smart folders.** Favorites, Hidden, Recently Deleted, Videos and Live are derived from stored fields, so quiet cycles stay quiet. Screenshots, Panoramas, Slo-mo, Time-lapse and Bursts are read from iCloud's index for each selected folder every cycle, because the subtype fields behind them are not verified. Hidden and recently deleted photos are saved only to their own smart folders when selected. A photo in any regular album or any selected smart folder gets no unfiled copy.

**Date range.** The fixed capture-date range filters planning. Widening it plans the newly included photos from the catalog without a new listing.

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

## Commands

Every command that reads state uses `src/sync_core/access.rs`. It resolves `<account>.sync.db`, upgrades a previous-engine file first if one exists, and refuses state bound to another account before reading or changing anything. Offline commands check the configured account; signed-in commands also check, or first record, the provider identity.

| Command | Behavior on the sync core |
|---|---|
| `status` | Saved, owed, failed and locally modified copies, copies carried over from the previous state, each library's resume point, and the last run. `--failed`, `--pending` and `--downloaded` list paths. |
| `verify` | Checks every saved copy exists; `--checksums` compares it with its recorded SHA-256. |
| `manifest` | Exports every copy row as JSON or CSV: library, photo, version, relative path, album, size, SHA-256, state and whether it is still selected. |
| `reconcile` | Owes again every saved, selected copy whose file is missing (`--dry-run` only lists them). `--inspect-path` prints a copy's fingerprint; `--confirm-damage` owes it again so the next sync replaces exactly those bytes. |
| `reset state` | Deletes the previous and current state files after checking ownership of each. |
| `reset sync-token` | Forgets every library's position, so the next sync lists each again. Saved copies stay. |
| `import-existing` | Runs one cycle that adopts local files at planned paths with the expected size and downloads nothing. `--dry-run` plans into a scratch file and reports matches without writing state. |
| `doctor` | Reports whether the state opens and its saved, owed and failed counts. |

## Upgrade

The upgrade establishes the new state, validates it, then removes the old history. Users report oversized state databases, so the upgrade must end with a compact database, not only a new schema. It is implemented in `src/sync_core/upgrade.rs` and runs before the first cycle when `<account>.sync.db` does not exist yet but the old `<account>.db` does.

1. **Verify ownership.** The old file is opened read-only and its account owner is checked against the authenticated account. A mismatch refuses the sync with the existing ownership error, and nothing is written.
2. **Build.** Create `<account>.sync.db.importing`, replacing any earlier unfinished one. Carry over each verified local copy the old state recorded (`assets`, `asset_metadata_paths` and verified `provider_active_destinations` rows): path, size, recorded SHA-256 and iCloud checksum. A copy is dropped from the import, and planned normally, when its file is missing or changed size, its path is outside the download directory, or another copy already claimed its path. Nothing else is carried over: no inbox, catalog, generations, journals or legacy evidence. The old file is never written.
3. **Validate and switch.** Check the new file's integrity, fold its write-ahead log into it, and rename it to `<account>.sync.db`. The old file stays where it was as the rollback point.
4. **First cycle.** Run a full listing. When the planner creates a copy, it takes over a carried-over copy of the same photo and rendition at the planned path, or in the same folder under an older name, without hashing or downloading. If iCloud now serves a different version, the copy is replaced only while it still matches its recorded hash. Files nothing takes over stay on disk.
5. **Clean up.** After a cycle in which every zone finished its listing without an error, delete the old file and its `-wal`, `-shm` and `-journal` companions, and the `.kei-schema-backup-*` bundles whose manifest names that file. Bundles for other databases, or with unexpected contents, stay. The freed bytes are logged. Until then, the previous release can still use the old file. After cleanup it cannot.

Because the new state lives in a fresh file, no `VACUUM` of the old database is needed, and the upgrade never needs more free space than the old file plus the new, smaller one.

## Budget

The sync core (state, cycle, plan, download driver) stays under 15,000 lines including its tests. One sync path, nine tables.

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
- **Session expiry.** A session that expires mid-run is not re-authenticated yet; the next cycle fails until restart.
- **Old-engine behavioral tests.** Seven `account_ownership` tests drive `kei sync` and assert previous-engine state (provider inbox, catalog, rank pages, sparse identity, legacy metadata keys and retired checkpoint tables). They go with that engine.
- **Strict import.** `import-existing --strict`, which checked a cloud prefix before adopting a same-name, same-size file, is refused by this build. `--recent` is retired.
