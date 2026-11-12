# v0.30 sync simplification implementation plan

## Goal and starting point

Drastically simplify Kei's sync engine: delete execution paths, remove redundant state and configuration choices, and stop running historical compatibility and repair machinery inside every sync. Preserve the user behavior agreed below. A smaller diff is useful, but fewer independent policies, durable transitions, and recovery paths are the primary measures.

- Start: `3cdd14f31d826fe77c18519910806bbe51fea8e9` from `origin/main`.
- Delivery branch: `work/v0.30.0-sync-simplify`.
- Worktree: `/home/robhooper/dev/kei-sync-simplify`. This planning session created it for the rewrite; the next task owner assumes its lifecycle. It contains source and this plan, not private provider evidence. Retain it while delivery is active.
- Preserve the old beta branch, its dirty checkout at `/home/robhooper/dev/kei`, and its private qualification evidence. They are reference material, not a base to transplant.
- This document records requirements and proposed delivery order. It does not claim that main is qualified, the beta fixes work, or any implementation has been completed.

## Rules that prevent another overbuilt rewrite

1. Each production slice must name the old code, options, state writers, or transitions it removes. A feature-only slice must be explicitly identified and justified by an agreed requirement.
2. Replace one responsibility in its current owner and remove the superseded route in the same slice. Do not build a second general-purpose sync engine beside the first.
3. No new shadow ledger, proof framework, generic workflow engine, or compatibility dispatcher without a concrete reason the existing owner cannot carry the required fact. Prefer deleting duplicate sources of truth.
4. Normal sync consumes current state. Version-specific interpretation belongs at the upgrade boundary, with one-way migration and preserved evidence, not branches spread through planning and publication.
5. Keep filesystem safety primitives. Removing checks is not simplification when it permits wrong-owner writes, lost retry work, or unjustified checkpoint advancement.
6. Preserve actual user scenarios in tests. Remove tests only when their behavior is explicitly retired or equivalent coverage replaces them. Never make a failing scenario disappear by weakening fixtures or assertions.
7. Keep each slice independently reviewable and runnable. Use short-lived child branches or atomic commits as agreed; do not accumulate all phases into another giant PR.
8. An intermediate release must never silently lose previously supported behavior. If a replacement needs several preparatory slices, keep those bounded and name the final deletion slice before starting them. Temporary code needs an owner and removal condition.
9. Do not add switches for every decision below. Defaults, simple exclusions, and explicit operations should replace overlapping modes.
10. Errors should explain how to resolve the problem: name the affected setting or operation and give a supported configuration edit or command. Apply this to changed diagnostics in each slice. Never recommend a repair flag that does not exist; broader diagnostic cleanup is separate work.

## Agreed product contract

### Source, media, and selection

- One-way sync from iCloud. Never upload local changes.
- Download full-resolution originals and the edited versions iCloud exposes. Keep one edited version if iCloud keeps one, replacing it safely when it changes; do not invent edit history.
- Include both Live Photo components and available RAW/JPEG variants by default. Support simple subtractive choices, such as skipping Live Photo videos. Remove resolution fallback and rendition-option matrices.
- Primary library by default; shared libraries opt-in. Support shared ownership and albums whose items cross library boundaries.
- All regular albums and no-album items by default. Smart folders are opt-in and behave like albums once selected.
- Support an optional fixed capture-date range. No rolling latest-N-days or newest-N selection. Files that cease to be selected remain local and stop receiving updates.
- Do not silently retain filename/media selection matrices merely because they exist today. Inventory their consumers and propose exact public option removals for approval; the agreed selection surface is libraries, albums/smart folders, the fixed date range, and simple rendition exclusions.

### Paths and album membership

- Mirror iCloud's folder hierarchy. Use original filenames by default and stable unique names for collisions.
- Each selected album/smart folder gets an independent file copy. Do not use hard links.
- Rename album folders when iCloud renames them, carrying user-added contents with the folder. If the destination exists, choose a unique folder rather than merge unrelated folders.
- Changes to folder templates or naming settings affect future downloads only. A provider filename change updates the database pairing without renaming existing media.
- Remove unchanged Kei-owned copies from departed albums. Retain copies in remaining selected albums. Use no-album only when no regular album owns the item and no selected smart folder contains it.
- A photo belonging only to excluded regular albums is skipped. Membership in an excluded album does not suppress a copy in an included album. Unselected smart folders do not prevent no-album placement.
- Unknown membership uses no-album temporarily. Once membership is established, relocate unchanged owned files to selected folders or remove the fallback if only excluded albums own the photo. Report unresolved membership until it is resolved.
- If an album is deleted but its photos remain, redistribute unchanged copies by current membership. Preserve user-added and locally modified files in the old folder.
- Removing an album from configuration leaves its files untouched. Reselecting it reuses those files and catches up.

### Ownership and local modifications

- Kei modifies only files it owns. A folder rename may carry unowned files, but does not confer ownership of them.
- Newest wins independently for each local copy: compare the local filesystem modification time with iCloud's edit time. Preserve local media when local time is newer, times are equal, or the cloud edit time is unavailable.
- An unchanged local copy may receive the current cloud rendition. A newer local copy is not uploaded.
- Locally modified copies that would be removed or relocated because of album changes stay in place and become unmanaged. Remaining managed copies continue syncing independently.
- Provider deletion or expungement updates catalog state but leaves every local copy in its existing location, including album folders. This takes precedence over album-membership disappearance caused by whole-photo deletion.
- Local cleanup for cloud-deleted photos is future functionality. Album-membership removal is separately approved behavior and must be guarded accordingly.
- A missing recorded file is downloaded again when selected. A user-moved file is simply missing at the recorded path; do not search for moves during normal sync or modify the moved copy.

