# Account-bound state migration

New state, session, cache, lock and encrypted-credential filenames use a
versioned hash of the exact configured username and iCloud realm (`com` or
`cn`). Punctuation, case and Unicode spelling are significant. kei does not
infer that two login names are aliases. Existing OS keyring entries remain
keyed by the exact username.

Every state command checks the database's independent account-owner header.
Sync and import also compare its authenticated provider identity. A missing
or mismatched owner stops state access before schema migration. A legacy
filename, matching asset name, zone or checkpoint does not prove ownership.

## Adopt a legacy database

1. Stop all kei processes that use the legacy database, including offline
   state commands. Keep a consistent backup of its database and SQLite
   companions, configuration, and media.
2. Verify independently that the complete legacy database belongs to the
   configured account. Mixed or unknown ownership is not safe to confirm.
3. Supply the exact source path and your ownership confirmation:

   `kei migrate-state --legacy-db /path/to/legacy.db --confirm-ownership`

   Configure the username, realm and data directory as for normal commands.
   This command performs a fresh password login and may require 2FA, even if
   a cached session exists. For noninteractive use, supply a supported
   password source and `--code` when required. A failed login publishes no
   state database; retry adoption with a fresh login.
4. Inspect `kei status` and `kei manifest` before resuming sync.

Adoption uses SQLite backup, including committed WAL content, and preserves
all rows, unknown tables, mappings, checkpoints and retry debt. It binds and
migrates a staged copy, checks SQLite integrity and publishes it without
replacing an existing destination. Conflicting stored account or realm
provenance stops adoption. A bound database is not a legacy source. The
source and its companions remain; kei does not merge databases or remove
legacy files. If publication reports an uncertain filesystem error, inspect
the destination before retrying. An existing destination is never replaced.

Legacy auth files are preserved and ignored. Fresh adoption clears only the
new namespace's session, cookie jar and validation cache while holding its
account lock. Legacy encrypted credentials are not read, copied or
configured automatically. Users of that backend must re-save their password
with `kei password set`; exact-username keyring entries and configured
password sources remain usable. Do not rename old auth files into the new
namespace.

Every existing user adopting unowned legacy state needs this confirmation
and fresh login, including users without a visible filename collision.
After binding, ordinary session reuse resumes. Do not resume an older binary
against the preserved legacy copy; it is a historical snapshot after adoption.
A fresh account with no legacy database follows the normal login flow. Missing authenticated
provider identity stops binding rather than manufacturing ownership proof.

This migration does not alter checkpoint policy, authorize media deletion,
resolve unknown source identities, or promise an atomic provider snapshot.
Normal sync retains SQLite WAL/NORMAL durability. Migration fsync and
staging do not add a general power-loss or filesystem support guarantee.
