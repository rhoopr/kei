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
