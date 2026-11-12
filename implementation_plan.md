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
