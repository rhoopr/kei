# Support diagnostic export

Run this with the same configuration and data directory as your sync process:

```sh
kei --config ~/.config/kei/config.toml support-export --output kei-support.json
```

For Docker, use the existing container and mounted configuration:

```sh
docker exec kei kei --config /config/config.toml support-export --output /config/kei-support.json
```

Fresh container launches also skip ownership initialization for this command,
including when PUID/PGID are set. Prepare a writable output directory owned by
that UID:GID yourself; collection does not fix its permissions. Older images
may initialize volume ownership on fresh launches, so use the existing-container
route above or explicitly bypass their wrapper with `--entrypoint /usr/local/bin/kei`
and `--user UID:GID`.

Copy the JSON file from your configuration mount, review it locally, and attach
it to your bug report if you choose. Describe the symptoms, when they occurred,
and what you expected. Nothing is uploaded automatically. Choose a new filename
for each export; existing files are never replaced.

The command works offline. It does not log in, execute password commands, call
the provider, run a sync, scan media, calculate checksums, test writes, adopt
files, repair state, or migrate a database. Collection writes only the requested
output file. No new sync is needed to collect saved evidence.

Older versions may not have this command or retained history. If it is missing,
fails, or reports partial evidence, submit your report anyway. Include the
version or image tag and describe the unavailable section. Keep the partial
file when one was produced. Do not attach your database, raw cycle report,
authentication directory, configuration, or logs without reviewing and redacting
them separately.

## What the file contains

The version 1 export has a versioned allowlist and these sections:

- Exact available build revision, version, platform, features and detected
  installation method. A missing Docker digest is explicitly unavailable.
- Current file/environment configuration shape and retained effective cycle
  settings: counts, booleans and fixed modes. Names and paths are excluded.
- Observed local filesystem family, health timestamps and failure counts.
  Host OOM, supervisor and container restart evidence is unavailable.
- Bounded read-only account state totals and durable retry, unresolved identity,
  metadata, publication and reconciliation work counts.
- Retained operation/cycle IDs, UTC times, outcomes, transfer versus disk-write
  totals, checkpoint decisions, reasons, recovery actions and persistence
  results, token receiver and recovery counts.
- Grouped typed lookup, owner/discovery, inventory, sparse-reference, metadata,
  transfer, import, publication and completion evidence. New free-text provider
  failures remain unavailable until an explicit safe contract is added.

Inventory scope failures record expected and supplied owner class and JSON type,
never the owner value. A supplied owner is `not_recorded` when the failure did not
reach the response or child-reference scope being diagnosed. An observed missing
owner is `absent`; an explicit JSON null is the string category `null`.
Discovery records distinguish primary and other zones, validated private-primary
eligibility, selected indexing and actual publication of the qualified primary
library. Indexing an unrelated private zone does not establish primary ownership.
Older discovery records with `owner = other` have no precise owner type. Missing
historical fields remain unavailable; they are not filled with absent-owner or
successful-publication claims.

No credentials, cookies, tokens, Apple IDs, provider IDs, album names, private
paths, checksums, provider URLs, responses or arbitrary error text are exported.
Random aliases permit limited correlation within a bounded process lifetime.
Aliases change after a restart and are renumbered within each export.

## Limits and partial evidence

Normal sync, service-run and import operations retain evidence independently of
log verbosity. Recording is optional evidence and cannot change backup or
checkpoint decisions. History is account-scoped and persists across restarts.
Normal sync/import/service startup may create the support history and its lock
before pre-authentication validation completes, so validation failures can be
reported later. Authentication/session/database state is still not created by
failed pre-authentication validation. The export command creates only its
requested output file.
It retains at most 16 records, 128 diagnostic groups per record and 512 KiB.
Per-item success details are sampled at 64 groups per record. Later failures can
replace older detail/failure groups, and terminal summaries take priority over
both. Each new record reports observations_total, observations_shown and
observations_omitted; these count observations, not distinct assets. Older saved
records without accounting report null for these fields. Import scan_index is a
cycle-local ordinal, not an asset identifier. Group omissions and queue drops
remain explicit, so sampled detail is never presented as exhaustive.
An asynchronous queue holds at most 256 observations. If a cycle handoff is
lost, its later observations are omitted rather than assigned to a prior cycle. Repeated observations
with identical fixed fields are grouped; overflow and rotation are counted.
The worker serializes accepted observations, fsyncs a private staging file and
atomically replaces the history under a single-writer lock. Writes coalesce for up to 500 ms; orderly shutdown
flushes accepted observations. An abrupt exit can lose that unsaved tail. An interrupted
staging write leaves the previous snapshot usable. A later writer clears its
owned staging file. A second process cannot write the same history concurrently.

