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

**Startup.** Dispatch resolves configuration before opening account-owned state. `src/config/paths.rs` discovers one application root so a new XDG setting cannot silently split existing credentials and state. Ambiguous roots fail closed. Linux service installation records the resolved config path and path overrides. The writable state opener verifies schema upgrades and retires their temporary backups before post-upgrade inbox compaction. See [startup, account ownership, and schema backups](architecture/startup.md).

**Offline inspection.** Doctor distinguishes a loaded config, an absent file, and a load error. Its password-file check tests read access without reading the secret or invoking a password command. Status describes recorded sync work; verify compares local bytes only when requested and a checksum baseline exists.

**Sync.** The runner resolves the session, libraries, and pass plans. Each cycle dispatches work per library, evaluates source progress per zone, and reports the result to watch control. A cursor records where to resume; the retained inbox holds source observations; destination receipts record local work. None substitutes for the others. See [provider capture and destination replay](architecture/provider-replay.md) and [checkpoint decisions and enumeration](architecture/checkpoints.md).

A sparse change in PrimarySync can depend on a scoped Shared scan from the same cycle. If a Hidden-only scan has no decision for the exact Shared asset, its current `isHidden` field can establish membership. Every selected destination still requires a verified local copy before the source checkpoint advances.

When an observation names both the child and its master, kei requests both records together from the bound media zone. During unfiled inventory it can confirm up to 50 first-time observations per request and fetch the next batch while it processes the current one. In enumeration order, kei checks each pair's identities, relationship and selected facts before acting on that asset. If enumeration or a lookup reports an expired session, read-ahead holds the checkpoint immediately and stops new confirmation requests. A batch reply does not advance the checkpoint. If only the child is known, kei first looks up its current master.

For each asset, kei reads the retry deadline and any retained selection decision in one SQLite transaction. Repair and destination planning use that decision's frozen facts. The transaction that admits a destination still checks current ownership, and a repair still needs current replacement authority.

Replay scheduling looks for due work, then validates the decisions in the selected window before advancing its replay cursor. An empty search does not prove checkpoint readiness: the checkpoint debt check still validates the scope. The download producer loads path reservations and receipts on its first asset, so empty album streams can finish without building a path planner.

A standalone full inventory that establishes a zone's baseline checks work from the current selection, not older plans for that selection. Its generation and zone cursor commit together. Retained path ownership and prepared publication still protect local files. Incremental refreshes and inventories that require a retained-cursor bridge keep their existing coverage checks.

If a continuation is refused, Kei can still save newly verified media from accepted pages in a standalone unfiled pass or in eligible named albums within a mixed selection. Each named album needs fresh proof of its own link. Neither publication nor membership proof advances the held cursor.

After indexing finishes, a quiet zone change feed can establish coverage across disagreeing album query tokens. Qualification retains the trusted cursor for replay. See [checkpoint decisions and enumeration](architecture/checkpoints.md).

**Local delivery.** The planner selects a destination. The pipeline writes and verifies a `.part`, publishes the file, syncs the parent directory, and finalizes durable state. An expired download URL leaves only that transfer for cleanup; other transfers continue. Import uses the shared path rules and verifies an existing file before adoption. Unresolved work needs durable retry evidence. See [publication, reconciliation, and import](architecture/publication.md).

**Metadata.** An enabled setting authorizes later updates to its managed fields. Omitted version-1 settings and no-config runs keep writers disabled. Capture-timestamp replacement requires explicit repair. Immediate and queued writes share the stable-input and ownership requirements. See [metadata write boundaries](architecture/metadata.md).

Ordinary sync commits eligible provider checkpoints before draining the local metadata queue. A verified destination with a recoverable writer receipt can still owe metadata after its zone is synced. Only the writer can settle that receipt. Missing provider evidence or failure to save the obligation still holds the checkpoint.

## Data-safety invariants

Keep publication, current work and checkpoint decisions separate. A hold on the checkpoint does not stop independently verified downloads. An item failure keeps that item retryable; it does not stop unrelated work unless the failure makes that work unsafe too.

Keep the previous checkpoint on incomplete token proof, interruption, stale planning, or non-durable recovery. A validated full inventory can replace older planning for the selection it covers. Historical uncertainty alone does not require repair before current progress. Unknown **current** provider identity keeps work pending. A missing observation or verified local copy does not prove that the provider deleted its source.

Unknown indexing readiness before enumeration defers the affected library after two short retries. An unknown closing probe holds its checkpoint immediately. Independent libraries can advance their own checkpoints. Watch mode retries incomplete discovery before publishing a library map. See [checkpoint decisions and enumeration](architecture/checkpoints.md).

Selection evidence limits apply to individual writes and validation operations. Retained history does not consume the next library's or restarted baseline's write allowance.

Once selection freezes a destination, a downloaded receipt at a different path cannot satisfy it. With stable source evidence, a missing selected copy stays due until kei publishes it at the promised path or records proof that resolves that destination.

When selection hashes an existing file to check a catalog receipt, it retains the confined handle. Before committing reuse or local preservation, kei checks that the path still names the same file and the bytes have not changed. On Unix, a settled inode change time can establish that the original digest still applies; otherwise kei verifies the bytes again. If projection redirects the destination, kei checks the selected file instead. The digest does not authorize a later write or carry across a restart.

Writable sync may retire raw enumeration responses only after verifying the selection seal and finding no unfinished identity, destination, metadata, or repair work. Compact rank proofs preserve the original seal and source links.

A new selection filters previous metadata writers by scope, configuration and destination inputs before reading files. It hashes the installed media and any configured XMP packet before opening the SQLite write transaction. Reuse requires matching hashes, frozen grouping and effective capture offset. At commit, kei checks both retained file identities. On Unix it also checks the inode change times sampled before hashing. On Windows the retained handles exclude writes and replacement. If kei cannot verify either file, the destination and its aliases stay pending for the ordinary writer. Reuse does not confer publication ownership. Prepared writes, preserved local files, managed layouts and explicit refresh or repair keep their existing recovery paths.

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
| `SOURCE_CHECKPOINT_REQUIRES_DURABLE_RECOVERY` | `src/sync_cycle.rs`, `src/download/orchestration/` | A zone checkpoint needs complete current source coverage and durable recovery for unfinished current work. A validated baseline supersedes obsolete plans, not physical recovery records. A lookup that can retry from its retained child/master pair may advance with the zone; it still blocks one-shot success and the database pre-check. |
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
