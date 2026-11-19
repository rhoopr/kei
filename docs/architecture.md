# Architecture

kei syncs iCloud Photos media and metadata to local storage. Use this map to find the owner of a decision and the conditions that protect user data. Read that module and its direct callers before changing behavior.

## Owners

| Decision | Start here | Boundary |
|----------|------------|----------|
| Which command runs, with which settings and local state? | `src/lib.rs`, `src/config/`, `src/service/` | Dispatch selects the command. Config resolves policy and paths. Services enter the same sync runner as the CLI. |
| Which collections are in scope? | `src/selection.rs`, `src/commands/service.rs` | Selection parses selectors; library and pass planning resolves them. Sync and import share the plan. |
| What did iCloud report? | `src/icloud/photos/` | The adapter validates CloudKit records, identity, and enumeration evidence. It cannot authorize local replacement or advance a checkpoint. |
| What should run next? | `src/sync_loop/`, `src/sync_cycle.rs` | The runner coordinates watch and recovery. The cycle owns per-zone checkpoint decisions. A database pre-check can avoid a poll but cannot certify zone completion. |
| What work belongs to this destination? | `src/download/orchestration/`, `src/state/db/provider_generations.rs` | Current selection and destination evidence govern work admission. Paths render that decision; they do not decide eligibility. |
| Can this file be published or adopted? | `src/download/file/`, `src/download/pipeline/`, `src/commands/import.rs` | Transfer and import establish local evidence. State finalization records the outcome. |
| Can metadata change? | `src/download/metadata_rewrite/`, `src/download/metadata/` | Rewrite policy selects authorized fields. Format writers must preserve concurrent edits. |
| What survives interruption? | `src/state/schema.rs`, `src/state/db/` | Schema and transactional stores own durable transitions. Provider parsing stays in the adapter; filesystem mutation stays with the file owner. |
| What can an operator observe? | `src/commands/status.rs`, `src/report.rs`, `src/support/` | Reports project execution evidence. Counters and diagnostic labels cannot grant checkpoint or publication authority. |

For a specific component, see the [module index](architecture/module-index.md). For a change that crosses layers, see [search anchors, affected consumers, and verification](architecture/change-impact.md).

## Main flows

**Startup.** Dispatch resolves configuration before opening account-owned state. `src/config/paths.rs` discovers one application root so a new XDG setting cannot silently split existing credentials and state. Ambiguous roots fail closed. Linux service installation records the resolved config path and path overrides. The writable state opener owns post-upgrade inbox compaction and retries. See [startup, account ownership, and schema backups](architecture/startup.md).

**Sync.** The runner resolves the session, libraries, and pass plans. Each cycle dispatches work per library, evaluates source progress per zone, and reports the result to watch control. A cursor records where to resume; the retained inbox holds source observations; destination receipts record local work. None substitutes for the others. See [provider capture and destination replay](architecture/provider-replay.md) and [checkpoint decisions and enumeration](architecture/checkpoints.md).

**Local delivery.** The planner selects a destination. The pipeline writes and verifies a `.part`, publishes the file, syncs the parent directory, and finalizes durable state. Import uses the shared path rules and verifies an existing file before adoption. Unresolved work needs durable retry evidence. See [publication, reconciliation, and import](architecture/publication.md).

**Metadata.** An enabled setting authorizes later updates to its managed fields. Omitted version-1 settings and no-config runs keep writers disabled. Capture-timestamp replacement requires explicit repair. Immediate and queued writes share the stable-input and ownership requirements. See [metadata write boundaries](architecture/metadata.md).

## Data-safety invariants

Keep the previous checkpoint on incomplete token proof, interruption, stale planning, or non-durable recovery. Unknown provider identity keeps work pending. A verified local copy does not prove that the provider deleted its source.

File publication must preserve existing media unless replacement has exact authorization. Metadata writers must verify their input before replacement. A failed write leaves enough evidence to retry without overwriting a concurrent edit.

Keep secrets and provider identifiers out of logs and reports. See [diagnostic evidence](architecture/support.md) for redaction, offline collection, and interpretation limits.

## Safety contract catalog

Stable IDs connect safety rules to production owners and focused tests.
`scripts/check-contracts` rejects missing links in that chain.

