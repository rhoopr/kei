# Removed compatibility surfaces

There are no active warning windows. Removed flags, commands and TOML keys fail during CLI parsing or config validation. Removed durable-setting env vars are ignored; they do not supply sync settings.

## v0.20.0

The v0.20 cleanup removed the v0.13-v0.19 compatibility aliases. Use [example.config.toml](example.config.toml) for current settings and [account-bound state migration](docs/account-state-migration.md) before adopting legacy state. Do not copy or rename old authentication files into the current session directory. The historical v0.20 migration guide is not current adoption guidance.

Removed examples include:

- durable sync flags such as `--download-dir`, `--album`, `--smart-folder`,
  `--library`, `--skip-videos`, `--threads`, and `--watch-with-interval`
- durable sync env mirrors such as `KEI_DOWNLOAD_DIR`, `KEI_ALBUM`,
  `KEI_LIBRARY`, `KEI_THREADS`, and `KEI_WATCH_WITH_INTERVAL`
- old TOML aliases such as `[filters].album`, `[filters].exclude_albums`,
  `[filters].library`, `[download].threads_num`, and `[metrics].port`
- hidden command aliases such as `kei setup`, `kei retry-failed`, and
  `kei reset-state`
