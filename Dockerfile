# ── Build stage ──────────────────────────────────────────────────────
FROM --platform=$BUILDPLATFORM rust:1-bookworm AS builder

# Install cross-compilation toolchains when cross-compiling
ARG TARGETPLATFORM
RUN case "$TARGETPLATFORM" in \
      "linux/arm64") \
        dpkg --add-architecture arm64 && \
        apt-get update && \
        apt-get install -y gcc-aarch64-linux-gnu ;; \
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
    rm -rf src target/$TARGET/release/deps/icloudpd*

COPY src/ src/

RUN export TARGET=$(cat /tmp/target) && \
    export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc && \
    cargo build --release --target $TARGET && \
    cp target/$TARGET/release/icloudpd-rs /icloudpd-rs

# ── Runtime stage ────────────────────────────────────────────────────
FROM gcr.io/distroless/cc-debian12

COPY --from=builder /icloudpd-rs /usr/local/bin/icloudpd-rs

VOLUME ["/config", "/photos"]

ENTRYPOINT ["icloudpd-rs"]
CMD ["sync", "--config", "/config/config.toml", "--cookie-directory", "/config", "--directory", "/photos"]
