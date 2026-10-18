# OpenCode advisory reviews

The `OpenCode advisory review` workflow reviews non-draft pull requests on
`opened`, `synchronize`, `reopened`, and `ready_for_review`. It publishes one
`github-actions[bot]` COMMENT review for each successfully reviewed head SHA.
The review is advisory: demonstrable defects, regressions, and concrete missing
safety coverage. Humans verify findings and decide whether to merge. It never
approves, requests changes, fixes code, pushes branches, or merges. Do not add
this workflow to required branch-protection checks. There is no backfill or
scheduled review of existing PRs. A manual dispatch reviews exactly one selected
PR without changing its branch or PR state.


## New issue intake

After merge to main, the workflow also handles **issues.opened only**.
Editing or reopening an issue
does not trigger intake. There is no issue backfill, manual issue dispatch,
schedule, or automatic retry after a stale result is discarded.

Two independent reviews inspect the same pinned current-main SHA and issue
content snapshot:

- **Product Review** reads the complete docs/product-charter.md and
  docs/architecture.md. It recommends address, defer, out_of_scope, or
  needs_information, with direction, scope, invariant, and risk reasoning.
  This recommendation does not validate the issue's technical claims.
- **Code Validation** inspects selected main source and tests. It distinguishes
  a statically confirmed defect, plausible_unverified behavior, and
  insufficient_evidence. The not_reproduced status requires trusted execution;
  this text-only workflow rejects that conclusion because no reproduction runs.
  Comments explicitly state that static test inspection is not test execution.

Each reviewer has its own inference call and ordered fallback chain from the
model table below. Neither receives the other's output. Both use the same
existing API key, endpoints, no-overage prerequisite, and fixed routing; there
is no pay-as-you-go expansion. At most eight inference POSTs are made for an
issue, four per reviewer. Missing credentials or invalid evidence fail closed.
One failed reviewer does not cancel the other: the review and publication
matrices disable fail-fast, and publication runs after a partial failure.
Only an available, validated artifact can become a comment. Missing artifacts
leave a visible download-step warning and never become an empty review.

A read-only pin job resolves main once and records the issue content
fingerprint. Each reviewer checks the issue and main before preparing context.
The fingerprint includes the issue ID, number, title, body, creation time,
state, and lock state. It excludes updated_at, which also changes on comments,
so the two bot comments cannot invalidate each other. Title/body changes,
closure, locking, or a changed main SHA discard pending results. An edit
reverted to exactly the original content has the same content fingerprint.

Product context contains the complete charter and architecture. Code context
contains the charter, the first 135 architecture lines as an owner map, and
at most ten source/test excerpts. Deterministic selection uses literal path
mentions or path-word matches against issue title/body in the pinned main tree:
up to six Rust/script source files and four matching test files. Regular
executable files are eligible; symlinks are excluded. Files larger than 12 KiB
use a whole-line window starting up to 25 lines before the strongest issue-word
match. Start line, completeness and selection counts are supplied to the model. Each fetched file is capped at
256 KiB, issue metadata at 32 KiB, and the full JSON prompt at 300 KiB.
Missing owners, callers, later code, tests, logs, or reproduction inputs remain
limitations. This is bounded static investigation, not a repository-wide
audit. Citations must point to supplied lines at the pinned main SHA.

All issue text and linked material are untrusted data. Links and attachments
are not fetched. The workflow never executes issue-provided code, shell,
dependencies, test commands, agent instructions, or plugins. It invokes the
trusted workflow-commit Python client only. Issue content, selected main source
and tests are sent to OpenCode; they are checked for known credential patterns
and exact available tokens before inference. The existing secret-filter
limitations apply. Prompts, source, issue bodies, raw responses and reasoning
are not uploaded as artifacts or printed.

Issue inference jobs have only contents: read and issues: read. Their
collection steps have the GitHub token; only inference steps receive the
OpenCode key. Publisher jobs have contents: read and issues: write, no
inference credentials, and no model execution. They validate artifact identity,
schema, citations and secrets again, check bot-authored per-reviewer/content
dedup markers with bounded pagination, and recheck main and issue content
immediately before the single comment POST. GitHub offers no atomic
content/main-and-comment operation; each comment identifies both pinned
snapshots if a change races the last check. Publication is never retried
automatically after an uncertain POST.

