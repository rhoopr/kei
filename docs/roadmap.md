# icloudpd-rs v1.0 Roadmap

## Current State (v0.2.1)

**Solid foundation.** Core sync, auth, state tracking, parallel downloads, resumable transfers, watch mode, systemd integration, graceful shutdown, shared library support — all shipped.

**What's NOT done** (from CHANGELOG "Not Implemented" + planning):

| Category | Issue | Feature |
|----------|-------|---------|
| ~~Config~~ | ~~#51~~ | ~~TOML config file~~ |
| Distribution | #40 | Docker images |
| Auth | #21 | SMS-based 2FA |
| Auth | #36 | Headless MFA (`--submit-code`) for Docker |
| Auth | #22 | OS keyring integration |
| Auth | #38 | Legacy 2SA |
| Auth | #37 | Python LWPCookieJar import |
| Content | #19 | XMP sidecar export |
| Content | #14 | Multiple size downloads |
| Content | #17 | `--only-print-filenames` |
| Content | #52 | HEIC to JPEG conversion |
| Lifecycle | #28 | Auto-delete (Recently Deleted scan) |
| Lifecycle | #29 | Delete after download |
| Lifecycle | #30 | Keep iCloud recent days |
| Notifications | #31 | Email/SMTP on 2FA expiry |
| Notifications | #32 | Notification scripts |
| Notifications | #55 | Prometheus metrics |
| Config | #33 | Multi-account support |
| Config | #34 | OS locale date formatting |
| State | #69 | Schema migration improvements |

---

## Proposed Phases

### Phase 0: Config Foundation (pre-v0.3) ✅ COMPLETE

*Shipped in `feat/config-toml` branch*

1. ~~**TOML config file support (#51)**~~
   - Loads from `~/.config/icloudpd-rs/config.toml` (or `--config`)
   - Layered resolution: CLI > TOML > hardcoded default
   - Grouped structure (`[auth]`, `[download]`, `[filters]`, `[photos]`, `[watch]`)
   - CLI flags remain fully functional — config file is additive, not required

2. ~~**Kill the `LegacyCli` shim**~~
   - `Config::build()` now merges `Cli` + optional `TomlConfig` directly via `resolve()` helpers

### Phase 1: Docker & Unattended Operation (v0.3)

*The single highest-leverage milestone for user acquisition*

3. **Docker image (#40)**
   - Multi-stage build, `scratch` or `distroless` base (tiny image, massive selling point vs Python's)
   - Single `/config` volume (cookies, state DB, config.toml) + `/photos` volume
   - Support `ICLOUD_USERNAME`, `ICLOUD_PASSWORD`, `TZ` env vars
   - `docker-compose.yml` example in README
   - GitHub Actions to build + push to `ghcr.io/rhoopr/icloudpd-rs`

4. **Headless MFA (#36)**
   - `--submit-code <code>` to complete 2FA non-interactively
   - Enables `docker exec icloudpd-rs icloudpd-rs --submit-code 123456`
   - Critical for Docker — without this, 2FA re-auth in containers is painful

5. **Session expiry notification (#31/#32)**
   - Start with `--notification-script <path>` — a generic hook that gets called with event type + message
   - Covers 2FA expiry, sync completion, failures
   - Users wire it to whatever they want (webhook, Telegram, email, ntfy)
   - Avoids maintaining N notification integrations

### Phase 2: Lifecycle & Parity (v0.4)

*Features the Python user base depends on*

6. **Auto-delete / Recently Deleted scan (#28)**
   - Detect files removed from iCloud, optionally delete local copies
   - The SQLite state DB makes this tractable — compare DB state vs API

7. **Delete after download (#29)**
   - Move photos off iCloud after successful download
   - High-risk feature, needs `--confirm-delete` safety gate

8. **`--only-print-filenames` (#17)**
   - Already parsed (hidden flag), just needs wiring

9. **Schema migration (#69)**
   - Needs to be solid before v1.0 since users will upgrade across versions

### Phase 3: Polish & Quality of Life (v0.5-v0.9)

*Nice-to-haves that round out the experience*

10. **OS keyring integration (#22)** — store password in macOS Keychain / Linux Secret Service
11. **XMP sidecar export (#19)** — metadata preservation for Lightroom/Darktable users
12. **SMS-based 2FA (#21)** — broader device compatibility
13. **Multi-account support (#33)** — `[[account]]` arrays in TOML config
14. **Prometheus metrics (#55)** — for the monitoring crowd
15. **HEIC to JPEG conversion (#52)** — popular in Docker wrapper community

### Explicitly out of scope for v1.0

- Web UI (community will build wrappers)
- Legacy 2SA (#38) — Apple is deprecating this
- Python LWPCookieJar import (#37) — migration guide covers this better
- OS locale date formatting (#34) — niche

---

## The "Takeover" Strategy

Aligned to engineering phases.

| Trigger | Action | Depends on |
|---------|--------|------------|
| **Phase 1 ships** | Write migration guide (wiki page mapping Python flags to Rust flags/TOML, `import-existing` workflow) | Docker + headless MFA |
| **Python icloudpd breaks or maintainer steps down** | Brief, factual comment on their repo/issues linking to icloudpd-rs | Migration guide exists |
| **Phase 2 ships** | Show HN / r/selfhosted / r/homelab launch post. Lead with "icloudpd is losing maintenance, here's what I built" + concrete numbers (2.5-3x faster, 30s enumeration, SQLite state) | Auto-delete + Docker = covers most users |
| **Docker image live** | Post on boredazfcuk wrapper issues / discussions with env var mapping doc | Docker image + compose example |

### Launch post narrative

Don't lead with "I rewrote X in Rust." Lead with the problem:

- icloudpd is losing its maintainer
- Large libraries take forever to enumerate
- No persistent state means re-scanning everything
- Threading was removed because it was buggy
- Here's a tool that fixes all of that — and it's a single binary

---

## Priority Order (what to build next)

1. ~~TOML config (#51)~~ — **DONE**
2. Docker image (#40) — **unblocks everything else on the strategy side**
3. Headless MFA (#36) — **makes Docker actually usable for unattended**
4. Notification script (#32) — **2FA expiry notification is the #1 operational pain**
5. Auto-delete (#28) — **the feature gap most Python users will notice**
6. Schema migration (#69) — **must be solid before calling it v1.0**

Items 1-4 get you to a launchable state. Items 5-6 get you to v1.0.
