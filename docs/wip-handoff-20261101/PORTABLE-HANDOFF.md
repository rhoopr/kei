# OpenCode handoff: start here

This is a wind-down snapshot, not permission to release. Read HANDOFF.md, ACCEPTANCE.md and WORKTREE-MAP.md; PORTABLE-HANDOFF.md combines them. The existing repository and scratch packets on CachyOS are the evidence. First recheck HEAD, remote, process state and later owner handoffs; do not rerun or kill an existing job blindly. Source guidance: AGENTS.md, CONTRIBUTING.md, docs/plans/v027-sync.md, tests/README.md and relevant architecture sections. The plan's metadata-writer acceptance wording has an identified correction in progress; ordinary sync must preserve existing media, metadata, timestamps and recorded paths.

At 2026-11-01 18:46 UTC, `/home/robhooper/dev/kei-v027-sync` branch `simplify/v027-baseline-incremental` was clean, HEAD and actual origin both `9a0b5eb58c0a3735ba7bafa28cafc2de27bc9586`. This is a branch checkpoint only, not a beta qualification. No merge, tag, release, PR publication or live-provider operation is authorized by this handoff. Check new owner handoffs, current logs and actual remote before decisions; paths and PIDs below are snapshots, not ongoing-state guarantees.

## Final snapshot and archival branches

At 18:54 UTC all owners and reviewer had stopped; no Cargo, compiler or shared-lock process was observed. Recheck before resuming. Published beta remained clean and remote-matched at `9a0b5eb58c0a3735ba7bafa28cafc2de27bc9586`; no release qualification is claimed.

| Separate WIP branch (not beta-ready) | Verified remote commit |
|---|---|
| `wip/mixed-fixture-unqualified-20261101` | `a1f3020144be24e2a81c2163c6d6b6b43eb5aa80` |
| `wip/retry-unqualified-20261101` | `2343e52a1c1d053befbd4cf065e4ff6400d02eb7` |
| `wip/embedded-owner-unqualified-20261101` | `76a927cceed41fbc30e0f227a4f252bee0599e18` |
| `wip/visible-archive-unqualified-20261101` | `c787bb1fa77c1322943926be7492b6c15406e5b3` |

The Hidden deletion WIP is local-only at `/home/robhooper/dev/kei-hidden-deletion-gate`, base `dd47eca26e882cf4a722ee6702f35bfd72932a93`, tracked diff SHA-256 `c6bd6c8d53773c2ce2d3d51e74f6056e47f682d0fcba7888e9eda88b53153acf`. It was not archived because its production files overlap a prior denied operation and independent scope was not established. See the detailed boundary before acting.

---

# Detailed handoff

## Operating rules and state

Work on CachyOS via the already established authenticated `ssh -o BatchMode=yes cachyos` route; keep sessions short. Tests/builds run there, not on Mac. Do not change SSH/network/security settings or credentials. Use synthetic isolated state/media; never use or expose credentials, private archives or provider data. Do not touch the user's separate `/home/robhooper/dev/kei` checkout. Preserve all worktrees, untracked source, logs, copied qualification binaries, DB/media fixtures, manifests and scratch unless explicitly authorized. No resets, force-push, merges, release/tag, public PR or coauthor trailers. Use `rg`, read applicable guidance and inspect exact owner/consumers. No workaround of an actual denial.

The user authorized reviewed and tested checkpoints on the existing beta branch, not release or merge. Production policy changes require the actual scope of prior approvals and review; do not infer that a passing test authorizes an unrelated change. Ordinary sync preserves existing media, sidecars, bytes, mtimes and paths; explicit refresh, new-download sidecars and recorded-operation recovery have separate contracts. Do not restore obsolete automatic layout convergence or manufacture completion by resetting state. Previous rejected/excluded broad collision investigation stays excluded; no cleanup of synthetic DB/media/logs or ignored scratch without explicit scope.

## Published checkpoints

- `9a0b5eb58c0a3735ba7bafa28cafc2de27bc9586`: sparse double-bridge proof carry, exact diff SHA256 `30549e501db692e7347eed57e087c5cc6892b1b46057a534d3a8bb80b5c7b24c`. Both modes exact sparse test, 14 markers including real inter-bridge target mutation, separate atomic rollback, target authority/pending promotion and generation/reopen controls, strict Clippy/fmt/diff passed on integrated source. See `.scratch/sparse-bridge-integration-ff3d-resume/RESULTS.md`, statuses/logs and copied binaries. Mixed-library fallback remains unresolved and not part of this patch.
- `ff3d24452f11a59ad8ca52286ba22860c099d8cf`: rejected-page replay fixture correction, exact patch `f23b9271169fdf430f525cc1466151b7a125e94d59d7f90d61df8adbf8531650`; both modes collecting=true/false pass. Fixture is unowned synthetic, not authenticated selected-ledger or equal-content cross-child replay proof. `.scratch/checkpoint-replay-fixture/REPORT.md`.
- `5105a9024a51f79e8307f7e45b830a796a4a054e`: URL retry fixture correction; exact old test passes both modes and collision/healthy markers. Synthetic task has no selected publication binding; not selected-ledger proof. `.scratch/url-retry-fixture-v4/REPORT.md`.
- `c5a321d8916782f38323b5e17b41012bfdced295`: one reverted-edit preservation fixture corrected; both modes pass. HEIC/JPEG occupied-path changed-rendition pending remains separate. `.scratch/layout-obsolete-cluster-review-v3/PUBLICATION.md`.
- `983dd4963149193faebb3cc62b1de1a64caa0a97`: reject-only metadata activity preflight; account schema 41 -> 42. Only test callers currently admit activity: production guard is infrastructure, NOT live crash safety, settlement, retirement or writer activation. Scan is scoped but unbounded and point-in-time, not filesystem race fence. Old-binary refusal used a future-version marker, not an actual migrated V6 account. `.scratch/reject-only-integration-check/{ASSESSMENT.md,PUBLICATION.md}`.
- `ec63da04b957f3c69cce993e86df3a9642fffef2`: safe hold after interrupted recorded refresh. Both sidecar variants passed, but successful automatic recovery and actionable CLI guidance remain unproven. `.scratch/review501-safe-hold-clean/PUBLICATION.md`.
- `75c5540ab48418232282709f5c9548a0d56f0ff3`: ordinary-sync preservation and snapshot prerequisite tests. AF reached both debt variants; ND XMP test absent. Existing `review501_explicit_metadata_refresh_uses_preserving_layout_owner` failed `QueryReturnedNoRows` before requesting refresh: source diagnosis says fresh unowned fixture expects new layout convergence now disabled, not evidence of a refresh production defect. Authenticated previously recorded-operation recovery remains unproven. `.scratch/preservation-combined-e475/{RESULTS.md,PUBLICATION.md}` and `.scratch/explicit-refresh-diagnosis/REPORT.md`.
- `e47557894cc593b7fa776febe398c3ea2b53f290`: test-only preservation/scale/preview/service/old-writer and #845 quiet/repeated observation coverage. Empty-changes quiet case avoids early movie GET; equivalent repeated observation still produced two movie GETs before old future deadline in both modes, generation attribution unavailable. This is an unresolved production backoff concern, not fixed by characterization. `.scratch/followon-quiet-split/RESULTS.md` and `.scratch/issue845-integrated-observation/RESULTS.md`.

## Unpublished work: do not conflate with HEAD

All four owners and the reviewer reported stopped at wind-down. Their final handoffs are included in this packet; inspect source manifests and current process state before resuming. Do not apply WIP wholesale or treat a prior-source binary as a final-source pass.

- Production retry/backoff and schema 43: current lane `/home/robhooper/dev/kei-retry-rebase-9a` and related `/home/robhooper/dev/kei-resume-compose-983`; inspect worktree `.scratch` and latest owner packet. Must establish authenticated exact scope/target/source equivalence and retain distinct updated URLs/resources and independent progress. Schema 42 is already published; retry 43 is uncommitted pending review/integration. See `.scratch/identity-schema44-coordination/STATUS.md` for prior mapping, not a claim 43 or 44 has shipped.
- Embedded/sidecar crash and writer safety: multiple candidates, latest visible `/home/robhooper/dev/kei-embedded-rating-owner-v19`, packet `.scratch/embedded-rating-owner-v19/` and older `.scratch/embedded-rating-owner-v16/COORDINATION-CHECKPOINT.md`. V19 has tracked WIP and complete-source manifest; it is not a published fix. Actual publisher participation, cancellation, retained identity, XMP path staging, no-follow and race proofs need exact final-source review and tests. Owner approved direction mentioned in checkpoint: operation-owned private XMP stage, pin post-toolkit output; it is not proof against arbitrary same-user namespace manipulation or Windows. No Created-sidecar settlement/admission retirement/owner-token activation may be assumed authorized merely from diagnostic source. Keep blocked/denied prior operations blocked; consult latest explicit approval and source reviews. Tentative identity schema 44 and retirement 45 depend on actual reviewed 43/44 integration, not reserved published versions.
- Visible archive/recorded-operation recovery: `/home/robhooper/dev/kei-visible-archive-final`, `.scratch/visible-archive-final/`, and `.scratch/visible-archive-packet/NARROW-OWNER-STATUS.md`. The owner’s build ended exit 101; read its final handoff and individual statuses. No claim of accepted automatic recovery or general archive migration.
- Confined owner/exchange primitive: `/home/robhooper/dev/kei-confined-owner-route`, `.scratch/owner-exchange-primitive/`; The owner’s queued primitive build was canceled before it started. Review and qualification remain incomplete.
- Mixed fallback: `.scratch/mixed-fallback-owned/{STATUS.md,CHECKPOINT.md,NEXT-CYCLE-REPORT.md}`. Incomplete delta may trigger full enumeration; owner-backed proof and durable coverage are unresolved. Do not alter checkpoint expectation based on old unowned synthetic fixture or conflate with sparse bridge fix.
- Hidden-only confirmed deletion #1044: selected baseline/service/reopen, exact scoped source/rendition deletion evidence and no data loss remain required. Catalog truth is not selected coverage or checkpoint authority; NOT_FOUND, missing master/zone or inventory absence alone does not authorize deletion.
- #845 sibling diagnostic: `/home/robhooper/dev/kei-sibling-diag2-9a/.scratch/diagnostic/build-worker.sh` was stopped while waiting, before it started. Prior retained synthetic fixture `/home/robhooper/tmp/kei-845-observation-retained/issue845-mO9kk7` and `.scratch/issue845-integrated-observation/RESULTS.md`; do not remove. Both retries/generations and early GETs must remain honestly distinguished.
- Already-admitted ambiguous root repair remains incomplete; prevention subset is published. Historical mixed-version migration, selected identity scope, progress/capacity/health/watch and full beta gates remain partial or unproven; see ACCEPTANCE.md.

## Final job status

At 2026-11-01 18:54 UTC, no Cargo, rustc, Clippy, build-worker or shared-lock process was present. All four owners and the reviewer reported stopped. The verified sibling diagnostic waiter was stopped before compilation; the mixed owner’s previously queued ND compile started before it could be stopped and completed, but no runtime result follows. The visible-archive batch ended exit 101. PIDs and state are time-bound: recheck the system and original logs before resuming; do not restart uncertain jobs blindly.

## Qualification and build discipline

Latest completed whole-suite run `.scratch/five-bars/qual-dd47eca-20261031/HANDOFF.md`: AF 4,880 pass / 31 fail / 80 ignored; ND 4,556 / 24 / 79, both exit 101. Old failure inventory `.scratch/qualification-failures-983/per-test.tsv` contains scoped reruns on older 983; URL-retry, reverted-edit and rejected-page fixtures were subsequently corrected and individually passed. Do not subtract counts and call current head qualified. All remaining failing or stale tests need diagnosis/disposition against current policy and final source. Old review501 failed before refresh due obsolete fixture setup, not proved refresh defect. Exact-head full suites and independent final review are still required after production batches.

Shared Cargo target `/home/robhooper/dev/kei-v027-sync/target`, lock `.scratch/beta-build.lock`, `TMPDIR=/home/robhooper/tmp`. Guard at least 15 GiB available before build/Clippy. One target per active validation batch; serialize compilation/Clippy and shared target mutation under lock, compile once per mode, copy immutable binaries and hash while locked, list exact tests and require nonzero results/markers, run isolated fixtures outside lock. Prior shared target returned stale executables despite `fresh`; require demonstrable compilation/source provenance, full source before/after manifests, artifact hashes and test-list checks, and use a private target only when shared attribution cannot be established. Never edit frozen source during a qualifying run. Process-global reach markers require separate isolated exact test processes.

