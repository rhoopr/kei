# Startup and local state

Follow the process from its entry point to the account-owned database. For operator instructions, see [account state migration](../account-state-migration.md).

## Container startup

`docker/entrypoint.sh` owns command collision routing and PUID/PGID privilege
drop. The explicit command list mirrors visible clap commands before checking
external executables. Without PUID/PGID, it executes as root without ownership
repair. With both numeric IDs, it recursively repairs UID and GID on /config
and /photos, including symlinks themselves without following their targets.
Traversal or repair failures warn before privilege drop. Every restart scans
all inodes; KEI_CHOWN_PHOTOS=0 skips /photos repair when host permissions are
already managed, while /config remains repaired.

For `support-export`, the wrapper queries the owning Rust CLI parser before
volume initialization. The private query ignores captured CLI environment
bindings and returns an exit status without runtime, config-file reads or
credential resolution. Export launches and their help routes skip both
volume traversal and ownership repair, including incomplete export arguments,
then drop to the requested numeric UID:GID. Option values cannot select this
exception. Status 0 selects export, and only status 1 permits ordinary volume
initialization. An unexpected query exit, including a crash or signal, aborts
before traversal. Normal launches retain their existing ownership initialization.

The Docker HTTP healthcheck defaults to port 9090. A deployment using a custom
`[server].port` must set `KEI_HEALTHCHECK_PORT` to that same port, including
when overriding the command with a custom config path. Docker does not parse
TOML separately. Disabled HTTP servers require a deployment healthcheck override.
See `docker-compose.yml` for runtime examples.

## Command dispatch

```text
src/main.rs
  -> kei::main_inner
  -> src/lib.rs::run
  -> src/cli.rs
  -> command owner, service owner, or src/sync_loop.rs
```

`run` composes private phases in `src/lib.rs`: `load_startup_config`,
`resolve_startup_output`, `initialize_logging`, and `dispatch_command`.
Config loading retains the Docker fallback and passes parse errors to
`doctor` without blocking its diagnostic report. Output resolution keeps the
friendly-mode request separate from the mode allowed by the environment.
The log-writer guard stays in `run` until dispatch returns. Startup injects
the captured environment password before dispatch. Command dispatch retains
the setup wizard's one-shot sync path and the service owner's entry point.

`config/paths.rs` discovers one application root for default config, cookies, credentials and state. An absolute `XDG_CONFIG_HOME` selects its `kei` subdirectory on a fresh install; a lone existing `~/.config/kei` wins for compatibility. Discovery fails if both roots exist and resolve to different directories. An explicit config path bypasses config discovery only. An explicit data directory bypasses state discovery. Purge validates the discovered root before service removal and refuses custom locations.

Linux keeps the user unit under the systemd configuration directory, even when kei selects the legacy application root. The unit records the absolute config path and install-time `XDG_CONFIG_HOME` and `KEI_DATA_DIR` overrides. It clears absent overrides so the service manager's environment cannot redirect state.

The CLI requires a subcommand. `kei sync` enters the sync path. Commands such
as `status`, `doctor`, and `manifest` read local state without entering the
normal iCloud sync loop.

`main_inner` records whether stdin is interactive before it starts the async
runtime. Command owners use that single input mode for password and 2FA
prompts. A foreground command returns `TwoFactorRequired` to the exit
classifier, which reports auth exit code 3 and the `login get-code` and
`login submit-code` recovery flow. Only the sync owner may convert that error
into a durable wait, and only when the resolved configuration is in watch or
service mode.

Interactive login and `login get-code` may retry once from clean local auth
state when Apple rejects verification-code delivery with HTTP 403 and the
attempt loaded a persisted cookie jar or session. The failed session removes
only its cookie jar, session, and validation cache while it still holds the
per-account lock, then the shared auth owner creates one clean replacement
session. Password material and SQLite state remain unchanged. `reset session`
exposes the same locked cleanup explicitly. Cleanup advances a generation in
the existing lock file. A watch process that released its lock for idle sleep
detects a changed generation when it wakes and stops before its old in-memory
session can recreate reset credentials. Reauthentication handoffs carry their
pre-release generation into replacement-session creation, so a reset in that
gap also wins. The session guard explicitly unlocks on drop, including failed
generation validation and rejected principal replacements. Closing only its own
descriptor would leave a forked child's copy holding the shared lock until
exec or exit. An explicitly released guard does not unlock again on drop;
generation checks and independent-process exclusion remain in force. Blocking
generation writes retain the shared guard until completion even if their caller
is canceled, so cleanup cannot unlock while a generation write is pending.

## Account ownership and legacy adoption

Writable download dispatch requires a state store before provisional, full or incremental routing can start. `sync_loop/runner.rs` supplies authenticated owned state and fails startup if opening it fails. `download/orchestration/dispatch.rs` also rejects missing state at its entry boundary, before provider calls or local file work. Dry-run and filename previews may enumerate without a store and do not create one.

`state/db/account.rs::open_sync_state` is the shared authenticated startup boundary for sync and import. It checks the configured namespace and authenticated provider identity before creating directories, resolves the state path, and delegates schema upgrades and matched config backups to `SqliteStateDb::open_owned_with_config`. Unowned legacy state still requires explicit ownership evidence; this boundary does not infer ownership from an account-like filename.

