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
local files; `src/sync_core/support.rs` reads the state file through an
immutable SQLite connection without upgrading it, and reports counts only. A
previous release's file is reported as waiting for upgrade. A nonempty WAL or
journal, another account's file, or a change during the read degrades
explicitly. Export output uses exclusive creation.

Normal sync, import and service operations publish typed evidence from their
existing owners through a bounded optional worker. The worker groups only
allowlisted fields, persists account-scoped history with private atomic writes
and a single-writer lock, and never feeds decisions back to provider, sync position,
state or publication policy. Its tracing layer is independent of log verbosity.
Stored history is revalidated against the same contract before export. Unknown
fields, labels, arbitrary errors and provider identifiers cannot broaden it.
The Photos owner projects only fixed owner morphology and JSON types for rejected
inventory scopes. Private discovery records identify primary versus other zones
and validated primary eligibility. A primary-publication observation follows the
actual qualified-primary assignment; selected indexing alone does not imply it.
Owner categories are revalidated as strings, so a malformed numeric owner cannot
escape through an aggregate-count allowance. Missing older fields remain missing.

Histories from earlier releases can hold `support_token_equivalence_v1` and
missing-reference inventory records. `support/token_equivalence.rs` revalidates
those rows at export and rejects malformed or duplicate ones; nothing writes
them now.

<a id="explicit-trusted-local-bytes-for-legacy-migration"></a>

### Retired legacy local-trust commands

`reconcile trust-legacy-files plan/apply` remains parseable and documented in CLI help only to refuse early with guidance to run `kei sync`. `commands/reconcile.rs::run_reconcile` refuses the nested action before resolving the database path; no authentication, state opening, plan creation or media work follows. It adds no declaration and leaves existing trust records, plans and media unchanged. No useful sync consumer of new local-trust declarations remains after coverage certification retires.

Earlier `trusted_legacy_file:v1:` records stay in the previous release's state file, which is read only for the upgrade. They are not carried into the sync core and grant nothing.