Each successful reviewer publishes a separate advisory issue comment. A rerun
skips a comment already published for that reviewer and issue content, even if
main has since advanced. Normal users cannot spoof bot identity for dedup.
No labels, closures, branches, pull requests, or other issue mutations are
authorized by this workflow. Maintainers decide what to do with the advice.

## Models, billing, and setup

Review attempts use this fixed order and stop at the first validated result:

| Order | Model ID | Route |
| --- | --- | --- |
| 1 | `deepseek-v4.1-flash-free` | Free standard endpoint |
| 2 | `glm-5.3-flash-free` | Free standard endpoint |
| 3 | `deepseek-v4.1-flash` | Go subscription endpoint |
| 4 | `glm-5.3-flash` | Go subscription endpoint |

The free endpoint is
`https://opencode.ai/inference/openai/v1/chat/completions`. The subscription
endpoint is `https://opencode.ai/inference/go/openai/v1/chat/completions`.
The free model IDs are listed as free for input, output, and cached reads, for a
limited time. Paid model IDs are sent only to the already-purchased Go route.
The client never sends a paid model ID to the standard endpoint. Every public
review names the model used for its validated findings.

**The Go URL alone does not prevent extra charges.** Official Go documentation
says enabling Console **Use balance** makes exhausted Go allowance fall back to
pay-as-you-go credits. The client has no documented request-level control to
prevent that. Go fallback therefore remains blocked unless a maintainer has
confirmed **Use balance is disabled** and set repository variable
`OPENCODE_GO_NO_OVERAGE_CONFIRMED=true`. Do not set that variable until the
Console setting is verified; clear it if the account setting changes. This is
an explicit setup prerequisite, not a server-verified billing guarantee.
When both free requests fail and this confirmation is absent, the workflow fails
clearly before sending any Go request.

`OPENCODE_API_KEY` was confirmed present as a repository Actions secret using
name-only metadata during setup. Its value was not read. A maintainer must merge
workflow changes themselves. For installation or replacement:

1. In `rhoopr/kei`, open **Settings > Secrets and variables > Actions > Secrets**.
2. Select **New repository secret**, name it `OPENCODE_API_KEY`, paste the
   existing OpenCode key directly into GitHub, and select **Add secret**.
   Do not put the key in an issue, PR, chat, repository variable, or workflow file.
3. Verify **Use balance** is disabled in OpenCode Console before enabling Go
   fallback. Then use the repository's **Variables** tab to add
   `OPENCODE_GO_NO_OVERAGE_CONFIRMED` with value `true`. Leaving it unset or
   `false` permits free-model requests only. Other values fail configuration.
4. The existing optional repository variable `OPENCODE_GO_MODEL` remains
   compatible when absent or set to `glm-5.3-flash`. Other values are rejected.
   It cannot reorder or bypass the reviewed four-model policy. Changing the
   model order requires a code change.
5. After the workflow is merged, run one manual review from the default branch:
   `gh workflow run opencode-review.yml --repo rhoopr/kei --ref main -f pr_number=989`.
   Alternatively use Actions > OpenCode advisory review > Run workflow, choose
   the default branch, and enter one PR number. This invokes only this workflow;
   it does not update the PR or dispatch Rust CI. Other branch/tag refs are
   rejected. Draft/closed PRs and already-reviewed heads still skip.
   Adding a secret/variable does not trigger a run or broad backfill. Re-running
   an old run uses its original trusted workflow commit; after a script fix,
   dispatch a new run from the updated default branch instead.

For a single API connectivity test instead of a PR review, dispatch:

```sh
gh workflow run opencode-review.yml --repo rhoopr/kei --ref main -f mode=api_smoke
```

The default route is `free`. To test only the Go subscription route after
confirming Console **Use balance** is disabled and the no-overage variable is
`true`, add `-f smoke_route=go`. This is a separate, explicit request; the smoke
test never changes routes automatically.