| Contract | Owner | Required behavior |
|----------|-------|-------------------|
| `FILE_PUBLISH_NO_OVERWRITE` | `src/download/file.rs`, `src/download/pipeline.rs` | Replacement requires exact path and fingerprint authorization from explicit fingerprint-confirmed or truncated repair, the per-copy local replacement policy, or a journaled managed-layout handover with independent preservation evidence. Timestamp-based authorization also guards the observed local mtime through publication. A no-replace collision succeeds only when the verified `.part` and destination bytes are identical. Different or unverifiable bytes retain retry evidence and cannot reach metadata writes or downloaded finalization. |
| `TEMP_FILE_DELETE_REQUIRES_DURABLE_OWNERSHIP` | `src/download/orchestration/cleanup.rs`, `src/download/pipeline.rs`, `src/fs_util.rs`, `src/state/db.rs` | Orphan cleanup deletes only an exact stale path claimed in durable state. It retains verified filesystem handles through removal and never follows a directory or file symlink. Normal completion and graceful interruption retire the claim. |
| `SYNC_TOKEN_ADVANCE_REQUIRES_CLEAN_CYCLE` | `src/sync_cycle.rs` | The database pre-check token advances only after a successful non-dry-run cycle with a current pass plan. |
| `SOURCE_CHECKPOINT_REQUIRES_DURABLE_RECOVERY` | `src/sync_cycle.rs`, `src/download/orchestration/` | A zone checkpoint advances only with complete token evidence and durable recovery for unfinished work. |
| `MALFORMED_REQUIRED_ASSET_FIELDS_BLOCK_CHECKPOINT` | `src/icloud/photos/asset.rs`, `src/icloud/photos/album/fetch.rs`, `src/icloud/photos/album/lookup.rs`, `src/download/orchestration/incremental.rs`, `src/sync_cycle.rs` | A live asset with a missing or invalid required identity or capture date blocks its zone checkpoint before filtering or path planning. |
| `UNKNOWN_PROVIDER_IDENTITY_REMAINS_PENDING` | `src/download/retry.rs` | Inconclusive provider identity retains the pending row and records verification evidence. |
| `POLICY_EXCLUDED_REQUIRES_EXPLICIT_SOURCE_DELETION` | `src/download/retry.rs`, `src/state/db.rs` | Policy-excluded rows become source-deleted only after targeted provider deletion evidence. Present or inconclusive responses retain them outside actionable pending work. |
| `METADATA_WRITES_REQUIRE_OPT_IN` | `src/download/metadata_rewrite.rs` | Media and sidecar metadata writes run only for explicitly enabled metadata flags. |
| `METADATA_EMBED_REWRITE_REQUIRES_STABLE_INPUT` | `src/download/metadata.rs`, `src/download/file.rs`, `src/download/metadata_rewrite.rs` | Every embedded metadata rewrite prepares a uniquely owned sibling and replaces the media only while both the destination and prepared bytes match their approved fingerprints. Failure preserves concurrent edits and durable retry evidence. |
| `HEIF_EMBED_REWRITE_REQUIRES_STABLE_INPUT` | `src/download/heif.rs`, `src/download/metadata.rs`, `src/download/file.rs`, `src/download/metadata_rewrite.rs` | A HEIF-family embedded rewrite accepts tone-map insertion only when `dimg` and primary Exif `cdsc` relationships prove the exact target and no existing XMP owns that tone map, prepares a uniquely owned sibling, and replaces the media only while both the destination and prepared bytes match their approved fingerprints. Failure preserves concurrent edits and durable retry evidence. |
| `XMP_SIDECAR_REWRITE_REQUIRES_STABLE_INPUT` | `src/download/metadata.rs`, `src/download/metadata_rewrite.rs` | An existing XMP sidecar is replaced only when it parses and its bytes still match the writer's initial read. Failure preserves the sidecar and durable rewrite marker. |
| `XMP_GPS_ACCURACY_REQUIRES_MATCHING_LOCATION` | `src/download/metadata.rs`, `src/download/metadata_rewrite.rs`, `src/state/db.rs` | Native horizontal accuracy requires valid native latitude and longitude equal to the exported provider coordinates, plus a matching verified-download source checksum. Recovered local or download checksums cannot establish native provenance. Readable missing, invalid, or mismatched evidence omits accuracy and clears only obsolete kei-owned values. Source I/O failure preserves unknown fields and durable retry evidence. |
| `METADATA_CAPTURE_REVISION_REPAIR_IS_DURABLE` | `src/download/orchestration/maintenance.rs`, `src/state/db.rs` | Revision repair updates catalogue metadata and configured rewrite evidence before promotion, stays library-scoped, and preserves the provider checkpoint on unresolved work. |

## Maintaining this guide

Update the existing owner, flow, or invariant when its meaning changes. Keep the contract IDs in this file: `scripts/check-contracts` checks them against production owners and tests. Put subsystem mechanics in the linked reference, operator procedures in the relevant guide, and release history in the release notes. If only an implementation detail changes, update its reference where needed.
