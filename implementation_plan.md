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
11. Historical state is evidence to preserve, not a prerequisite that every current sync must repair or certify. Establish authoritative current provider facts and local work, isolate unresolved history, and let independently identified healthy items finish. A new baseline must have a valid provider boundary and durable current retry work, but must not require every old receipt, attribution or cursor chain to become consistent.

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

## Approved slice 1b: remove newest-N sync selection

The user approved this slice after publication of slice 1a at `1e4982042521cf0c9e42b38b30bd8078bdfc00c7`. The approval covers production/test deletion and public sync/config removal. Import-only `import-existing --recent N` sampling remains an explicit exception. No live tests, PR, main change or release is authorized by this approval.

- Remove sync/service `--recent` and `--recent-scope`, `[filters].recent` and `recent_scope`, and their setup/config plumbing. Retire `RecentScope` and the shared `RecentLimit` representation; import uses a positive-count argument with import-specific guidance.
- Remove global-frontier/per-filter caps, count-specific enumeration/token decisions, the recent-only inventory/recovery route, active receipt writers/readers and watch wakeups. Delete `download/orchestration/recent.rs` only when its outstanding obligations have a proven surviving path.
- Preserve fixed-date filtering, import's bounded provider transport, ordinary generation inventory/replay, sparse identity and legacy evidence, per-destination media/metadata obligations, publication safety and existing checkpoint gates.
- Keep historical uncapped hash/profile encodings so unchanged users and retained generations do not receive gratuitous configuration drift. Preserve original `recent_selection_recovery:<library>` metadata as historical evidence; do not delete it or add a new ledger.
- Reject retired settings before authentication or startup writes with instructions to remove them or select fixed sync dates. Import guidance must not recommend unsupported date flags. A refused command changes no config/state/media and needs no backup. Any required state conversion needs its own explicit migration/backup/rollback decision.
- Replace the live sync harness's implicit count cap with an explicitly supplied fixed-date selection; fail closed when the bound is absent. Keep import sampling separate. This does not authorize running live tests.

### Prerequisite and stopping condition

Before downstream deletion, prove that the existing uncapped inventory/checkpoint path handles outstanding count-era receipts, including already-uncapped incomplete recovery, malformed/future receipts, quiet provider responses, interruption/restart and failed activation. In particular, check whether enumeration drift and the retained-cursor bridge preserve the obligation after recent-only precheck wakeups and readers are removed. If any case lacks reachable recovery, stop for the agreed bounded migration decision. Do not remove the hold, leave a dormant recent engine, or replace it with a parallel compatibility engine.

Start evidence: clean worktree on the published slice 1a commit; `cargo check` passed. The prerequisite found a deletion blocker. Production removal has not started.

Live-harness discovery: `tests/data/live-selection.toml` currently supplies `recent = 10` to both Rust (`tests/common/live_selection.rs`) and shell (`tests/shell/lib.sh`) config writers. Import helpers also extract that value for their separate CLI cap. `scripts/full-test/run_live_smokes.sh` adds a sync CLI cap independently. Any implementation must remove both sources of sync caps, retain import-only counts, and validate the fixed-date bounds before provider preflight. No actual live capture-date range has been selected or live command run during this task.

### Prerequisite result: stop for a migration decision

Native 6.1 Sol source investigation and coordinator inspection found that the existing uncapped path does not cover every retained obligation. These are source-backed counterexamples, not newly executed failure reproductions:

1. `orchestration/recent.rs::active` and `needs_inventory` (`:64-110`) force recovery for incomplete, malformed and future-version receipts even without a current count limit. If the active enumeration hash already matches the uncapped configuration, `sync_cycle.rs::check_and_persist_enum_config_hash` (`:785-802`) returns `Unchanged`. A missing active hash returns `Initial`, which also does not force inventory. A retained cursor then selects incremental enumeration (`:2494-2513`). With quiet deltas and no previously queued target, ordinary pending recovery cannot discover an omitted asset (`download/retry.rs:776-783`). Deleting the recent reader would remove the inventory trigger.
2. `sync_loop/precheck.rs:174-182` supplies the corresponding quiet-watch wakeup. The other retained-work owners are not universal replacements: `icloud/photos/album/selection_capture.rs:110-155` and `album/work.rs:123-134` require private scope. A shared-endpoint case with matching uncapped hashes cannot rely on those owners to replace the deleted wakeup or inventory trigger.
3. The completed-receipt edge also needs migration coverage. `recent.rs::record` (`:113-134`) writes a completed receipt before checkpoint activation. With no current count, a version-1 receipt marked unbounded and complete makes `active()` return false without checking its cursor or fingerprint. `requires_recovery()` then skips cursor validation. The existing failed-cursor regression retains a count of two and does not prove the already-uncapped case. No real-provider diagnosis is inferred from this inspection.

