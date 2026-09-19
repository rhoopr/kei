# kei roadmap

Updated 19 September 2026. The product promise is a trustworthy, usable local
backup with clear evidence of incomplete work. See the
[product charter](product-charter.md).

GitHub milestones and issue acceptance criteria define release scope. Release
numbers below describe planned work, not publication dates. An issue without
a milestone is uncommitted and may never be implemented.

## Release sequence

| Release | User outcome | Scope |
| --- | --- | --- |
| [v0.25 internal consolidation](https://github.com/rhoopr/kei/milestone/32) | Make the existing engine easier to maintain without changing its supported promises. | The existing #780 plan, unchanged. |
| [v0.26 reliable recovery and bounded resources](https://github.com/rhoopr/kei/milestone/29) | Existing backup workflows recover and converge within practical resource limits. | Recovery, truthful results, supported naming, memory bounds, traversal and Docker correctness. |
| [v0.27 backup visibility and unattended operation](https://github.com/rhoopr/kei/milestone/25) | Inspect backup state, export captured facts and test alerting without manual guesswork. | Bounded operational JSON, streaming manifests, notification tests, one generic webhook and compatible headless setup. |
| [v0.28 maintained metadata and media usability](https://github.com/rhoopr/kei/milestone/26) | Enabled sidecar outputs stay current, with a curated fidelity and naming scope. | Sidecar catch-up, selected still-image properties, remaining precision assessment/corrections and edited-version naming. |

## v0.25: internal consolidation stays unchanged

[Issue #780](https://github.com/rhoopr/kei/issues/780) remains the complete plan
and source of truth. This roadmap does not add, remove, reprioritize or relax
any of its children, dependency edges, acceptance criteria or release gates.

Retain the existing correctness prerequisites, responsibility-based splits,
shared decision ownership, contributor workflow, test consolidation and fixture
work. Preserve all supported behavior, public interfaces, configuration,
schemas, serialization, media/metadata safeguards and checkpoint guarantees.
Keep the existing requirement for approved retained-versus-removed test
evidence. Changes integrated on the development branch are not automatically
released.

The post-v0.25 plan starts after that existing plan completes. None of the
future releases below adds a prerequisite to #780.

## v0.26: reliable recovery and bounded resources

### Outcome

Supported sync and configuration changes reach an explainable steady state.
A large library or opted-in HEIF output does not require unbounded working
memory or repeated whole-catalogue work to recover.

### Scope and order

1. Close remaining recovery and evidence gaps:
   [#765](https://github.com/rhoopr/kei/issues/765), [#770](https://github.com/rhoopr/kei/issues/770), [#818](https://github.com/rhoopr/kei/issues/818), [#819](https://github.com/rhoopr/kei/issues/819).
   Resolve #765's remaining provider-identity/hydration and status evidence;
   #826 already merged the authentication-recovery fix. A clean run without a
   421 does not prove live session recovery. Fix counter aggregation and
   optimized-build fixture failures independently.
2. Bound memory and repeated work:
   [#761](https://github.com/rhoopr/kei/issues/761), [#746](https://github.com/rhoopr/kei/issues/746), [#747](https://github.com/rhoopr/kei/issues/747), [#745](https://github.com/rhoopr/kei/issues/745), [#762](https://github.com/rhoopr/kei/issues/762).
   Establish the HEIF memory budget and independent guard first, then satisfy
   #761's streaming requirements. Scope/reuse catalogue loads, use stable
   keyset walks, batch resumable reconciliation and persist asset-level facts
   once. Retain the existing issue acceptance criteria.
3. Correct supported file and container behavior:
   [#773](https://github.com/rhoopr/kei/issues/773), [#582](https://github.com/rhoopr/kei/issues/582).
   Derive new Live Photo companions from the owned still path. Audit current
   Docker behavior before applying historical checklist items.

Resolve owner overlap before implementation. #770 does not depend on completing
the entire #745 redesign. A partial memory guard is not completion of #761.

### Release evidence

- Production-path tests prove interruption, durable retries, checkpoint holds
  and an unchanged follow-up cycle for the affected transitions.
- Status, reports and health distinguish recorded local completion, unresolved
  provider evidence and pending metadata work without contradictory claims.
- Named large-library/media workloads record memory, query counts and provider
  amplification under documented supported settings. Limits are measured,
  not universal claims.
- Smart-folder path changes converge, transferred bytes/media totals remain
  accurate, and existing Live Photo stills remain untouched.
- Optimized offline validation completes without hiding fixture failures.
- Existing bytes, ownership, opt-in rules and conservative identity/checkpoint
  gates remain intact.

No new provider, destination, deletion command or metadata field expansion
belongs in this release.

## v0.27: backup visibility and unattended operation

### Outcome

Operators can inspect existing backup facts and test the normal notification
path. Integrations can read provider metadata through a supported local export
without rewriting media or opening the private state database.

### Scope and order

1. Establish JSON status and a bounded operational output contract:
   [#680](https://github.com/rhoopr/kei/issues/680), [#699](https://github.com/rhoopr/kei/issues/699).
   #680 owns status. #699 covers list albums/libraries, verify, reconcile and
   compatible manifest JSON selection. Human defaults and existing formats
   remain. A complete retrofit of every CLI command is not required.
2. Make exports practical before expanding their payload:
   [#748](https://github.com/rhoopr/kei/issues/748), [#708](https://github.com/rhoopr/kei/issues/708).
   Stream ordered rows with valid output/error semantics and expose already
   captured catalogue metadata with documented null meanings.
3. Make alerting testable:
   [#414](https://github.com/rhoopr/kei/issues/414), [#482](https://github.com/rhoopr/kei/issues/482), [#592](https://github.com/rhoopr/kei/issues/592).
   Keep script compatibility; add one bounded generic webhook and a small
   Prometheus/Grafana documentation example. No desktop backend, native MQTT
   or service-specific notification adapters.
4. Align unattended setup and compatibility:
   [#679](https://github.com/rhoopr/kei/issues/679), [#681](https://github.com/rhoopr/kei/issues/681), [#678](https://github.com/rhoopr/kei/issues/678), [#696](https://github.com/rhoopr/kei/issues/696).
   Require explicit noninteractive reset consent, keep CLI/service root
   discovery consistent, document and deprecate direct password inputs with a
   compatibility window, and validate the existing OCI image under Apple's
   container runtime. Do not implement a supervisor or second image.

### Release evidence

- Successful structured output is parseable and versioned; errors and
  truncation are truthful. Failed partial output is not valid success.
- Export memory stays bounded on a named large catalogue and write failures
  stop work promptly.
- Notification tests exercise the configured production dispatch path.
  Delivery failure remains visible without becoming a media failure.
- Legacy config roots and credential flows remain usable through documented
  transitions. Password removal is not bundled into the initial deprecation.
- Runtime guidance and smoke evidence match actual supported container
  behavior, including ownership, shutdown and authentication limitations.

Agent context, named configuration profiles, a general query platform and
browser-based remote control are not release prerequisites.

## v0.28: maintained metadata and media usability

### Outcome

Users who enable sidecars receive the supported improvements through ordinary
sync. Existing and new downloads converge under one documented ownership
contract, while originals and unrelated metadata remain protected.

### Scope and order

1. [#799](https://github.com/rhoopr/kei/issues/799) owns output revisions, durable per-rendition/path completion,
   bounded catch-up and the sidecar ownership contract. Decide managed-field
   authority, external edits, legacy markers and unowned values before
   implementation. The roadmap does not preselect a broader overwrite policy.
2. [#797](https://github.com/rhoopr/kei/issues/797) adds only the confirmed curated still-image properties:
   camera make/model, lens, focal length, aperture, exposure time and ISO;
   creation software only where its meaning is established. Reuse #799.
   Geometry, colour, track/audio expansion and exhaustive copying are excluded.
3. [#803](https://github.com/rhoopr/kei/issues/803) starts from the shipped #805 precision correction. Audit and
   correct demonstrated residual modification/deletion precision, hashing or
   ordinary capture-drift behavior; explicitly disposition each residual path.
   Native embedded subsecond writing is not committed. Reuse existing repair
   and #799 where needed rather than introducing another backfill subsystem.
4. [#501](https://github.com/rhoopr/kei/issues/501) adds the bounded edited-version naming policy. Preserve
   originals, stable ownership and collision behavior. No silent historical
   renaming, deletion or cleanup.

#501 is independent of sidecar catch-up. #797 cannot claim automatic convergence
before #799 is implemented and validated.

### Release evidence

- Existing sidecars update without a provider delta or unnecessary media
  download, and resume after interruption.
- Only successful publication and durable finalization complete the planned
  output revision for the same inputs and location.
- Disabled outputs, source media and unrelated/unowned metadata remain
  protected under the chosen documented policy.
- Unsupported/absent fields converge; retryable I/O failures remain visible.
- An unchanged cycle performs no repeated output-maintenance scan or rewrite
  once convergence completes.
- Independent readers validate added fields and timestamp meanings.
- Naming transitions preserve existing data and clearly document historical
  copies that require separate user review.

Movie enrichment, exhaustive metadata conversion, shared-album implementation
and cross-library deduplication remain outside this release.

## Unmilestoned: evidence or design required

Deferred work has no milestone. It is not v0.29 and not a promise to implement
every idea. An open issue preserves evidence and discussion; it does not authorize
implementation.

| Issues | Disposition and promotion threshold |
| --- | --- |
| [#422](https://github.com/rhoopr/kei/issues/422) incremental streaming | Measure remaining buffered-delta pressure after consolidation and the v0.26 changes. Reuse the bounded pipeline; preserve complete enumeration and checkpoint proof. |
| [#804](https://github.com/rhoopr/kei/issues/804) provider revisions | Research measured savings and prove that unchanged source tags cannot hide local pending work or decoder/output changes. |
| [#801](https://github.com/rhoopr/kei/issues/801) movie metadata | Require a named consumer need, explicit format/property mappings and #799 convergence. No general native-tag framework. |
| [#701](https://github.com/rhoopr/kei/issues/701) bounded listings/profiles | Separate bounded-read needs from named configuration discovery. Promote only a demonstrated continuation/filtering need; named profiles are not currently planned. |
| [#589](https://github.com/rhoopr/kei/issues/589) catalogue query | First show a frequent question that status and streaming manifest cannot answer adequately. No general query language or photo UI. |
| [#481](https://github.com/rhoopr/kei/issues/481) browser-assisted 2FA | Validate the remote/container operator journey and access model after recovery is correct. Keep any future surface narrow; no remote dashboard. |
| [#593](https://github.com/rhoopr/kei/issues/593) shared-album research | Permit bounded discovery of fidelity, identity and incremental semantics. Research may conclude that implementation should remain deferred. |
| [#559](https://github.com/rhoopr/kei/issues/559) cross-library deduplication | Quantify waste and prove ownership, lifecycle and companion semantics before changing storage identity. |
| [#700](https://github.com/rhoopr/kei/issues/700) agent context | No implementation commitment. Reassess a concrete discovery failure after the operational CLI is consistent. |
| [#634](https://github.com/rhoopr/kei/issues/634) native MQTT | No native backend planned. Existing scripts/adapters remain the route; repeated unmet demand is required to reopen the product decision. |
| [#591](https://github.com/rhoopr/kei/issues/591) prune planning | No command planned until deletion, selection and uncertainty evidence is trustworthy. A candidate list must not imply unsupported deletion safety. |
| [#442](https://github.com/rhoopr/kei/issues/442) Cargo workspace | No split planned without a named enforceable boundary that current modules cannot provide. |
| [#586](https://github.com/rhoopr/kei/issues/586) service lifecycle boundary | Decide long-term ownership before expanding or removing native lifecycle commands. Existing users need a migration assessment. |
| [#684](https://github.com/rhoopr/kei/issues/684) STRICT SQLite | Deferred hardening. Reassess demonstrated integrity benefit against broad durable-state migration cost. |
| [#549](https://github.com/rhoopr/kei/issues/549) full-enumeration planning | Re-audit after #780. Close as superseded if appropriate, or name the concrete residual duplicated responsibility. |
| [#443](https://github.com/rhoopr/kei/issues/443), [#630](https://github.com/rhoopr/kei/issues/630), [#436](https://github.com/rhoopr/kei/issues/436), [#438](https://github.com/rhoopr/kei/issues/438) lint/allocation/filesystem hygiene | Opportunistic or evidence-driven only. No blanket lint, allocation or sync-to-async campaign. |

New providers, downstream upload destinations, object storage, photo management,
destructive sync and exhaustive metadata translation also remain uncommitted.
Promotion requires a named user outcome, evidence, bounded scope, ownership and
compatibility rules, and a complete validation gate. A downstream outage must
not invalidate an independently useful local archive.

## Release discipline

- Preserve v0.25/#780 exactly as planned. The new sequence governs work after
  that release.
- Work through v0.26, v0.27 and v0.28 in order; prepare later policy decisions
  without adding implementation prerequisites to the active release.
- After v0.25, independently validated existing-behavior fixes may ship in
  maintenance releases before the next themed release. Record the actual
  shipped tag/commit; do not mark an unresolved issue complete for a partial
  mitigation.
- Freeze the active release around the issue outcomes above. Any added scope
  needs an explicit recorded tradeoff, rather than attaching adjacent requests.
- Complete assigned acceptance criteria or record a deliberate scope change
  and move remaining work before declaring a release complete.
- Run the applicable repository gate and affected release/live/platform
  validation. Attribute results to the exact candidate; record unavailable
  checks and limitations. No skipped check is a pass.
- Publish dates only when candidate evidence supports them. This roadmap
  supplies ordering and exit criteria, not calendar estimates.

## Shipped baseline

- v0.22 delivered normal sync safety, durable retry and checkpoint guards,
  status, reconcile, manifest export and initial diagnostics.
- v0.23 improved provider metadata capture and configured-output recovery.
- [v0.24.0](https://github.com/rhoopr/kei/releases/tag/v0.24.0), published
  17 September 2026, delivered backup-correctness and metadata-preservation
  fixes. Its limitations remain documented in the
  [upgrade guide](v0.24-upgrade.md#known-limitations) and
  [validation record](v0.24-validation.md).

Merged development work, closed issue state and a planned milestone are not
interchangeable with a published release.