Each dispatch sends exactly one non-streaming request containing only user text
`test`, capped at 2,048 output tokens and a 128 KiB response. GLM requests use
`reasoning_effort=low`; reasoning remains enabled. The `free` choice fixes
`glm-5.3-flash-free` and its standard endpoint; `go` fixes `glm-5.3-flash` and the
Go subscription endpoint. Unknown routes are rejected, and unconfirmed Go setup
blocks before network use. There is no fallback or retry, even on quota,
authentication, or availability errors. It uses the existing
Actions secret only inside a separate read-only job. It sends no PR context,
performs no publication, and creates no artifact. Logs show only fixed route,
numeric HTTP status when available, fixed validation outcomes, finish reason,
response-shape booleans, and bounded token counts when provided. Response bodies
and credentials are never printed. Truncated/invalid
responses fail explicitly even if the endpoint accepted the HTTP request.
The mode must first be merged into the trusted default branch; it cannot be
safely executed from this PR's branch. A free-model success proves endpoint
acceptance, not that the key has Go access, because free models can allow
unauthenticated calls.

Manual PR numbers must be positive canonical integers. The read-only preparation
step resolves the PR through GitHub and records its exact head as a job output.
Publication receives that head, validates the artifact's repository/number/SHA,
and rechecks current head as usual. Dispatch and PR events share the same per-PR
concurrency group. There is no user-supplied checkout ref, endpoint, or code path.

A missing key fails the inference step with `OPENCODE_API_KEY is missing; no
review completed.` It does not publish a successful empty review. Draft, closed,
stale, and previously reviewed heads skip explicitly. A cancelled run or failed
API/schema/diff validation does not establish a completed review.

## Trust boundary and publication

`pull_request_target` supplies a trusted base-side workflow. Both jobs check out
only `github.workflow_sha`, disable persisted checkout credentials, and run the
base-side Python script in isolated mode. Never replace that ref with a PR head,
merge ref, branch, or artifact. The workflow must already exist on the target
branch; changes to its script in a PR are not executed by this review workflow.

The first job has only `contents: read` and `pull-requests: read`. Its collection
step uses GitHub's API to fetch metadata, complete patches, and bounded source at
the exact head SHA. PR paths are URL-encoded data, never local filesystem paths
or shell arguments. It does not fetch PR dependencies, execute PR code or hooks,
build anything, or load agent instructions, plugins, or PR configuration. Even
AGENTS.md and workflow files in the diff are untrusted text for the model.

Inference receives the OpenCode key only in its step environment, with no GitHub
token in that environment or persisted by checkout. It sends a fixed system
prompt and JSON data to fixed HTTPS endpoints, with no tools, agent runtime,
shell access, or model-controlled requests. Redirects and environment proxies
are disabled. Only schema-validated findings and fixed identity fields become
a one-day artifact. Source context, prompts, raw responses, reasoning, and keys
are not uploaded or logged. Known credential patterns and exact available tokens
are rejected in input/output, including decoded findings. Pattern checks cannot
recognize every possible secret already committed in PR code; remove sensitive
material before opening a PR. Source necessarily leaves GitHub for review.

The second job receives only `contents: read` and `pull-requests: write` and has
no OpenCode key or model execution. It accepts only the artifact from this run,
revalidates its schema and identity, re-fetches the complete diff to validate
finding locations, rejects duplicates, then rechecks the current open/non-draft
head immediately before publishing. It renders model text as escaped text,
suppresses mentions/autolinks, and generates source links itself at the reviewed
SHA. Findings can reference only surviving right-side hunk lines, including
hunk context. Deleted files have no valid right-side finding location.

Concurrency cancels older runs for the same PR. Duplicate detection requires the
bot identity, COMMENTED state, exact commit ID, and the head-specific marker;
an author cannot spoof it with a normal comment. At most 1,000 historical reviews
are scanned, failing closed if absence cannot be established. GitHub has no
atomic current-head-and-publish operation: a push can race the final check, but
`commit_id` and the body always identify the reviewed SHA. A removed review can
be recreated on a later run. Human judgment remains necessary.

## Bounds, failures, and usage

The client accepts at most 40 changed files, 2,000 added/deleted lines, 128 KiB of
patch text, 256 KiB per fetched source, 1 MiB total fetched source, and a 300 KiB
JSON user prompt. Head context has no separate byte cap; it remains part of the
bounded total prompt. The local prepared-file reader retains its 428 KiB limit
for the prompt data and validated source locations. Context includes up to 12
lines around each right-side hunk line. Hunk counts, file totals, PR totals, and
exact head source must agree. Binary files, omitted/truncated patches, unsupported
source, missing patches for pure renames, and oversized data fail the whole
review clearly. It never calls a partial diff a complete review. An empty
findings list means only that no supported finding was produced within this scope.

