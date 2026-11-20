# CI qualification

## PR baseline

[PR baseline](../.github/workflows/pr.yml) runs the same three Linux jobs in parallel for every PR, including drafts. Paths, labels and repository variables do not select work.

| Job | What it checks |
|-----|----------------|
| Repository checks | Rust formatting, shell/Python/workflow lint, the safety contract catalog, workflow hardening and CI policy tests. |
| Clippy | `cargo clippy --locked --all-targets --all-features -- -D warnings`. |
| Sync safety | Two threads in one all-feature library harness run the transfer, checkpoint, bundled sync fixture, asset parsing, destination path, SQLite state/schema, import, filtering and selection groups, plus named safety contracts and a compact copies/reseed fixture. Run `bash scripts/ci/pr-smoke.sh` locally. Each group must contribute a passing test; the original three publication and retry cases must pass by name. |

Branch protection requires `CI`. It passes only when all three jobs succeed for the PR merge commit. New runs supersede old runs for the same PR. The target is seven minutes from PR update to the required result for 90% of updates. Track cold and warm builds and code-changing PRs separately.

## Broader checks

Pushes to `main` run the full Linux, macOS and Windows offline suites, Linux no-default-feature checks, copies stress, docs and dependency audit. To qualify a feature branch before merging, dispatch **Heavy qualification** for that branch. Qualification adds native service smoke, coverage, unused-dependency analysis, bounded fuzzing, optimized tests, five release archives and two container architectures. It lists ignored tests without running credentialed tests.

Nightly and release use that same qualification workflow. Release also requires separate local evidence before publishing. For local development, run focused tests for the behavior you changed. `just gate` remains available for offline diagnosis.

## Local release prerequisites

When a maintainer explicitly requests a release, run these local prerequisites
before starting release CI. Verify the configured isolated test account, selected
fixtures, and an unused host-service slot. Ask for input if those prerequisites
are unavailable or ambiguous. Never use an existing media/state tree as
disposable test data, and never replace an existing service. An ordinary PR or
CI rehearsal does not authorize credential-bearing checks.

1. Check out the final release commit with a clean worktree. Do not change it
   after qualification starts.
2. Run `KEI_FULL_TEST_REAL_SERVICE=1 just release-local` on the prepared Linux host.
   This builds the live CLI/container fixtures, then runs provider, shell,
   release-binary, import, and real service checks. It does not repeat the
   hosted offline suite. It requires working credentials, Docker, and an unused
   user-service slot. Retain the finalized report and private logs.
3. Run `python3 scripts/ci/local_release.py record --repo rhoopr/kei --sha COMMIT --report REPORT`.
   The validator rejects missing, skipped, rate-limited, failed, or dirty-head
   evidence. All discovered shell suites and the real host-service phase must
   pass. Cross-zone testing remains an explicit fixture-dependent extension.
4. Post only the redacted summary and report digest to the release PR. Do not
   upload private logs, credentials, provider identifiers, or local account paths.
5. As part of the authorized release, repeat the record command with
   `--publish --evidence-url COMMENT_URL`. The release gate accepts only the latest
   repository-owner status for that exact commit, no older than seven days.
6. Start the release only after those prerequisites pass. A tag starts heavy
   qualification; manual dispatch of Release rehearses it without publication.

The attestation is a maintainer trust boundary, not independent proof that local
commands ran. Keep the underlying evidence. A new commit requires a new run.
Nightly results do not replace this requirement.

## Nightly operation

When enabled, `Nightly qualification` runs on main at 07:23 UTC each day (03:23 EDT or
02:23 EST). It runs even when the commit is unchanged, since dependencies and
runner images can change. It has no publication permission. Manual dispatch on
main uses the same route; manual Heavy qualification can test a feature branch
without publishing or posting notifications.

When enabled, `Nightly freshness` checks at 19:23 UTC. A missing, failed, cancelled, or stale
completed run opens one incident issue. Incidents carry `bug`, `@infra`, `@test`,
and `nightly fail` labels. Missing labels are added to an existing incident
without replacing its other labels; the reporter creates `nightly fail` if needed.

A failure identity is the workflow run and its attempt. The reporter and the
freshness check use the same identity, and message text does not identify a
failure. An incident body records each observed finding. Both checks read open
and closed bot incidents that start with the trusted marker. Closing an
incident acknowledges its recorded findings, so the check does not refile them.
Only one incident stays open. Later findings are recorded on it without a
comment, and closing it acknowledges those findings too. An issue body changes
only when a finding is missing, and repeated checks do not add comments.

Missing and stale qualification have their own identities, separate from failed
runs. Acknowledging an old failure does not clear a later freshness alert.
After a qualification run has completed, the missing identity includes that run,
so closing a missing incident before a later recovery does not hide a new
missing episode.
Suppression changes notifications only; the checks still report failure.
Recovery adds a comment and closes the incident. A completed run must have
started within 36 hours. Both notification jobs share the `nightly-incident`
concurrency group, so incident updates stay serialized. Scheduled Actions can be
delayed or disabled; this second schedule detects missing qualification runs but
is not an external availability monitor for GitHub itself.

Reports and candidate artifacts are retained for 14 days. PR, nightly, and
release concurrency groups are separate. Nightly work cannot cancel a release.

## Advisory OpenCode review

[OpenCode advisory review](opencode-review.md) documents the optional bot review,
secret setup, provider data transmission, limits, and security boundary. It is
advisory and does not contribute to the required CI gate.

## Resume a partially published release

The Release workflow accepts optional `publication_run` and `publication_tag`
inputs together when dispatched from main. This mode publishes retained container
artifacts and updates Homebrew; it does not rebuild, republish archives, or move
the release tag. The tag must be the latest public stable release, and the original
tag-push run must have successful validation, heavy qualification and archive
publication for that exact commit.

The resume path checks artifact checksums and provenance again. After loading each
container, it verifies the exact config digest and ordered uncompressed layer
digests from a fresh Docker save, rather than comparing storage-backend-specific
image IDs. Missing, ambiguous or mismatched evidence stops publication.
