# ── Build stage ──────────────────────────────────────────────────────
FROM --platform=$BUILDPLATFORM rust:1.95.0-bookworm AS builder

# Install cross-compilation toolchains when cross-compiling.
# xmp_toolkit vendors Adobe's C++ XMP Toolkit and compiles it via `cc` on
# every build, so the arm64 branch needs g++-aarch64-linux-gnu in addition
# to the C compiler.
ARG TARGETPLATFORM
RUN case "$TARGETPLATFORM" in \
      "linux/amd64") \
        apt-get update && \
        apt-get install -y libdbus-1-dev ;; \
      "linux/arm64") \
        dpkg --add-architecture arm64 && \
        apt-get update && \
        apt-get install -y gcc-aarch64-linux-gnu g++-aarch64-linux-gnu libdbus-1-dev:arm64 ;; \
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
    export CXX_aarch64_unknown_linux_gnu=aarch64-linux-gnu-g++ && \
    export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_CXX=aarch64-linux-gnu-g++ && \
    export PKG_CONFIG_ALLOW_CROSS=1 && \
    export PKG_CONFIG_PATH=/usr/lib/aarch64-linux-gnu/pkgconfig && \
    mkdir src && echo 'fn main() {}' > src/main.rs && \
    cargo build --release --target $TARGET && \
    rm -rf src target/$TARGET/release/deps/kei*

COPY src/ src/

RUN export TARGET=$(cat /tmp/target) && \
    export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc && \
    export CXX_aarch64_unknown_linux_gnu=aarch64-linux-gnu-g++ && \
    export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_CXX=aarch64-linux-gnu-g++ && \
    export PKG_CONFIG_ALLOW_CROSS=1 && \
    export PKG_CONFIG_PATH=/usr/lib/aarch64-linux-gnu/pkgconfig && \
    cargo build --release --target $TARGET && \
    cp target/$TARGET/release/kei /kei

# ── Runtime stage ────────────────────────────────────────────────────
FROM debian:bookworm-20260918-slim

RUN apt-get update && \
    apt-get install -y --no-install-recommends bash curl ca-certificates libdbus-1-3 gosu && \
    rm -rf /var/lib/apt/lists/*

COPY --from=builder /kei /usr/local/bin/kei
COPY docker/entrypoint.sh /usr/local/bin/entrypoint.sh
RUN chmod +x /usr/local/bin/entrypoint.sh

VOLUME ["/config", "/photos"]

# Always-on HTTP server: /healthz (health check) and /metrics (Prometheus).
# Default port 9090; override with [server] port in TOML.
EXPOSE 9090

HEALTHCHECK --interval=60s --timeout=5s --start-period=15m --retries=3 \
  CMD curl -f http://localhost:9090/healthz || exit 1

# entrypoint.sh drops to PUID:PGID when those env vars are set; otherwise
# exec's kei as root (preserves prior behavior). Required for NAS
# deployments where files must be host-user-owned (Synology Photos,
# Unraid, TrueNAS Scale).
ENTRYPOINT ["/usr/local/bin/entrypoint.sh"]
# Container runtime glue: keep sessions, state DB, and credentials on the
# mounted /config volume. Persistent user settings live in /config/config.toml.
ENV KEI_DATA_DIR=/config
# Keep glibc from reserving one large malloc arena per worker thread. This
# lowers virtual memory size for long-running watch containers without changing
# kei's actual resident memory target.
ENV MALLOC_ARENA_MAX=2
CMD ["service", "run", "--config", "/config/config.toml"]
