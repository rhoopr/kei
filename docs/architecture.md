# Architecture

kei syncs iCloud Photos media and metadata to local storage. Use this map to find the owner of a decision and the conditions that protect user data. Read that module and its direct callers before changing behavior. The sync engine's design and its product decisions are in [the sync core](sync-core.md).

## Owners

| Decision | Start here | Boundary |
|----------|------------|----------|
| Which command runs, with which settings and local state? | `src/lib.rs`, `src/config/`, `src/service/` | Dispatch selects the command. Config resolves policy and paths. Services enter the same sync runner as the CLI. |
| Which session and libraries does a sync use? | `src/sync_loop/`, `src/commands/service.rs` | Startup signs in, resolves libraries and hands them to the core runner. Re-authentication after an expired session lives here too. |
| Which collections are in scope? | `src/selection.rs`, `src/sync_core/plan.rs` | Selection parses selectors; planning applies them to the local catalog. Sync and import share the plan. |
| What did iCloud report? | `src/icloud/photos/feed.rs`, `src/icloud/photos/asset.rs` | The adapter turns `changes/zone` pages and record lookups into observations. It cannot authorize local replacement. |
| What survives interruption? | `src/sync_core/state.rs` | One SQLite file per account. A zone's position commits with the records it covers; owed downloads are rows that retry on later cycles. |
| Which local copies should exist, and where? | `src/sync_core/plan.rs`, `src/download/filter/` | Planning derives paths with the shared path rules. Chosen paths are stable and never move; with `edited_as_primary` the plain filename's contents follow edits and reverts. |
| Can this file be published, adopted or replaced? | `src/sync_core/drain.rs`, `src/download/core_io.rs`, `src/download/file/` | Downloads are verified before publication. Replacement needs the bytes kei recorded. Import adopts only verified matches. |
| How does a previous release's state become this one? | `src/sync_core/upgrade.rs`, `src/sync_core/access.rs`, `src/state/db/account.rs` | The old file is read without changes, verified copies are carried over, and the old file is removed only after a complete cycle. |
| Can metadata change? | `src/sync_core/metadata.rs`, `src/download/metadata_pass/`, `src/download/metadata/` | The pass runs only when a writer is enabled, on copies that still hold kei's recorded bytes. Writers preserve concurrent edits. |
| What can an operator observe? | `src/commands/status.rs`, `src/report.rs`, `src/metrics.rs`, `src/support/` | Reports project recorded state. Counters and diagnostic labels cannot grant publication authority. |

## Main flows

**Startup.** Dispatch resolves configuration before opening account-owned state. `src/config/paths.rs` discovers one application root so a new XDG setting cannot silently split existing credentials and state. Ambiguous roots fail closed. See [startup and account ownership](architecture/startup.md).

**Offline inspection.** Doctor distinguishes a loaded config, an absent file, and a load error. Status reports recorded copies and the last run; verify compares local bytes with recorded checksums only when asked.

**Sync.** Each cycle observes every selected zone, then plans, then downloads. Observation reads `changes/zone` from the saved position; without one it lists the zone from the start. A rejected position is forgotten and the zone is listed again in the same cycle. A malformed record is skipped and counted. While iCloud is still indexing, the zone is listed anyway, the queried smart folders wait, and the zone is listed once more after indexing finishes. Planning starts only when every selected zone has finished a full listing, because a photo in one library can be placed by albums in another. Downloads look up current records in batches for fresh URLs, looking up the next batch while earlier downloads run. An expired URL is refreshed once in the same cycle. When iCloud refuses the session, the runner signs in again and repeats the cycle from the saved positions.

**Local delivery.** A download is written beside its destination, verified against iCloud's size and checksum, given its capture time, published without replacing anything, and its parent directory is synced before the row is marked done. A file already at the planned path with the expected size is adopted after hashing. A new iCloud version replaces a copy only while it still has the bytes kei recorded; otherwise the copy is kept and reported as locally modified.

**Upgrade.** A previous release's state file is opened read-only and its ownership checked. Verified copies inside the download directory are imported into a scratch file, which is integrity-checked and renamed into place. Planning takes over a carried-over copy of the same photo and rendition at the same path or in the same folder when its size matches; a path the old state gave to several photos goes to the one whose recorded bytes the file holds. After the first cycle in which every zone listed to the end without error, the old file, its SQLite companions and its schema backups are removed.

**Metadata.** Metadata writing is opt-in. After downloads, the pass updates saved copies whose metadata may be stale, but only while they still hold the bytes kei recorded. An embedded write records its output as kei's bytes, with a receipt written first so a restart can tell it from a local edit. Movie layout validation accepts both FullBox and bare QuickTime `meta` atoms while rejecting malformed bounds; the patched XMP Toolkit is pinned in `Cargo.toml`. It never affects a sync position or download completion. See [metadata write boundaries](architecture/metadata.md).

