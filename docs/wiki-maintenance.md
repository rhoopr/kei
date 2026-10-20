# Maintain the wiki's release contract

The wiki is independent `rhoopr/kei.wiki.git`. This repository does not generate or publish it. Keep long procedures in repository guides and link to exact reviewed revisions from wiki gateways. Publishing a repository PR does not publish the wiki, and updating the wiki does not merge a repository PR.

## Run the checks manually

Use Python 3.11 or newer and a full source clone with the pinned commits available. No extra Python packages are required. Clone the wiki separately, or use an existing clean isolated wiki worktree:

```sh
python3 -m unittest discover -s scripts/docs -p test_wiki_docs.py
python3 scripts/docs/wiki_docs.py --wiki /path/to/kei-wiki
```

`docs/wiki-contract.json` declares the supported stable version, its code revision, the reviewed guide revision, the exact wiki snapshot, a compatibility baseline, development reference revision, historical exceptions and explicit command argv fixtures. Check a candidate in a worktree at the declared `wiki_revision`; uncommitted candidate edits are inspected too. The output records the wiki HEAD. Require a clean final worktree when recording publication evidence.

The checker reads commands and long flags from the pinned CLI source, checks nested command names against their source enums, scans literal shell examples, parses TOML snippets, checks dictionary key coverage and basic example types against the pinned dictionary, checks local page/heading links and pinned repository path/fragments, rejects mutable main/master blob links, and preserves every baseline Markdown filename and heading anchor. It never evaluates shell text or follows network links. Review historical exceptions individually; stale exception entries fail the check.

For supplementary offline parser checks against an already-built binary:

```sh
python3 scripts/docs/wiki_docs.py --wiki /path/to/kei-wiki --binary /absolute/path/to/kei
```

Only the explicit argv fixtures execute, always with `--help`, a temporary home/data directory and a clean environment. Their ledger records the effects that the corresponding command would have without help. Examples containing Docker, command substitutions, credentials or placeholders are never executed. The output records the binary SHA-256; a package version string does not prove its source revision. Record the build's provenance separately before treating it as a release-binary qualification.

These are bounded documentation checks, not a replacement for Clap's entire grammar or config resolution. Source flag checks do not prove value ranges, every conflict/requirement, shell portability, authentication, filesystem effects or safety of running a procedure. TOML checks cover syntax, known leaf keys and basic dictionary types; resolver ranges/enums need focused executable tests when changed. Markdown handling covers maintained headings, inline/reference links and wiki syntax; verify moved headings against GitHub's actual rendered IDs. External availability and live rendering are separate bounded publication checks.

These tools run manually against the declared wiki snapshot. This change adds no GitHub Actions workflow, job, trigger or required merge check. The regression tests live under `scripts/docs`, outside the existing `scripts/ci` test discovery. Existing repository lint and the existing Home/binary CI guard keep their current behavior; no unrelated workflow is changed. The tools do not publish anything.

## Release checklist

1. Refresh remote main and wiki before editing. Record exact heads, the installed/released version and source revision. Use isolated clean worktrees and preserve concurrent changes.
2. Compare the new release's CLI structures/actions, configuration dictionary/default resolver, supported platform assets and changed state/metadata/output behavior. Keep main-only features under an exact development revision until released. Do not infer build identity from `kei --version` alone.
3. Update `stable.version` and `stable.source_revision` together in the contract. Use a commit containing the released code. Review its action inventory and every explicit argv/effect fixture. Add fixtures for newly maintained command families, including source/conflict boundaries relevant to the change.
4. Review the canonical native/Docker trial, migration, upgrade, maintenance, metadata, layout and support guides. Fix shared procedures in their owning repository first. After merge, set `stable.guide_revision` to the actual reviewed documentation commit and repin the affected wiki links. A later guide commit can describe an older release; record both revisions.
5. Review Home, Upgrade, Docker/Synology, Configuration, command/flag reference and Development. Check account compatibility, matched backup/rollback, initialization effects, supported credentials and output limits. Use fresh isolated fixtures for any explicitly authorized runtime qualification; never substitute production media/accounts for a docs check.
6. Preserve old page names, nonbreaking-hyphen CLI slugs and old heading anchors. When content moves, keep a local summary/link at the old heading and record the full destination. Keep historical version mappings and the complete legacy index; do not mass-delete orphan compatibility pages.
7. Run the regression suite and full checker against the candidate. Investigate malformed/broken page/source links, lost headings, unsupported flags/actions, unknown config keys/types, stale exceptions and revision mismatch. Use binary help fixtures only with known build provenance; distinguish help parsing from executing the procedure.
8. Check public destinations and GitHub-rendered pages/navigation separately. A 200 response alone does not establish applicable release content. Preserve evidence for transient destination failures and independently verify their target rather than silently replacing valid history.
9. Refresh the remote wiki head immediately before publication. Reconcile concurrent work, inspect the complete diff and rerun checks against the actual candidate. Commit, set `wiki_revision` to that exact commit in the repository contract, and use a normal fast-forward push only after publication authorization. Do not force-push unexpected edits away.
10. Verify the published remote SHA and changed live pages, actual heading IDs, sidebar and new task/index links. Update the repository contract PR with the published wiki SHA, check its pinned snapshot, and retain evidence. The contract PR stays draft until the user chooses the normal review/CI/merge process; it does not block independent correct wiki content.
11. Advance `compatibility_revision` only after the new baseline has been checked to preserve prior URLs/anchors. Record the reason; never advance it merely to silence a missing-page failure. Keep the prior release record available.

## Procedure ownership

| Reader need | Owner |
|---|---|
| First small native/Docker backup | Release-scoped README / `docs/docker-first-backup.md`; wiki Home/Docker route there |
| Upgrade and matched rollback | Versioned upgrade guide; wiki Upgrade |
| Existing media / legacy account state | `docs/migration-from-icloudpd.md` / `docs/account-state-migration.md` |
| Repair, metadata and layout changes | `docs/backup-maintenance.md`, `docs/metadata.md`, layout guides |
| Password policy and account compatibility | Wiki Credentials / Authentication; command pages keep syntax |
| Native background registration, control, update and removal | Wiki Service; old Install service anchors keep summaries |
| Reports, metrics and privacy | Wiki Reports-and-Metrics / Support and `docs/support-export.md` |
| Historical flags | Wiki Legacy-Flags and retained compatibility pages |

Keep warnings local when needed to make an action safe, then link the owning procedure. Reducing duplication is not permission to drop useful examples, output tables or evidence limits.