Real enumeration-hash drift does reach full inventory, the retained-cursor bridge and atomic activation (`sync_cycle.rs:1326-1336`, `:1565-1703`, `:2294-2357`). That is the surviving recovery mechanism, but not every old receipt currently reaches it. Merely writing `pending_enum_config_hash` is insufficient: the matching-active-hash branch deletes both pending configuration and pending zone checkpoints (`:785-799`). Clearing the active hash alone is also insufficient because `Initial` does not request inventory.

The prerequisite initially suggested converting count-era receipt obligations into pending inventory/reconciliation while retaining the old bridge requirement. The user's clarification below changes that design goal: establish a current baseline and retire historical authority, rather than make historical receipts consistent. Migration still needs a coherent state/config backup, preserved original evidence and a defined pre-file-work rollback point. Exact pending-state representation and the new baseline's provider boundary require source-level design before implementation; no representation or cursor bypass is approved by this finding alone.

Acceptance must cover incomplete/malformed/future receipts, already-uncapped completed receipts with failed activation, matching and missing active hashes, shared scope, quiet watch, healthy-peer progress, interruption/reopen, preserved original evidence and two quiet cycles after successful recovery. Existing count-era regression fixtures remain intact until those replacement proofs exist.

Checks on the unchanged production/test baseline: `cargo check` passed; `cargo test --lib recent_` passed 68 tests; `cargo test --lib sync_loop::runner::tests::configuration::` passed 35 tests; formatting, all-target/all-feature Clippy and `git diff --check` passed. Counts overlap. No production or test files changed, and no full gate or live checks ran. Only this plan was updated; slice 1b remains blocked at its agreed prerequisite.

### User clarification: current work must proceed despite unresolved history

The user identified historical state and receipts preventing successful sync as a central failure behind the #1044 reports. The target is safe forward progress with fewer consistency mechanisms. Do not answer that problem by adding more historical certification, receipt repair or recurring full inventories.

- Use old count receipts as a conservative reason to establish current coverage once. Their validity, fingerprint, completion bit and old cursor agreement are not prerequisites for current-item progress. Keep their original bytes as historical evidence.
- Classify state by its responsibility, not its age or the word "receipt". File ownership, interrupted publication and selected pending work remain operational safety state until completed or safely migrated. Archiving history must not hide a current retry obligation or remove a guard protecting user files.
- Define completion using the selected current provider observations and the durable work they require. Healthy current items finish independently, retain their successful state across restart, and are not downloaded or replanned repeatedly because unrelated history is ambiguous.
- Historical ambiguity alone must not make a completed current sync fail, keep the service unhealthy, force another inventory on every cycle, or hold an unrelated library's checkpoint. Report unresolved historical evidence separately from current backup completeness.
- Current selected failures and unresolved current provider references remain durable, bounded retry work. One-shot still returns nonzero for unfinished selected current work. Cursor advancement requires current coverage or sufficient durable evidence to recover every outstanding current observation; a count, error string or discarded unknown record is not that evidence.
- Existing files with confident identity and content evidence are reused. Preserve unmatched or ambiguous files; the interview permits downloading a current identified item to a unique recorded path when reliable reuse is impossible. Repeated cycles must reuse that new pairing rather than create more suffix copies. No guessed ownership, overwrites, truncation, or inventory-absence deletion is allowed.
- A fresh baseline needs a valid provider boundary, including changes concurrent with inventory. Do not assume replay from an old checkpoint must succeed merely to certify history. Prove the current boundary using the provider protocol and surviving owner before changing activation. Retain old checkpoints as recovery/rollback evidence during transition.
- Current explicit deletion facts must update the catalog even when unrelated historical evidence remains unresolved. Keep all local media for cloud-deleted items.

Public evidence was refreshed from the live text of #1044 (including comments), #1024, #954, #955 and #957. #1044 includes a report of completed inventory/replay and no transfer/state-write failures followed by a legacy-preservation hold, and a separate report of missed explicit deletions during held replay. #1024 reports repeated old-cursor capture growing the raw inbox until every sync fails. #954 preserves ambiguous attribution while requiring independently identified current children to progress. #955 distinguishes backup incompleteness from health-driven restarts; #957 describes repeated inventory while checkpoint/config activation remains pending. These are reported observations, not diagnoses reproduced in this checkout. No private attachments were fetched and no issue resolution is claimed.

