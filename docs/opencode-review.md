# OpenCode advisory PR review

The `OpenCode advisory review` workflow reviews non-draft pull requests on
`opened`, `synchronize`, `reopened`, and `ready_for_review`. It publishes one
`github-actions[bot]` COMMENT review for each successfully reviewed head SHA.
The review is advisory: demonstrable defects, regressions, and concrete missing
safety coverage. Humans verify findings and decide whether to merge. It never
approves, requests changes, fixes code, pushes branches, or merges. Do not add
this workflow to required branch-protection checks. There is no backfill or
scheduled review of existing PRs.

## Models, billing, and setup

The primary model is **GLM-5.3-Flash Free**, exact ID `glm-5.3-flash-free`, at
`https://opencode.ai/inference/openai/v1/chat/completions`. It is listed as free
for input, output, and cached reads, for a limited time. The only permitted
fallback is **GLM-5.3-Flash**, exact ID `glm-5.3-flash`, at
`https://opencode.ai/inference/go/openai/v1/chat/completions`, through the
already-purchased Go subscription. No other models or pay-as-you-go route are
supported.

**The Go URL alone does not prevent extra charges.** Official Go documentation
says enabling Console **Use balance** makes exhausted Go allowance fall back to
pay-as-you-go credits. The client has no documented request-level control to
prevent that. Go fallback therefore remains blocked unless a maintainer has
confirmed **Use balance is disabled** and set repository variable
`OPENCODE_GO_NO_OVERAGE_CONFIRMED=true`. Do not set that variable until the
Console setting is verified; clear it if the account setting changes. This is
an explicit setup prerequisite, not a server-verified billing guarantee.
When the free request fails and this confirmation is absent, the workflow fails
clearly before sending any Go request.

`OPENCODE_API_KEY` was confirmed present as a repository Actions secret using
name-only metadata during setup. Its value was not read. A maintainer must merge
the workflow PR themselves; there is no trusted execution path for this new
workflow before it exists on the target branch. The first bounded live smoke
review remains pending that merge. For installation or replacement:

1. In `rhoopr/kei`, open **Settings > Secrets and variables > Actions > Secrets**.
2. Select **New repository secret**, name it `OPENCODE_API_KEY`, paste the
   existing OpenCode key directly into GitHub, and select **Add secret**.
   Do not put the key in an issue, PR, chat, repository variable, or workflow file.
3. Verify **Use balance** is disabled in OpenCode Console before enabling Go
   fallback. Then use the repository's **Variables** tab to add
   `OPENCODE_GO_NO_OVERAGE_CONFIRMED` with value `true`. Leaving it unset or
   `false` permits free-model requests only. Other values fail configuration.
4. Optional repository variable `OPENCODE_GO_MODEL` selects the fallback model.
   It defaults to `glm-5.3-flash`. Only that exact value is accepted; another
   model requires an explicitly reviewed code change. Free primary routing is
   fixed and cannot be bypassed through a variable.
5. Open or update one small non-draft PR after setup for the authorized live
   smoke review. Adding a secret/variable does not trigger a run or broad backfill.
   Re-running a failed run works if its head is still current. No real inference
   was used to validate this implementation; the first run still needs to verify
   provider acceptance of the existing key and completion behavior.

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
patch text, 256 KiB per fetched source, 1 MiB total fetched source, 128 KiB of
head context, and a 300 KiB JSON user prompt. Context includes up to 12 lines
around each right-side hunk line. Hunk counts, file totals, PR totals, and exact
head source must agree. Binary files, omitted/truncated patches, unsupported
source, missing patches for pure renames, and oversized data fail the whole
review clearly. It never calls a partial diff a complete review. An empty
findings list means only that no supported finding was produced within this scope.