### Metadata

- Embedded writes are enabled by default for new installations. Users may choose embedded metadata, sidecars, or both.
- Default fields: capture date/timezone, location, title/caption, keywords, orientation, rating/favourite. Album memberships and people tags are optional. Do not embed provider history or internal sync fields by default.
- Metadata can update even when newer local media content is preserved. Change only Kei-managed fields, preserve unrelated fields, and clear managed fields when cleared in iCloud.
- Metadata failure retains the successfully downloaded media and becomes retryable work. Other items continue.
- Record Kei's own writes so they do not masquerade as user modifications. Do not use an incidental mtime change as the sole proof of ownership or intentional editing.

### State, upgrade, and adoption

- Every sync uses a state database. Remove stateless sync paths.
- Reuse safely identified existing state and files automatically. Migrations are automatic, with a recoverable state/config backup before changes. Failed migration leaves the prior state/config usable.
- Initial runs against another tool's archive and recovery after database loss automatically verify and adopt confident file matches.
- Preserve unmatched local files; download current selected photos normally, using unique paths when needed. Duplicates are acceptable when no reliable association exists. Do not let uncertain history block identified current work.
- A state/config backup alone is not a rollback guarantee after media has changed. Define supported rollback points and preserve matched media/state/config where a later rollback requires them.

### Recovery and operations

- Normal sync checks existence of recorded selected files, not full content integrity. Do this and process retry work even when provider prechecks report no changes.
- Bound attempts per sync cycle; retain unfinished work across cycles with no lifetime abandonment. Three retries was the user's example, not a frozen numerical interface.
- Retain unresolved provider references and retry them while healthy items progress. Unknown identity is not deletion or permission to forget work.
- A successful Live Photo component stays complete while its failed peer retries.
- Expired tokens trigger an automatic rescan, reusing files and retaining pending work. For previously known photos missing from inventory, use targeted provider lookup for explicit deletion evidence. Inventory absence alone never proves deletion.
- Integrity checking is a separate process. It attempts repair automatically when damage is confidently established. Preserve uncertain differences that may be user edits. If no cloud replacement exists, retain the damaged file and report failure.
- Keep one-shot and interval-based watch operation. A one-shot returns nonzero when selected work remains failed or unresolved after bounded retries.
- Pending retries do not make the process unhealthy; report backup completeness separately. Full/unavailable storage and authentication requiring intervention make `/healthz` report an error.
- Storage failure stops file work for the cycle and retains pending work for the next attempt.
- Dry run reports downloads, replacements, moves, and removals without changing media or sync state.

## User reports and acceptance scenarios