Each review run makes at most four inference POSTs, one per model in the table.
Any provider HTTP error (including 403, 429, or 503), transport failure, oversized
response, malformed response, or invalid completion can advance to the next
model. There are no repeat attempts for the same model. Transport uncertainty
can mean an earlier request already consumed quota. Both Go attempts require
the no-overage confirmation; without it, both free models may be tried, then
review fails before any Go request. Failure of all four models produces no
review artifact or public review.

Missing secrets, invalid configuration, oversized/untrusted input, detected
credential material (including reasoning or decoded findings), invalid finding
source references, draft/stale heads, and duplicate reviews do not authorize
fallback. Input is checked before the first attempt; security failures stop the
chain immediately. Only a complete, validated result can reach publication.

Each inference POST is capped at 16,384 output tokens and 512 KiB of HTTP response
data, for at most 65,536 requested output tokens over the full chain. GLM uses
`reasoning_effort=low`; it cannot disable reasoning. DeepSeek uses its default
reasoning settings. These caps allow reasoning and answer space but do not
guarantee completion. The 64-token smoke request on 2026-10-17 returned HTTP 200
without passing text validation; its original logs did not retain finish reason
or usage, so the token cap is a plausible explanation, not a confirmed cause.
Smoke tests now allow 2,048 output tokens and remain exactly one request.

Only one completed assistant text choice with `finish_reason=stop` is accepted;
tool calls, truncation, malformed JSON, extra fields, invalid locations, and more
than five findings are rejected. Findings have bounded title/body sizes. Failed
completions cannot become an empty successful review. Publication is never
retried automatically. A timed-out publication has an uncertain outcome; inspect
the PR before re-running. The duplicate check protects a subsequent run when
the review exists.

GitHub GETs have at most three attempts, 20-second socket timeouts, and 1/2-second
retry delays for rate limits/transient failures. Inference has a 90-second socket
timeout; all requests have response-size limits and a 150-request operation
budget. The read job has a 10-minute deadline and publication has five minutes.
Provider response/error bodies never appear in diagnostics. Each attempted
model logs one fixed record: allowlisted route/model, numeric HTTP status or
null, outcome, allowlisted finish reason or `unknown`, assistant/text/tool-or-refusal
booleans, and bounded integer prompt/completion/total/reasoning token counts or
null. Unknown remote strings are never printed. Security rejection omits response
metadata. Usage fields are observations, not billing guarantees, and absent
counts are not zero. This distinguishes token truncation from empty text on a
future run without saving a response body. Repeated pushes can consume usage
when requests have already reached the provider; this is not a repository-wide
quota controller. Draft PRs defer inference.

Free model capacity and availability can change, and the offer is limited time.
Go has model-specific rolling five-hour, weekly, and monthly allowances; check
current usage in Console. The workflow transmits patches, selected head source,
paths, and SHA to OpenCode and its serving provider. PR title/body are not sent.
The model can still make mistakes or follow injected instructions; deterministic
validation limits output capabilities, not the correctness of its conclusions.

## API evidence and local checks

Verified against official documentation and public catalogs on 2026-10-17:

- [Console models](https://opencode.ai/v2/docs/console/models/) lists
  `glm-5.3-flash-free` and `deepseek-v4.1-flash-free` with the standard
  chat-completions endpoint, free pricing, and limited-time availability. Both
  exact IDs also appear in the public
  [inference catalog](https://opencode.ai/inference/v1/models).
- [Go endpoints, client headers, limits, privacy, and Use balance](https://opencode.ai/v2/docs/console/go/)
  specify `glm-5.3-flash` and `deepseek-v4.1-flash` at the Go chat endpoint, a
  distinct `User-Agent`, and a stable `x-opencode-session`.
- [GLM API reference](https://docs.z.ai/api-reference/llm/chat-completion)
  documents always-enabled reasoning for GLM-5.3-Flash, `low/high/max` effort,
  `max_tokens`, finish reasons, and token usage. Its output limit is much higher
  than this client's caps; these bounds are local usage policy, not model limits.
- [DeepSeek Chat Completions](https://api-docs.deepseek.com/api/create-chat-completion/)
  documents `max_tokens`, the `length` finish reason for exhausted output/context,
  and optional `usage.completion_tokens_details.reasoning_tokens`. The client
  records that count only when supplied as a bounded integer.
- [Inference API](https://opencode.ai/v2/docs/console/api/inference/) documents
  JSON POST requests, `Authorization: Bearer <token>`, and `model`/`messages`.
- [Official OpenAI-compatible adapter documentation](https://ai-sdk.dev/providers/openai-compatible-providers)
  confirms Bearer authentication and chat compatibility. The client uses the
  [Chat Completions response shape](https://developers.openai.com/api/reference/resources/chat)
  (`choices[0].message.content` and `finish_reason`) and validates it strictly.

The model documentation lists both free models as not used for training with
zero retention. Go lists zero-day retention for GLM and DeepSeek; DeepSeek's ZDR
agreement is renewed monthly, currently valid through October 31, 2026. Treat
these as provider policies, not client-enforced guarantees. Verify current terms
before sending sensitive code. A failed attempt can transmit the same bounded
PR context to the next listed model/provider.

Run offline fixtures and existing CI policy checks with:

```sh
python3 -m unittest discover -s scripts/ci -p 'test_*.py'
python3 .github/scripts/check_workflow_hardening.py
actionlint .github/workflows/*.yml
ruff check scripts/ci/opencode_review.py scripts/ci/test_opencode_review.py
```

These exercise issue opened-only admission, pinned main/content snapshots,
independent review calls and partial failure, issue comment dedup/spoof resistance,
static-only conclusions, issue artifact/citation validation, the real issue CLI
pipeline, and synthetic GitHub/provider responses, draft/stale/duplicate skips,
missing secrets, malicious fork text and paths, complete/oversized diffs, context
limits, malformed/tool/truncated output, credentials, safe rendering, rate
limits, uncertain publication, success at each of the four positions, HTTP/transport/invalid-completion fallback,
no-overage gating, exhaustion of all models, forbidden model overrides, and the base-only workflow
contract, default-branch dispatch, prepared-head propagation, invalid PR/ref
rejection, and safe finish-reason/token/status diagnostics. They do not contact inference
endpoints or execute Rust tests. The
existing CI Python discovery already runs these fixtures.

## Manual official CLI Free smoke

The separate OpenCode CLI Free smoke workflow tests the genuine OpenCode CLI
without changing production review routing. Once this workflow is merged to the
default branch, run it manually from the Actions tab on that branch. It starts
one independent CLI invocation for each of DeepSeek V4.1 Flash Free and GLM 5.3
Flash Free. A failed model does not cancel its sibling.

The workflow downloads official release v1.19.3 and checks the archive against
its pinned GitHub release SHA-256 before extracting the CLI. It has no repository
checkout, GitHub token permissions, secrets, publication, or uploaded artifacts.
The CLI uses its built-in public Free access in an empty temporary directory,
with fresh home/config/data/cache/state directories and no inherited credentials.
Only the selected Free model is allowlisted, including for lightweight tasks;
there is no configured Go or paid-model fallback. All tools are denied.
Session sharing, update checks, project config, external plugins, automatic
compaction, and OpenTelemetry export are disabled.

Each invocation has a 120-second deadline, an 8192-token output cap, and a 512 KiB
stdout cap. The CLI can apply its own bounded transient-error retries; the
workflow adds no retry loop. Logs contain only a fixed summary, a numeric error
HTTP status when the CLI provides one, and TEST_OK when validated final text
matches exactly and one terminal stop event is present. Raw CLI events,
reasoning, errors, and temporary session files are not published. Temporary CLI
state is removed when the runner exits.

OpenCode documents [noninteractive automation](https://opencode.ai/docs/cli/#run)
and [GitHub Actions](https://opencode.ai/docs/github/). Its
[model catalog](https://opencode.ai/v2/docs/console/models/#free-models) describes
these models as free on OpenCode for a limited time. This test measures actual
runner eligibility; it does not guarantee availability or resolve the broad
automation wording in the [service terms](https://opencode.ai/legal/terms-of-service).
An access denial fails the test without switching clients or models.

The offline test executes the exact inline workflow runner with a fake CLI.
It checks configuration, credential isolation, terminal completion, errors,
tool rejection, timeouts, and output limits without inference or Rust builds.