At the final 18:54 UTC snapshot `/home` was Btrfs on Samsung 970 EVO Plus NVMe SSD, about 83 GiB available; not tmpfs. Approved cleanup removed six historical target dirs and nine superseded binaries; exact hashes/paths in `.scratch/approved-reclamation-20261101/deletion-manifest.tsv`. Previous narrow incremental deletions are documented in conversation. Do not recreate deleted old targets merely for audit or delete any other artifacts without scoped authorization. Retain current/final copied binaries, manifests, logs and fixtures. Coordinate large builds serially and monitor headroom; do not silently bypass guard or remove shared lock.

## Safe read-only commands

```
ssh -o BatchMode=yes cachyos 'git -C /home/robhooper/dev/kei-v027-sync status --short --branch'
ssh -o BatchMode=yes cachyos 'git -C /home/robhooper/dev/kei-v027-sync rev-parse HEAD'
ssh -o BatchMode=yes cachyos 'git -C /home/robhooper/dev/kei-v027-sync ls-remote origin refs/heads/simplify/v027-baseline-incremental'
ssh -o BatchMode=yes cachyos 'df -h /home; fuser /home/robhooper/dev/kei-v027-sync/.scratch/beta-build.lock 2>/dev/null'
```

For status use named owner log/exit/PID artifacts, `ps` and current worktree diffs. Verify any uncertain push via `ls-remote` before retry. Do not print secrets, private files, account identifiers or unredacted provider payloads. More exact commands and prior test hashes are in the cited packets.

## WIP archival authorization

The user subsequently authorized committing and pushing appropriate WIP for resumption. Preserve unqualified work only on clearly labeled separate WIP branches, never on `simplify/v027-baseline-incremental`; no merge, tag, release or force push. Do not commit any source or action that remains expressly denied. Freeze and identify each owner’s exact tracked and required untracked source before archiving; exclude build outputs, synthetic databases/media unless normal checked-in fixtures, private logs, credentials and large scratch. Mark each commit/branch unqualified with its exact base and dependencies; preserve tests, failures and review limits in the handoff. Inspect actual remote before retrying an uncertain push. Source-specific ownership and denial boundaries must be resolved before archiving that source; user authorization for WIP preservation does not supersede an express denial.

## Parent coordination addendum

The original plan at `ceb4cd1` excludes automatic rewriting/backfilling of completed metadata, timestamps or recorded paths. The user reaffirmed this on November 1. New downloads, explicitly requested refresh and already-authorized unfinished work remain valid. Preserve existing hardlink bytes/inodes/mtime and genuinely unresolved evidence; general hardlink expansion is not a requirement. Identical authenticated same-file/URL/ownership retries must keep their wait; a changed resource is evaluated separately. Hidden deletion is confirmed-identity catalog truth only, never local deletion; parked `/home/robhooper/dev/kei-hidden-deletion-gate` WIP remains open pending owner APIs.

Original dd47 qualification AF 4880/31 failed/80 ignored, ND 4556/24 failed/79 ignored. Targeted 983 rerun reported 30 AF / 23 ND old tests failed; one removed historical test is not a pass. The three fixture fixes `c5a321d`, `5105a902`, `ff3d244` and production sparse fix `9a0b5eb` were subsequently published. There is no current full-suite pass and absent CI is not green. See `.scratch/qualification-failures-983/REPORT.md` and `.scratch/backlog-partition-5105/REPORT.md`.

Remaining gates include real embedded/sidecar crash lifecycle; retry identity/waits, component progress and bounded charged retention; Hidden catalog deletion; recorded Primary/Shared paths and upgrade fixtures; independent progress across restarts/config; bounded provider requests, checkpoint and health; old failures; final combined qualification/review/draft PR. GPS explicit refresh is a separate sidecar owner-authority gap; do not weaken its guard. Mixed fallback: a missing descriptor alone is intentional; a speculative mid-enumeration token race is unverified provider timing, not proven loss. General hardlink expansion, global filesystem census, universal external rename guarantees and permanent history for all completed 410s are not requirements. Synthetic upgrade fixtures do not prove real-user archives. The blocked source write `into_lease` remains excluded; the user did not approve a bypass.

All current workers are to stop after handoff and not resume automatically. Do not start new work simply because this packet contains commands; first inspect owner final handoffs and explicitly establish what the user wants resumed. One shared target per active batch, final-only copied binary retention, 15 GiB guard and shared lock for compilation/Clippy limit storage growth. Six old targets plus nine superseded executables were user-approved and removed, freeing approximately 62.4 GiB; manifest `.scratch/approved-reclamation-20261101/deletion-manifest.tsv`. No broad deletion authorization follows.

## Final wind-down update at 18:52 UTC

The owner-identified sibling diagnostic wrapper PID 3511609 and waiter 3511611 were verified by exact command, parentage, empty child list and kernel lock-wait state, then stopped before their build began. Do not treat its `waiting` status file as a live process or a test result. The mixed fixture waiter acquired the lock before it could be stopped and its ND Cargo compile subsequently finished; no runtime test or review conclusion follows. Preserve `.scratch/mixed-fixture-fix/RESUME-HANDOFF.md` and `build-nd-cargo.log`; inspect binary/source provenance and owner outcome before any retry. At 18:52 UTC no Cargo/rustc/Clippy/shared-lock process was seen. PIDs and state are time-bound; recheck before work. The visible-archive owner reports its batch terminated exit 101, AF focused 7/10 pass and 3 fail, AF strict Clippy failed; ND built but tests and Clippy were not run. No new qualification claim follows.

Source-only archival branches, separately based and unqualified:
- `wip/mixed-fixture-unqualified-20261101` = `a1f3020144be24e2a81c2163c6d6b6b43eb5aa80`, base 9a0b5eb. Test-only; includes frozen untracked fallback module; no passing current-source runtime qualification.
- `wip/retry-unqualified-20261101` = `2343e52a1c1d053befbd4cf065e4ff6400d02eb7`, base 9a0b5eb. Exact frozen retry source f81a667c; exact-source Clippy failed and no runtime tests; excludes known unsafe sole-precheck and denied lease extraction. Schema 43 unintegrated.
- `wip/embedded-owner-unqualified-20261101` = `76a927cceed41fbc30e0f227a4f252bee0599e18`, base 983dd49, includes untracked modules and composed unpublished 43/44 source; owner seal uncompiled/untested. Denied `into_lease` source was never executed and is absent. This is not rebased to current beta.
- `wip/visible-archive-unqualified-20261101` = `c787bb1fa77c1322943926be7492b6c15406e5b3`, base 9a0b5eb. Owner-filtered patch 13a1736f is not identical to tested full source and has not itself been built/tested. AF full candidate had 3 failures and strict Clippy failed. Excludes denied-target consumer.

A retained synthetic SQLite inspection for the visible-archive cross-binary consumer was rejected by automatic approval review for possible private path metadata; the read did not execute. Do not inspect that retained target, copy it, run its consumer, or obtain the data indirectly. The locally retained owner handoff names the denied target; the remotely shared archival handoff redacts the path. A new route or elapsed time is not authorization. Another denied `into_lease` write likewise remains prohibited. No WIP branch is beta qualification or permission to bypass either boundary.

## Parked Hidden source: local-only, not archived

`/home/robhooper/dev/kei-hidden-deletion-gate` is detached at `dd47eca26e882cf4a722ee6702f35bfd72932a93`; its complete tracked diff SHA-256 is `c6bd6c8d53773c2ce2d3d51e74f6056e47f682d0fcba7888e9eda88b53153acf`, with no non-ignored untracked source at inspection. It is local-only and unqualified. A prior exact production deletion/revalidation routing and settlement edit was rejected before execution; its scope overlaps the parked WIP’s changed production files, and archival independence from that denial was not established. Therefore no Hidden source branch was pushed. Preserve the local tree and `.scratch/hidden-deletion-gate/{DENIED-PROPOSAL.md,CHECKPOINT.md,ORDERING-TEST-HANDOFF.md}`; do not recreate or route around the denied operation. Determine scope and obtain appropriate clearance before any archival or integration. Hidden-only confirmed deletion remains open, pending owner APIs.

---

# Acceptance gates at the wind-down snapshot

No gate below is a release pass. Updated source: `.scratch/acceptance-matrix-e475/MATRIX.md`, with subsequent checkpoint evidence in HANDOFF.md. The `docs/plans/v027-sync.md` metadata auto-writer acceptance wording is under correction; the original ordinary-sync no-rewrite preservation contract governs.

| Gate | Status and remaining outcome |
|---|---|
| Crash, embedded/sidecar and journal cleanup | Unproven. Complete reviewed physical-writer coordination, cleanup race, interruption/reopen and preserved bytes/debt; dormant schema-42 reject-only preflight is not live protection. |
| Identical repeated observations and backoff | Open. Two early movie GETs before the prior future deadline were observed in synthetic equivalent observations. Integrate and qualify an approved exact-equivalence fix without suppressing genuinely changed work. |
| Historical mixed-version Primary/Shared reuse and preview | Partial. Synthetic two-rendition receipts and source-built old-writer control pass; representative full scoped migration, preview parity, actual eligible identities and no duplicate GET across reopen remain. |
| Sparse/unselected/foreign progress | Partial. Sparse double-bridge fix published and tested; prove remaining authenticated foreign/participant boundaries and bounded durable work while independent work proceeds. Mixed fallback is separate and unresolved. |
| Hidden-only confirmed deletion | Unproven. Selected baseline/service/reopen must record explicit scoped deletion/expungement for both renditions, maintain independent progress and preserve local files. |
| Capacity and quiet cost | Partial. Synthetic near-512 MiB and 1,024-item tests pass; representative bounded provider/state work and legacy-owner retirement remain. No 41,318-item result or extrapolation. |
| Checkpoint/config/health/service | Partial. Demonstrate actual normal-cycle transition faults, atomic promotion or concrete hold, quiet reopen, cancellation and distinction of incomplete backup from liveness. Synthetic 503 and service watch are not full #955/#958 resolution. |
| Existing metadata and recorded-operation recovery | Partial. Ordinary preservation test and bounded safe hold passed; authenticated successful recorded recovery and actionable CLI guidance are not proved. Obsolete review501 fixture failure predates explicit refresh call. |
| Full exact-head qualification | Unproven. Resolve/disposition old failure inventory; run focused and broad suites, both feature modes, strict checks and applicable platform/provider/service gates on one stable head; perform independent final review. Earlier dd47 suites failed, not current-head counts. |

Deferred/excluded: #1049 manifest deletion export; general hardlink and authentication expansion; unapproved broad collision probes; actual reporter archive/live access without authorization. No full beta or release readiness claim.

---

# Worktree and evidence map

- Published destination: `/home/robhooper/dev/kei-v027-sync`, branch `simplify/v027-baseline-incremental`, snapshot HEAD `9a0b5eb58c0a3735ba7bafa28cafc2de27bc9586`. User's separate `/home/robhooper/dev/kei` is not the destination and must be left alone.
- Published sparse integration: `/home/robhooper/dev/kei-sparse-bridge-integration-ff3d`; `.scratch/sparse-bridge-integration-ff3d-resume/RESULTS.md` and binaries/statuses. Prior source `.scratch/provider-progress-5105/STATUS.md`; old diagnostic/failed snapshots remain evidence.
- Retry WIP: `/home/robhooper/dev/kei-retry-rebase-9a`, `/home/robhooper/dev/kei-resume-compose-983`. Consult their current owner handoffs and source hashes. Do not assume schema 43 integrated.
- Embedded and crash WIP: `/home/robhooper/dev/kei-embedded-rating-owner-v19` and earlier v16-v18; packets `.scratch/embedded-rating-owner-v19/`, `.scratch/embedded-rating-owner-v18/`, `.scratch/embedded-rating-owner-v16/COORDINATION-CHECKPOINT.md`; `.scratch/identity-schema44-coordination/STATUS.md`. Do not infer final writer capability, identity proof or schema 44 from partial controls.
- Visible archive WIP `/home/robhooper/dev/kei-visible-archive-final`, packet `.scratch/visible-archive-final/`; confined-owner WIP `/home/robhooper/dev/kei-confined-owner-route`, packet `.scratch/owner-exchange-primitive/`; sibling diagnostic `/home/robhooper/dev/kei-sibling-diag2-9a`. Inspect current process/log status and owner handoffs first.
- Prior failed #845 observation remains `.scratch/next-reviewed-packet/VALIDATION-RESULTS.md` and `.scratch/issue845-integrated-observation/RESULTS.md`, synthetic DB/media `/home/robhooper/tmp/kei-845-observation-retained/issue845-mO9kk7`. The later empty-quiet/repeated split is `.scratch/followon-quiet-split/RESULTS.md`.
- Old broad failure inventory `.scratch/five-bars/qual-dd47eca-20261031/HANDOFF.md`, `.scratch/qualification-failures-983/per-test.tsv`; old counts are not current-head outcomes.
- User-approved storage cleanup manifest `.scratch/approved-reclamation-20261101/deletion-manifest.tsv`; preserve remaining outputs. A full read-only registered worktree list at snapshot is `WORKTREES-ALL.txt`. This list includes user or stale worktrees and does not imply permission to edit/delete any of them.

