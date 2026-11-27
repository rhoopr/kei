# Trusted Legacy Files

Run `kei sync` instead. Legacy coverage certification belonged to the engine that v0.30 replaced, so a local-trust declaration no longer has any effect.

These command spellings remain available in help, but both refuse the operation before authentication, state opening or media work:

```sh
kei reconcile trust-legacy-files plan --library <zone> --output <new-file.json>
kei reconcile trust-legacy-files apply --plan <file.json>
```

Neither command creates a plan, accepts files or writes a trust declaration, and neither changes state or media.

## Existing declarations

Declarations recorded by earlier versions are not carried over by the [v0.30 upgrade](upgrade.md). The upgrade carries over the local copies the previous state recorded as verified, and the first sync takes them over without downloading them again. The old state file, and the declarations in it, are removed after the first cycle in which every library finished its listing. See [retired maintenance](backup-maintenance.md#retired-maintenance).
