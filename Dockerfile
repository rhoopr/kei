# ── Build stage ──────────────────────────────────────────────────────
FROM --platform=$BUILDPLATFORM rust:1-bookworm AS builder

# Install cross-compilation toolchains when cross-compiling
ARG TARGETPLATFORM
RUN case "$TARGETPLATFORM" in \
      "linux/amd64") \
        apt-get update && \
        apt-get install -y libdbus-1-dev ;; \
      "linux/arm64") \
        dpkg --add-architecture arm64 && \
        apt-get update && \
        apt-get install -y gcc-aarch64-linux-gnu libdbus-1-dev ;; \
    esac

WORKDIR /build

# Resolve target triple and linker from TARGETPLATFORM once.
# Shared by the dependency-cache and real build steps.
ARG CARGO_TARGET
ARG CARGO_LINKER_ENV
RUN case "$TARGETPLATFORM" in \
      "linux/amd64") echo "x86_64-unknown-linux-gnu"  > /tmp/target ;; \
      "linux/arm64") echo "aarch64-unknown-linux-gnu" > /tmp/target ;; \
    esac && \
    rustup target add $(cat /tmp/target)

# Cache dependency compilation: copy manifests first, build a dummy, then
# copy the real source. This means changing src/ doesn't invalidate the
# dependency layer.
COPY Cargo.toml Cargo.lock ./
RUN export TARGET=$(cat /tmp/target) && \
    export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc && \
    mkdir src && echo 'fn main() {}' > src/main.rs && \
    cargo build --release --target $TARGET && \
    rm -rf src target/$TARGET/release/deps/kei*

COPY src/ src/

RUN export TARGET=$(cat /tmp/target) && \
    export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc && \
    cargo build --release --target $TARGET && \
    cp target/$TARGET/release/kei /kei

# ── Runtime stage ────────────────────────────────────────────────────
FROM debian:bookworm-slim

RUN apt-get update && \
    apt-get install -y --no-install-recommends bash curl jq ca-certificates libdbus-1-3 && \
    rm -rf /var/lib/apt/lists/*

COPY --from=builder /kei /usr/local/bin/kei

VOLUME ["/config", "/photos"]

HEALTHCHECK --interval=60s --timeout=5s --start-period=15m --retries=3 \
  CMD test -f /config/health.json \
   && test "$(jq -r '.consecutive_failures' /config/health.json)" -lt 5 \
   && { LAST=$(jq -r '.last_sync_at' /config/health.json); \
        [ "$LAST" = "null" ] \
        || test "$(( $(date +%s) - $(date -d "$LAST" +%s) ))" -lt 7200; }

ENTRYPOINT ["kei"]
CMD ["sync", "--config", "/config/config.toml", "--cookie-directory", "/config", "--directory", "/photos"]
