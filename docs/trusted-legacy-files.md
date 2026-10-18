# Trusted Legacy Files

Older libraries can have intact local files whose provider source receipts were
never recorded. Explicit Trusted Legacy File acceptance lets preserved legacy
families use those local bytes for migration without downloading them again.
It records local trust separately; provider verification remains unknown and
`asset_metadata_paths.source_checksum` stays NULL.

`kei reconcile trust-legacy-files plan --library <zone> --output <new-file.json>` creates a
reviewable offline plan for one library. `kei reconcile trust-legacy-files apply --plan
<file.json>` accepts exactly the reviewed entries. Entries may be removed from a
plan; changing their bindings requires a new plan. The command uses the configured
account lock and owned catalogue. It creates no provider client, reads no media
from iCloud, and changes no media, aliases, legacy rows, or ownership mappings.
Plan files contain private catalogue identifiers and paths; keep them local.

The first implementation is deliberately limited to ready current-child
renditions in families already protected by legacy preservation. It accepts at
most 128 mapped renditions in one library and a 4 MiB plan. It requires an exact
current path/provider/local checksum binding, an existing SHA-256 baseline, no
source receipt, and no pending metadata work. It hashes each confined local file
without following symlinks. Pending temporary writes and replacement recovery
refuse acceptance. Hardlinks retain the existing explicit
`[download].legacy_preservation_allow_hardlinks` policy.

Apply rechecks account, library, child, master, rendition, provider generation,
metadata, path and immutable family evidence. It retains confined file handles
and rehashes before the atomic database commit. Stale, cancelled, duplicate or
partially invalid plans commit no acceptances. Reapplying an unchanged plan is
idempotent. Acceptance persists across restart; changed bindings require a new
explicit plan.

Acceptance alone cannot advance a provider checkpoint. The next ordinary sync
still needs complete fresh current-child inventory, matching provider generation
and metadata, file revalidation, readiness, and the existing checkpoint bridge.
Unaccepted files remain held. Other source-verification consumers continue to
require verified source receipts. `kei status` reports stored acceptances
separately; the count includes historical acceptances and is not a current
provider-verification count.

The provenance name is reusable for future migrations where older libraries face
new safety checks. Each additional consumer needs its own bounded acceptance
rules; this command does not enable general adoption or infer historical file
ownership from matching bytes.