Read inputs are bounded: configuration 1 MiB, health 32 KiB and history 512 KiB.
State queries inspect at most 10,001 rows per table; the extra row signals that
a 10,000-row sample is incomplete. Metadata-capture revision progress retains at
most 32 anonymous primary/other scope records. Omitted row counts are unknown. Export size
is capped at 2 MiB. Counts refer to their documented operation/account scope,
and diagnostic observation counts can overlap. For example, paired lookup
counts of 604, 17 and 604 remain separate observations, not 1,225 unique assets.

The collector rejects live nonempty WAL/journal evidence and detects observed
main-file changes during collection. It explicitly reports an unavailable
snapshot instead of reading a stale WAL checkpoint. Stop the sync process
cleanly before trying again; a new sync is unnecessary. Unsupported, older,
missing, unreadable or mismatched-account state is reported without migration.
Immutable SQLite collection creates no WAL, SHM or journal files.

An unfinished record means that completion was not saved. It does not prove a
crash. A pre-commit SQL publication observation describes an attempted state
transition, not durable finalization. A successful receipt recovery describes
state recovery without a new byte publication. Fractional metadata observations
contain booleans and verification results, never capture dates or locations.
Absent fields were not recorded; null selection comparability is unavailable.
Initial inventory, delta bridge and combined invocation counts, token receiver
counters, recovery attempts/results and blocked reasons are labeled separately.
These records survive at the default log level, so an earlier failed repair or
successful inventory is not replaced by the later bridge outcome. Token proof
also records the initial receiver counts and the same-cycle retry counts
separately, preserving an initial mismatch even when retry repairs it. Token
proof completion describes receiver agreement; enumeration and checkpoint
fields independently describe whether source progress is safe. Token presence
and usability are separate booleans. Query completion is recorded by the provider fetcher; aggregate
absence of errors never asserts inventory completeness. Import records identify
child versus legacy-master selection and path-shape matches without identifiers.
Metadata plan records compare source and actual planned precision in one event;
publication failure does not assert whether replacement committed when unknown.
Current CLI overrides appear in retained effective settings rather than the
collector's current-file view.

History can be unavailable when recording cannot start or save, for example
because the data directory is unwritable. Compare record timestamps with the
incident window. The command gives next actions for missing, unfinished,
checkpoint-blocked and truncated evidence. A maintainer may still request
case-specific evidence for a novel provider field, external restart cause or
real-platform media verification.

### Full-enumeration token relationships

New runs can retain `support_token_equivalence_v1` when full enumeration reaches
token classification. Each record contains up to 64 selected passes, identified
only by local ordinal, with fixed pass/query kinds and scope-comparison categories.
`same_first_pass` compares the selected zone name, full zone ID, endpoint and
container with the first selected pass; it is a comparison, not new authorization.
No album names, provider identifiers or opaque tokens are included.

Within a single record, equal group numbers mean equal nonblank tokens. The
recovery record includes both initial and retried observations: unchanged groups
show persistent values, whereas disjoint groups show that the values changed.
Neither result alone identifies the provider cause. Never compare group numbers
between records or accounts. `not_retried` distinguishes retained initial proof
from a fresh recovery observation. Missing, blank, dropped, incomplete, oversized
and unobserved token evidence remains explicitly unavailable.

Completion order records stream drainage within each round, before all media
work necessarily completes. It does not measure token arrival or server snapshot
time or prove natural provider EOF; filtered/cancelled streams can also drain.
A null means drainage was not observed. `passes_omitted`, `rows_rejected`
and `rows_unavailable`, together with the existing history/queue omission counts,
identify partial evidence. Older exports have no such record, and cannot recover
these relationships retroactively. Unanimity and checkpoint holds are unchanged.
