# Roadmap

Current version: **v0.4.1**

## v0.5.0 — Config, env vars, validation

Make unattended/containerized operation first-class.

- [#6](https://github.com/rhoopr/icloudpd-rs/issues/6) Typed session struct (replace stringly-typed HashMap)
- [#53](https://github.com/rhoopr/icloudpd-rs/issues/53) Threads default bump (1→3) and bandwidth throttling
- [#117](https://github.com/rhoopr/icloudpd-rs/issues/117) `--print-config` flag
- [#118](https://github.com/rhoopr/icloudpd-rs/issues/118) Env var loading for all CLI params
- [#125](https://github.com/rhoopr/icloudpd-rs/issues/125) Input validation (auth fields, numeric bounds)
- [#126](https://github.com/rhoopr/icloudpd-rs/issues/126) Path and filesystem validation

## v0.6.0 — Albums & filtering

Organization and control over what gets downloaded and where it lands.

- [#5](https://github.com/rhoopr/icloudpd-rs/issues/5) Separate album enumeration concurrency
- [#80](https://github.com/rhoopr/icloudpd-rs/issues/80) Album-based folder structure (`%a` token)
- [#88](https://github.com/rhoopr/icloudpd-rs/issues/88) `--exclude-album`
- [#96](https://github.com/rhoopr/icloudpd-rs/issues/96) `--filename-exclude`
- [#97](https://github.com/rhoopr/icloudpd-rs/issues/97) Shared album support

## v0.7.0 — Auth & delete workflows

Close the loop: download, delete, and run unattended securely.

- [#22](https://github.com/rhoopr/icloudpd-rs/issues/22) Password providers with priority ordering
- [#28](https://github.com/rhoopr/icloudpd-rs/issues/28) Auto-delete (Recently Deleted album scan)
- [#29](https://github.com/rhoopr/icloudpd-rs/issues/29) Delete after download

## v0.8.0 — Metadata & sidecars

Preserve and expose iCloud's rich metadata.

- [#19](https://github.com/rhoopr/icloudpd-rs/issues/19) XMP sidecar export
- [#83](https://github.com/rhoopr/icloudpd-rs/issues/83) Metadata export (JSON/CSV)
- [#84](https://github.com/rhoopr/icloudpd-rs/issues/84) Favorite flag → EXIF rating
- [#93](https://github.com/rhoopr/icloudpd-rs/issues/93) Adjusted video and live photo MOV downloads

## v0.9.0 — Polish & UX

Quality-of-life before the 1.0 stability commitment.

- [#46](https://github.com/rhoopr/icloudpd-rs/issues/46) Watch interval progress bar
- [#52](https://github.com/rhoopr/icloudpd-rs/issues/52) HEIC → JPEG conversion
- [#82](https://github.com/rhoopr/icloudpd-rs/issues/82) Run report improvements
- [#85](https://github.com/rhoopr/icloudpd-rs/issues/85) Re-download on metadata change
- [#86](https://github.com/rhoopr/icloudpd-rs/issues/86) Staleness warning
- [#87](https://github.com/rhoopr/icloudpd-rs/issues/87) Version check on startup
- [#95](https://github.com/rhoopr/icloudpd-rs/issues/95) Granular live photo control
- [#103](https://github.com/rhoopr/icloudpd-rs/issues/103) Separate video/live photo directories
- [#120](https://github.com/rhoopr/icloudpd-rs/issues/120) AUR package

## v1.0.0 — Stability & completeness

Internal cleanup, test hardening, full Python parity.

- [#7](https://github.com/rhoopr/icloudpd-rs/issues/7) `tempfile::TempDir` in tests
- [#14](https://github.com/rhoopr/icloudpd-rs/issues/14) Multiple `--size` values
- [#21](https://github.com/rhoopr/icloudpd-rs/issues/21) SMS-based 2FA
- [#33](https://github.com/rhoopr/icloudpd-rs/issues/33) Multi-account support
- [#41](https://github.com/rhoopr/icloudpd-rs/issues/41) Global `--recent` across albums
- [#55](https://github.com/rhoopr/icloudpd-rs/issues/55) Prometheus metrics

## Deferred — needs design

- [#30](https://github.com/rhoopr/icloudpd-rs/issues/30) `--keep-icloud-recent-days` — destructive, overlaps #29
- [#89](https://github.com/rhoopr/icloudpd-rs/issues/89) Album-scoped sync deletions — dangerous, needs state DB design
- [#100](https://github.com/rhoopr/icloudpd-rs/issues/100) Tag/keyword filtering — depends on undocumented API fields
- [#101](https://github.com/rhoopr/icloudpd-rs/issues/101) Public sharing link downloads — entirely different API
