# OpenCode advisory reviews

The `OpenCode advisory review` workflow reviews a non-draft PR when a human with repository write, maintain, or admin access posts a new standalone `/opencode review` comment in its conversation. Leading and trailing whitespace are allowed; case changes, quoted commands, code blocks, extra text, edited comments, bots, and comments on issues do not request a review. The script checks the author's current repository permission through GitHub at preparation and publication; author association alone never grants access. A permission lookup failure stops the run before inference or publication.

PR opens, pushes, rebases, reopening, and marking ready do not trigger reviews. Post a new command after the head changes or a failed request. A successfully reviewed head is skipped on repeated commands, duplicate events, reruns, and dispatches: at most one bot COMMENT review per head SHA while that review remains present. A removed review can be recreated on a later explicit request. Requests pin the current PR head when preparation starts, rather than the head at comment creation; stale results are discarded.

The review is advisory: demonstrable defects, regressions, and concrete missing safety coverage. Humans verify findings and decide whether to merge. It never approves, requests changes, fixes code, pushes branches, or merges. Do not add this workflow to required branch-protection checks. There is no backfill or scheduled review of existing PRs. Default-branch manual dispatch remains an explicit escape hatch for one selected PR.


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
existing API key, Go endpoint, no-overage prerequisite, and fixed routing; there
is no pay-as-you-go expansion. At most four inference POSTs are made for an
issue, two per reviewer. Missing credentials or invalid evidence fail closed.
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

## Comment format

Issue comments use a short verdict, one evidence/uncertainty paragraph, an
actionable next step, and up to three pinned source links. The usual target is
60-90 words of review content. Product scope remains conditional when existing
support is unknown; the charter alone cannot establish a regression. Code
comments retain a fixed notice that no tests or reproduction ran.

Both issue schemas require summary, rationale, next_step and evidence, plus
decision or status. Summary and next_step each allow 240 characters; rationale
allows 600. Oversized or malformed output is rejected for the existing fallback
chain, never truncated into a published result. The issue fingerprint remains in
the internal dedup marker rather than a visible preamble.

PR reviews retain every validated finding and its pinned source link. The prompt
asks for 1-3 short sentences per finding describing the trigger, impact, evidence
and smallest next step. Existing finding count and size limits remain unchanged.
An empty result says only that the supplied diff had no supported findings.

A compact footer names the advisory status, actual model (Go subscription models)
and pinned main/head commit. Repeated headings, scope essays and generic
disclaimers are omitted. Model text remains escaped plain text: quotes stay
literal, angle brackets and ampersands are neutralized, and Markdown, mentions
and autolinks remain inert. No HTML entities are broadly decoded; a source
literal such as &lt; stays literal. This avoids the apostrophe entity/Markdown
escape interaction visible in issue #1004 and older PR comments.

## Models, billing, and setup

Review attempts use this fixed order and stop at the first validated result:

| Order | Model ID | Route |
| --- | --- | --- |
| 1 | `deepseek-v4.1-flash` | Go subscription endpoint |
| 2 | `glm-5.3-flash` | Go subscription endpoint |

Both models use only
`https://opencode.ai/inference/go/openai/v1/chat/completions`.
The client permits no standard-endpoint or pay-as-you-go route. Every public
review names the model used for its validated findings.

**The Go URL alone does not prevent extra charges.** Official Go documentation
says enabling Console **Use balance** makes exhausted Go allowance fall back to
pay-as-you-go credits. The client has no documented request-level control to
prevent that. Every inference request remains blocked unless a maintainer has
confirmed **Use balance is disabled** and set repository variable
`OPENCODE_GO_NO_OVERAGE_CONFIRMED=true`. Do not set that variable until the
Console setting is verified; clear it if the account setting changes. This is
an explicit setup prerequisite, not a server-verified billing guarantee.
Without this confirmation, the workflow fails before its first inference request.

`OPENCODE_API_KEY` was confirmed present as a repository Actions secret using
name-only metadata during setup. Its value was not read. A maintainer must merge
workflow changes themselves. For installation or replacement:

1. In `rhoopr/kei`, open **Settings > Secrets and variables > Actions > Secrets**.
2. Select **New repository secret**, name it `OPENCODE_API_KEY`, paste the
   existing OpenCode key directly into GitHub, and select **Add secret**.
   Do not put the key in an issue, PR, chat, repository variable, or workflow file.
3. Verify **Use balance** is disabled in OpenCode Console before enabling Go
   reviews. Then use the repository's **Variables** tab to add
   `OPENCODE_GO_NO_OVERAGE_CONFIRMED` with value `true`. Leaving it unset or
   `false` blocks all inference requests. Other values fail configuration.