## Data-safety invariants

Sync never deletes local files. A copy whose photo left an album, or whose photo was deleted in iCloud, stays on disk; its row stops being wanted.

A zone's position advances only in the transaction that records the page it covers. Owed work is a row that survives restarts, so a position can move ahead of downloads without losing them.

A photo without its master record stays dirty and is planned when the master arrives. Absence from a page is not deletion.

File publication preserves existing media unless replacement is authorized by the recorded SHA-256. A no-replace collision succeeds only when the verified download and the existing file are identical.

The previous release's state is never modified. It is removed only after the new state was integrity-checked and a cycle listed every zone completely.

Keep secrets and provider identifiers out of logs and reports. See [diagnostic evidence](architecture/support.md) for redaction, offline collection, and interpretation limits.

## Safety contract catalog

Stable IDs connect safety rules to production owners and focused tests.
`scripts/check-contracts` rejects missing links in that chain.

| Contract | Owner | Required behavior |
|----------|-------|-------------------|
| `FILE_PUBLISH_NO_OVERWRITE` | `src/download/file/publication.rs`, `src/download/core_io.rs` | Publication never replaces an existing file unless replacement is authorized by exact recorded bytes. A no-replace collision succeeds only when the verified download and destination bytes are identical. |
| `REPLACE_REQUIRES_RECORDED_BYTES` | `src/sync_core/drain.rs` | A new iCloud version replaces a local copy only while the copy hashes to the SHA-256 kei recorded. A changed copy is kept and marked locally modified. |
| `POSITION_COMMITS_WITH_OBSERVATIONS` | `src/sync_core/state.rs` | A zone's sync position is written in the same transaction as the records of the page it covers. |
| `SYNC_NEVER_DELETES` | `src/sync_core/state.rs` | Replanning marks departed copies as no longer wanted; it never removes their rows or files. |
| `UNKNOWN_PROVIDER_IDENTITY_REMAINS_PENDING` | `src/sync_core/plan.rs` | A photo whose master record is unknown stays pending and is planned once the master arrives. |
| `UPGRADE_CLEANUP_REQUIRES_VALIDATED_STATE` | `src/sync_core/upgrade.rs` | The previous state file and its backups are removed only after the new state was integrity-checked and a cycle listed every zone without error. |
| `TEMP_FILE_DELETE_REQUIRES_DURABLE_OWNERSHIP` | `src/download/file/replacement_recovery.rs` | A prepared file is removed only while it is the exact inode kei created and still holds the bytes kei wrote; its folder is synced afterwards. |
| `METADATA_WRITES_REQUIRE_OPT_IN` | `src/download/metadata_pass/mod.rs` | The metadata pass runs only when a writer is enabled. Enrichment choices such as album names do not enable one. |
| `METADATA_EMBED_REWRITE_REQUIRES_STABLE_INPUT` | `src/download/metadata/prepared.rs` | Every embedded metadata rewrite prepares a uniquely owned sibling and replaces the media only while both the destination and prepared bytes match their approved fingerprints. |
| `HEIF_EMBED_REWRITE_REQUIRES_STABLE_INPUT` | `src/download/metadata/heif_writer.rs` | A HEIF-family embedded rewrite accepts tone-map insertion only when `dimg` and primary Exif `cdsc` relationships prove the exact target and no existing XMP owns that tone map, and replaces the media only while both files match their approved fingerprints. |
| `MOVIE_EMBED_REWRITE_PRESERVES_MEDIA` | `src/download/metadata/movie_layout.rs` | An XMP Toolkit movie rewrite is published only when every box outside the XMP packet, padding and the toolkit's `Cr8r` and `cprt` user data keeps its bytes, and every top-level box its offset. Any other change leaves the original and is recorded as a lasting refusal. |
| `XMP_GPS_ACCURACY_REQUIRES_MATCHING_LOCATION` | `src/download/metadata_pass/mod.rs` | A sidecar carries native horizontal accuracy only when the file's coordinates equal iCloud's and the file still holds the bytes kei downloaded. |
| `XMP_SIDECAR_REWRITE_REQUIRES_STABLE_INPUT` | `src/download/metadata/sidecar.rs` | An existing XMP sidecar is replaced only when it parses and its bytes still match the writer's initial read. |

## Maintaining this guide

Update the existing owner, flow, or invariant when its meaning changes. Keep the contract IDs in this file: `scripts/check-contracts` checks them against production owners and tests. Put engine mechanics in [the sync core](sync-core.md), operator procedures in the relevant guide, and release history in the release notes.