Configured login and realm determine collision-resistant names for state,
auth artifacts and encrypted credentials. Account-owner format version 1 is
independent of the application schema version and must be checked before its
migrations. Production connections require an owner expectation; unbound
connection helpers are restricted to tests. All state commands, including
read-only export and reset, enforce this boundary. State reset checks ownership
before confirmation and requires `--yes` when the process input mode cannot
prompt. An absent database remains a successful no-op. Sync and import require a
usable authenticated provider identity and pin it independently of filenames.
Watch validation, replacement sessions and initialization retries retain that
principal pin. Changed or missing authenticated identity stops before further
provider or state work; it cannot become a recoverable quiet-watch warning.

Legacy discovery fails closed rather than creating fresh state over retained
debt. Explicit `migrate-state` requires verified operator ownership
confirmation and a fresh login under the account lock. SQLite backup retains
rows, unknown tables and committed WAL content, then binding, schema migration
and integrity validation occur in a private stage. Same-directory publication
refuses replacement and fsyncs the file and parent. The legacy source and
companions remain. Conflicting scoped provenance is rejected, not rewritten.
Legacy session and encrypted credential files are preserved and ignored;
keyring identity remains unchanged. See [account migration](../account-state-migration.md)
for operator steps and compatibility limits.

## Owned-schema upgrade backups

`kei status` counts sparse references preserved in the v0.30 upgrade snapshot separately from current unresolved references. The archived count is informational: it does not schedule retries, hold a checkpoint or certify recovery of an original.

Schema 38 establishes the v0.30 fresh-start boundary in the existing database. Before older migration steps interpret provider payloads, `state/schema.rs` copies historical provider inbox, catalog, selection, work, active-generation and sparse-identity rows into `historical_v030_*` tables and clears their active counterparts. It preserves zone and scoped database cursors, configuration checkpoints and historical identity holds in those snapshots, then removes their active authority. Preservation, retirement and schema advancement share one transaction. A failed upgrade restores the prior active state.

The next selected-library sync uses the existing full-enumeration path to establish current work and a new checkpoint. Current provider failures, unresolved identities and unfinished current work retain their retry and checkpoint guards. Subsequent opens do not repeat retirement. Historical snapshots have no runtime writers or replay consumers. Current inbox capture remains active; removing that subsystem is a later simplification boundary.

Asset records, local path and ownership evidence, metadata work, temporary-file ownership and publication journals remain with their existing owners. The transition does not write media or sidecars. Historical provider destinations retain their original path and progress bytes in the snapshot without keeping obsolete plans executable.

Schema 44 records one historical deletion check per eligible downloaded iCloud identity present at upgrade. Fresh databases start empty. A current observation retires the check, including an in-flight lookup. After current work succeeds, or on a quiet watch cycle, sync spends at most one five-second exact lookup on up to 32 candidates in a selected, indexed library. An omitted record, provider error, duplicate record or mismatched zone or owner leaves the check retryable after one hour. A confirmed live record retires the check. An explicit deletion or expungement of the exact asset or master marks its downloaded renditions deleted, retaining any provider deletion time. Protected historical records and prepared capture repairs retain their guards. The check writes no file and neither schedules downloads nor gates checkpoints or backup health.

`kei manifest` includes deleted renditions. JSON and CSV expose `is_deleted` independently of download `status`. `deleted_at` is a UTC timestamp when known, otherwise JSON null or an empty CSV field.

`config/source.rs` retains the exact configuration bytes loaded at startup, or the absence of a configuration file. Debug output redacts both bytes and paths. Command dispatch passes this context to `SqliteStateDb::open_owned_with_config`; production has no context-free writable opener. CLI overrides, environment values and credentials are not included in the snapshot.

For an existing older schema, the opener validates account ownership and holds a SQLite immediate transaction through backup publication and migration commit. `state/db/migration_backup.rs` copies the committed database through a separate read connection, including WAL content, into a private `.kei-schema-backup-<uuid>` directory beside the database. It writes the loaded configuration or records its absence, restoration instructions, and a checksum manifest. Exclusive creation prevents replacing earlier evidence. On Unix, files and directories are synced before the completion manifest is published without replacement. On Windows, creation-time protected owner-only ACLs secure the bundle and its contents; a native write-through, no-overwrite directory move publishes the completed `.partial` stage. A directory ending in `.partial`, or a bundle without `manifest.json`, is incomplete. Failed or interrupted bundles are retained.

Backup failure refuses migration. Migration failure rolls back the upgrade transaction and retains the completed backup. Fresh and already-current databases do not create upgrade backups. Missing configuration context refuses an old-schema upgrade. Platforms outside Unix and Windows refuse old-schema upgrades. Windows storage must support persistent ACLs. The Windows publication contract uses the repository's native write-through operation; it does not claim POSIX directory-fsync equivalence or physical power-loss survival. Native Windows runtime qualification remains pending.

Windows publication retains the populated stage's original identity and parent confinement through the directory move. Before releasing its no-delete handle, it opens and validates a rename-compatible handle. The published directory must match the original identity; substituting another directory cannot authorize migration.

Restore instructions are in each bundle's `restore.txt`. Stop every state writer, verify the manifest checksums and matching database/account, and preserve the current database together with its `-journal`, `-wal` and `-shm` companions before installing the standalone snapshot. Never attach newer companions to the restored database. Restore the recorded configuration with a compatible previous binary and reapply invocation/environment settings separately. The bundle contains no media; rollback is supported before subsequent media or metadata changes, or with a separately matched media backup.

An interrupted DELETE-mode migration can leave a hot rollback journal. If SQLite requires writable recovery before the account-owner header can be read, the opener refuses with restoration guidance. It does not recover a potentially foreign database before validating ownership. Preserve the journal; deleting it or forcing writable recovery bypasses this boundary. Restore a verified, matching backup instead.