4. The existing optional repository variable `OPENCODE_GO_MODEL` remains
   compatible when absent or set to `glm-5.3-flash`. Other values are rejected.
   It cannot reorder or bypass the reviewed two-model policy. Changing the
   model order requires a code change.
5. After merge, request a review by posting `/opencode review` as a new PR conversation comment. For manual dispatch, use `gh workflow run opencode-review.yml --repo rhoopr/kei --ref main -f pr_number=989` or Actions > OpenCode advisory review > Run workflow, select the default branch, and enter one PR number. This does not update the PR or dispatch Rust CI. Other branch/tag refs are rejected. Draft/closed PRs and already-reviewed heads still skip. Adding a secret/variable does not trigger a run or broad backfill. Re-running an old run uses its original trusted workflow commit; after a script fix, post a new command or dispatch from the updated default branch.

Manual PR numbers must be positive canonical integers. The read-only preparation step resolves the PR through GitHub and records its exact head as a job output. Publication receives that head, validates the artifact's repository/number/SHA, and rechecks current head as usual. Comment requests and dispatches share a per-PR publication concurrency group. There is no user-supplied checkout ref, endpoint, or code path.

A missing key fails the inference step with `OPENCODE_API_KEY is missing; no
review completed.` It does not publish a successful empty review. Draft, closed,
stale, and previously reviewed heads skip explicitly. A cancelled run or failed
API/schema/diff validation does not establish a completed review.

## Trust boundary and publication

The `issue_comment` event supplies the workflow from the default branch; dispatch is restricted to that branch. Both jobs check out only `github.workflow_sha`, disable persisted checkout credentials, and run the trusted Python script in isolated mode. Never replace that ref with a PR head, merge ref, branch, or artifact. The workflow must already exist on the default branch; changes to its script in a PR are not executed by this review workflow.

The first job has only `contents: read` and `pull-requests: read`. Its collection
step uses GitHub's API to fetch metadata, complete patches, and source at
the exact head SHA. PR paths are URL-encoded data, never local filesystem paths
or shell arguments. It does not fetch PR dependencies, execute PR code or hooks,
build anything, or load agent instructions, plugins, or PR configuration. Even
AGENTS.md and workflow files in the diff are untrusted text for the model.

Inference receives the OpenCode key only in its step environment, with no GitHub
token in that environment or persisted by checkout. It sends a fixed system
prompt and JSON data to the fixed Go HTTPS endpoint, with no tools, agent runtime,
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

Comment runs are grouped by comment ID, so unrelated or unauthorized comments cannot cancel an authorized review. PR runs do not cancel in-progress runs. Publication is serialized per PR across comments and dispatches, with the duplicate check inside that serialized job. GitHub concurrency can replace a pending publication with a newer one; it does not guarantee FIFO ordering. Duplicate detection requires the bot identity, COMMENTED state, exact commit ID, and the head-specific marker; an author cannot spoof it with a normal comment. At most 1,000 historical reviews are scanned, failing closed if absence cannot be established. GitHub has no atomic current-head-and-publish operation: a push can race the final check, but `commit_id` and the body always identify the reviewed SHA. A removed review can be recreated on a later run. Human judgment remains necessary.

## Capacity, failures, and usage

PR reviews have no local file-count, changed-line, aggregate-source, diff-byte or prompt-byte allowance. The client paginates the changed-file list and submits the complete diff with up to 12 lines of head context around each right-side hunk line. It does not truncate the review to conserve subscription usage. Hunk counts, file totals, PR totals, and exact head source must agree. Binary files, omitted/truncated patches, unsupported source and missing patches for pure renames still fail the whole review. GitHub API availability and the serving model's actual capacity still apply; there is no batching or partial-review fallback. An empty findings list means only that no supported finding was produced within the supplied context.

GitHub's file-list API exposes at most [3,000 files per PR](https://docs.github.com/en/rest/pulls/pulls#list-pull-requests-files). If metadata reports more files than the API returns, collection rejects the incomplete listing before fetching head source. This platform limit does not impose a separate local review allowance.

The Go subscription allowance is the usage budget. Reviews may exhaust it; the client does not impose a smaller spending allowance. The required no-overage confirmation and Go-only route remain unchanged. Issue intake retains its separate deterministic context-selection limits described above.

Each review run makes at most two inference POSTs, one per model in the table.
Any provider HTTP error (including 403, 429, or 503), transport failure, oversized
response, malformed response, or invalid completion can advance to the next
model. There are no repeat attempts for the same model. Transport uncertainty
can mean an earlier request already consumed quota. Both attempts require
the no-overage confirmation before the first request. Failure of both models produces no
review artifact or public review.

