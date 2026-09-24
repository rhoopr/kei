# Backup maintenance

Keep an independent backup before local repairs. Do not delete conflicting
files, temporary siblings, or the state database to make a warning disappear.

## Choose the operation

| Need | Operation | What can change |
| --- | --- | --- |
| Inspect recorded progress | `kei status` | No repair |
| Check recorded bytes | `kei verify --checksums` | Integrity diagnostics, not restoration |
| Preview missing/truncated state | `kei reconcile --dry-run` | No writes |
| Queue missing/truncated files | `kei reconcile` | SQLite state, not media |
| Replace a recorded truncated file | `kei sync --repair-truncated` | Verified media replacement with explicit consent |
| Refresh provider metadata | `kei sync --refresh-metadata` | Catalogue plus configured local outputs |
| Replace capture timestamps | `kei sync --refresh-metadata --repair-capture-timestamps` | Supported embedded timestamps, including camera values |
| Change folder layout | Edit path configuration, then `kei sync` | Verified local copies; old copies remain |

Automatic path reconciliation after configuration changes is separate from the
`reconcile` integrity command. Neither is a general container-corruption repair.

## Refresh catalogue and configured outputs

Use a separate repair configuration that preserves your existing `data_dir`,
credentials, and download directory. Remove the `albums` and `smart_folders`
keys entirely, even when they contain `all` or `none`: explicit selectors reject
this repair. Also remove date bounds and `recent`/`recent_scope`. This selection
covers all visible libraries and enables the unfiled pass:

```toml
[filters]
libraries = ["all"]
unfiled = true
media = ["photos", "videos", "live-photos"]
```

Choose the libraries you intend to repair. Do not add one-run date or recent
filters. Then run:

```sh
kei sync --config /path/to/repair.toml --refresh-metadata
```

The command refreshes every downloaded version encountered in those libraries
before current resolution, filename, or Live Photo download policy is applied.
It does not download media again. Embedded and sidecar changes follow the
configured metadata writers. Disable embedded outputs if media bytes must stay
unchanged; disable sidecar output too if no local metadata should change.

This is one-shot even when the TOML file sets a watch interval. It rejects
`--dry-run`, `--retry-failed`, `--only-print-filenames`, `--recent`, narrowing
selection, and `service run`. See [Metadata output](metadata.md) for automatic
bounded capture repair, which is different from this explicit full sweep.

### Replace capture timestamps

First enable `metadata.set_exif_datetime = true` in the repair configuration.
Then explicitly authorize replacement:

```sh
kei sync --config /path/to/repair.toml --refresh-metadata --repair-capture-timestamps
```

This can overwrite camera-supplied timestamps. It writes a supported
capture-local timestamp with its matching Apple offset. It requires usable
offset evidence and recorded file/checksum evidence. Unknown provenance,
changed bytes, or unsupported layouts can leave durable repair work pending.
Prepared-output receipts allow a later explicit repair to finish interrupted
state finalization without trusting unrelated bytes. Do not promise that every
historical file can be repaired. Ordinary writes preserve native timestamps.

## Repair a truncated file

Inspect before changing state:

```sh
kei reconcile --dry-run
kei reconcile
kei sync --repair-truncated
```

Reconcile checks recorded paths and metadata-aware sizes, then marks missing
or truncated downloads for retry. It reads media but writes state unless
`--dry-run` is set. Periodic watch reconciliation also writes state.

Only explicit repair may replace an existing recorded truncated file. It
requires the durable truncation marker, verifies the new download and configured
metadata writes, and checks the old fingerprint before atomic replacement.
A changed target remains a failure rather than permission to overwrite it.
Normal downloads and missing-file retries keep no-overwrite publication.

Truncated-file repair is one-shot and rejects `service run`, `--dry-run`,
`--only-print-filenames`, and `--refresh-metadata`. Unlike metadata refresh,
it can use your current selection. Files excluded by that selection are not a
promise of completed repair. Inspect the result before resuming a service.

## Change the backup layout

Path-setting changes reconcile recorded media through verified local copying.
Old sources remain. Ordinary copies receive capture mtime; a safe same-path
change between equivalent relative and absolute root spellings only changes
catalogue spelling, preserving mtime and existing or absent sidecars.

Eligibility changes, such as date bounds, do not by themselves relocate every
file. Active provider checkpoints remain while replacement inventory proof is
built. Path reconciliation also preserves checkpoints rather than clearing them.