All `.scratch` paths above are under `/home/robhooper/dev/kei-v027-sync` unless an absolute path is given. Snapshots may age while in-flight owners finish. Check current status and use exact source/binary hashes from each packet before drawing conclusions.

- Parked Hidden WIP: `/home/robhooper/dev/kei-hidden-deletion-gate`, base `dd47eca26e882cf4a722ee6702f35bfd72932a93`, tracked diff SHA-256 `c6bd6c8d53773c2ce2d3d51e74f6056e47f682d0fcba7888e9eda88b53153acf`; local only due unresolved prior denial boundary. See HANDOFF.md.

---

# #1044 requirements inventory — prior read-only research, 2026-10-31

Source: https://github.com/rhoopr/kei/issues/1044 and its three comments; direct historic reports #845, #853, #1024, #953–#958 and umbrella #765 were researched. No private attachments or fresh code qualification. Historic closure is not proof. Hidden-only smart-folder coverage is supported; do not substitute unrestricted Unfiled for it. Maintainer clarification: https://github.com/rhoopr/kei/issues/1044#issuecomment-6496878741 covers identity/bridge plus legacy holds and missed deletion including both Live Photo renditions and held replay. Inventory absence alone is not deletion proof. Confirmed provider deletion updates catalog, never removes local media. Manifest reporting #1049 is explicitly deferred.

Beta outcomes: fresh selected-library/local baseline without trusting old checkpoints, later incremental changes, evidence-qualified reuse, visible pending uncertainty with no overwrite or duplicate escape, quiet retries, no normal raw-response shadow store, bounded repeated work and durable unresolved evidence. Preserve existing media, metadata, paths and timestamps; no resets/deletions to force completion. Beta on a complete copy; stable remains primary backup recommendation. Rollback requires matching pre-upgrade state, config, media and old binary.

## Direct reports

