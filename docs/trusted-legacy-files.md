# Trusted Legacy Files

Run `kei sync` instead. Legacy coverage certification is retired, so adding a local-trust declaration no longer helps sync complete or advance a checkpoint.

These command spellings remain available in help, but both refuse the operation before authentication, state opening or media work:

```sh
kei reconcile trust-legacy-files plan --library <zone> --output <new-file.json>
kei reconcile trust-legacy-files apply --plan <file.json>
```

Neither command creates a plan, accepts files or writes a new trust declaration. Existing declarations, plans, legacy records and media remain unchanged. The refusal performs no schema migration or file rewrite. Do not reset state or remove protected originals to get past it.

## Existing declarations

Older versions recorded explicit local-file trust separately from provider verification. Those declarations remain historical evidence. `kei status` reports their stored count; it can include obsolete bindings and is not a count of currently verified provider files. A declaration does not supply a missing provider source receipt, confer ownership or authorize adoption, overwrite or checkpoint advancement.

Normal sync retains ownership, protected-path, current identity, transfer, metadata and publication guards. Missing historical coverage certificates alone no longer force an inventory, fail a cycle or make the process unhealthy. Historical attribution can still make status report `Backup status: unsafe`; that describes unproven historical completeness, not process health. See [Backup maintenance](backup-maintenance.md#historical-legacy-evidence) for the counters and remaining recovery limits.
