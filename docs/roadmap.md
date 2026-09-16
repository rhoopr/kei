# kei roadmap

This roadmap is directional. Versioned GitHub milestones track committed
release work. Deferred issues retain their own scope and release disposition.

## Current focus

Backup correctness, with safe recovery as the user-visible proof.

v0.24 has a frozen backup-correctness scope: safe path reconciliation,
metadata ownership and recovery, provider checkpoint and identity fixes, and
truthful headless authentication. The safe HEIF writer is already merged.
Remaining release work includes documentation and validation, including HEIF
memory measurements. This is not a promise to close every correctness issue.
See [candidate limitations](v0.24-upgrade.md#known-limitations).

The milestone order is v0.24 backup correctness, v0.25 internal consolidation,
v0.26 headless automation, v0.27 catalog and scale, and v0.28 media fidelity.
Milestones describe planned work, not published releases.

## Roadmap themes

### Backup confidence

Help users prove kei is syncing safely and recovering correctly.

Candidate work:

- Harden normal sync stability.
- Harden interrupted sync recovery.
- Improve failed-download visibility and retry behavior.
- Guard against unsafe sync-token advancement.
- Show active sync work in `kei status`.
- Detect missing or damaged local files during incremental sync.
- Extend the shipped manifest export and `kei doctor` diagnostics.
- Add a redacted support bundle.
- Expand `kei doctor` from the first local checks into backup-confidence
  diagnostics once status and reports can say whether the last run was safe.

Success criteria:

- Normal sync is dependable.
- Interrupted sync resumes correctly.
- Failed downloads are visible and recoverable.
- Sync tokens advance only after safe, complete work.
- Users can see what kei is doing during a long unattended run.
- A damaged or missing local file can be found and repaired without guesswork.

### Headless operations

Make unattended operation easier to run and support.

Candidate work:

- Add a notification test command.
- Improve webhook and desktop notification flows.
- Make browser-based watch-mode 2FA easier.
- Document Grafana or Prometheus examples.
- Keep service output machine-readable and predictable.

Success criteria:

- Service users can tell whether kei is healthy without opening an interactive
  shell.
- Notification and metrics setup can be tested before waiting for a real sync
  event.
- Support reports explain the local installation without exposing credentials.

### Catalog and scale

Expose useful catalog data and reduce wasted work on large libraries.

Candidate work:

- Export provider metadata through the JSON manifest.
- Add bounded listings and discoverable named configurations.
- Add a narrow read-only catalog query command.
- Stream large incremental syncs.
- Plan full-library syncs before downloads where that avoids duplicate or stale
  work.
- Share incremental delta handling between streaming and collecting paths.
- Keep sync-token advancement tied to complete and safe enumeration.

Success criteria:

- Large libraries start producing useful work quickly.
- Operators and integrations can inspect catalog data without parsing local
  media files or opening the state database.
- Selected albums, smart folders, and libraries avoid unnecessary whole-library
  scans where the provider model allows it.
- Incomplete enumeration does not advance tokens.

### Media fidelity

Preserve the media users expect.

Candidate work:

- Handle edited-photo naming better.
- Extend the shipped opt-in HEIC, HEIF, and AVIF writer to more proven layouts
  and reduce whole-file memory use.
- Improve cross-library deduplication.
- Research shared albums separately from shared libraries.

Success criteria:

- Originals, edits, Live Photos, RAW siblings, and metadata stay traceable.
- File rewrites remain opt-in and safe.
- Provider-specific media quirks stay documented.

### Destinations and providers

Grow beyond local iCloud-to-folder backup once the local catalog is strong.

Candidate work:

- Immich integration.
- Google Takeout import.
- Nextcloud or WebDAV.
- S3 or object storage.
- More packaging where it has clear user demand.

Success criteria:

- New sources or destinations reuse the local catalog instead of bypassing it.
- Destructive or upload workflows have explicit dry-run and safety gates.
- The file-backed backup remains useful even when a downstream destination fails.

### Destructive lifecycle workflows

Add cleanup only after backup confidence surfaces exist.

Candidate work:

- `kei prune --dry-run` for local deletion planning.
- A later explicit delete mode, if dry-run proves the model.
- iCloud-side deletion only as a separate later workflow.

Success criteria:

- The first cleanup slice is read-only.
- Users can see why each file is a candidate.
- No destructive behavior is bundled into normal sync.

## Near-term milestones

### v0.22 - Stability and reliability - shipped

v0.22 focused on normal sync safety, recovery, reporting, and token gates.

Shipped work:

- Normal sync retries known pending or failed asset-version rows before trusting
  incremental progress.
- Interrupted, token-unsafe, and failed state-write paths keep sync tokens from
  advancing until the work is safe.
- `kei status` shows active sync work and backup-safety state before completed
  run history.
- `kei reconcile` detects missing and truncated local files and can mark them
  failed for re-download.
- `kei manifest` exports the local state catalog without contacting iCloud.
- The first `kei doctor` slice provides redacted local diagnostics and optional
  live session checks.
- Hard-delete recovery can use durable asset-to-master mappings when Apple only
  sends an asset record name.

Still out of scope:

- General catalog query.
- Local file deletion.
- iCloud-side deletion.
- Immich upload.
- Provider expansion.
- UI work.

### v0.23 - Metadata correctness and recovery - shipped

v0.23 focused on capturing provider metadata consistently and repairing stale
catalog or sidecar state without downloading media again.

Shipped work:

- Incremental sync captures metadata-only provider edits before advancing the
  provider checkpoint.
- `kei sync --refresh-metadata` repairs catalog metadata and configured local
  metadata outputs through a complete library sweep.
- Full-enumeration skips tag already-downloaded files for metadata rewrite when
  provider metadata changes.
- iCloud location decoding uses Apple's longitude field correctly.
- Zone discovery selects photo-library zones without treating shared-album
  collection zones as libraries.
- Bounded and fallback full syncs can revalidate pending provider identities.
- CLI help and runtime errors link to command documentation and redacted
  diagnostics.

### v0.24 - Backup correctness

Goal: close known correctness gaps before adding new product surface.

User outcome: provider changes, retries, and metadata repairs converge without
duplicate downloads, stale sidecars, forgotten work, or misleading success.

Committed work:

- Make provider metadata refresh atomic across full and incremental sync paths.
- Render capture timestamps from Apple's per-asset offset.
- Resolve legacy pending and policy-excluded rows from durable provider evidence.
- Keep asset identity stable when provider records cross query-page boundaries.
- Make sidecar rewrites reproduce current album, people, and cleared metadata.
- Track metadata-capture revisions and schedule bounded repair after semantic
  changes.
- Make non-interactive authentication fail fast with truthful exit codes.

Success criteria:

- Provider metadata is durable before a checkpoint can advance.
- The same provider asset keeps one local identity across enumeration paths.
- Catalog and configured local metadata converge after provider edits.
- Inconclusive provider responses preserve retry evidence without claiming
  success.

### v0.25 - Headless automation

Goal: make unattended Docker, service, and automation workflows predictable.

User outcome: operators and automation can inspect state, provide input, test
notifications, and diagnose failures without parsing friendly terminal output.

Committed work:

- Standardize machine-readable output and add JSON status output.
- Export provider catalog metadata through the JSON manifest.
- Ship versioned agent context and workflow metadata.
- Tighten non-interactive reset, password, and configuration behavior.
- Improve Docker packaging and support Apple's container runtime.
- Add testable notification paths, including webhook, desktop, and MQTT options.
- Document Prometheus and Grafana setup.
- Add a browser-assisted watch-mode 2FA path.

Success criteria:

- Data-returning commands have stable structured output.
- Headless input requirements fail clearly instead of hanging or reporting
  success.
- Notification and metrics setup can be tested before a real sync event.
- Published containers and service diagnostics agree with supported commands.

### v0.26 - Catalog and scale

Goal: expose useful catalog data and reduce wasted work on large libraries.

User outcome: integrations can inspect the local catalog, and large or filtered
syncs start useful work without unnecessary whole-library buffering.

Committed work:

- Add bounded listings and discoverable named configurations.
- Add a narrow read-only catalog query command.
- Stream large incremental syncs through bounded processing.
- Plan full-library syncs before downloads where that reduces waste.
- Share delta-state handling between streaming and collecting paths.
- Remove avoidable allocation from retry and reconcile scans.

Success criteria:

- Catalog consumers have bounded, supported read paths.
- Large syncs start useful work earlier with bounded memory.
- Enumeration and checkpoint safety remain conservative.

### v0.27 - Media fidelity

Goal: improve how kei preserves media variants and metadata.

User outcome: edited media, HEIC-family metadata, duplicates, and shared media
are easier to understand and preserve.

Committed work:

- Let edited photos use the primary filename when configured.
- Restore HEIC, HEIF, and AVIF embedded metadata writes only through a proven
  safe path.
- Avoid duplicate local copies across primary and shared libraries when the
  underlying media identity is the same.
- Research shared albums separately from shared libraries.

Success criteria:

- Media variants stay traceable.
- Local metadata writes remain explicit and safe.
- Cross-library deduplication never removes or overwrites existing media.

### Later - Lifecycle, integrations, and maintenance

This milestone is categorized backlog, not a release commitment.

Candidate work:

- Read-only prune planning before any destructive lifecycle workflow.
- Immich, Google Takeout, Nextcloud, WebDAV, and object storage integrations.
- Decide the boundary between kei and native service management.
- SQLite schema hardening and focused internal maintenance.

Success criteria:

- New workflows build on the backup and catalog model.
- Users can inspect proposed changes before any destructive action.
- Maintenance work is pulled into a versioned milestone only when it supports a
  committed user outcome.
