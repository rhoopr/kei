# Phase 1: Docker & Unattended Operation — Implementation Plan

Phase 0 (TOML config, LegacyCli removal) is complete. Phase 1 has three deliverables:

1. **Headless MFA (#36)** — `submit-code` subcommand
2. **Notification script (#32)** — `--notification-script` hook
3. **Docker image (#40)** — Dockerfile, compose, CI

Build order matters: headless MFA and notifications should land first since Docker depends on both for a good unattended experience.

---

## 1. Headless MFA (`submit-code` subcommand)

### Problem

When 2FA is required and stdin is not a TTY (Docker, systemd, cron), the current code bails:

```
Session expired and re-authentication may require 2FA.
Run `icloudpd-rs --auth-only` interactively to re-authenticate.
```

In Docker, there's no interactive TTY. Users need:

```sh
docker exec icloudpd-rs icloudpd-rs submit-code 123456
```

### Design

**New subcommand**: `submit-code` (not a flag on sync, because it's a separate invocation)

```
icloudpd-rs submit-code <CODE> [--username <user>] [--config <path>]
```

**Flow**:

1. Load session from cookie directory (same as normal auth)
2. Perform SRP login if session token is expired (uses stored/provided password)
3. Submit the 2FA code via `verify/trusteddevice/securitycode`
4. Trust the session via `2sv/trust`
5. Save cookies to disk, exit 0
6. The main sync process picks up the refreshed session on its next watch cycle

**Changes needed**:

| File | Change |
|------|--------|
| `src/cli.rs` | Add `SubmitCode` variant to `Command` enum with `SubmitCodeArgs { auth: AuthArgs, code: String }` |
| `src/auth/twofa.rs` | Extract code submission logic from `request_2fa_code()` into a new `submit_2fa_code(session, endpoints, client_id, domain, code)` that takes the code as a parameter. The existing `request_2fa_code()` becomes a thin wrapper that prompts + calls `submit_2fa_code()`. |
| `src/auth/mod.rs` | Add `pub async fn authenticate_with_code(...)` — same as `authenticate()` but takes an optional `code: Option<&str>`. When `Some`, skip the interactive prompt and call `submit_2fa_code()` directly. When `None`, use the existing interactive flow. Refactor `authenticate()` to call this. |
| `src/main.rs` | Add `Command::SubmitCode(args)` match arm → call `authenticate_with_code()` with the provided code, print success/failure, exit. |
| `src/main.rs` | In `attempt_reauth()`: when headless + watch mode, instead of bailing, log a message like `"2FA required. Submit code: icloudpd-rs submit-code <CODE> --username {username}"` and fire the notification script (if configured). Then return an error that the watch loop treats as "skip this cycle, try again next interval". |

**Key constraint**: The `submit-code` invocation must be a separate process. It loads the same cookie directory, authenticates, and saves cookies. The running sync process will pick up the fresh session on its next validation pass (which happens between watch cycles via `attempt_reauth()`).

**Edge cases**:
- Wrong code → exit 1 with clear error
- Session doesn't need 2FA → "Session is already authenticated" + exit 0
- No session exists → perform full SRP auth first, then submit code

### Tests

- CLI parsing: `submit-code` subcommand with code argument
- `submit_2fa_code()` unit test with mock session (verify it sends correct JSON payload)
- Integration: `submit-code` with `--config` flag resolves auth from TOML

---

## 2. Notification Script (#32)

### Problem

In unattended mode, users need to know when:
- 2FA re-authentication is required (most critical)
- A sync cycle completed successfully
- A sync cycle had failures

Without notifications, a Docker container silently stops syncing when 2FA expires.

### Design

**CLI flag**: `--notification-script <path>` (on `SyncArgs`)
**TOML**:
```toml
[notifications]
script = "/path/to/notify.sh"
```

**Invocation**: The script is called with environment variables:

| Variable | Description | Example |
|----------|-------------|---------|
| `ICLOUDPD_EVENT` | Event type | `2fa_required`, `sync_complete`, `sync_failed`, `session_expired` |
| `ICLOUDPD_MESSAGE` | Human-readable message | `"2FA required for user@example.com. Run: icloudpd-rs submit-code <CODE>"` |
| `ICLOUDPD_USERNAME` | The iCloud account | `user@example.com` |

```sh
# Example: ntfy.sh
#!/bin/sh
curl -d "$ICLOUDPD_MESSAGE" ntfy.sh/my-icloud-topic
```

**Changes needed**:

| File | Change |
|------|--------|
| `src/cli.rs` | Add `notification_script: Option<String>` to `SyncArgs` |
| `src/config.rs` | Add `TomlNotifications { script: Option<String> }` to `TomlConfig`. Resolve in `Config::build()`. Add `notification_script: Option<PathBuf>` to `Config`. |
| `src/notifications.rs` | New module. `Notifier` struct holding `Option<PathBuf>`. Methods: `notify(&self, event: Event, message: &str, username: &str)`. `Event` enum: `TwoFaRequired`, `SyncComplete`, `SyncFailed`, `SessionExpired`. Spawns the script via `tokio::process::Command` with env vars. Fire-and-forget (log errors, don't block sync). |
| `src/main.rs` | Create `Notifier` from config. Call `notifier.notify()` at key points: after successful sync cycle, after failed sync cycle, when 2FA is needed in headless mode. |

**Key decisions**:
- Fire-and-forget: don't let a broken script block sync
- Timeout: kill the script after 30 seconds
- No stdin: script gets no input, only env vars
- Log script stderr on failure for debugging

### Tests

- CLI parsing: `--notification-script` flag
- TOML: `[notifications] script` resolves correctly
- `Notifier::notify()` with a test script that writes env vars to a file → verify env vars are set correctly
- `Notifier` with no script configured → no-op

---

## 3. Docker Image (#40)

### Prerequisites

Items 1 and 2 above. Without headless MFA, Docker is painful. Without notifications, users don't know when 2FA expires.

### Dockerfile

Multi-stage build. Two considerations for the base image:

**Option A (recommended)**: Switch reqwest to `rustls-tls` → enables `scratch`/`distroless/static` base (no glibc/OpenSSL dependency). ~5-10MB final image.

```toml
# Cargo.toml change
reqwest = { version = "0.13", default-features = false, features = [
    "cookies", "json", "stream", "http2", "rustls-tls-webpki-roots"
] }
```

**Option B (fallback)**: Keep native-tls, use `debian:bookworm-slim` base with `ca-certificates` + `libssl3`. ~80MB final image.

Option A is strongly preferred — a 5MB Docker image vs Python icloudpd's ~400MB image is a compelling differentiator.

```dockerfile
# ── Build stage ──────────────────────────────────────────
FROM rust:1-bookworm AS builder

WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY src/ src/

# Build for the target platform
ARG TARGETPLATFORM
RUN case "$TARGETPLATFORM" in \
      "linux/amd64")  TARGET=x86_64-unknown-linux-gnu ;; \
      "linux/arm64")  TARGET=aarch64-unknown-linux-gnu ;; \
    esac && \
    rustup target add $TARGET && \
    cargo build --release --target $TARGET && \
    cp target/$TARGET/release/icloudpd-rs /icloudpd-rs

# ── Runtime stage ────────────────────────────────────────
# With rustls: use distroless/static (no glibc needed if fully static)
# With native-tls: use debian:bookworm-slim
FROM gcr.io/distroless/cc-debian12

COPY --from=builder /icloudpd-rs /usr/local/bin/icloudpd-rs

# Default volumes
VOLUME ["/config", "/photos"]

# Default config location inside container
ENV ICLOUDPD_CONFIG=/config/config.toml

ENTRYPOINT ["icloudpd-rs"]
CMD ["sync", "--config", "/config/config.toml", "--cookie-directory", "/config", "--directory", "/photos"]
```

**Note**: For fully static builds with musl (scratch-compatible), we'd need `x86_64-unknown-linux-musl` / `aarch64-unknown-linux-musl` targets. This may require musl cross-compilation tooling in CI. Start with `distroless/cc` (has glibc) and optimize later.

### docker-compose.yml

```yaml
services:
  icloudpd-rs:
    image: ghcr.io/rhoopr/icloudpd-rs:latest
    container_name: icloudpd-rs
    restart: unless-stopped
    environment:
      - ICLOUD_PASSWORD=${ICLOUD_PASSWORD}
      - TZ=${TZ:-UTC}
    volumes:
      - ./config:/config
      - /path/to/photos:/photos
```

With a `config/config.toml`:

```toml
[auth]
username = "user@example.com"

[download]
folder_structure = "%Y/%m/%d"
set_exif_datetime = true

[watch]
interval = 3600

[notifications]
script = "/config/notify.sh"
```

### CI Workflow (`.github/workflows/docker.yml`)

```yaml
name: Docker

on:
  release:
    types: [published]
  push:
    branches: [main]
    paths: [src/**, Cargo.toml, Cargo.lock, Dockerfile]

jobs:
  docker:
    runs-on: ubuntu-latest
    permissions:
      contents: read
      packages: write
    steps:
      - uses: actions/checkout@v4
      - uses: docker/setup-qemu-action@v3     # ARM64 emulation
      - uses: docker/setup-buildx-action@v3
      - uses: docker/login-action@v3
        with:
          registry: ghcr.io
          username: ${{ github.actor }}
          password: ${{ secrets.GITHUB_TOKEN }}
      - uses: docker/metadata-action@v5
        id: meta
        with:
          images: ghcr.io/rhoopr/icloudpd-rs
          tags: |
            type=semver,pattern={{version}}
            type=semver,pattern={{major}}.{{minor}}
            type=sha,prefix=
            type=raw,value=latest,enable={{is_default_branch}}
      - uses: docker/build-push-action@v6
        with:
          context: .
          platforms: linux/amd64,linux/arm64
          push: true
          tags: ${{ steps.meta.outputs.tags }}
          labels: ${{ steps.meta.outputs.labels }}
          cache-from: type=gha
          cache-to: type=gha,mode=max
```

### Config / Volume Layout

```
/config/
├── config.toml          # TOML config (optional, can use env vars instead)
├── <sanitized_user>/    # Cookie directory (auto-created)
│   ├── session_data.json
│   └── cookies.json
├── <sanitized_user>.db  # State database
└── notify.sh            # Notification script (optional)
```

The `--cookie-directory /config` flag (or `[auth] cookie_directory = "/config"` in TOML) makes all session/state data live under the single `/config` volume.

### Env Var Support

Already partially supported:
- `ICLOUD_PASSWORD` → clap `env` attribute (already wired)

Need to add:
- `ICLOUD_USERNAME` → add `env = "ICLOUD_USERNAME"` to the username clap arg
- `TZ` → handled by the OS/container runtime, no code needed

---

## Implementation Order

```
1. Headless MFA (#36)                    ~2-3 sessions
   ├── Extract submit_2fa_code()
   ├── Add submit-code subcommand
   ├── Modify attempt_reauth() for headless
   └── Tests

2. Notification script (#32)             ~1-2 sessions
   ├── Add notifications module
   ├── Wire into CLI + TOML config
   ├── Fire at key sync lifecycle points
   └── Tests

3. Docker image (#40)                    ~2-3 sessions
   ├── Switch to rustls-tls (test locally first)
   ├── Dockerfile + docker-compose.yml
   ├── Add ICLOUD_USERNAME env support
   ├── CI workflow
   └── README Docker section
```

### What ships as v0.3

All three items. Tag `v0.3.0` when Docker image is live on ghcr.io with working headless MFA and notification support. This is the "launchable" milestone from the roadmap.