The older beta text in #1044 says uncertain association must not lead to a second copy. The later interview explicitly permits duplicates when no reliable association exists, while preserving unmatched files. The interview and this clarification govern this branch; do not transplant the old beta's stricter blocking contract or its implementation.

Revised acceptance: seed preserved ambiguous/malformed historical evidence alongside independently identified selected assets. The current assets must download or reuse safely, persist their outcomes across restart, and reach two quiet cycles without requiring the historical evidence to disappear. Inject a current transfer or state-write failure and require durable recovery before claiming current completeness. Exercise valid current provider-boundary activation, quiet-cloud retries, independent libraries, stable paths and explicit deletion facts while the historical evidence remains preserved. Apply the relevant subset to each bounded slice; do not claim count-receipt removal fixes every historical-state gate.

### Deletion-first design follow-up

Read-only source review recommends replacing the recent engine with one current fact: a zone needs inventory. Provisionally, `inventory_reconciliation_required:<zone>` would live in the existing metadata/checkpoint store. An upgrade would seed it from old receipt existence without parsing or certifying the receipt, preserve the original bytes, and use the upgrade version to avoid reseeding. It would wake the selected zone, use normal inventory, and be retired by current checkpoint activation. This is a candidate representation, not an implemented or approved new durable key. Its purpose is finite current recovery, not tracking whether historical evidence has been repaired.

Reusing other markers unchanged would carry the wrong semantics: matching hashes clear `pending_enum_config_hash`; `enum_in_progress` is cleared at natural EOF before activation; indexing markers invoke a different recovery protocol. Keep the current request with the cycle/checkpoint owner rather than adding a scheduler, acknowledgment ledger or compatibility dispatcher.

The provider boundary remains a concrete implementation decision. Existing first-full inventory can activate a unanimous query-token candidate without an old-cursor bridge (`full.rs:1459-1483`, `sync_cycle.rs:451-520`, `:1577-1595`). Its final-token/EOF checks do not establish a provider-guaranteed snapshot across changing rank queries (`album/fetch.rs:197-204,279-293`, `album/completion.rs:102-168`). Qualify a fresh current anchor plus ordinary inventory and current delta processing, including concurrent changes, instead of assuming either old-cursor replay or simply replacing the token is universally required or safe. Reuse the provider owner; do not introduce another receipt proof layer.

Two current obligations survive any fresh-baseline design: observations need durable recoverable work before their cursor passes them, and previously identified catalog items absent from inventory need targeted current lookup without inferring deletion. The existing retry reader starts from pending rows and does not automatically cover every absent downloaded catalog item. These are bounded current recovery tasks, not demands to make all historical attribution consistent.

The broader historical-blocker removals have concrete targets: `LegacyCycle::begin/certify/finish` and the state checkpoint validator in `state/db/checkpoints.rs:129-147`; all-history debt checks in `state/db/provider_generations.rs:2186-2207`; and historical-count-to-cycle/health errors in `sync_cycle.rs:2384-2440` and `cycle_reporter.rs`. Preserve ownership, unfinished physical destinations and reachable current retries while removing historical authority from those decisions. Raw-inbox retirement remains another explicit deletion boundary because startup replay and live capture can fail before current events reach consumers. Count-policy removal alone cannot claim to solve those gates.

Migration should meet the same current-observation and filesystem safety requirements as a fresh sync, with safe reuse of existing files. It must not impose a stronger history-reconstruction standard merely because an old database exists. No production behavior or state representation changed during this design follow-up.

### Approved prerequisite: pre-migration backup

On 2026-11-12 the user approved a bounded backup prerequisite before resuming slice 1b. Existing owned SQLite state must have a consistent snapshot, including committed WAL data, and matching configuration saved before migration. Publication must be private, durable and no-overwrite. Backup failure must refuse migration; interruption and recovery require focused tests.

The shared database opener owns this boundary because sync and several state commands can migrate before returning the database to their callers. Propagate configuration context through those callers instead of protecting only the sync runner. Fresh and already-current databases should not create unnecessary upgrade backups. Preserve published backups after failure and distinguish incomplete staging from usable recovery copies. Document the pre-media-work restore boundary and any invocation settings not captured by a configuration-file snapshot.

This is an explicitly approved feature-only prerequisite for the count-policy deletion slice. It does not authorize a new migration framework, additional CLI options, media changes, a schema bump solely for backups, or changes to provider checkpoint authority. Implementation and qualification are pending.

