# Support diagnostic export

Run this with the same configuration and data directory as your sync process:

```sh
kei --config ~/.config/kei/config.toml support-export --output kei-support.json
```

For Docker, use the existing container and mounted configuration:

```sh
docker exec kei kei --config /config/config.toml support-export --output /config/kei-support.json
```

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

No credentials, cookies, tokens, Apple IDs, provider IDs, album names, private
paths, checksums, provider URLs, responses or arbitrary error text are exported.
Random aliases permit limited correlation within a bounded process lifetime.
Aliases change after a restart and are renumbered within each export.

## Limits and partial evidence

Normal sync, service-run and import operations retain evidence independently of
log verbosity. Recording is optional evidence and cannot change backup or
checkpoint decisions. History is account-scoped and persists across restarts.
It retains at most 16 records, 128 diagnostic groups per record and 512 KiB.
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
Initial inventory, delta bridge and combined invocation counts are labeled
separately. Query completion is recorded by the provider fetcher; aggregate
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