Each run makes one free-model POST, plus at most one Go POST when eligible and
explicitly configured. Fallback is allowed only for HTTP 404, 429, 500, 502, 503,
or 504 from the free endpoint. Authentication/authorization errors, bad input,
transport uncertainty, malformed output, validation failures, and truncated
completions do not trigger fallback. A Go failure or exhausted allowance ends
the run; there is no second fallback or standard paid-route retry. Every
published review reports the model actually requested for the accepted result.

Each inference POST is capped at 4,096 output tokens and 128 KiB of HTTP response
data. Only one completed text choice with `stop` is accepted; tool calls,
truncation, malformed JSON, extra fields, invalid locations, and more than five
findings fail closed. Findings have bounded title/body sizes. The same POST is
never automatically retried because quota use or publication may have already
occurred. A timed-out publication has an uncertain outcome; inspect the PR
before re-running. The duplicate check protects a subsequent run when the
review exists.

GitHub GETs have at most three attempts, 20-second socket timeouts, and 1/2-second
retry delays for rate limits/transient failures. Inference has a 90-second socket
timeout; all requests have response-size limits and a 150-request operation
budget. The read job has a 10-minute deadline and publication has five minutes.
Provider errors and bodies never appear in diagnostics. Repeated pushes still
consume usage when requests have already reached the provider; this is not a
repository-wide quota controller. Draft PRs defer inference.

Free model capacity and availability can change, and the offer is limited time.
Go has model-specific rolling five-hour, weekly, and monthly allowances; check
current usage in Console. The workflow transmits patches, selected head source,
paths, and SHA to OpenCode and its serving provider. PR title/body are not sent.
The model can still make mistakes or follow injected instructions; deterministic
validation limits output capabilities, not the correctness of its conclusions.

## API evidence and local checks

Verified against official documentation and public catalogs on 2026-10-17:

- [Console models](https://opencode.ai/v2/docs/console/models/) lists
  `glm-5.3-flash-free`, the standard chat-completions endpoint, free pricing, and
  limited-time availability. The exact ID was also present in the unauthenticated
  [inference catalog](https://opencode.ai/inference/v1/models).
- [Go endpoints, client headers, limits, privacy, and Use balance](https://opencode.ai/v2/docs/console/go/)
  specify the fallback model/endpoint, a distinct `User-Agent`, and a stable
  `x-opencode-session`. The [Go catalog](https://opencode.ai/inference/go/v1/models)
  contained `glm-5.3-flash`; it did not contain the free model ID.
- [Inference API](https://opencode.ai/v2/docs/console/api/inference/) documents
  JSON POST requests, `Authorization: Bearer <token>`, and `model`/`messages`.
- [Official OpenAI-compatible adapter documentation](https://ai-sdk.dev/providers/openai-compatible-providers)
  confirms Bearer authentication and chat compatibility. The client uses the
  [Chat Completions response shape](https://developers.openai.com/api/reference/resources/chat)
  (`choices[0].message.content` and `finish_reason`) and validates it strictly.

The model documentation lists GLM-5.3-Flash Free as not used for training with
zero retention; Go lists zero-day retention for GLM-5.3-Flash. Treat those as
provider policies, not client-enforced guarantees. Verify current terms before
sending sensitive code.

Run offline fixtures and existing CI policy checks with:

```sh
python3 -m unittest discover -s scripts/ci -p 'test_*.py'
python3 .github/scripts/check_workflow_hardening.py
actionlint .github/workflows/*.yml
ruff check scripts/ci/opencode_review.py scripts/ci/test_opencode_review.py
```

These exercise synthetic GitHub/provider responses, draft/stale/duplicate skips,
missing secrets, malicious fork text and paths, complete/oversized diffs, context
limits, malformed/tool/truncated output, credentials, safe rendering, rate
limits, uncertain publication, free success, eligible Go fallback, no-overage
gating, Go exhaustion, forbidden model overrides, and the base-only workflow
contract. They do not contact inference endpoints or execute Rust tests. The
existing CI Python discovery already runs these fixtures.