Use public reports linked from [#1044](https://github.com/rhoopr/kei/issues/1044), including its comments, as requirements evidence. Fetch their current text before attributing a resolution. Do not copy private attachments into fixtures or request reporter reruns as a substitute for our tests.

| Report | Required outcome |
| --- | --- |
| [#845](https://github.com/rhoopr/kei/issues/845) | Stable Live Photo and multi-album destinations; no byte-identical suffix growth on repeated cycles; stale URLs retry without a whole-library loop. |
| [#853](https://github.com/rhoopr/kei/issues/853) | Hidden/shared and legacy upgrade cases preserve files, reuse confidently matched items, and avoid mass suffix redownload caused by changed identity interpretation. Unmatched files may coexist with new downloads under the agreed contract. |
| [#1024](https://github.com/rhoopr/kei/issues/1024) | Replaying unchanged provider data does not grow a raw inbox toward a fixed capacity failure. Preserve existing unresolved evidence during migration. |
| [#953](https://github.com/rhoopr/kei/issues/953) | Former-library references remain visible and retryable without blocking healthy work or treating missing zones as deletion. |
| [#954](https://github.com/rhoopr/kei/issues/954) | Ambiguous legacy attribution remains preserved; current independently identified items progress across restart. |
| [#955](https://github.com/rhoopr/kei/issues/955) | Backup incompleteness does not cause liveness restart loops; automatic migration failure and supported rollback are tested. |
| [#956](https://github.com/rhoopr/kei/issues/956) | Exact account, owner, zone, child/master and resource matching; active shared-library work progresses without guessing ownership. |
| [#957](https://github.com/rhoopr/kei/issues/957) | Valid replay activates checkpoint/configuration consistently; unchanged subsequent cycles do not repeat inventory because activation was partial. |
| [#958](https://github.com/rhoopr/kei/issues/958) | Visible scan/replay phases, bounded waits where supported, cancellation, and restart without lost state. Do not invent a diagnosis for the historical long wait. |
| [#765](https://github.com/rhoopr/kei/issues/765) | Parent context for distinct identity, legacy, checkpoint and operational cases; do not collapse them into one inferred cause. |
| #1044 deletion comments | Confirmed deletions are reflected for original and Live Photo renditions even while unrelated replay is held. Retain all local media. |
| [#1049](https://github.com/rhoopr/kei/issues/1049) | Separate reporting follow-up: expose deletion fields in JSON/CSV, retaining default rows and unknown timestamps. Not a prerequisite for replacing the engine. |

These reports include unresolved observations and proposed fixes. A closed ticket or a synthetic test is not proof that the reporter's full scenario has been resolved.

## Target shape and complexity budget

Prefer one flow: obtain provider observations, normalize them at the provider boundary, update current catalog facts and durable due work, plan owned destinations, execute safe file operations, finalize outcomes, report progress. Full scan, incremental scan, and targeted lookup should feed the same downstream decisions. They need not have identical provider transport mechanics.

Keep conceptual ownership small: provider facts and cursor coverage; selected per-destination file work; actual local ownership and last successful write; pending metadata work where it cannot share media completion; preserved historical evidence read at migration/adoption boundaries. These are responsibilities, not a mandate for new tables or frameworks. Audit existing structures before choosing storage.

Separate provider cursor coverage from local backup completeness only where the missing work is durably recoverable and the provider protocol permits it. Retaining an error string is not sufficient proof to advance a cursor. Preserve existing gates until an explicitly reviewed replacement proves replay, retry and crash behavior. This is a design acceptance condition, not permission to bypass a hold.

For each slice record before/after: production code added/deleted, independent execution routes, state tables/keys and their active writers, supported policy switches, and tests proving retained behavior. Net line reduction alone is insufficient; moving code to new modules or retaining dormant legacy paths does not count as removing complexity.

## Delivery sequence

Each numbered item is a separate reviewable slice. Dependencies are explicit. The first slice establishes exact deletion targets and may refine later boundaries before implementation. Do not implement the whole table in one task.

| Slice | Outcome and deletion target | Dependencies | Acceptance and upgrade boundary |
| --- | --- | --- | --- |
| 0. Baseline and deletion map | Qualify main enough to attribute failures; map the actual sync call graph, durable writers and option consumers. Name the surviving owner and deletion target for every overlapping route. No new engine code. | None | Run baseline check and representative offline scenarios; record exact failures. Produce a compact removal matrix and identify any contract conflicts requiring approval. Stop for review of the first production slice. |
| 1. Trim media/selection policy | Keep full-resolution originals/current edits and simple exclusions. Remove thumbnail/medium fallback, newest-N/rolling selection and superseded option combinations. | 0, exact CLI/config approval | Exercise actual commands and help. Upgrade old settings with an explicit diagnostic or deterministic mapping; never silently expand selection. Prove fixed dates, RAW/JPEG and partial Live Photo success. Delete retired routing branches and replace their tests with migration tests. |
| 2. Require state; isolate upgrade/adoption | Remove stateless dispatch. Put schema/version interpretation and confident file adoption behind one startup boundary used for old state, first-run archives and lost-state recovery. | 0, state/interface approval | Test migration interruption, backup failure, wrong account, ambiguous files, confident reuse and reopen. Default changes must preserve existing explicit opt-outs. No periodic legacy full scans added to ordinary sync. Preserve old evidence until conversion is proven. |
| 3. Fix owned destinations as durable facts | One owner maps provider renditions to local copies. Remove re-derivation of established paths on each album pass/retry, filename drift relocation and template-change reconciliation of existing files. | 1, 2 | Multi-album/Live Photo repeated cycles create no extra suffixes. Template changes affect new files only; cloud filename changes preserve recorded paths; collisions remain stable across restart. |
| 4. One retry path | Route failed transfers, missing files and current lookups through the same per-destination work owner. Delete competing retry-only orchestration and lifetime exhaustion branches where superseded. | 3 | Three representative bounded failures followed by quiet-cloud recovery; restart retains work; a failed Live Photo component does not repeat its peer; no repair bypasses for protected paths. CLI changes need approval. |
| 5. One provider-to-work path | Full/incremental/targeted discovery feed common normalized decisions. Remove duplicate streaming/collecting policy, overlapping queues and redundant checkpoint bookkeeping only after their responsibilities move. Stop generating raw shadow pages when all live consumers have replacements. | 4, checkpoint design approval | Replay is idempotent; database failure holds progress; unresolved identity survives restart and healthy peers progress. Repeated equivalent responses do not grow raw history. Migrate or retain unresolved old evidence without discarding it. Split narrowly if needed, with a named final deletion commit. |
| 6. Rescan and checkpoint recovery | One automatic expired-token recovery path; simplify configuration activation and delete obsolete inventory/bridge fallback combinations where provider evidence supports replacement. Apply explicit deletion facts independently of unrelated local failures. | 5 | Expired token, interrupted rescan, rejected lookup, indexing, config drift and held-replay deletion tests. Original/live renditions tombstone together; files remain. No deletion inferred from absence; next unchanged cycle is quiet. Do not promise removal of protocol-required bridging before proving an alternative. |
| 7. One album placement policy | Add the agreed membership transitions to the destination owner and remove pass-specific path-flapping and alternate layout policies they replace. | 3, 5, 6 | Album rename/hierarchy, destination collision, multi-membership, excluded albums, selected smart folders, unknown-membership fallback and album deletion. Verify cloud-deleted copies stay put. File move/removal must preserve the last required copy across process death. |
| 8. One local-change policy | Replace scattered local-match/rewrite exemptions with ownership plus last Kei write and the agreed per-copy timestamp decision. Remove normal-sync relocation searches and content-integrity scans. | 3, 7 | Newer/equal/unknown timestamps; local modification during execution; unmanaged files; detached modified album copy; independently updated peers. Own metadata writes do not trigger false user-edit classification. Normal sync detects absence without hashing every media file. |
| 9. One metadata completion path | Converge initial writes and later refresh on the same managed-field writer and retry behavior. Remove redundant revision backfills and separate retry machinery once migrated. Apply default embedded writes and optional sidecars/grouping fields. | 4, 8, instruction/default approval | Metadata failure preserves media and peer progress; cloud-cleared fields clear locally; unrelated metadata survives; newer local media still receives configured metadata; retries recover across reopen. Preserve format-specific byte-safety checks. |
| 10. Explicit integrity repair | Move remaining full integrity work out of sync; reuse publication and retry primitives for confirmed-damage repair. Remove embedded automatic integrity/repair dispatch from normal cycles. | 4, 8, 9 | Corruption repairs automatically; intentional/uncertain edits remain; unavailable source preserves damaged bytes; missing-file normal recovery remains functional. Do not build another general recovery engine. |
| 11. Unified orchestration and reporting | Reduce runner/watch to scheduling the same cycle, cancellation and reporting. Delete duplicate follow-up cycle policies; expose separate completeness and operational health. Dry run uses the same planner without durable mutation. | 4 through 10 | One-shot nonzero for unfinished selected work; pending work stays healthy; storage/auth blockers fail health; watch handles quiet-cloud retries and missing files. Dry run changes neither media nor state and previews actual decisions. |
| 12. Final removal and upgrade qualification | Delete remaining superseded modules, schema writers, compatibility flags and dead fixtures identified in slice 0. Finish docs and supported upgrade paths. | All prior slices | No parallel old engine remains. Search all retired literals/consumers. Run the issue-derived matrix, crash/reopen and multi-cycle checks; qualify the actual integrated candidate and required CI. Publish a before/after complexity account. |

Slice 11 reporting corrections that are independent of the new flow may ship earlier as small fixes. Feature work in slices 7 through 10 must still retire competing policies; otherwise split out the feature and obtain scope approval rather than hide net complexity growth in the rewrite.

## Common acceptance and failure rules

- Demonstrate progress as well as refusal through the production runner or CLI. A test that sets up a desired database row without executing the relevant transition is not acceptance.
- For durable changes: interrupt before/after transaction and publication boundaries, reopen in a fresh process where relevant, recover, then run two unchanged cycles. Check bytes, paths, metadata, retry state, cursor/configuration and healthy peers.
- Freeze a candidate for independent review and acceptance. Distinguish author tests, independent review, runtime acceptance and release qualification. Synthetic fixtures must represent the public reports faithfully; live tests require fresh explicit authorization.
- Preserve `.part` staging, checksums, no-overwrite publication, directory durability and transactional finalization. Authorized replacements/removals require exact ownership and concurrent-change checks; plan-time approval alone is insufficient.
- Each schema slice explains failure before activation, failure after activation, and the supported rollback point. Never suggest resetting state or downgrading only the binary after incompatible state/media changes.
- Follow `CONTRIBUTING.md`, scoped architecture sections, and `tests/README.md`. Finish code slices with formatting and all-target/all-feature Clippy checks. Use focused tests locally; hosted CI owns the full offline gate unless unavailable or needed for diagnosis.
- CLI removals require actual help/command checks and updates to configuration docs, Docker/service invocation and relevant packaging. Default metadata writes conflict with the current opt-in wording in `AGENTS.md`; align that instruction explicitly in the approved policy slice rather than silently ignoring it.
- Main remains unchanged. Do not merge, release, tag, push or open PRs without the authorization required by repository instructions. The request that created this plan authorizes the branch and plan, not all downstream implementation or publication.

## First task for a fresh agent

Execute slice 0 only. Read the repository instructions and this document, run `cargo check`, inspect the actual main-based owners and consumers, and select a small first removal slice. Report its exact files/options/state, behavior it retires, retained guarantees, migration treatment, acceptance commands and any product decision still required. Update this plan with evidence and a short baseline/removal matrix. Ask for approval of the bounded production slice before deleting code or changing public contracts. Do not restart the paused beta repair work.

The stopping condition is a verified, source-bound deletion plan for the first production slice, not an architectural framework or a partially implemented replacement engine.

## Slice 0 findings (2026-11-11)

Status: discovery complete; slice 1a subsequently approved below. The interview contract above remains the target. The first slice below is only part of delivery slice 1.

### Baseline and evidence limits

- Checkout: `/home/robhooper/dev/kei-sync-simplify`, branch `work/v0.30.0-sync-simplify`, initially clean. HEAD `002c9d3456211534ee1d6e0c8cbdb038b47572dc` is the planning-document commit directly above the recorded source baseline `3cdd14f31d826fe77c18519910806bbe51fea8e9`. The package reports `0.26.1-dev`; v0.30 is the delivery goal, not the current binary version.
- Read `AGENTS.md`, `CONTRIBUTING.md`, architecture owners/flows/invariants/impact sections, and the test guide. Two read-only native tasks traced execution/state and configuration respectively. No old-beta checkout or private evidence was inspected or changed.
- `cargo check`: passed, default features, Linux, 31.83 seconds including initial compilation.
- `cargo test --lib download::file::publication::tests::`: 4 passed, covering successful landing, identical-byte collision, differing-byte refusal and missing staging file.
- `cargo test --lib unresolved_identity_survives_restart_and_other_zone_success_then_recovers`: 1 passed through the runner regression. This is bounded synthetic evidence for retained identity debt and healthy-peer progress, not qualification of the historical reports.
- `cargo test --lib run_cycle_date_bound_expansion_preserves_existing_media_and_reaches_steady_state`: 1 passed through the production cycle regression.
- `just test scenario config-docs`: 3 passed (supported example settings, migration guidance, contributor commands).
- `cargo fmt --all --check`: passed. `cargo clippy --all-targets --all-features -- -D warnings`: passed.
- No failures occurred in these checks. The complete offline gate, live provider tests, real service checks, and release qualification were not run. Public issue text was not fetched because no resolution is being attributed. Historical observations remain unresolved evidence, separate from these current baseline results.

### Actual execution and authority

Source references below describe this baseline; symbols are the durable navigation anchors.

1. `src/lib.rs` dispatches sync and setup's immediate sync to `sync_loop::run_sync`; `src/service/run.rs` uses the same entry. `src/sync_loop/runner.rs:296-390` opens authenticated account-owned state, attaches `ShadowCapture`, and aborts on state-open failure. Only dry-run deliberately supplies no database. There is no active public `--stateless` or `--no-state` option to remove.
2. The runner replays the retained catalog (`runner.rs:514`), plans library passes, performs database precheck and local drift wakeups, then calls `run_cycle` (`:705`). Metadata refresh has a separate tail (`:723`); scoped database-token persistence follows cycle evidence (`:758-765`). `src/sync_loop/planning.rs` delegates pass policy to `src/commands/service.rs`.
3. `src/sync_cycle.rs` owns configuration reconciliation, source enumeration choice and checkpoint decisions. It calls `download_photos_with_sync` after optional path reconciliation (`:1474-1509`) and again for the retained-cursor bridge (`:1619`). `source_checkpoint_decision` and `state/db/checkpoints.rs::commit_checkpoint_transition` retain checkpoint authority.
4. `src/download/orchestration/dispatch.rs:357-695` recovers interrupted publications/layouts, runs capture repair, resets retries, projects retained provider work, and selects active-generation or ordinary full/incremental routes. Incremental streaming and collecting remain separate policy paths in `incremental.rs`; targeted album and smart-folder backfills add further routes.
5. `TaskPlanner` and `pipeline/producer.rs` derive tasks and write pending rows, mappings, membership and reservations. Streaming producer/consumer execution and explicit retry passes share `pipeline/task.rs`, `file/transfer.rs`, and `finalize.rs`. `finalize_downloaded_with_proof` calls `mark_verified_download`; failure records retry work rather than treating a landed file as durably complete.
6. Provider observations enter `icloud/photos/album/changes.rs` before emission, through `inbox.rs` into `provider_inbox.rs` and `provider_catalog.rs`. `queue_projection.rs` revalidates retained identities against current provider records before writing selection shadows and admitted work. `generation.rs` also drives active physical destinations. These are live consumers, not a removable archive-only subsystem.

The publication owner remains `download/file/publication.rs::publish_part_to_final`: no-overwrite landing, fingerprint comparison for collisions and parent-directory fsync. Transfer retains `.part` identity, length/content/checksum checks and safe resume. State finalization follows publication. Conditional replacement and interrupted-layout journals retain their separate concurrent-change proof. None of these guarantees is a deletion target.

### Removal matrix

Each row names a future deletion boundary, not permission to delete now. State names are search anchors; tables with live obligations must be converted or drained before their writers disappear.

| Existing responsibility and source | Surviving owner | Code, state or options to remove | Retained behavior and dependency |
| --- | --- | --- | --- |
| Relative selection: `cli.rs::RecentLimit`, `config/resolve.rs`, `config/runtime.rs::parse_created_date_filter` | Existing input/resolution owners and fixed-date eligibility | `Days`, days serialization/translation/conflicts, relative clock arithmetic and setup invitation | Fixed capture dates and explicit datetimes; proposed first slice below. No database change. |
| Newest-N: `orchestration/full.rs:528-674`, `recent.rs`, `incremental.rs:795-960`, `generation.rs:1344-1428` | Fixed selection in configuration and planner | Sync `recent`, `recent_scope`, global frontier/per-filter caps, `recent_selection_recovery:<zone>` writer and recovery dispatch | Keep bounded enumeration and checkpoint proof. Later approval must explicitly decide `import-existing --recent N`, a separate sampling consumer. |
| Rendition/filename/media matrices: `config/resolve.rs`, `filter/eligibility.rs`, `filter/versions.rs`, `filter/expected_paths.rs` | Existing rendition selector and eligibility owner | TOML `resolution`, `live_resolution`, `force_resolution`, `filename_exclude`, `media`, and superseded `raw_policy`, `edited`, `alternative`, `edited_naming` combinations after exact replacement approval | Original/current-edit/available RAW-JPEG contract and simple exclusions. Preserve historical rendition/resource identity and pending rows; do not delete provider variants with user options. |
| Optional state and adoption: `sync_loop/runner.rs:296-390`, `state/db/account.rs`, `commands/import.rs` | Existing authenticated startup/adoption boundary | Writable optional-store branches after dry-run gets an explicit read-only planning contract; repeated runtime legacy interpretation | Ordinary sync already requires state. No fictional stateless flag removal. Automatic verified adoption, backup and failure rollback require a separate approved slice. |
| Full/incremental and special backfills: `orchestration/dispatch.rs`, `full.rs`, `incremental.rs` | Existing orchestration, provider normalization and `TaskPlanner` | Duplicate downstream streaming/collecting policy and superseded targeted-album/smart-folder orchestration | Keep distinct provider transports where necessary, membership evidence, natural EOF, cancellation and malformed-record holds. |
| Raw pages and historical catalog indexing: `icloud/photos/inbox.rs`, `state/db/provider_inbox.rs:699-859`, `provider_catalog.rs:71-162` | Provider adapter plus transactional current source facts | New `capture_shadow_page` writes to `provider_shadow_pages/records/receipts`; per-page `provider_catalog_records/references/debt/pages` history once consumers are replaced | Preserve unresolved original evidence and provenance. Stop generating redundant history only after replay/admission no longer requires it; receipts are not checkpoint authority. |
| Shadow selection and retained work: `orchestration/queue_projection.rs:100-278`, `state/db/provider_selection.rs:388-473`, `provider_work.rs:205-364` | Planner plus one existing transactional due-work admission owner | `capture_selection_shadow`, equivalence replay and their `provider_selection_*` shadow writers; competing `provider_work_receipts/obligations/scan/retries` scheduling when superseded | Retain current lookup confirmation, asset/mapping writes, metadata obligations, conflicts and generation fences. Current retained-work admission covers a restricted private Unfiled profile, not all libraries. |
| Active generations and copies: `orchestration/generation.rs`, `state/db/provider_generations.rs` | Planner decisions and physical-destination state/finalization | Superseded `provider_selection_rank_*` replay, repeated declarations, redundant `provider_active_generations/decisions/sources` writers after responsibility transfer | `provider_active_destinations` holds real per-copy paths and media/metadata completion. It is not disposable shadow state; canonical asset rows alone cannot replace it. Exact schema consolidation remains undecided. |
| Competing retries: `orchestration/recovery.rs`, `download/retry.rs`, `pipeline/pass.rs`, `state/db/assets.rs:367-425` | Existing exact retry-task resolver and shared executor | Retry-only orchestration/reset mode, append-only-after-success scheduling, duplicate URL-refresh pass construction, runtime legacy backfill after migration | Bounded cycle attempts, durable unknown identity and independent completed peers. `prepare_for_retry` already resets transfer attempts each cycle; do not claim a transfer lifetime cap exists. |
| Checkpoint/config/recovery: `sync_cycle.rs`, `state/db/checkpoints.rs:108-173` | Cycle checkpoint authority and transactional state store | Superseded path-config staging/reconciliation keys and duplicate inventory/bridge policies; finite `retained_checkpoint_hold` exhaustion when replaced | This hold is distinct from transfer retry counts. Preserve stale-plan fences, atomic activation, explicit deletion proof and required protocol bridging until reviewed rescan/replay proof replaces them. |
| Paths/layouts: `orchestration/reconciliation.rs`, `primary_layout.rs`, `state/db/primary_layout.rs:282-445` | Recorded per-copy destinations in planner; existing safe file handover | Template/root/filename re-derivation of established paths and alternate layout dispatch; superseded layout writers only after pending operations migrate | `primary_layout_operations/members/claims/preserved/bindings/superseded_paths` include ownership and interrupted-publication evidence. Preserve it, user edits and all required copies. |
| Legacy attribution: `sync_cycle.rs:1253-1312`, `download/legacy_preservation.rs`, `state/db/legacy_preservation.rs:481-662` | Upgrade/adoption boundary with retained protected-path evidence | Normal-cycle `LegacyCycle::begin/certify/finish`, `complete_legacy_inventory`, recurring mapping backfill and legacy coverage activation after conversion | Preserve `unattributed_legacy`, paths and provenance. Unknown historical ownership cannot be guessed or discarded; independently identified current children must progress. |
| Metadata: `orchestration/maintenance.rs`, `metadata_rewrite/queued.rs`, full/collecting/runner drain callers | Existing managed-field writers plus one per-path completion/retry owner | Periodic revision backfills, duplicate drain scheduling and redundant revision retry writers after migration | Keep successfully landed media, unrelated fields and prepared-output recovery. `asset_metadata_paths` also holds source/publication evidence. Default-write policy needs explicit instruction alignment. |
| Local recovery/watch: `sync_loop/reconcile.rs:92-203`, runner and watch owners | Runner scheduling, selected-file existence recovery and explicit integrity command | Truncated-file repair during sync, periodic diagnostic cadence, `local_drift_probe_offset_v1` once replaced, metadata-specific follow-up cycles | Current drift probe samples size/existence, not full hashes or every selected file. Preserve quiet-cloud wakeups and missing-file recovery; completeness and operational health remain distinct. |

Historical evidence must remain separate from current work. Old raw pages and manifests do not establish the newest provider version. Confirmed transfers, per-copy metadata debt, sparse references and interrupted publications remain actionable even when discovered through history. `generation.rs::hold_retained_debt` currently couples retained debt to checkpoint holds; removing that coupling requires replay/restart proof. `delta.rs` already applies explicit source/family deletion through the state owner. Preserve those facts and all local media; inventory absence is never deletion evidence.

### Configuration consumers and corrected assumptions

- Input flows through `cli.rs` and `config/input.rs`, then `config/resolve.rs` into runtime types. Setup and `config/persistence.rs` write durable TOML. `commands/config_cmd.rs` resolves and serializes `config show`; startup in `lib.rs` handles parse failures, with an existing diagnostic exception for doctor.
- Relative `recent` becomes an instant boundary at config-build time. Direct `skip_created_before/after = "Nd"` reaches the same parser. The runner captures resolved bounds once (`runner.rs:406-407`); this is not a continuously moving window in an already-running watch process.
- Fixed bounds pass through `filter/config.rs`, `filter/eligibility.rs:100-111`, `orchestration/full.rs:623-674` and `orchestration/selection.rs:300-301`. Provider enumeration receives limits/bounds rather than relative-date text. Keep conservative date enumeration and bounded-stream checkpoint vetoes.
- Bounds affect configuration/coverage fingerprints in `orchestration/config.rs`, scoped prechecks in `sync_loop/precheck.rs`, selection generations, retained-work projection and recent recovery. Their serialized fixed-date/instant formats must remain unchanged in the first slice.
- Media and filename settings are TOML today; apparent CLI overrides often belong to test-only/internal construction. Tests already reject `--size`, `--force-size`, `--filename-exclude` and former rendition switches. There are no current environment bindings for the selected recent/date settings.
- `version_with_fallback` can fall back from a requested rendition to original. Do not describe it as always substituting a smaller image. RAW alignment can swap logical Original/Alternative while preserving distinct provider resource keys.
- Import shares photo/path/media resolution, but `PathDerivationConfig::from_path_fields` initializes date and filename filters empty. Import already rejects days-form recent and supports count sampling. Shared types alone do not prove identical sync/import selection behavior.
- Docker CMD (`Dockerfile:87`) and Linux service `ExecStart` (`src/service/linux.rs:124`) use service/config entry points without relative literals. The Homebrew formula generated by `.github/workflows/release.yml:298-333` installs `bin/kei` and tests version/help without selection arguments. Read-only inspection of `rhoopr/homebrew-kei/main/Formula/kei.rb` during slice 1a confirmed the same invocation shape in the published v0.26.0 formula. No packaging change is needed for this slice.

## Approved first production slice: remove relative-date selection

Approval status: approved by the user, including production/test deletion, public input removal and refusal-based upgrade. The user additionally requires actionable errors that explain how to resolve the problem. This is slice 1a, independently deliverable before newest-N or rendition changes. It removes one clock-dependent selection policy and its overlapping input forms. It does not satisfy the full slice 1 acceptance matrix by itself.

### Exact scope

| File | Approved change |
| --- | --- |
| `src/cli.rs` | Remove `RecentLimit::Days` and successful relative parse/display/serialization arms. Reject `--recent Nd`; preserve positive count parsing and integer/string input round trips. Update sync/service-shared and import help. |
| `src/config/resolve.rs` | Remove days-to-bound translation, days conflicts and days-specific scope validation. Preserve fixed-bound precedence, count scope validation and contradictory-window reporting. |
| `src/config/runtime.rs` | Remove relative interval arithmetic from `parse_created_date_filter`; keep date-only and explicit datetime parsing and `CreatedDateFilter` fingerprints. |
| `src/config/input.rs` | Reject retired relative strings in both TOML date fields before CLI precedence can mask them. Use the existing input owner and direct validation, with no migration registry or runtime legacy variant. |
| `src/commands/import.rs` | Remove the now-unreachable Days-specific rejection branch; preserve count sampling and existing import selection semantics. |
| `src/setup.rs` | Remove relative-date prompt text and route rejection through existing date validation. |
| Tests in those owners, `src/config/persistence.rs`, `tests/behavioral/configuration.rs`, and relevant CLI integration tests | Replace successful relative-policy tests with refusal tests. Convert positive persistence/config-show fixtures to fixed dates while retaining their original round-trip purpose. Add startup preservation/precedence acceptance below. |
| `README.md`, `example.config.toml`, `docs/migration-from-icloudpd.md`, `docs/architecture.md`, `implementation_plan.md` | Describe fixed boundaries, supported count behavior for this intermediate slice, and the explicit old-setting diagnostic. Preserve historical changelog and old-version descriptions as history. |

Remove all relative forms together: sync/service `--recent Nd`, TOML `[filters].recent = "Nd"`, and `Nd` in both `--skip-created-before` / `--skip-created-after` and their TOML fields. Keep `YYYY-MM-DD` and `YYYY-MM-DDTHH:MM:SS`; the latter is currently host-local, not RFC3339. No new dependency, schema/table/key, execution engine, publication policy or metadata default is needed.

Count-form `recent`, `recent_scope`, fixed dates, media matrices and existing adoption remain supported in this intermediate slice. The named deletion condition for count support is the separately approved newest-N slice, including its durable recovery receipt and the import-sampling decision. Do not turn this bounded input change into that larger removal.

### Upgrade and rollback

Approved behavior: fail before authentication, provider requests, state opening/migration or media work when retired relative settings are present. Name the key and recommend an explicit fixed boundary. Preserve the config bytes, state and media; a fixed CLI override must not hide an obsolete TOML value. Doctor retains its existing ability to report an invalid config without entering sync.

Suggested diagnostic: `Relative date windows are no longer supported. Set an explicit fixed boundary in skip_created_before or skip_created_after. Configuration was not changed.`

There is no exact behavior-preserving automatic conversion from a relative boundary across future starts to a fixed date. Do not remove the limit, freeze it to today's date, or infer an intended capture date from a stored instant. The user approved refusal as a bounded exception to the general automatic-upgrade requirement. Any later automatic conversion needs its own agreed semantics.

No state/config rewrite or backup is performed by this refusal-only slice because it mutates neither. Existing historical hashes and instant evidence remain intact; supported settings retain identical hashes and serialization. Before any successful sync using a user-changed fixed boundary, the unchanged old config/state/media remain a rollback point. After such a sync, this slice makes no new media rollback guarantee; restoring only an older binary/config is not a rollback of later file work.

### Acceptance and deletion accounting

1. Reject relative values through sync CLI, service-run CLI, TOML, config-show and setup validation. Include both date bounds, `recent`, and a retired TOML value masked by a valid fixed CLI override. Diagnostics identify the unsupported setting.
2. Exercise the real binary with populated synthetic config/state/media. Assert nonzero refusal, no authentication/provider access, unchanged config/database/media bytes, and no new sync artifacts. Include an invocation that would otherwise be ready to sync so an unrelated missing-credential error cannot satisfy the assertion.
3. Accept fixed lower/upper capture dates and host-local datetimes. Preserve inclusive capture-local eligibility, conservative provider bounds, contradictory-range reporting and original TOML persistence. Run the existing production date-expansion/reopen/steady-state regression; excluded local files remain untouched.
4. Retain count parsing, scope and import sampling; run the existing recent reopen/convergence and Live Photo companion recovery regressions. This input slice must not change successful-component completion or retry identity.
5. Preserve golden hashes for supported configurations and inspect actual `sync --help`, `service run --help`, and `import-existing --help`. Recheck Docker CMD, Linux ExecStart and packaging consumers; no new runtime wrapper is permitted.
6. Search `RecentLimit::Days`, `recent_days`, relative prompt/help examples and all `parse_created_date_filter` callers. Remaining relative literals belong only to rejection tests or explicitly historical docs. No successful relative runtime branch survives.

Focused implementation checks: `cargo test --lib config::`; `cargo test --lib cli::tests::recent`; `cargo test --lib bundled_date_filters_select_exact_assets`; `cargo test --lib run_cycle_date_bound_expansion_preserves_existing_media_and_reaches_steady_state`; `cargo test --lib recent_scope_overflow_reopens_then_increases_removes_and_converges`; `cargo test --lib recent_one_asset_keeps_both_companion_renditions_and_recovers_unchanged_tail`; `cargo test --lib golden_`; `cargo test --test cli`; `cargo test --test behavioral config_show`; the new startup-refusal test by exact name; `just test scenario config-docs`; formatting and all-target/all-feature Clippy. These are future acceptance commands, except where separately recorded in the baseline above. Confirm each filter selects tests. Hosted CI owns broader offline qualification.

Before/after accounting for this slice: relative selection policy 1 -> 0; accepted relative entry forms 6 -> 0 (three CLI values and three TOML values); clock-dependent date-resolution branch 1 -> 0; days-to-date translation route 1 -> 0; database tables/keys/active writers unchanged; sync execution routes unchanged; dependencies unchanged. Record actual production lines added/deleted at implementation review, not an invented estimate. The retained count policy and unchanged engine routes must remain visible in the final accounting.

Approval received for this exact production/test deletion scope and public input removal, refusal of old relative config as the bounded upgrade exception, and retention of fixed host-local datetimes and count-form recent during this intermediate slice. Implementation is limited to slice 1a; later slices still need their own approval.

### Slice 1a implementation evidence

- Removed relative recent representation, serialization, translation/conflict handling and clock arithmetic. Fixed-date inputs retain their existing runtime types and hash formats; count-form recent remains supported.
- Saved relative TOML is rejected by the input owner before precedence. Both CLI date flags validate through the existing date parser during Clap parsing. Rejection messages name the setting or flag and explain how to remove the limit or use fixed dates; recent errors also give a supported count alternative.
- A read-only 6.1 Sol review found that initial CLI date rejection happened after support-recording files and the data directory could be created. Moving date validation into Clap resolved that finding on static re-review. The completed binary regressions also verify unchanged populated state/media/config, no support artifacts, and no creation of an absent data directory on CLI refusal.
- The simplification pass removed a now-redundant intermediate lower-bound variable. No additional engine, migration dispatcher, dependency, database writer or schema change was introduced.
- The refusal matrix executes 27 real-binary invocations across sync/service CLI, TOML, config-show, masked TOML overrides and absent data directories. A supported fixed-date/count control reaches the offline authentication boundary, rather than passing because the fixture cannot reach sync. Import-relative rejection now runs offline; provider-dependent import tests remain ignored.
- Final read-only 6.1 Sol review of the completed test changes found no correctness or acceptance gaps. That review inspected source and assertions; it did not independently rerun tests.
- Production-section diff against HEAD, including comments and excluding test modules: 76 lines added, 89 deleted, net 13 removed. Relative selection policy and all six accepted relative input forms are removed. Engine execution routes and database tables/keys/writers are unchanged. The larger removal targets remain future slices.
- Cross-file coherence check: no `RecentLimit::Days`, successful relative resolution, or relative setup/help invitation remains. Supported date/count serialization and six golden hash tests pass. Remaining relative literals are rejection cases, upgrade instructions, or historical release documentation. Actual sync/service/import help and `sync --recent 30d` were inspected; the latter exits 2 with removal/fixed-date/count instructions. Docker, systemd and Homebrew consumer checks are recorded above.

| Slice 1a check | Result |
| --- | --- |
| `cargo check` | Passed |
| `cargo test --lib config:: --quiet` | 381 passed |
| `cargo test --lib cli::tests:: --quiet` | 181 passed |
| `cargo test --lib setup::tests:: --quiet` | 43 passed |
| `cargo test --test cli --quiet` | 136 passed |
| `cargo test --test behavioral configuration:: --quiet` | 66 passed |
| `cargo test --test import_existing_live --quiet` | 15 passed, 15 ignored; no live tests run |
| Four named date-selection/date-expansion/recent-convergence/Live Photo regressions listed in acceptance above | Each passed, 1 test per command |
| `cargo test --lib golden_ --quiet` | 6 passed |
| `just test scenario config-docs` | 3 passed |
| `cargo test --test behavioral relative_ --quiet` after replacing boolean test-helper arguments with named input cases | 3 passed, preserving the full refusal matrix |
| `cargo fmt --all --check` | Passed after fixing three formatting differences |
| `cargo clippy --all-targets --all-features -- -D warnings` | Passed on final Rust changes |
| `git diff --check` | Passed |

Counts are per command and overlap. These are focused Linux checks, not full offline, live-provider or release qualification. Qualification preceded commit and push. The user subsequently authorized committing and pushing this branch, then proposing the next slice. That authorization does not include implementing the next slice, opening a PR, changing main or releasing.