Missing secrets, invalid configuration, invalid input, detected
credential material (including reasoning or decoded findings), invalid finding
source references, draft/stale heads, and duplicate reviews do not authorize
fallback. Input is checked before the first attempt; security failures stop the
chain immediately. Only a complete, validated result can reach publication.

Inference requests omit `max_tokens` and `reasoning_effort`, leaving output length and reasoning effort to the serving provider's defaults. Those defaults and the model's context window can still prevent completion. Such failures follow the existing fallback policy and cannot become a successful empty review.

Only one completed assistant text choice with `finish_reason=stop` is accepted;
tool calls, truncation, malformed JSON, extra fields, invalid locations, and more
than five findings are rejected. Findings have bounded title/body sizes. Failed
completions cannot become an empty successful review. Publication is never
retried automatically. A timed-out publication has an uncertain outcome; inspect
the PR before re-running. The duplicate check protects a subsequent run when
the review exists.

GitHub GETs have at most three attempts, 20-second socket timeouts, and 1/2-second retry delays for transport failures and transient server errors (500, 502, 503, 504). GitHub access-denial or rate-limit responses (403, 429) fail immediately with a fixed diagnostic and no automatic retry; the client does not wait out the rate-limit window. Inference has no client-side socket timeout. The existing 15-minute PR and issue inference job deadline is the overall stop, including collection and artifact upload. DeepSeek may use the remaining job time; GLM is attempted only if DeepSeek fails before that deadline. A job timeout is an unavailable review, not a completed review or an empty finding set. The provider may continue processing after the job stops. Individual HTTP responses are limited to 16 MiB by default to protect runner memory, including reasoning returned by the provider. Endpoint-specific issue metadata/tree limits and publication artifact/body limits remain. There is no cumulative HTTP request allowance; changed-file pagination is tied to the PR's reported file count, and duplicate-history scans remain bounded. Publication has a five-minute deadline.
Provider response/error bodies never appear in diagnostics. Each attempted model logs one fixed record: allowlisted route/model, numeric HTTP status or null, outcome, failure category or null, allowlisted finish reason or `unknown`, assistant/text/tool-or-refusal booleans, and bounded integer prompt/completion/total/reasoning token counts or null. Request failure categories are `timeout`, `transport`, `malformed_json` and `response_too_large`; raw exception text is never printed. Unknown remote strings are never printed. Security rejection omits response metadata. Usage fields are observations, not billing guarantees, and absent counts are not zero. These diagnostics distinguish transport failures, response parsing failures, token truncation and empty text without saving a response body. Repeated pushes can consume usage when requests have already reached the provider; this is not a repository-wide quota controller. Draft PRs defer inference.

Go has model-specific rolling five-hour, weekly, and monthly allowances; check
current usage in Console. The workflow transmits patches, selected head source,
paths, and SHA to OpenCode and its serving provider. PR title/body are not sent.
The model can still make mistakes or follow injected instructions; deterministic
validation limits output capabilities, not the correctness of its conclusions.

## API evidence and local checks

Verified against official documentation and public catalogs on 2026-10-17:

- [Go endpoints, client headers, limits, privacy, and Use balance](https://opencode.ai/v2/docs/console/go/)
  specify `glm-5.3-flash` and `deepseek-v4.1-flash` at the Go chat endpoint, a
  distinct `User-Agent`, and a stable `x-opencode-session`.
- [GLM API reference](https://docs.z.ai/api-reference/llm/chat-completion)
  documents always-enabled reasoning for GLM-5.3-Flash, `low/high/max` effort,
  `max_tokens`, finish reasons, and token usage. The client leaves reasoning effort and output length unset rather than overriding the provider defaults.
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

Consult the current Go documentation and serving-provider privacy terms before
sending sensitive code. Retention and training policies are provider policies,
not client-enforced guarantees. A failed attempt can transmit the same supplied
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
limits, uncertain publication, success at both model positions, HTTP/transport/invalid-completion fallback,
no-overage gating, exhaustion of all models, forbidden model overrides, and the base-only workflow
contract, exact created-comment admission, bot/quoted/edited/non-PR rejection,
current repository permission authorization instead of association, denied permission lookups,
comment/dispatch publication serialization, default-branch dispatch, prepared-head propagation, invalid PR/ref
rejection, and safe finish-reason/token/status diagnostics. They do not contact inference
endpoints or execute Rust tests. The
existing CI Python discovery already runs these fixtures.