Reconciliation uses retained directory handles, no-follow traversal, checked
file identity, exclusively created temporary files, and no-overwrite publication.
It rejects symlinks, non-regular leaves, unsafe/replaced parents, `..` components,
and a distinct destination hard-linked to its source. Ordinary relative roots
remain supported. These restrictions describe reconciliation, not every command.
On Unix, temporary copies start owner-only; completed copies receive source
permissions. Ambiguous temporary entries remain for inspection.

With sidecars enabled, a valid source packet is copied byte-for-byte, including
custom fields and ownership markers. A missing packet is generated by the normal
metadata planner. Source-read failure stops before an incomplete packet lands.
Migration does not upgrade an existing packet or create original GPS provenance.

Reservations distinguish library, asset, rendition, and provider content.
Equivalent path spellings share ownership. Another owner's reservation can
select a stable identity-suffixed destination. Changed provider content can
receive a new reservation while old and unselected renditions remain protected.
Conflicting bytes never authorize overwrite or a fresh filename on every retry.
Ordinary download collision naming remains unchanged.

Catalogue finalization follows capture mtime, required sidecar work, and final
input validation. A metadata or state-write failure keeps pending work and its
selected destination. A safe retry can finish without copying media again.

### Recover from a blocked move

1. Stop the affected worker and preserve its state and reported conflicting
   paths. Inspect identities and bytes without modifying the originals.
2. Check configured roots, permissions, parent directories, and the reported
   media or sidecar conflict. Remove `..` from reconciliation roots.
3. If a conflicting entry must be moved, first back it up and choose an explicit
   destination outside the planned path. Do not apply blanket deletion commands.
4. Retry the same configuration and inspect the result. The old catalogue path
   remains until all required finalization succeeds.

Blocking failure skips that library's normal source/download/adoption pass and
withholds its checkpoint and the aggregate database precheck token. Selected
smart-folder reconciliation still has the limitation in #770.

## Interpret recovery evidence

A policy-excluded row is not provider-deleted. Pending retries use targeted
provider revalidation and durable identity evidence, with full-enumeration
fallback where needed. Unknown or transient responses retain retry evidence.

Complete enumeration proof plus durable exact retry work can permit zone-token
progress after a transfer failure. Identity, grouping, or state failures without
recovery proof cannot. Aggregate database prechecks have stricter clean-cycle
gates. Zero failed downloads does not establish successful metadata, state,
enumeration, or checkpoint work.

Provider MMCS identity, original-download SHA-256, post-metadata local SHA-256,
and verified `source_checksum` provenance serve different purposes. Historical
download hashes are not proof of unmodified source bytes. Adoption/import have
evidence limits; integrity commands cannot prove completeness against iCloud or
restore damaged media. Never remove temporary files by suffix and age alone:
cleanup requires durable ownership evidence.

## Unresolved provider identities

If logs show `asset_delta_hydration_incomplete`, keep the state database and
media. Kei retains the provider checkpoint so a later cycle can retry the
unresolved records. Other valid assets can still download. Do not reset state
or use a metadata refresh to bypass this guard.

For valid sparse-share records, kei retains per-source retry evidence across
restarts. An unchanged unresolved lookup retries after one hour, then doubles
the delay up to 24 hours. Each library execution selects up to 100 due sparse
source lookups. Exact saved source/master mappings can recover immediately;
malformed or changed evidence does not reuse the negative result. `kei status`
shows how many retained sparse records are unresolved and deferred. A deferred
lookup does not mean that the record was skipped safely or deleted. These
limits reduce repeated lookups; they do not repair a missing provider identity.

This state uses schema 26. Stop workers and back up the state database before
trying this build. Older binaries that support only schema 25 cannot open the
upgraded database. Keep a compatible backup if you need to return to an older
build. Do not reset state or lower its schema version to bypass this check.

Kei marks unresolved asset identity cycles incomplete. Status
remains unsafe across restarts and successful syncs in other libraries until
the affected checkpoint can advance safely. Health does not update
`last_success_at` for incomplete cycles. A deployment that previously looked
healthy can therefore start reporting unhealthy without a new transfer error.

For a diagnostic retest, disable automatic restarts driven by `/healthz`.
Keep health monitoring enabled. Do not use this sync-health endpoint as a
Kubernetes liveness probe during the retest. Repeated provider failures can
produce HTTP 503 while the process is still running and able to retry.

Record `kei --version`, the Docker image ID when applicable, `kei status`,
health output, and the aggregate `Asset identity lookup` log lines from one
cycle. Include whether the accounts use a Shared Photo
Library. Redact account details and local paths from other logs. Do not upload
the state database, session files, or raw provider responses.

The diagnostics do not establish why Apple omitted an identity. Ambiguous
metadata-capture matches are a separate condition and remain pending.