- [#845](https://github.com/rhoopr/kei/issues/845): duplicate downloads, companion stem/date/pass drift, pending 410 and expiring URL issues. Preserve files/paths/times and receipt history, prove selected resource and destination ownership, preserve valid completed evidence despite pending status, never guess historical ownership from equal bytes. Verify tracked-still companion stems, alternate-pass receipt reuse, qualified fresh URL retry, bounded repeated 410/auth behavior and quiet reopen. Latest v0.26 report: 759 genuinely new files, no new duplicate/suffix files, retries 604→0, but metadata/sparse holds and long scans persisted: https://github.com/rhoopr/kei/issues/845#issuecomment-6382272826. Earlier bounded L1 preserved publication time/hash/94 receipts but did not prove zero media requests: https://github.com/rhoopr/kei/issues/845#issuecomment-6099509899.
- [#853](https://github.com/rhoopr/kei/issues/853): Hidden/shared identity, mixed owners and upgrade duplicates. Do not reparent ambiguous historical master rows to surviving siblings, infer ownership from equal bytes, equate provider signatures with local SHA256 or manufacture source receipts. Preserve original rows, dates, sidecars, all siblings and hardlink bytes/times; current-child coverage and legacy attribution remain distinct. Real upgrade/archive and 41,318-path claim are not proved by synthetic tests. Explicit hardlink-opt-in report: https://github.com/rhoopr/kei/issues/853#issuecomment-6303442347; reporter path counts and mapping hypothesis: https://github.com/rhoopr/kei/issues/853#issuecomment-6388935005. The separate two-account report is not proved the same issue.
- [#1024](https://github.com/rhoopr/kei/issues/1024): held replay filled a 512 MiB shadow inbox. Bound durable state and repeated work without deleting unresolved evidence or falsifying cursor completion; recover useful current work on full-state upgrade without reset. Dedupe/prune commands are proposals, not mandated implementations. Refilled 511.8 MiB / 703 pages / 52 cursors: https://github.com/rhoopr/kei/issues/1024#issuecomment-6409190823.
- [#953](https://github.com/rhoopr/kei/issues/953): former Shared Photo Library sparse references and strict `NUMBER_INT64 isExpunged=0` parsing. Missing zone/master, malformed fields or empty search are not deletion proof. Require authenticated account/zone/owner/child/master evidence, durable bounded holds and useful independent work; no manual retirement. Operational context: https://github.com/rhoopr/kei/issues/953#issuecomment-6221419351.
- [#954](https://github.com/rhoopr/kei/issues/954): preserve ambiguous historical families and originals/sidecars before and after receipt, without guessed attribution or broad sync abort. A claim is releasable only with exact mapped independently proven child; preserve malformed evidence, metadata debt, sidecar collisions and foreign-owner holds. Existing production success followed manual recovery and is not universal migration proof: https://github.com/rhoopr/kei/issues/954#issuecomment-6382196680.
- [#955](https://github.com/rhoopr/kei/issues/955): incomplete cycles led to health-driven Kubernetes restarts, not a demonstrated engine crash. Separate incomplete-backup status from liveness; preserve diagnosis and matching rollback state. No final reporter qualification.
- [#956](https://github.com/rhoopr/kei/issues/956): another instance's files cannot discharge source identity. Require current authenticated account/zone/owner/child/master/resource/selection proof; do not use unselected discovery as proof. Maintain generation-checked debt and independent progress; auth failure stops lookups. Explicit deleted child+master can be identity-only proof, not normal download admission.
- [#957](https://github.com/rhoopr/kei/issues/957): checkpoint/config activation must be atomic after validated current coverage/replay; incomplete replay holds with an actual reason and unchanged reopen is incremental. Pending config is not necessarily unstable hashing; inventory totals are not proof.
- [#958](https://github.com/rhoopr/kei/issues/958): bound repeated provider/catalog work, expose phase/progress without private IDs and preserve started writes and incomplete work through cancellation/reopen; no universal shutdown deadline. Long traced hydration: https://github.com/rhoopr/kei/issues/958#issuecomment-6251346501; later sparse/inbox holds: https://github.com/rhoopr/kei/issues/958#issuecomment-6375014507.
- [#765](https://github.com/rhoopr/kei/issues/765) is umbrella context, not an extra duplicate gate. Zero failures alone does not prove complete backup.

## Later #1044 and linked boundaries

Kuhjosw's independent sparse and legacy holds must remain separate and bounded: https://github.com/rhoopr/kei/issues/1044#issuecomment-6459938813. Mmenanno's confirmed deleted/expunged original and motion were missed while replay lagged: https://github.com/rhoopr/kei/issues/1044#issuecomment-6486280375. Baseline must recover confirmed pre-baseline catalog deletion without deleting local media. [#1049](https://github.com/rhoopr/kei/issues/1049) is deferred read-only manifest reporting; no invented timestamps or required filter flags.

[#884](https://github.com/rhoopr/kei/issues/884) broader hardlink safety remains separate; protect existing inode bytes/timestamps but do not promise general new hardlink support. [#924](https://github.com/rhoopr/kei/issues/924) mixed Unfiled/Hidden bridge is distinct from no-Hidden reports: preserve actual successor and atomically activate only after valid coverage. [#643](https://github.com/rhoopr/kei/issues/643) siblings sharing a master must both emit; [#773](https://github.com/rhoopr/kei/issues/773) companion naming follows the verified tracked still without silently renaming old media. [#719](https://github.com/rhoopr/kei/issues/719) is older liveness context, not proven #955 cause. [#937](https://github.com/rhoopr/kei/issues/937) authentication diagnosis is separate from the beta sync-engine gate. #794, #847, #855, #857, #869, #870, #881, #882, #902, #922, #923, #928, #933, #952, #961, #962, #970, #971, #973, #998, #1005, #1012 and #1015 are implementation/diagnostic context, not independent user acceptance demands.

---

# kei beta retry acceptance: OpenCode handoff

## Start here

Resume read-only first. The user requested wind-down; do not merge, push, tag, release, delete evidence, or edit the main worktree. Use the connected Mac solely as the SSH bridge to the existing authenticated multiplexed master: `ssh -o BatchMode=yes cachyos`. Do not force a new connection, alter network or credentials, run builds on the Mac, use live accounts or private archives, or assume equal bytes establish authenticated ownership. Read `/home/robhooper/dev/kei-v027-sync/docs/plans/v027-sync.md`, `.scratch/branch-audit-1044/AUDIT.md`, repository instructions and relevant `.agents/skills` again as needed. The main repository is `/home/robhooper/dev/kei-v027-sync`; the most recently reported published head is `9a0b5eb58c0a3735ba7bafa28cafc2de27bc9586` (verify read-only before use). All modifications belong in isolated worktrees. Coordinate every compile and Clippy invocation using `/home/robhooper/dev/kei-v027-sync/.scratch/beta-build.lock`; freeze complete source and manifest before a qualifying build, copy/hash immutable binaries, and execute them with isolated state outside the lock. Do not edit source during a qualifying build. Preserve all files and failed/uncertain evidence.

First inspect the status of the owned queued diagnostic job described below, current disk headroom, and main/head without disrupting other workers. Then decide how to integrate the 9a retry candidate with a correctly authenticated sibling scheduler and fix the changed-master reference handoff. Do not integrate the unsafe sole-precheck WIP or treat the existing passing tests as final qualification.

## Current worktrees and source identities

- `/home/robhooper/dev/kei-retry-rebase-9a`: detached HEAD/base `9a0b5eb58c0a3735ba7bafa28cafc2de27bc9586`. Dirty full `git diff --binary HEAD` equals `.scratch/args-integrated-frozen/combined.patch`, SHA-256 `f81a667cfbafa8f5e18b193821065e7d62c417af3f677279a368232207b15d2e`. Complete frozen source manifest SHA-256 `a39b18406085efcf89c7e1fd6498f25d250c8059ed1f3d14229e7db4797bf22d`. This integrates reviewed typed ordinal refusal and eight reviewed argument groupings, but not a sibling scheduler or changed-master fix. ND immutable binary `.scratch/args-integrated-frozen/nd-bin` SHA-256 `1d6fd6f277a7f5fad44868e259c135e02b24c862ad91e4e08bf7deddf42917ea`. Compile passed; exact-source Clippy failed and no runtime tests ran on this revision. The complete dirty-file inventory is represented by its frozen patch and source manifest; verify both before editing, including untracked files.
- `/home/robhooper/dev/kei-component-scheduler-9a`: HEAD `9a0b5eb58c0a3735ba7bafa28cafc2de27bc9586`, dirty reconciled 9a source plus WIP edits in `generation.rs` and untracked `immutable_epochs.rs`. `.scratch/sole-precheck/sole-precheck-wip.patch` SHA-256 `985a4fababe7c4acd0bc6f01226f93da341baa18b8bf4c48ffe3a9dd8646ea8d`; preimages `.scratch/sole-precheck/before`. **Do not integrate:** independent review found its due-only candidate query can miss a different future-deadline decision with a runnable never-dispatched sibling. Correction was not written. Pre-edit job PID 3465762 exited 0 and is no longer running.
- `/home/robhooper/dev/kei-sibling-diag2-9a`: HEAD `9a0b5eb58c0a3735ba7bafa28cafc2de27bc9586`, dirty frozen manifest SHA-256 `0f2357ee481b0803df80b32b6eb13c8f6afa3838a7fda3e1e134120540f93ac1`; test-only diagnostic delta `.scratch/diagnostic/pending-terms-v1.patch` SHA-256 `196e89ff842392a66df7220dd0bae19c4e02df62c1367012ef60cc0eece2a5dc`. This adds sanitized capped predicate counts, not production behavior, and is unbuilt/unrun at last report. Owned ND compile PID **3511609** remained alive/queued, `.scratch/diagnostic/status=waiting`; it was not cancelled. Check its current status and lock before any action. Do not kill shared or unowned processes.
- `/home/robhooper/dev/kei-master-control-9a`: HEAD `9a0b5eb58c0a3735ba7bafa28cafc2de27bc9586`, dirty reconstructed safe-lint source manifest SHA-256 `750edb3af22e4f57b02e922ffa7c06213bbd33527c2ff936861c8b7e5b664f85`; no changed-master fixture edits were made.
- `/home/robhooper/dev/kei-resume-recovery-control`: HEAD/base `983dd4963149193faebb3cc62b1de1a64caa0a97`. Tracked unstaged diff SHA-256 `70302b1470507998414c56a6894203db80417530995cc1d4cdad5d1b790d4731`; staged diff empty. This hash **does not cover numerous preserved untracked source files**. Frozen deliverables/manifests remain under `.scratch`. No owned build was active at wind-down.
- `/home/robhooper/dev/kei-retirement-983`: diagnostic retirement draft `.scratch/retirement-v44/bounded-reference-wired-wip-v4.patch`, SHA-256 `212b30dc9a2df5afc47897d57ea3bda9783c0edbfffda3560948860dc34be21b`. **Do not integrate as schema 44**: it remains unqualified, does not solve general finite retirement, and conflicts in numbering with an unpublished identity-index proposal.
- `/home/robhooper/dev/kei-retirement-test-c1`: source-only threshold fixture `.scratch/reference-controls/real-65-410-setup-wip-v4.patch` SHA-256 `46681b0953e9b88856801a000f00f418a19d25d534f88511b688148f132f5e71`; it does not establish the actual 65-cycle reachability, full quota effect, populated upgrade, or an unrelated real wait. Another test-only barrier `.scratch/reference-controls/retirement-barrier-only-v1.patch` SHA-256 `2595d98a025d04bad49d509c0c5ace8dc35027958a6e87db45173c717db86a9f` pauses before the SQLite writer but retains the destination staging guard; it is source-only, and its singleton requires serialized tests.

## Implemented and observed

The 9a reconciliation preserves the published sparse double-bridge guards, bridge proof propagation, and existing tests, according to independent source review. The reviewed typed ordinal refusal distinguishes a corrupt source ordinal from optional missing proof. It propagates `InvalidSourceOrdinal` through a source-bearing wrapper and `ShadowPageError` so dispatch classifies capture refusal; the staged stream stops before raw ingestion or cursor advancement. On `.scratch/ordinal-integrated-frozen/` manifest SHA-256 `a3c82a86c83f28295916af1ca29172f1f2b0aa768478d6f1f16d6a17084d0ca1`, seven exact ordinal and sparse-bridge controls passed in **both ND and AF**, one test reached each, using strict independently reviewed isolated runners. ND binary SHA-256 `2fd66b4e2972af0f99a156d85030faf88c8ffae708f360bf7a900bd3bcb97858`; AF `7b0029e328a3a7b43a3d7ace5ceb066a79cec30dc58605d4b687bf223e3e963a`. Evidence `.scratch/ordinal-integrated-frozen/reviewed-synthetic-{nd,af}/`. These do not qualify the later args-integrated source.

Earlier coherent retry evidence, base 983, is in `/home/robhooper/dev/kei-resume-compose-983/.scratch/coherent-resource-frozen/`: eight focused retry, URL/resource, 503-accounting, cancellation, recovery, v42 and v43 controls passed in ND and AF on source manifest `b8b369c1a0d8367cbcf5636a3678469e9660e5f33d7c8b679c554368bcbbb0bc`. Same-tag refreshed-URL and changed-resource pages are explicitly **synthetic coalesced-current approximations**, not proof of iCloud event ordering or byte-exact replay; the 503 marker says scheduler was not run. An additionally approved clean ND run at `approved-coherent-eight-q3q9jtff/` accidentally targeted this earlier binary `9fff5c4198fd6d0d28662c62e2d9684d87fb67aaaf4115f51ae3e889be042075`, not the later intended inner-opaque binary. Preserve it as earlier-source evidence only.

Typed invalid-route refusal was implemented in the earlier packet, with checks classified as `CaptureRefused` rather than static/full inventory fallback. Selected relation-stage actual outer-cycle control passed ND on `/home/robhooper/dev/kei-resume-compose-983/.scratch/outer-route-integrated-frozen/`, source manifest `83e9e9d308af2c6ecf581640a558ad7a650e6803a4fa56def37a868ff5a60629`, binary `d3954b352ede372fa6e9a25261b9e98c2c6b265dd99def04f4982c547574c49a`, clean logs `reviewed-synthetic-nd/`; it retained the synthetic checkpoint and earlier relation effect. Ordinary event-stage outer-cycle control passed ND on `.scratch/ordinary-oracle-frozen/`, manifest `28c111e4d3b039c8bd9e547f8219175342a8b92c40c1fb7f60b9856f394ee546`, binary `7bb505f8c57345ed57904c625695d1cdbd423f1180bf126afa6b4152d4076bf4`, log `reviewed-synthetic-nd/ordinary.nJPi4oOE/test.log`. It retained debt identity and checkpoint while normal preflight reset failed debt to pending; the two earlier CheckIndexingState queries are expected. The task-local fallback counter covers only instrumented Safe incremental error branches, not every possible full/reconciliation path. Relation-stage test uses a Value-only synthetic session, not authenticated raw-source proof.

The real selected never-dispatched sibling reproduction ran ND on `/home/robhooper/dev/kei-sibling-batch-9a/.scratch/frozen/`, binary SHA-256 `27da6636fe6f929be3057a9b7cbc68bf17e4df0f49d97f028d13cf27610fdc90`, source manifest `28e0f3756ca37c99b55865529b91b6c071124ada7f99bc64432183afcbb3a9d5`. Both tests reached one and **failed** watch-precheck `SkipAll` expectations (`Proceed`). Positive had already reached a real committed still 410, authenticated admitted movie `never`, abort and guard drain, reopen, unchanged future owner/no repeat still GET, movie GET 1 and receipt/bytes, independent GET 1 and receipt/tree. Negative reached exact authenticated future shared wait, old attempts 1/new 0, no repeated-cycle movie 410, independent GET 1 and two observations; later due assertions were not reached. Follow-up diagnostic `/home/robhooper/dev/kei-sibling-diag-9a/.scratch/diagnostic/`, binary SHA-256 `2047fd6984fa5c16802990cdaba0b62b0067bc16a03c4e338aaafe4c1f0e4751`, logs `never-sibling.eIz4XQQy/test.log` and `own-future-wait.g98Q3w2e/test.log`, established `has_pending_selection` on current root returned true; recent recovery and retained-root branches were false. That is an OR across deferred, media/metadata and refreshed-inflight; exact term/row is not yet known. The queued diag2 job above was intended to print sanitized capped term counts. Fixture uses unlimited download attempt cap and local precheck, not full watch loop, and does not prove previously dispatched/503 sibling behavior.

The legacy Value-only full-date reproduction remains a concrete failure: after re-enumeration it observed two distinct retry generation rows and a second movie GET while the still stayed at one GET. Its source adapter cannot authenticate original response bytes, so matching parsed facts do not prove this meets the approved exact shared-wait policy or establish reporter root cause. A genuinely quiet changes fixture is still unproven: held work prevents promotion of `current-1`; an empty response from retained `current-0` would omit the uncommitted observation.

## Open correctness and static work

1. **Independent sibling scheduling:** `has_due_selection_work` is active through `sync_loop/precheck.rs`; do not repeat the retracted claim that it is uncalled. Child-wide deadline filtering in retained-root/page selection and producer confirmation can hide independent component work. A naive `state=never` hint can instead falsely wake a component with its own authenticated future shared wait. A proposed paid scope revision and component wake projection was not implemented. It needs complete semantic mutation invalidation, exact current asset/policy/planner and wait proof, bounded/fair local rebuild/continuation, quota accounting and already-full-ledger upgrade, no busy-wake, and final transactional/no-follow dispatch revalidation. Existing `replay_after` is a replay-order key without mutation revision/completion proof; do not treat it as scheduler authority. A 503/dispatched sibling needs exact terminal outcome and policy evidence, not just `dispatched` state. Do not treat the unsafe sole-precheck WIP as a solution.
2. **Changed-master reference handoff:** source review found live `reference_refresh.rs` unconditionally deletes from `provider_transfer_observations`, for which no `CREATE TABLE` appears in source; `generation.rs` can call this transaction. This is source-backed, not runtime-observed. Merely skipping the missing DELETE or creating a dummy table is unsafe: the transaction rewrites predecessor manifest/attempts and deletes mutable transfer rows while retained compact observations, immutable 410/shared waits and refreshed tickets/receipts can bind prior proof. Parent-only triggers protect DELETE, not necessarily predecessor UPDATE. Audit target and source/reverse references, unexpected/corrupt/orphan evidence and schema integrity; hold before mutation if not provably safe. Existing `changed_reference_sigkill_transfers_new_bytes_without_repeating_completed_motion` covers actual handoff, but not retained 410/shared-wait ownership. Production-path clean-handoff and evidence-bearing rollback/independent-progress controls are still needed.
3. **Clippy:** exact args-integrated check `cargo clippy --locked --offline --all-targets --all-features -- -D warnings` exited 101 with 26 library/18 test errors; logs in `.scratch/args-integrated-frozen/`. Eight argument groupings and companion call sites were source-reviewed and compiled, but no runtime was run on that exact packet. Remaining include live projection APIs with too many arguments, unused `CurrentPassPolicy` and predecessor/provenance graphs, and test helpers. Do not wire pass-level policy merely to silence lint: it does not prove current asset/path eligibility. Do not suppress or delete machinery purely for lint. Independently classify speculative uncalled wrappers vs capabilities needed for sibling/recovery. `selection_work_due` and `decision_has_due_work` are related to the open component scheduling gap and are not disposable solely because unused. The old uncalled `bind_observation_and_waits` refers to the absent observation table and must not be activated as a lint fix; its underlying active validation primitives have live callers.
4. **Bounded retirement:** separate capacity/release gate, not a prerequisite to claiming a correctly proven retry checkpoint if independently scoped. The plan calls for bounded state and retirement of completed evidence; permanent reconstruction of every completed 410 was incorrectly inferred earlier and is **not** a stated requirement. Preserve unresolved work, live references and compact retry authority. Draft v44 needs an exact ordered forward migration, bounded complete reverse-reference audit (including corrupt/unknown entries), current obligations, attempt/deadline dominance across clock rollback, quota-atomic accounting, real unrelated-wait progress, populated upgrade and crash/reopen controls. Draft limits ≥65 waits, malformed unrelated evidence, 65 distinct-generation histories and verified targets; it is not general finite GC. No real state was deleted.

## Schema coordination

Published crash prerequisite is schema 42; retry uses 43 after that exact published 42. The main shared packet `.scratch/identity-schema44-coordination/schema44-ddl.txt` and `.scratch/identity-index-43-preflight-v2/STATUS.md` describe an unpublished identity-index prototype, combined patch SHA-256 `99441f65c5a85362ae29772b755e7b99e9f6ca460086d9bd15894d7e0c7e1bde`. It is explicitly **DO NOT INTEGRATE** and has unresolved identity/owner/completion blockers. Coordinate the actual embedded identity-index owner and stable source before choosing the retirement version; likely order 42 → retry 43 → stable identity migration → retirement next free version, but do not assume or overwrite two incompatible 44 migrations. Test populated upgrades/rollback and both index sets.

## Approval and execution boundaries

The user explicitly approved synthetic migration/recovery tests and their checkpoint checks, and separately approved the exact earlier eight-test batch. Automatic review nevertheless rejected a subsequent `require_escalated` action that copied `run-focused.sh` and `run-recovery.sh` from `.scratch/coherent-resource-frozen` into `.scratch/inner-opaque-frozen` with path substitution and ran them; review cited unestablished script contents/data access and possible session material. **That exact rejected copied-script/runtime action remains paused: do not retry, split or replace it as a workaround.** The user-approved clean-environment earlier-source run mentioned above targeted the wrong binary and does not qualify inner-opaque. Prior rejected arbitrary retained-DB query `SELECT key,value FROM metadata WHERE key LIKE '%sync_token%'` must not be retried or substituted. The user approved only synthetic test checkpoint checks, not real-account/token inspection.

For a genuinely new permitted control, freeze the complete target, produce an explicit manifest naming exact binary path/hash, source/patch/fixture hashes, feature mode, full test names and reached markers. Independently review the **actual runner** against it before submitting through normal approval. Use `env -i`, explicit checked-in fixtures and source cwd, fresh unique HOME/TMP/XDG and synthetic databases per test; verify `running 1 test`, markers and exit codes. The local template is `final_coherent_test_manifest_template.md` alongside this handoff, SHA-256 `1602cbf98e25a8f3892f7c2b1a410c82c8b687bb63f3c6b3ba6142712034e668`. Never count an empty status/manifest as success. On any denial stop that action and report it; do not change route or split it. Past permission-review timeouts are not denials, but some read-only SSH requests exhausted the permitted retry; preserve uncertainty.

## Owned work at handoff

Only the state-wait diagnostic ND build PID `3511609` was reported alive and waiting at wind-down, with durable status path given above. It was not cancelled; inspect before interacting. Other reporting workers stated their owned jobs were complete and no shared processes were killed. Disk headroom was approximately 85 GiB at the final test-owner report, but recheck. No final runtime, clean Clippy, scheduler, retirement, PR, merge, push, tag or release claim is made.

## Archival WIP coordination

The user additionally authorized committing and pushing useful WIP for resumption. The integrator thread owns archival branches; do not race its push or treat an archival branch as beta integration. The primary coherent source to consider is the complete `args-integrated-frozen/combined.patch` (`f81a667c…`) and `source.manifest` (`a39b1840…`) on detached base `9a0b5eb…`, including every untracked source file represented in the manifest. Preserve the small human-readable handoff and test/result reports, the 9a ordinal/refusal controls, and their precise manifest/patch identities. The test-only sibling fixture `never-sibling-wip-v2.patch` (`12c6d297…`) and the sanitized pending-terms diagnostic (`196e89ff…`) may be archived separately as **unqualified** source/evidence, with their 9a base and all limitations stated; neither is an implemented scheduler. The diagnostic retirement draft (`212b30dc…`) may be preserved only as an explicitly unqualified isolated WIP with its unresolved schema-order dependency; it must not be integrated as schema 44. The sole-precheck draft (`985a4fab…`) is known unsafe and must not be presented or integrated as a fix; retain its local evidence and independent-review finding. Coordinate whether to archive that unsafe draft as quarantined evidence, rather than promoting it into the candidate.

Exclude copied binaries, cargo targets, caches, SQLite databases, session/token material, live-account or private logs/archives, credentials, and bulky raw logs. Synthetic test reports and hashes may be retained after checking their contents. The previously denied `into_lease` action and any other expressly rejected/unapproved source action are excluded; do not recreate or publish them by another route. The previously denied copied-script/runtime action and the retained-database token query remain paused, as described above. The integrator must verify every tracked and untracked source against its manifest before committing and label the branch as unqualified WIP, with no release or beta-gate claim.

---

# Independent reviewer handoff — 2026-11-01

This is a design and evidence handoff, not release clearance. The paths below are on CachyOS under `/home/robhooper/dev/kei-v027-sync/.scratch/` unless otherwise noted. Source-only recommendations must not be reported as tested implementations.

## Retry scheduler and quota

- Child-level `next_retry_at` filters at retained-root selection, decision paging, and watch precheck can hide a never-dispatched sibling before the component guard runs. Conversely, `never` alone is not runnable: an exact authenticated future wait can coexist with it. See `provider_generations.rs` retained/current selectors and `provider_generations/transfer_wait.rs` `never_dispatched_due` and `decision_has_due_work`; source-only test is `/home/robhooper/dev/kei-sibling-control-9a/.scratch/sibling-control/never-sibling-wip-v2.patch`.
- Recommended narrow route: an indexed fixed-size due/dirty projection on the existing charged per-component dispatch row, atomically maintained or invalidated with all relevant wait, destination, URL, policy and admission mutations. It is a scheduling hint only: exact authenticated owner/manifest/resource/wait proof is revalidated before GET. Use bounded fair paging, actual future deadlines, and no repeated wake for a valid wait. Verify existing fixed charge covers any added accounting; do not assume logical quota covers SQLite/WAL bytes. Independent libraries and eligible siblings must progress at full logical capacity. An indexed on-demand approach is acceptable if it proves the same bounded fairness and all wait variants without a new global scanner.
- Reserve a conservative operation-bound terminal evidence budget in the existing authenticated pre-GET transaction; consume/refund it atomically on actual terminal outcome. Full quota must leave this item unattempted and visible without inventing a 410 or blocking independent work. Post-publication charging, 16 MiB serialization limits, cancellation and finite evidence retirement remain separate gates. `resume-compose-983/.scratch/frozen/` contains earlier composed evidence; consult the owner's latest packet for newer hashes.
- The plan does not require indefinitely reconstructible history of every completed 410. `docs/plans/v027-sync.md:207–234` calls for bounded retirement of validated unreferenced evidence while preserving unresolved work. Retire only after authenticated proof and reverse dependencies are checked in bounded owner transactions; elapsed deadline or terminal label alone is insufficient. Preserve current retry attempt/deadline and source/checkpoint authority. Raw URL text also appears in retained source blobs, not only the URL column.

## Embedded writer coordination

- `embedded-rating-owner-v16/COORDINATION-CHECKPOINT.md` distinguishes selected no-write owner tests from unqualified physical-writer coordination. Path mutexes do not fence different spellings of the same live file; a physical writer that already passed preflight can replace inode A between the no-write proof check and its DB commit. This can falsely acknowledge work; it is not a demonstrated bytes-loss incident. No general cross-process or arbitrary same-user filesystem guarantee is established.
- Narrow design: participating same-process embedded writers acquire per-file coordination before their guard/admission and retain input A and actual post-SDK staged output B through publication and exact owner transaction. Reserve B before exchange so entrants opening either inode conflict; independent files retain separate slots. Move lease ownership through `spawn_blocking` and owner settlement despite cancellation. A mere `Ok(true)`, callable `settled()` flag, or preflight is not proof of the actual physical commit; unknown outcomes remain held.
- The XMP toolkit's pathname temporary/rename behavior does not itself establish a need for a descriptor-I/O FFI rewrite or disabling ordinary XMP writes. Use a fresh operation-owned private same-filesystem stage, allow supported SDK pathname operations within it, then no-follow reopen, fingerprint and pin the actual post-close output; retain ambiguous cleanup artifacts. Prove toolkit temporary paths stay within that namespace and preserve existing conditional public publication checks. This does not solve external same-user mutation, cross-process fencing, Windows sharing, or FS/DB atomicity. Real writer/no-write overlap, cancellation, output substitution, rollback/reopen and platform tests remain required.
- The approved reject-only prerequisite on published schema 42 does not create production activity records. Earlier Created-sidecar settlement/admission-retirement activation was rejected by automatic approval review and remains separately blocked; do not retry or route around it. Ordinary sync must not rewrite completed existing metadata.

## Visible copied XMP and historical operations

- `visible-archive-packet/DIAGNOSIS.md`, `OWNER-DEPENDENCY.md`, and `visible-archive-baseline/REPORT.md` document an authenticated recorded operation that copied original XMP exactly, then rewrote the staged packet with explicit refresh off. The original visible byte-equality test remains a real open issue. `sidecar-fence-fixture-v3/` is useful additional queue-fence characterization, not a replacement for that assertion.
- Minimal first slice: selected original archive, recorded refresh off, capture Preserve, source exactly the same operation's authenticated claimed `member.old`; copy verified original media and sidecar without metadata rewrite. Restrict visible destination to absent/create-only or exact operation-owned installed prepared identity; hold replacement/foreign/unclaimed cases unless independently authorized. Existing old-path claims, retained fingerprints and process-local locks help, but do not constitute an atomic cross-process fence. Pin/revalidate source and debt before publication and at owner commit; commit-only checks cannot undo publication.
- Persist a truthful per-member Copied outcome distinct from actual Applied and verified NoWrite. Copied bytes do not establish selected metadata completion, clear catalogue/path/capture debt, or advance capture revision. Preserve prior markers and prepared capture evidence or hold if safe transfer is impossible; audit generic upsert/download-status effects. Existing prepared/publishing journals must retain saved bytes and recorded semantics; old-reader behavior must fail closed. A real old-producer prepared journal is documented in `visible-archive-final/ALIAS-BLOCKER.md`; its new consumer/recovery had not run at that review.
- Exact native selected paths and normalized layout keys differ. Leave unrelated selected rows/prepared fields/progress unchanged; create-only publication prevents overwriting an existing physical equivalent alias. For a genuinely required replacement of an existing equivalent path, require targeted ownership proof or hold that operation; a global selected-row census is not established as necessary. `visible-archive-final/current-unqualified.patch` SHA `eb11f9f4516ff0f01b1c47d6299919fa04411237e64743ef8ae962307bf1f796` was source-only at review, not a fix.

## Mixed fallback and sparse bridge

- `mixed-fallback-owned/DIAGNOSTIC.md`: authenticated invalid-token fallback reached complete inventory and peer progress, but the combined test failed because the source `observed_selection` descriptor was absent. Bare Inventory is intentionally not promotable: `provider_generations/observation.rs` requires a matching Delta/Baseline proof. Absence alone means uncertified/unknown observed status, not proof of unsafe token advancement. Do not preserve a stale descriptor or blindly promote Inventory. Whether the inventory-produced token covers changes during enumeration remains unproved. For certified gap-free fallback, obtain a fresh authenticated boundary before complete inventory, bridge its delta, and atomically promote matching token and proof; incomplete bridge retains work and prior safe checkpoint. Malformed capture refusal is a separate path.
- `provider-progress-5105/STATUS.md` identifies a distinct sparse self-block: first bridge has generation-fenced proof, repeated bridge sees its still-durable row before the checkpoint transaction can settle it. Carry only that unchanged proof provisionally into the repeated bridge; revalidate current identity generation, retained target/receipt/config/file proof and all other sparse work in the final existing atomic checkpoint transaction. Never delete markers early or suppress independent holds. Source-level design only; see the latest owner packet for actual implementation and validation.

## Evidence and limits

The precise reviewer messages and packets contain hashes, feature modes, and failing control details. Do not attribute a prior binary's pass to a newer source snapshot; no-default may omit XMP-gated tests. No new tests or builds were run for this handoff. No archival commit, push, release, real account/archive access, state deletion, or rejected-action retry was performed by this reviewer.

---

# OpenCode resume handoff: mixed provider fixture

Recorded 2026-11-01 18:47 UTC. User directed wind-down and requested WIP archival, coordinated by the integrator. Do not publish or integrate this unfinished patch independently.

## First action: inspect the existing build

On CachyOS via the established normal SSH connection, inspect the already queued build before doing anything else. At handoff PID 3510632 (bash -s) and its PID 3510637 (flock -x 9) were waiting on /home/robhooper/dev/kei-v027-sync/.scratch/beta-build.lock. This batch owns the queued job; it was not killed and might acquire the lock and compile after handoff. Do not start a duplicate or edit the frozen source while pending/running. Inspect process and lock status, queued-build-nd.sh, build-nd-cargo.log, candidate-nd-bin.sha256 and timestamps. Determine whether the queued build finished, failed, or has an uncertain outcome before using its artifact. At handoff candidate-nd-bin and build-nd-cargo.log are the OLDER pre-review build, not evidence for the current patch. Preserved copies are candidate-pre-review-nd-bin and build-pre-review-nd.log. The invocation log was empty when saved as queued-build-local-output-at-handoff.log; future cargo output goes to build-nd-cargo.log. No new job was started during wind-down.

## Source and WIP archival manifest

Worktree: /home/robhooper/dev/kei-mixed-fixture-fix. Detached HEAD and base: 9a0b5eb58c0a3735ba7bafa28cafc2de27bc9586. Main /home/robhooper/dev/kei-v027-sync was clean at that head when last inspected but is shared and can advance. /home/robhooper/dev/kei remains untouched.

Exact useful source for coordinated archival on an explicitly WIP branch, NOT beta integration:
- M src/sync_loop/runner/tests/mixed_provider_shapes.rs SHA-256 7eb871e1cd1de7746750351c72e6d0bde42a7778ae303ff9d90fd6ce1f778ec2
- M src/sync_loop/runner/tests/v027_baseline/raw_replay/two_library.rs SHA-256 7f2dbad5bbac5f9fc2e4d5eee3846c01fc858d7362fd988e476b5c9c027cf083
- ?? src/sync_loop/runner/tests/v027_baseline/raw_replay/two_library/fallback.rs SHA-256 385dca11888ee3a68b4a0ca5ed04dd076a91c229964aa0ef963ce75c6003c1b2

Complete tracked plus untracked diff: candidate.patch SHA-256 9ec60e1f5cd342b5504de049d4082c5d5711d19703a960182dc96b4f7f35d41c. This is TEST-ONLY, UNQUALIFIED, and may fail. No production/schema/generated/dependency changes. Preserve small handoff/review reports as appropriate; exclude binaries, private logs/databases, synthetic database contents, build caches, secrets and unrelated worktrees. Do not include speculative rank-race research in the source WIP branch, rejected into_lease work, other denied actions, sparse/crash/retry owners' work, or unrelated changes. No WIP here is expressly denied for archival, but no patch is safe to publish as a beta fix without the gates below. Integrator owns branch creation/push coordination. No commit/push/PR/merge/tag/release occurred in this batch.

## Implemented fixture and source review

Original mixed_library_provider_shapes models two six-cycle histories, identity/legacy separation, bytes/path/receipt stability, independent library progress, reopen, quiet tails and provider-encoding equivalence. Its intended incomplete changes page was unreachable: the provider treated only absent/page-* cursors as inventory but the actual cursor was before. The expected first-two-cycle hold was therefore not caused by the injected page. Merely changing the expected token was not justified.

The current candidate makes a typed BAD_REQUEST reachable at before for IncompleteInventory in cycles 0 and 1; invalid-token fallback sees an underreported zero count hint, a valid master/child prefix at rank 0, then malformed rank page at rank 1. The hint creates one ordered tail fetcher; offset 1 shows it processed the prefix master. Separate counters require the token, prefix and later failure. Rank queries require explicit startRank and indexing-state requests are separately classified. Existing checkpoint/identity/bytes/retry/quiet assertions are retained. This is a different fault path from the old intended missing-moreComing changes response: do not claim endpoint-specific coverage of the latter.

Before now enters the paginated changes snapshot. Counters require a continuation cursor for PageCuts/DuplicateEmpty and a continuing empty page for DuplicateEmpty. They aggregate libraries/cycles: they prove at least one such response per replay, not every library or recovery cycle; duplicates are adjacent within a page, not cross-page. Healthy before now returns snapshot observations, so the exact original six-cycle test must pass before claiming encoding equivalence.

Separate fallback.rs controls use authenticated synthetic owner, ShadowCapture, selected private libraries, scoped receipts and actual media bytes. They cover complete/incomplete rank fallback, independent peer progress, byte-for-byte prior descriptor retention on incomplete work, source/local checksum, path/receipt assertions, reopen and later ordinary delta without repeated rank inventory. After a bare completed inventory, no active selected-observation descriptor is intentional conservative behavior; independent review confirmed missing descriptor alone is not a bug. Direct wiremock startup avoids a silent bind skip.

Independent read-only reviewer /root/sparse_review reviewed ordered-prefix modeling, pagination reachability, authenticated scope/receipts/checksums and descriptor retention; no source-level blocker subject to reached tests. The reviewer verified applied source matched reviewed candidates apart from formatting. It did not run tests or issue a final qualification disposition. No reviewer agent remains running.

## Exact result status and continuation

Initial cargo check --locked --offline passed on clean 9a0 base; see initial-check.log. rustfmt and git diff --check passed before freezing. Current patch 9ec60e1f has NOT run any tests in either mode; its queued ND build is only --lib --no-run. Both feature-mode executions, strict Clippy/static/boundary gates and independent final review remain open. Old ff3d diagnostic results cannot qualify this patch. Do not infer a pass from build/list or a skipped bind.

1. Determine queued build outcome and binary provenance, then run from immutable exact ND binary: original mixed_library_provider_shapes across both histories and all three encodings, plus both new fallback tests. Confirm nonzero test counts, no bind skip, reached prefix/failure/continuation/empty counters, and existing receipt/byte/checkpoint/reopen/quiet behavior. If failure, use one bounded KEI_MIXED_REPLAY trace and trace the actual branch; do not alter expectations to pass.
2. When source is stable and ND passes, compile once in all-features mode under shared lock, exact disk guard and copy/hash the executable while still holding the lock; run the same exact focused tests. Run repository-required strict Clippy both modes, fmt, diff and relevant static/boundary checks. No duplicate full suite.
3. Obtain independent final review of the exact patch and evidence. Apply/rebase only after checking current main and coordinating other owners. Integrator coordinates WIP archive; integration requires separate qualification and authorization.

## Research, dependencies and restrictions

Prior causal evidence: /home/robhooper/dev/kei-v027-sync/.scratch/provider-progress-5105/STATUS.md. Separate exploratory worktree /home/robhooper/dev/kei-mixed-fallback-owned at ff3d and packet /home/robhooper/dev/kei-v027-sync/.scratch/mixed-fallback-owned/NEXT-CYCLE-REPORT.md retain a conditional mid-rank insertion race. It fails only under a post-insertion query-token timing assumption not established for the actual provider. docs/architecture.md:1570 treats complete full-pass tokens as authoritative. Do not change production policy or integrate that speculative race on this evidence. Sparse V6 is published in base 9a0; leave its packet and other active crash/legacy/retry work alone.

All testing and source work on CachyOS via established normal SSH; no Mac tests, live provider, private archives, or unapproved access bypass. Shared compile/Clippy/target mutations require main .scratch/beta-build.lock and exact df -Pk >= 15728640 KiB guard, TMPDIR under /home. No source edits during qualifying build, no removed locks/evidence, no killing shared/unowned jobs, no duplicate full suites. Use isolated immutable binaries. Routine tests and safe source edits were already authorized; avoid repetitive approvals. No outstanding user denial relevant to this batch; normal require_escalated SSH was approved after sandbox network blocked initial attempt. Respect actual denials and check uncertain outcomes.

---

# Kei beta: embedded metadata owner / private-XMP handoff

Status: STOPPED at user request. This is unqualified WIP, not an integration, release or publication candidate. The user wants to resume in OpenCode and asked an integrator to coordinate archival WIP branches. Do not independently push or merge this work; give that integrator the exact source and manifests below. No production changes were made to main during this work. Main `/home/robhooper/dev/kei-v027-sync` was clean at `9a0b5eb58c0a3735ba7bafa28cafc2de27bc9586` on the last check; verify again before any subsequent action.

## First actions for OpenCode

1. Read this file, repository `AGENTS.md` and applicable `.agents/skills`, then read main `.scratch/process-death-fixture-correction/owner-lifetime-e475-wip-v4/REJECT-ONLY-DESIGN.md`, `.scratch/physical-xmp-owner-wip/OWNER-CAPABILITY-REVIEW-v2.md`, its independent review, and `.scratch/embedded-rating-owner-v16/COORDINATION-CHECKPOINT.md`. Use standard existing `ssh -o BatchMode=yes cachyos` access through the authorized Mac; no Mac tests. Do not change SSH/network/host mount settings or use credentials, live accounts or private archives.
2. Inspect active jobs and current branch/worktree status before retrying anything. Verify manifests and source hashes, including untracked files. The shared build lock is `/home/robhooper/dev/kei-v027-sync/.scratch/beta-build.lock`; serialize shared-target compilation, Clippy and mutation through it. Use `TMPDIR=/home/robhooper/tmp`, 15 GiB minimum headroom and copied immutable exact-source binaries with before/after manifests and nonzero exact test counts. Do not kill unrelated jobs or remove locks. Never edit a source snapshot during its qualifying build.
3. Review the sealed no-write owner increment in v19 against the evidence and owner-only denial boundary. It is not compiled or approved for integration. Then take a selective, reviewed physical-XMP owner delta from v5 and the owner-private exchange primitive, not wholesale patches. Before any production activation implement and review the complete authenticated selected-operation lifecycle, exact lease/owner transaction and checkpoint classification described below. Do not present prior-candidate controls as validation of the changed candidate.
4. The team integrator, not this worker, owns any archival WIP branch/push. Coordinate exact base and migration dependencies and have source independently reviewed before any beta integration. Never commit/push private logs, database snapshots, binaries, credentials, private archives, or unreviewed lease-extraction code.

## Frozen v19 source

Worktree: `/home/robhooper/dev/kei-embedded-rating-owner-v19`, detached HEAD `983dd4963149193faebb3cc62b1de1a64caa0a97`. This is intentionally isolated and based on older published schema-42 checkpoint plus unpublished composed retry-43 and identity-44 work. It is NOT a clean patch on current main. Coordinate with integrator before rebasing; do not invent an empty migration 43 or independently occupy migration 45.

- Full tracked + untracked source manifest: `complete-source.manifest`, SHA-256 `57170d3f525e045d5f0f6f094beae84bf763bd232f8857ddb13936411d71acb5`. `complete-source-recheck.manifest` matched at handoff.
- Exact tracked diff: `tracked-wip.patch`, SHA-256 `79adbd6cba968529e28b8ed58c0a6e8cdccd4d121d1faa269eb65dc2da44cd91`. This does NOT contain untracked files; preserve live worktree plus manifest.
- Exact dirty status: `dirty-files.txt`, SHA-256 `f9ac88e3faa36be99e5baf26ecc6827988d310eba40e57956ccdd2c495994c73`; reproduced below. Directory entries marked `??` contain files named by the full manifest.
- Key file SHA-256: `src/state/db/metadata.rs` `9a8d1629b3e4db603b9edb08a23aaa8171ac388b6332cdd69a3c40f90a4b3a38`; `src/state/db/metadata_activity.rs` `959c3bec3ad54ca9615741e472cf66059fbe2fca993d1f44d1aebc880bf5f075`; `src/state/db/metadata_identity.rs` `ef37a9cdd1e185216dd1e320e5062e3b8d0a7bf09d1dd9b7c6985d2e01b1eab6`; `src/download/orchestration/queue_projection/tests.rs` `99f7bb49d447949ccdacf422d1f83d06a1c9c057783b103d504a848d7a0c6825`.

No source modification follows the manifest snapshot.

Dirty files:

```text
 M src/auth/session.rs
 M src/download/file.rs
 M src/download/metadata_rewrite.rs
 M src/download/metadata_rewrite/planning/tests.rs
 M src/download/metadata_rewrite/queued.rs
 M src/download/mod.rs
 M src/download/orchestration/generation.rs
 M src/download/orchestration/incremental.rs
 M src/download/orchestration/models.rs
 M src/download/orchestration/primary_layout.rs
 M src/download/orchestration/queue_projection/tests.rs
 M src/download/orchestration/recovery.rs
 M src/download/orchestration/url_refresh.rs
 M src/download/orchestration/url_refresh/bounded_tests.rs
 M src/download/pipeline.rs
 M src/download/pipeline/consumer.rs
 M src/download/pipeline/outcome.rs
 M src/download/pipeline/outcome/tests.rs
 M src/download/pipeline/pass.rs
 M src/download/pipeline/pass/tests.rs
 M src/download/pipeline/producer.rs
 M src/download/pipeline/streaming.rs
 M src/download/pipeline/task.rs
 M src/download/pipeline/task/staging_tests.rs
 M src/download/pipeline/task/tests.rs
 M src/fs_util.rs
 M src/icloud/photos/album.rs
 M src/icloud/photos/album/changes.rs
 M src/icloud/photos/album/selection_capture.rs
 M src/icloud/photos/album/work.rs
 M src/icloud/photos/asset.rs
 M src/icloud/photos/cloudkit.rs
 M src/icloud/photos/mod.rs
 M src/icloud/photos/session.rs
 M src/state/db.rs
 M src/state/db/metadata.rs
 M src/state/db/metadata_activity.rs
 M src/state/db/provider_generations.rs
 M src/state/db/provider_generations/reference_refresh.rs
 M src/state/db/provider_generations/retirement.rs
 M src/state/error.rs
 M src/state/schema.rs
 M src/sync_cycle.rs
 M src/sync_loop/runner/tests/v027_baseline/fallback_publication.rs
?? src/download/orchestration/generation/
?? src/download/pipeline/retry_owners.rs
?? src/icloud/photos/source_causality.rs
?? src/state/db/metadata_identity.rs
?? src/state/db/provider_generations/compact_attestation.rs
?? src/state/db/provider_generations/immutable_epochs.rs
?? src/state/db/provider_generations/immutable_epochs.sql
?? src/state/db/provider_generations/refreshed_candidates.rs
?? src/state/db/provider_generations/refreshed_source.rs
?? src/state/db/provider_generations/refreshed_store.rs
?? src/state/db/provider_generations/refreshed_transfer.rs
?? src/state/db/provider_generations/refreshed_transfer.sql
?? src/state/db/provider_generations/transfer_wait.rs
?? src/state/db/retry_clock.rs
?? src/sync_loop/runner/tests/v027_baseline/fallback_publication/
```

## Implemented versus proposed

**Implemented in v19, untested after final edit:** the pre-existing selected explicit rating-only no-write producer now retains its live input identity through the real blocking SQLite owner transaction; exact selected receipt acceptance and exact admission retirement precede commit. After commit, `metadata.rs` constructs a private-field, non-Clone `CommittedLease` tied to an opaque live registry binding and operation ID. `metadata_identity::settled_with` consumes that seal and rejects wrong lease/operation; no production caller can construct the seal or obtain a lease-release accessor. `metadata.rs` contains a new test-only wrong-lease/wrong-operation control. The older unrestricted production `settled()` method was removed; test-only `test_settle` is `cfg(test)`. The physical publisher is NOT wired to this seal, and v19 has no production-complete cross-writer fence. An isolated ordinary source review found this no-write seal constructor occurs only after commit and is not returned; full code review and compile remain required.

**Do not integrate as a completed fix.** The actual embedded writer, selected authenticated private-stage producer, physical owner transaction, durable cleanup-debt classification and all route/checkpoint integrations are incomplete. Some earlier integration snapshots expose raw leases: v17 and v18 contain the test/draft `RetainedExchangeOutcome` with lease fields; preserve only as local evidence and do not publish/activate those draft interfaces. The formerly proposed `ExchangedPrivatePublication::into_lease` or equivalent accessor is explicitly excluded.

Review-only design: main `.scratch/physical-xmp-owner-wip/OWNER-CAPABILITY-REVIEW-v2.md`, SHA-256 `7a4887ee4cd8d3b774b0825ae1adc9c595248d1364cf08fac0d689efae948802`. Independent review: `OWNER-CAPABILITY-INDEPENDENT-REVIEW.md`, SHA-256 `ebe934c78a0b367b1287280bb21ff4bf2b9508d79a01495a08c8c319839f47a2`. The proposal retains A continuously from authenticated admission, adds the actual post-toolkit B to that same lease, and never exposes the lease or release token to a caller. Actual selected receipt/pass/config and a fresh private directory are durably owner-bound before creation; toolkit I/O remains inside that directory, and the actual output inode is pinned only after toolkit close. Before public mutation the exact account/provider/generation/pass/destination/operation/transform, persisted prepared output and both parent/name/file capabilities are revalidated in the same SQLite immediate transaction that runs one confined exchange, verifies public B and private displaced A, writes exact receipt/checksum plus separate durable cleanup witness, and commits. Only the owner then settles the private lease. No async await inside that transaction. If filesystem exchange succeeds and DB fails, SQLite rollback does not roll back the filesystem: retain both names, pending state and reservation, report attention, do not reverse exchange or delete unknown entries.

Default all-features JPEG uses XMP toolkit. Parent's later scope decision permits the existing toolkit's pathname operations inside a fresh authenticated operation-owned private stage; do not implement a toolkit FFI rewrite or broad XMP hold by default. POSIX SDK derives temporary names as siblings of its staged file and may replace the staged-output inode. This protects normal cooperative same-process operation only; it is not a guarantee against arbitrary same-user filesystem mutation, a cross-process lock, or FS/DB atomicity. The private work-copy path and separate toolkit output temp are different files: a valid branch test must record the output temp identity immediately before toolkit and after close, not compare work-copy inode to output. Preserve public-path ownership safeguards, recorded paths, prior metadata and timestamps; no automatic existing-file metadata rewrites, unresolved sidecar retirement/recovery, or broad normal-update disable is authorized. Unsupported or uncertain affected work retains evidence and reports attention while independent files/scopes continue.

**Separate physical owner WIP, NOT an integration patch:** `/home/robhooper/dev/kei-physical-xmp-owner-v5`; packet main `.scratch/physical-xmp-owner-v5/`. Complete 671-file source manifest SHA-256 `212d332e0684b727245cb2eeeb276ea0e6c7b3bb34ff281386c189c6b1dbbb8d`; comparison manifest `b8e8e2c3d5e07e91bc8c1cc463a68753f4e852e473224b590b530ace2f701cfe`; WIP patch `ce722a6e3c6bddc72ed22dda01aec21e100532abb47a3f63fd111b7c7ab9ff58`. It is based on an older draft, includes 23 differing files and may reverse v19 changes; do not apply wholesale. It has unnumbered prepared-output/cleanup-witness drafts, private stage reservation, paginated classification WIP, and source-only controls. `finish_selected_physical_stage` deliberately fails closed pending `metadata.rs` transaction + sealed post-commit orchestration. No compile/runtime test/strict check has run. Its earlier copied registry settlement interface is not the reviewed v19 seal. If archiving any selected WIP from it, first inspect exact hunks and exclude unreviewed/raw lease exposure, `.orig` files and generated data.

**Owner-private confined exchange draft:** `/home/robhooper/dev/kei-confined-owner-private/src/state/db/metadata_activity/owner_exchange.rs`, SHA-256 `4e3b963777d89fc6acbf3d2942aa30b9d5b599f04ddcdc36485a77b89d927eba`. No standalone patch or tests of this exact visibility variant exist. It is a child private to the owner, accepts retained parent/file evidence, does one confined exchange without reverse exchange, deletion or fallback, syncs both parents and verifies A/B. It must be consumed only by the owner inside the exact transaction, not made a crate-wide mutation API. On error, preserve all evidence. Platform and cancellation behavior are unqualified.

**Earlier route evidence only:** v17/v18 and main `.scratch/identity-route-adapter/route-xmp-exchange-ownerproof.patch` SHA `12d9f9378e59071a96c756da09a06346416c036c8476ebd4f36aca7775409c57` expose lease fields in an unactivated enum and use a test-only fixture that manually derives its directory and has no pass-bound durable reservation. Do not publish or apply as-is, and do not convert its lease field to any equivalent extractor. Its real toolkit/route test results are useful limited prior-candidate evidence only.

## Tests, failures and unrun gates

- Last before-seal selected-owner source: v16 `owner-v3-source.manifest`, SHA-256 `2a97b314bd444b4371f398f9dfd061fc34916c50173a4f85dfc0b630960773cd`. All-features binary SHA `c6b9e58e9ab30441ddc9ba8060993b339de85fceee049e483e9198b33df908d8`: five exact nonzero controls passed, including real selected-owner cancelled waiter at SQLite precommit, independent path debt, producer/reopen, rollback/reopen and atomic identity pair. No-default binary SHA `a6eb96f76fec86aafece82cc3f2ee1db8d1e011cfcf6cc679419f6a4f72bdf98`: four analogous exact controls passed. Both modes separately passed eight `metadata_identity` registry tests. Exact logs/binaries are main `.scratch/embedded-rating-owner-v16/owner-v3-*`; sources matched before/after. These binaries do NOT contain the final v19 owner seal.
- Earlier first owner build failed six `Send` lifetime errors in a test that used `tokio::spawn`; corrected test drops the actual locally pinned non-Send future. Failure log is `.scratch/embedded-rating-owner-v16/owner-build-af.log`, corrected prior-candidate logs are `owner-v2-*`.
- Separate route source manifest SHA `4827a73be580e17f8a23712f297d4b07ba8865d106f95ae0d906861a5b17f100`; all-features immutable binary SHA `3eed11130db2571438cc483d6774d7ddff2f7da6acbe8dabd8812200496b04f7`. Five exact tests passed: real toolkit output plus synthetic authenticated account, cross-parent retained exchange, target/stage substitution holds, unsupported exchange with no mutation, cancelled exchange waiter. Packet main `.scratch/embedded-rating-owner-v18/`. Fixture is not pass-bound or durably owner-reserved and this binary does not validate production dispatch or owner settlement.
- Current v19 `git diff --check` passed after the seal test edit. V19 has NOT been compiled or run in either mode; the seal negative test and entire physical producer, checkpoint integration, migration and strict Clippy have NOT passed. V14 prior strict AF Clippy failed with 137 diagnostics (including five test-only lifetime findings subsequently fixed in v15); do not waive remaining diagnostics or call them v19 results.
- Final candidate must run actual authenticated pass-bound XMP toolkit publication, before/after toolkit output-temp inode observation, real no-delete exchange and transaction, independent files/scope, wrong operation/lease/account/pass/payload, same-byte substituted inode, cancellation during actual blocking toolkit and owner commit, DB failure after exchange/rollback, retained cleanup debt and reopened status, missing/foreign/valid-JSON tampered witness, >256 settled-debt operations, completed-root retirement/reopen, same-scope successor with older pending activity, ordinary no-write/reopen and unchanged paths/bytes/mtime; both modes where applicable, strict Clippy/fmt/diff/boundary checks and independent code review. No Mac tests; do not claim untested Windows/macOS behavior.

## Outstanding checkpoint, migration and state risks

The uncompiled physical owner WIP has known defects in its earlier checkpoint draft: foreign/misbound rows can be omitted by account-filtered scans after an all-library count; selection=None rows may be skipped without proving independence; an unscoped generic token setter can reject valid settled cleanup debt; library `LIMIT 257` before filtering settled records can block unrelated work after 256 successes; retired generations may leave retained activity pointing at missing roots; and a length-64 output hash alone is not an independently authenticated witness. The v5 worker reported changes intended to page settled records, hold foreign evidence and same-scope unresolved successors, but its latest source has not compiled or been independently verified. Inspect the real source, not the report, and ensure exact immutable output binding, accountable foreign rows, scope-specific transaction guards and compact/retirement behavior. A differing seal does not alone prove old same-scope pending work irrelevant.

Published main schema was 42 when checked; v19 contains actual unpublished retry migration 43 and identity-index migration 44. A separate retry retirement effort may claim 45. The new XMP prepared-output/settlement witness schema is unnumbered and unapproved; choose the next migration only after integrating the real predecessor chain and coordinating with the integrator. Verify old-reader rejection, transaction rollback, owner auth, reset/retention and migration from real released schemas. Avoid unconditional deletion triggers that would block legitimate future reset/cleanup without reviewed semantics. Durable active+prepared+witness triplets may classify content settled plus cleanup debt only if exact authenticated bindings agree; a receipt alone is not settlement or cleanup authority.

## Approval and denial boundaries

The user authorized safe embedded explicit-refresh lifecycle implementation/testing, source/test edits on trusted CachyOS, synthetic tests and the private-XMP staging scope. No fresh approval is needed for routine authorized work. User separately asked to wind down and let the integrator coordinate archival WIP pushes; that instruction supersedes continuing this worker now. No live accounts, credentials, private archives, unrelated worktrees, automatic rewrites, sidecar-retirement activation, merge, tags or release.

Automatic approval review rejected the escalated `ssh -o BatchMode=yes cachyos 'bash -s'` write to `/home/robhooper/dev/kei-selected-xmp-exchange-fixture/src/exchanged_publication.rs`, which proposed `ExchangedPrivatePublication::into_lease` and would also declare the module in `src/lib.rs`. Review reason: allowing callers to extract/release the owner lease could bypass owner-only settlement. It was confirmed NOT executed; module/declaration absent. Do not retry that action, expose an equivalent accessor elsewhere or work around the denial. The newer owner-private seal is a distinct normal-reviewed action, not permission to bypass the rejection. Any further concrete protected action must use normal approval review; if denied stop that action and report exact target/reason/outcome.

## Jobs and safety at wind-down

This worker started no build after the wind-down instruction. The routes worker's own earlier queued no-default primitive build (`flock` PID 3509916, SSH session 49364) was safely canceled while still waiting: session exited 130, no compiler or build log existed, and no other process or lock was touched. It targeted an older crate-visible exchange primitive, not the owner-private variant; it has no result. At snapshot, unrelated shared archive qualification was running under `flock` PID 3506089 with cargo PID 3511624 and rustc PID 3511635; another unowned worker was queued. Do not kill, remove its lock, or assume it still runs later—inspect process/log state first. All original evidence, logs and retained synthetic databases are preserved; do not clean them up.

Key additional evidence: main `.scratch/embedded-rating-owner-v16/COORDINATION-CHECKPOINT.md`; `.scratch/process-death-fixture-correction/` including original fixture baseline, retained replay/authenticated-control evidence and independent reviews; `.scratch/remaining-qualification-groups/f0-process-death-diagnostic.md`; `.scratch/five-bars/qual-f0bdc218-20261030/HANDOFF.md`. Preserve prior snapshots and do not conflate their source/binary hashes with a final integrated candidate.

## Archival source inventory for the coordinating integrator

The exact candidate source set is the union of `changed-tracked-files.txt` (SHA-256 `2f46d27fea67fdf3d9f3e1f4ac23135ebcd36e42151dca8fd14600c6b1d62acd`) and `untracked-files.txt` (SHA-256 `3b1f14da97afca223bc1e6a621c5529cf69d4f372664ae0324154c36c024e8f2`), with per-file hashes in `complete-source.manifest`. The tracked patch alone omits these exact 15 new source files:

```text
src/download/orchestration/generation/compact_policy.rs
src/download/pipeline/retry_owners.rs
src/icloud/photos/source_causality.rs
src/state/db/metadata_identity.rs
src/state/db/provider_generations/compact_attestation.rs
src/state/db/provider_generations/immutable_epochs.rs
src/state/db/provider_generations/immutable_epochs.sql
src/state/db/provider_generations/refreshed_candidates.rs
src/state/db/provider_generations/refreshed_source.rs
src/state/db/provider_generations/refreshed_store.rs
src/state/db/provider_generations/refreshed_transfer.rs
src/state/db/provider_generations/refreshed_transfer.sql
src/state/db/provider_generations/transfer_wait.rs
src/state/db/retry_clock.rs
src/sync_loop/runner/tests/v027_baseline/fallback_publication/issue845_wait.rs
```

The candidate is unqualified and contains composed retry-43 + identity-44 dependencies; archive only on an explicitly labelled WIP branch based on `983dd4963149193faebb3cc62b1de1a64caa0a97`, after verifying the manifest and coordination with the retry owner. Include small human-readable handoff/review reports only if the integrator chooses their exact intended repository location; do not blindly add `.scratch`, generated manifests/logs, synthetic databases, retained files, binaries, credentials, private logs or archives. The v17/v18 lease-exposing draft interfaces, rejected `into_lease` action, v5 wholesale comparison patch with reverse hunks/`.orig`, build targets and the unreviewed old crate-visible mutation primitive must not be published as accepted code. The owner-private exchange draft is separate source evidence, not included in v19; review and archive only a separately labelled non-activating safe version if the integrator determines it is useful. No commit or push was performed by this worker.

---

# OpenCode handoff — recorded original sidecar preservation WIP

Status: **UNQUALIFIED; do not integrate, merge, tag, release, or represent as
a fix.** User requested wind-down; no further builds, tests, source changes,
or independent pushes were started after that request. The shared integrator
is coordinating archival WIP branches separately.

## First steps on resume

1. Read this handoff and the current checkout/agent instructions. Use only the
   established SSH route from the connected Mac to CachyOS; run no tests on the
   Mac. Do not touch the separate /home/robhooper/dev/kei checkout.
2. Check current processes, lock owner, logs and main/remote HEAD read-only
   before restarting anything. Our owned build batch has **terminated exit
   101**; no process from this lane remains. Do not kill other jobs, remove
   locks, or assume another lane is idle. Source was frozen during that batch.
3. Treat this patch as WIP with three observed AF failures and failing strict
   Clippy. Fix/review actual causes on a new isolated worktree, not main.
   Retain every original byte-preservation/ownership assertion. Do not treat
   a held operation as successful recovery.
4. Respect the denied database boundary below. Do not run the cross-binary
   consumer on that target, inspect it, or reach it indirectly. Ask the user
   for explicit authorization if access is still essential; an automatic
   approval rejection is not itself resolved by elapsed time or a new route.

## Source, base, and archival boundary

At handoff the published main checkout
/home/robhooper/dev/kei-v027-sync was clean at
9a0b5eb58c0a3735ba7bafa28cafc2de27bc9586 on
simplify/v027-baseline-incremental, matching its configured upstream status.
Recheck: other work may have advanced it.

The isolated worktree /home/robhooper/dev/kei-visible-archive-final is
detached at the same 9a0b5eb base. It has these modified tracked files:

- src/download/mod.rs
- src/download/orchestration/generation.rs
- src/download/orchestration/primary_layout.rs
- src/state/db/contracts.rs
- src/state/db/primary_layout.rs
- src/state/db/provider_generations.rs
- src/state/db/reconciliation.rs
- src/sync_loop/runner/tests/primary_layout.rs
- src/sync_loop/runner/tests/v027_baseline/indexing_layout.rs

There were no non-ignored untracked source files. Ignored scratch, target
artifacts, test roots and databases are not source and must not be committed.

Exact tested full diff:
unfiled-candidate.patch SHA-256
3a1d868c99282d9888a33f8a0c1aa00a64f87b5a548c706c82a8e08a080081f8.
unfiled-source-before.manifest and unfiled-source-at-handoff.manifest match;
each manifest SHA-256
c666f698cfc4e9f8274b961fc51fd6aa871d26783c566501ac47c344286ae098.
The original
primary_layout_preserves_sidecar_bytes_and_fences_old_metadata_queue test body
and its byte-exact visible archive assertion remain unchanged; an additive
owner-backed regression tests that safety obligation.

For the integrator's **archival branch only**, a source-only filtered patch is
archive-export/safe-wip.patch SHA-256
13a1736fe2b34a440841460fc1bc26633b45d571163989098365fc005ba045eb.
It is based on 9a0b5eb and passes git apply --cached --check against that base
index. It excludes the ignored historical consumer associated with the denied
database and has no added into_lease action. It contains no binaries, database,
or logs. **It is not byte-identical to the tested full patch and has not been
built or tested itself.** Integrator must inspect and label it unqualified WIP
on an archival branch; do not independently race pushes. Do not upload ignored
target binaries, SQLite files, WAL/SHM, private logs or retained synthetic
roots. This local handoff contains a path explicitly identified by auto-review;
redact that path and any private details from any remotely shared report.

## Implemented versus proposed

The draft identifies a selected original already present as the same recorded
operation's claimed member.old. With explicit refresh and capture repair off,
it copies the original XMP packet exactly even if its old metadata decision
differs. A new tagged Copied outcome retains source/sidecar fingerprints and
previous decision. The owner checks the exact source/claim/receipt and
conservatively holds existing original metadata or capture-repair debt before
stage, on saves, before preservation and before media and XMP publication, and
at commit. Untagged selected files with historical Some(decision) retain the
ff3d producer's established settlement behavior; missing decisions hold. The
old decision is historical provenance, not new proof every component wrote.

Per the later reviewer adjudication, a Copied archive with member.old is held
rather than replaced. With member.old=None it uses existing physical
no-replace publication; a retry may accept only the exact recorded prepared
file identity. Copied settlement returns before reading or modifying selected
rows and does not advance child capture revision. No global alias scan/index,
schema migration, global filesystem ownership system or cross-process
filesystem atomicity claim was introduced. Physical no-replace does **not**
prove normalized path uniqueness on a case-sensitive filesystem.

Source review of this exact full diff found no additional definite source
blocker within that narrow physical no-replace scope; runtime and strict gates
below override any impression of readiness. Race after a check, crash before
installed receipt, general normalized alias uniqueness, capture-prepared debt,
and unprepared refresh of an existing binding are not proven.

## Qualification evidence

Pre-edit cargo check on 9a0b passed. Frozen AF and ND lib binaries built under
the shared build lock and coordinated CARGO_TARGET_DIR
/home/robhooper/dev/kei-v027-sync/target, TMPDIR under /home.

- AF binary SHA-256
  416cc57c7ba16f0189390d2b6dcca5945740fbd6cb53010327b7d6c059171cbe.
- ND binary SHA-256
  a877f3e7421b7dff1f9417fdd096165fc4eda072fbe6f3d880f97758905f35cb.
- AF test results: unfiled-af-results.tsv; per-test
  unfiled-test-<exact-name>.log. Every listed AF filter ran one test.
- AF strict Clippy failed: unfiled-clippy-af.log reports one library error
  (collapsible if at state/db/primary_layout.rs:547) and 21 lib-test errors,
  including cmp_owned and MutexGuard-across-await. No fixes were attempted.
  The batch exited 101. ND Clippy and ND focused tests were **not run**.
  XMP-specific runtime tests are not applicable without XMP.
- No full suite or release qualification was run.

AF passed (one exact test each):
- recorded_selected_original_existing_metadata_debt_blocks_preparation
  reached VISIBLE_ARCHIVE_EXISTING_DEBT_HELD.
- recorded_selected_original_debt_after_prepare_blocks_visible_publication
  reached VISIBLE_ARCHIVE_DEBT_AFTER_PREPARE_HELD.
- unrelated_fresh_owned_download_keeps_sidecar_and_source_receipt reached its
  fresh receipt/sidecar marker.
- explicit_refresh_selected_original_applies_owner_metadata_before_archive
  reached EXPLICIT_REFRESH_OWNER_WRITER_REACHED. This is owner-planned
  preparation plus prepared recovery, not unprepared refresh of an existing
  binding. The test's GET assertions cover its named synthetic media endpoints.
- copied_archive_refuses_late_foreign_media_and_xmp reached both xmp=false
  and xmp=true markers. The XMP case admits exact prepared media before the
  foreign sidecar refusal; it is not a concurrent syscall-race test.
- copied_metadata_evidence_is_new_and_old_reader_fails_closed.
- primary_layout_legacy_float_journal_reopens_saves_and_commits_exactly.
  This is an existing fresh synthetic state unit, NOT the denied cross-binary
  retained-journal consumer.

AF failed (one exact test each):
- recorded_selected_original_preserves_exact_sidecar_packet_through_recovery:
  VISIBLE_ARCHIVE_EXACT_STAGE_REACHED, so the scoped claimed source produced
  byte-exact staged XMP. The later normal closed-owner cycle returned
  failed_count=2, skipped.uncertain=2, full_enumeration_reason=
  EnumConfigHashDrift, where the test required zero failure. The recovery
  completion and later quiet assertions were not reached. Cause is unknown.
- copied_original_does_not_settle_real_admitted_metadata_selection:
  QueryReturnedNoRows at its exact active-selection query
  (tests/primary_layout.rs:12325 in this frozen source). Do not claim the
  selected archive destination was admitted or the accounting check passed.
  Selector redirect is a source-supported hypothesis, not an established cause.
- copied_original_occupied_archive_holds_and_exact_install_retries:
  COPIED_ARCHIVE_OCCUPIED_HELD and COPIED_ARCHIVE_INSTALLED_RECEIPT_REACHED
  were printed. The later reopen cycle returned failed_count=2,
  skipped.uncertain=2 and EnumConfigHashDrift, where zero was expected. The
  closed-owner retry completion was not proven. The receipt marker is after
  durable installed evidence; it does not cover a crash before receipt.

An earlier AF baseline on published ff3d plus test-only patch failed at the
byte-exact staged archive XMP. Its log/provenance are in
../visible-archive-baseline/REPORT.md. It does not prove recovery.

## Remaining concrete work

- First inspect each failing log and the retained source; determine whether
  changed edited=false→true selection config caused a fixture bridge or a real
  supported progress hold. Do not clear genuine debt or weaken assertions.
- Make the active-selection test reach a genuine owner-admitted archive row or
  report the precise natural projection/redirect that prevents it. Never seed
  SQL authority or turn an absent row into a pass.
- Complete and independently review authentic capture-repair debt,
  prepublication race/hold, and legacy-ambiguous journal controls as needed;
  preserve exact no-replace and component ownership checks.
- Fix the recorded AF Clippy diagnostics only after source review; then freeze
  full source and hash manifests, build each applicable mode once under the
  shared lock, copy immutable binaries, and execute focused tests outside it.
  Run strict both-mode checks and broader qualification only when warranted.
- The old cross-binary prepared-journal consumer remains blocked as below.
  Do not infer it passed from the independent legacy-float unit test.

## Approval and denial boundaries

Automatic approval review rejected a read-only SSH/Python inspection of
[redacted denied retained SQLite target].
The stated reason was possible exposure of private SQLite destination/path
metadata without explicit authorization; it explicitly prohibited bypass by
workaround or indirect execution. The attempted read did not execute. Do not
read that target, run the consumer against it, retry indirectly, upload it, or
use a copied representation. The separate no-replace, debt, refresh and
fresh-download tests create their own independent synthetic state and did not
access the denied target. The historical consumer in the full local WIP source
is excluded from the proposed archival patch. Other rejected actions,
including into_lease, are not authorized by this WIP.

No live account, user archive, commit, push, PR, merge, tag or release was
performed in this lane. The user authorized routine tests, and the latest
wind-down explicitly forbids starting more work now. A later explicit user
authorization may be needed for the specific denied target; do not bypass
automatic review.

## Existing evidence and dependencies

- Full frozen source/AF logs: this directory.
- Earlier baseline and real byte mismatch:
  ../visible-archive-baseline/REPORT.md.
- Owner and source trace: ../visible-archive-packet/DIAGNOSIS.md and
  ../visible-archive-packet/NARROW-OWNER-STATUS.md. Some older owner-dependency
  discussion proposed broader admission; the later explicit adjudication
  narrowed this batch to physical no-replace and unchanged selected rows.
- Source-reviewed debt, explicit refresh, selected-accounting and no-replace
  test proposals: ../visible-archive-negative-controls/,
  ../explicit-refresh-owner-control/, ../copied-active-selection/,
  ../copied-noreplace-controls/ and ../copied-foreign-controls/.
- Main task coordination owns schema/retry/crash work. Do not reuse schema
  numbers, modify another owner's active source, or assume this patch supplies
  the separately needed sidecar admission/settlement lifecycle.