The first candidate implements the shared backup boundary and passes focused Linux checks, including process interruption and WAL recovery. Schema remains 36. Independent review is pending. The candidate currently refuses non-Unix old-schema upgrades; this is an unresolved supported-platform regression, not an accepted limitation. Slice 1b remains paused until review and portability are resolved. No commits, live checks or publication have occurred.

Independent review found two acceptance gaps: Windows upgrade refusal breaks a supported platform, and a killed DELETE-mode migration can leave a hot rollback journal that the read-only ownership precheck cannot recover. The existing process-death fixtures cover WAL; ordinary DELETE-mode error rollback does not establish restart recovery. A bounded repair is assigned for DELETE-mode interruption with ownership validation preserved. Windows private creation and durable publication are under source-level assessment; neither gap is waived.

The user separately approved the bounded Windows backend: owner-only protected ACL creation and native write-through, no-overwrite publication, enabling features on the existing `windows-sys` dependency. The implementation may reuse the existing Windows filesystem publication helpers. This approval accepts the stated native publication contract, not a claim of POSIX directory-fsync equivalence or physical power-loss survival. Native Windows runtime qualification remains required and unavailable on this host; the installed Rust target alone is not execution evidence. The backend and DELETE-mode repair have disjoint implementation assignments.

The DELETE-mode repair classifies `SQLITE_READONLY_ROLLBACK` and refuses writable recovery when ownership cannot first be checked. A forced-spill SIGKILL fixture exercises unchanged correct/wrong-account refusals, verified snapshot restoration and two quiet reopens. Its integration test currently awaits generated restore instructions that explicitly preserve `-journal` alongside WAL/SHM and require a matching backup/account. The Windows/root-file owner is integrating that wording; the failing test is not counted as passed.

Integration follow-up: restore guidance now preserves all three SQLite companions and requires a matching backup/account. The Windows backend implements protected creation-time ACLs, lossless tagged UTF-16LE paths and write-through publication of a `.partial` directory without replacement. Additional features are enabled on the existing `windows-sys` dependency; no crate, lockfile, CLI or schema-version addition was made. Error guidance now treats `.partial` directories as incomplete even when they contain a prepared manifest.

The integrated Linux candidate passes `cargo test --lib owned_migration_backup` (9 tests), `cargo fmt --all --check` and `cargo clippy --all-targets --all-features -- -D warnings`. Worker checks also passed schema tests (62), behavioral migration backup (2) and file publication tests (4). Source-bound Windows helper compilation and Clippy passed, but the full Windows workspace check stopped at missing `x86_64-w64-mingw32-gcc` for bundled SQLite. Native Windows runtime acceptance and final independent review remain pending. The prerequisite is implemented, not yet qualified across supported platforms; slice 1b has not resumed.

Final review accepted the DELETE-mode refusal/restoration repair but found a Windows stage-substitution race: publication captured a new directory identity after dropping the original staging guard. The Windows owner is repairing original-identity retention and parent confinement, with a substitution regression. Review also found the CLI/config backup matrix was Unix-only; a separate test-only assignment is enabling equivalent Windows command coverage. Earlier passing checks do not qualify these pending repairs, and native Windows execution remains required.

The CLI/config matrix is now enabled on Unix and Windows, preserving all eight command routes and existing history, WAL, media and refusal assertions. A portable classification regression excludes `.partial` directories even when a manifest exists; no-file cases check quiet reopens. Linux behavioral backup checks pass with default and no-default features (3 each), account-isolation checks pass (2), and formatting/Clippy pass. This closes the source-level CLI coverage gap, not native Windows execution. The stage-identity repair is still in progress.

The stage-identity repair now retains the original populated-directory guard and parent confinement through publication, validates a rename-compatible handle before releasing the no-delete handle, and compares the published identity with the original. Helper and production-opener Windows regressions perform actual substitution; the latter checks source/evidence preservation followed by recovery and quiet reopens. Linux backup tests (9), behavioral backup tests (3), formatting and Clippy pass. Source-bound Windows helper/regression compilation and lint pass; native execution is still unavailable. Independent review of this final repair is pending.

Final independent re-review found the stage-substitution issue resolved in source and no new material findings. It confirmed original-identity retention, handle handoff, substitution regressions and portable CLI coverage. The backup prerequisite is implemented and source-reviewed with passing Linux qualification. Full integrated Windows compilation and native runtime tests remain required before cross-platform acceptance; source-bound helper checks do not replace them. No further Linux rerun was requested because the reviewed production candidate is unchanged. Slice 1b remains paused at this qualification boundary. No commits or pushes were made.
