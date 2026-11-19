# Diagnostic evidence

Use these boundaries to interpret local evidence and decide what can appear in a support export.

## Secrets and diagnostics

Passwords stay behind `SecretString` and password-source boundaries. Logging
uses a redacting writer as a backstop, but code must not send Apple IDs,
passwords, session cookies, bearer tokens, or unredacted provider identifiers
to logs or machine output. Preserve process hardening that limits credential
exposure through core dumps.

`password set` prompts only in interactive input mode. Headless callers must
use a password file or password command. The command resolves the secret and
passes it directly to the credential store without printing it.

## Support evidence boundary

`support-export` dispatches before startup configuration construction, logging,
credentials or provider initialization. `src/support/collect.rs` reads bounded
local files; `src/state/db/support.rs` owns immutable SQLite inspection without
normal `open_owned` or schema migration. Unsupported schemas and nonempty WAL or
journal snapshots degrade explicitly. Export output uses exclusive creation.

Normal sync, import and service operations publish typed evidence from their
existing owners through a bounded optional worker. The worker groups only
allowlisted fields, persists account-scoped history with private atomic writes
and a single-writer lock, and never feeds decisions back to provider, checkpoint,
state or publication policy. Its tracing layer is independent of log verbosity.
Stored history is revalidated against the same contract before export. Unknown
fields, labels, arbitrary errors and provider identifiers cannot broaden it.
The Photos owner projects only fixed owner morphology and JSON types for rejected
inventory scopes. Private discovery records identify primary versus other zones
and validated primary eligibility. A primary-publication observation follows the
actual qualified-primary assignment; selected indexing alone does not imply it.
Owner categories are revalidated as strings, so a malformed numeric owner cannot
escape through an aggregate-count allowance. Missing older fields remain missing.

Full enumeration also emits `support_token_equivalence_v1` from
`full/token_diagnostics.rs`. Each self-contained snapshot relates initial and
same-cycle recovery token groups for at most 64 selected passes. Tokens are
compared only in a bounded invocation-local table (128 values, at most 4096 bytes
per value); only assigned ordinals reach the support boundary. The provider owner
supplies fixed query and scope-comparison categories. A passive stream wrapper
records drainage order without extra polling or tasks. Missing drainage is null;
this order is not token-arrival, HTTP-completion or media-publication order.
`support/token_equivalence.rs` revalidates nested rows at capture and export,
rejects malformed or duplicate rows, and preserves explicit omission markers.
Group numbers have meaning only within one snapshot, never across invocations,
cycles or accounts. The recovery snapshot repeats the initial evidence so history
retention cannot silently sever cross-round relationships. This evidence has no
consumer in enumeration, recovery or checkpoint policy.

`photos/album/preservation_proof/missing_reference.rs` owns observation-only
lookup at the first missing-parent inventory refusal. Four byte-sized typed
pre-compaction categories live alongside every latest inventory record and count toward its
retained-byte budget, without a separate identity/count sampling cap. Lookup
payload shape is separate from scope availability. Scope `matched` means the
child scope matched; reference scope is also required and checked for a
`live_reference` payload before family classification.
`PhotosSession::post_diagnostic` uses a dedicated 64 KiB transport with fixed errors,
no request dumps, error-body logging or retries. The caller owns cancellation and
a five-second deadline. Its response never enters inventory or proof construction;
only fixed categories enter support history and export.

Random process-local aliases are bounded and renumbered at collection time.
Counts are scoped observations and must not be added as unique asset counts.
SQL observations before transaction commit are explicitly identified as attempts.
`src/support/mod.rs::classify_support_error` owns diagnostic typed-error
projection. It exports fixed classes and numeric status codes from the cause
chain, never messages or provider payloads, and does not decide retry policy.

The inbox, catalogue and work owners publish fixed storage counters
after successful transaction commit and at existing capacity refusal branches.
The history worker retains a fixed summary per cycle, not response rows.
Budgets reuse existing admission accounting and are labeled last observed,
pre-admission logical charges. Historical selection and compaction fields remain readable, but their runtime event writers are retired. Migration retains its savepoints, rollback attempts and error reporting without emitting compaction events. Retained reasons and counters contain no source identity,
URL or arbitrary error text, and remain subject to queue loss and bounded
history retention. Unknown observations stay explicitly unavailable.

History and export schema version 1 are separate from report version 3 and state schema 42. Raw reports and databases are never packaged. Interrupted writes preserve the previous history; read errors and limits appear as partial evidence. See [the operator guide](../support-export.md) for limits and unavailable evidence.

### Explicit missing-reference inventory diagnostic

The opt-in `doctor --live --json --missing-reference-inventory` command branches
before normal startup, logging, service hooks and ordinary doctor filesystem
checks. `inventory_diagnostic.rs` reads bounded configuration and copied cached
auth only. It does not open a catalogue or construct a download/state owner.
`photos/diagnostic.rs` adapts the existing PhotosSession interface to privacy-safe
streamed requests with shared limits and no redirect/retry/auth fallback.
The existing PhotosService qualifies the private PrimarySync scope before the
preservation scanner's raw latest-record helper supplies fixed missing-reference
observations. The diagnostic performs no family hydration and returns no cursor
or CompleteLegacyInventory proof. At most one later lookup is observational.
Ordinary preservation retains its candidate, scope, date and completeness gates.
See `docs/missing-reference-inventory.md` for bounds and supervision requirements.

<a id="explicit-trusted-local-bytes-for-legacy-migration"></a>

### Retired legacy local-trust commands

`reconcile trust-legacy-files plan/apply` remains parseable and documented in CLI help only to refuse early with guidance to run `kei sync`. `commands/reconcile.rs::run_reconcile` refuses the nested action before resolving the database path; no authentication, state opening, plan creation or media work follows. It adds no declaration and leaves existing trust records, plans and media unchanged. No useful sync consumer of new local-trust declarations remains after coverage certification retires.

Existing `trusted_legacy_file:v1:` metadata records and their immutability guards remain historical evidence. Status counts stored declarations, including obsolete bindings, rather than currently verified files. They do not establish source provenance, ownership, adoption permission or checkpoint authority. Native source verification, sparse recovery, GPS and ordinary adoption retain their source and ownership requirements. See [Trusted Legacy Files](../trusted-legacy-files.md) for operator guidance.

## Provider lookup diagnostics

Targeted lookup rejections retain fixed `master_status` and `child_status`
response categories in bounded support history. These distinguish omitted,
not-requested, returned, tombstoned and provider-error members without storing
raw codes, messages or identifiers. `returned` describes response presence,
not owner, identity, resource or checkpoint proof. Lookup resolution and
checkpoint policy do not consume these diagnostic categories.
`child_reference_status` separately describes a missing, malformed, sparse,
matching or different reference, or an omitted, unavailable, invalid or
unrequested child. These fixed labels expose no reference values and do not
establish a usable pair.

Best-effort album identity hydration has task-scoped anonymous progress in
`icloud/photos/hydration_progress.rs`. Every 30 seconds and on return or drop,
it reports fixed phase labels, source ordinal/role (`owner` then `fallback`),
aggregate received-page and
record counts, request attempts/retries, and current-wait elapsed time. Counts
are observations, not completeness or checkpoint evidence. The scope does not
change request timeouts, retry policy, source priority, or identity acceptance;
provider identifiers, cursors, request bodies and raw errors never enter its
state. Other provider operations have no active diagnostic scope.
