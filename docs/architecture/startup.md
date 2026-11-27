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

## Account ownership and state files

Each account's sync state is one SQLite file, `<account>.sync.db`, opened through `src/sync_core/access.rs`. The file records the configured account and, after the first signed-in run, the authenticated provider identity. Offline commands check the configured account; `sync` and `import-existing` also check, or first record, the provider identity. A mismatch refuses before anything is read or changed. State reset checks ownership before confirmation and requires `--yes` when the process cannot prompt. An absent state file is a successful no-op.

Configured login and realm determine collision-resistant names for state, auth artifacts and encrypted credentials. Watch validation, replacement sessions and initialization retries keep the authenticated principal pinned. A changed or missing identity stops before further provider or state work.

A previous release's `<account>.db` is read only to upgrade. `state/db/account.rs` opens it read-only after checking its account owner; the sync core carries verified copies into a scratch file, checks its integrity and renames it into place. The previous file is never written. After the first cycle in which every zone listed to the end without error, it is removed with its `-wal`, `-shm` and `-journal` companions and the `.kei-schema-backup-*` bundles whose manifest names it. See [the sync core](../sync-core.md#upgrade).

Legacy discovery fails closed rather than creating fresh state over an unowned file. Explicit `migrate-state` requires verified operator ownership confirmation and a fresh login under the account lock. SQLite backup retains rows, unknown tables and committed WAL content; binding and integrity validation happen in a private stage. Same-directory publication refuses replacement and fsyncs the file and parent. The legacy source and companions remain. Legacy session and encrypted credential files are preserved and ignored; keyring identity remains unchanged. See [account migration](../account-state-migration.md) for operator steps and compatibility limits.
