# ── Build stage ──────────────────────────────────────────────────────
FROM --platform=$BUILDPLATFORM rust:1-bookworm AS builder

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
FROM debian:bookworm-slim

RUN apt-get update && \
    apt-get install -y --no-install-recommends bash curl ca-certificates libdbus-1-3 && \
    rm -rf /var/lib/apt/lists/*

COPY --from=builder /kei /usr/local/bin/kei

VOLUME ["/config", "/photos"]

# Always-on HTTP server: /healthz (health check) and /metrics (Prometheus).
# Default port 9090; override with --http-port / KEI_HTTP_PORT.
EXPOSE 9090

HEALTHCHECK --interval=60s --timeout=5s --start-period=15m --retries=3 \
  CMD curl -f http://localhost:9090/healthz || exit 1

ENTRYPOINT ["kei"]
CMD ["sync", "--config", "/config/config.toml", "--data-dir", "/config", "--download-dir", "/photos", "--watch-with-interval", "86400"]
