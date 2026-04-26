# Local dev recipes. Bare `just` lists them. No one-shot aliases over
# raw cargo commands - recipes only exist when they compose, set up
# env, or dispatch on a mode.

set shell := ["bash", "-euo", "pipefail", "-c"]

_default:
    @just --list

# Pre-push gate: fmt + clippy + offline tests + doc + audit + typos.
gate:
    cargo fmt --all --check
    cargo clippy --all-targets --all-features -- -D warnings
    cargo test --bin kei --test cli --test behavioral
    RUSTDOCFLAGS="-Dwarnings" cargo doc --no-deps --all-features
    cargo fetch --locked
    cargo audit
    typos

# Test dispatcher: offline | fast | live | concurrency | state | docker | PATTERN.
test MODE="":
    #!/usr/bin/env bash
    set -euo pipefail
    # Shared live-auth setup: sources .env if needed, applies the
    # maintainer's cookie-dir / album defaults so the Rust-live and
    # shell-live paths don't diverge. Overridable via the environment.
    _live_env() {
        if [ -z "${ICLOUD_USERNAME:-}" ] && [ -f .env ]; then
            set -a; source .env; set +a
        fi
        : "${ICLOUD_USERNAME:?ICLOUD_USERNAME must be set (via .env or environment)}"
        export ICLOUD_TEST_COOKIE_DIR="${ICLOUD_TEST_COOKIE_DIR:-$HOME/.config/kei}"
        export KEI_TEST_ALBUM="${KEI_TEST_ALBUM:-icloudpd-test}"
    }
    case "{{MODE}}" in
        "")
            cargo test --all-features
            ;;
        fast)
            cargo test --bin kei --test cli --test behavioral
            ;;
        live)
            _live_env
            cargo test --test sync -- --ignored --test-threads=1
            cargo test --test state_auth -- --ignored --test-threads=1
            ;;
        concurrency)
            _live_env
            ./tests/shell/concurrency.sh
            ;;
        state)
            _live_env
            ./tests/shell/state-machine.sh
            ;;
        docker)
            _live_env
            ./tests/shell/docker.sh
            ;;
        *)
            cargo test "{{MODE}}"
            ;;
    esac

# Coverage: (none) | html | live | patch [BASE]. `live` merges sync + state_auth into the offline baseline.
cov MODE="" BASE="main":
    #!/usr/bin/env bash
    set -euo pipefail
    _live_env() {
        if [ -z "${ICLOUD_USERNAME:-}" ] && [ -f .env ]; then
            set -a; source .env; set +a
        fi
        : "${ICLOUD_USERNAME:?ICLOUD_USERNAME must be set (via .env or environment)}"
        export ICLOUD_TEST_COOKIE_DIR="${ICLOUD_TEST_COOKIE_DIR:-$HOME/.config/kei}"
        export KEI_TEST_ALBUM="${KEI_TEST_ALBUM:-icloudpd-test}"
    }
    case "{{MODE}}" in
        "")
            cargo llvm-cov --all-features
            ;;
        html)
            cargo llvm-cov --all-features --html
            echo "Report: target/llvm-cov/html/index.html"
            ;;
        live)
            _live_env
            # --no-report accumulates coverage across multiple test
            # binary invocations so we can run the live suites under the
            # same profile data as the offline ones. The final `report`
            # call prints the merged summary.
            cargo llvm-cov clean --workspace
            cargo llvm-cov --no-report --bin kei
            cargo llvm-cov --no-report --test cli
            cargo llvm-cov --no-report --test behavioral
            cargo llvm-cov --no-report --test sync -- --include-ignored --test-threads=1
            cargo llvm-cov --no-report --test state_auth -- --include-ignored --test-threads=1
            cargo llvm-cov report --summary-only
            ;;
        patch)
            cargo llvm-cov --all-features --lcov --output-path head.lcov
            # --detach so the base worktree doesn't collide with any
            # existing checkout of the base branch (e.g. the main repo
            # already sitting on main).
            git worktree add --detach ../.kei-cov-base "{{BASE}}" >/dev/null
            (cd ../.kei-cov-base && cargo llvm-cov --all-features --lcov --output-path "$OLDPWD/base.lcov")
            git worktree remove ../.kei-cov-base >/dev/null
            python3 .github/scripts/patch_coverage.py \
                --lcov head.lcov \
                --base-lcov base.lcov \
                --base "{{BASE}}"
            rm -f head.lcov base.lcov
            ;;
        *)
            echo "Unknown mode: {{MODE}}" >&2
            echo "Modes: (none) | html | live | patch [BASE]" >&2
            exit 1
            ;;
    esac

# Run any kei subcommand under cargo run with .env + scratch data/photos dirs pre-applied.
dev CMD *ARGS:
    #!/usr/bin/env bash
    set -euo pipefail
    if [ -f .env ]; then
        set -a; source .env; set +a
    fi
    cargo run -- {{CMD}} \
        --data-dir "${KEI_DEV_DATA_DIR:-$HOME/.config/kei}" \
        --directory "${KEI_DEV_PHOTOS_DIR:-/tmp/kei-dev-photos}" \
        {{ARGS}}

# Docker: build | multiarch | run | shell | health.
docker MODE:
    #!/usr/bin/env bash
    set -euo pipefail
    case "{{MODE}}" in
        build)
            docker build -t kei:dev .
            ;;
        multiarch)
            # The default `docker` driver can't build multiple platforms;
            # bootstrap (or reuse) a `docker-container` driver builder
            # named `kei-multiarch` for this invocation only.
            if ! docker buildx inspect kei-multiarch >/dev/null 2>&1; then
                docker buildx create --name kei-multiarch --driver docker-container >/dev/null
            fi
            docker buildx build --builder kei-multiarch \
                --platform linux/amd64,linux/arm64 \
                -t kei:dev .
            ;;
        run)
            docker compose up
            ;;
        shell)
            docker run --rm -it --entrypoint bash kei:dev
            ;;
        health)
            container=$(docker ps --filter ancestor=kei:dev --format '{{{{.ID}}}}' | head -1)
            if [ -z "$container" ]; then
                echo "No running kei:dev container found." >&2
                exit 1
            fi
            docker exec "$container" cat /config/health.json
            ;;
        *)
            echo "Unknown mode: {{MODE}}" >&2
            echo "Modes: build | multiarch | run | shell | health" >&2
            exit 1
            ;;
    esac

# Reproduce release.yml's build + archive locally for TARGET (default host).
release TARGET="":
    #!/usr/bin/env bash
    set -euo pipefail
    target="{{TARGET}}"
    if [ -z "$target" ]; then
        target=$(rustc -vV | awk '/^host:/ {print $2}')
    fi
    case "$target" in
        aarch64-unknown-linux-gnu)
            export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc
            export CXX_aarch64_unknown_linux_gnu=aarch64-linux-gnu-g++
            export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_CXX=aarch64-linux-gnu-g++
            export PKG_CONFIG_ALLOW_CROSS=1
            export PKG_CONFIG_PATH=/usr/lib/aarch64-linux-gnu/pkgconfig
            ;;
    esac
    cargo build --release --target "$target"
    mkdir -p dist
    case "$target" in
        *-windows-*)
            archive="dist/kei-$target.zip"
            (cd "target/$target/release" && zip "../../../$archive" kei.exe)
            ;;
        *)
            archive="dist/kei-$target.tar.gz"
            tar -C "target/$target/release" -czf "$archive" kei
            ;;
    esac
    (cd dist && sha256sum "$(basename "$archive")") >> dist/SHA256SUMS.txt
    echo ""
    echo "Archive: $archive"
    echo "Checksum appended to dist/SHA256SUMS.txt"
    echo ""
    version=$(awk -F'"' '/^version = "/ {print $2; exit}' Cargo.toml)
    echo "=== CHANGELOG [$version] ==="
    awk -v ver="$version" '
        /^## \[/ { in_section = ($0 ~ "^## \\[" ver "\\]"); next }
        in_section { print }
    ' CHANGELOG.md | sed '/./,$!d' | awk 'NR==1 && /^$/ {next} {print}'

# Create branch NAME off a freshly fetched origin/main (CLAUDE.md branch-from-fresh-main rule).
branch NAME:
    #!/usr/bin/env bash
    set -euo pipefail
    git fetch origin main
    if git show-ref --verify --quiet "refs/heads/{{NAME}}"; then
        echo "branch '{{NAME}}' already exists locally" >&2
        exit 1
    fi
    git switch -c "{{NAME}}" origin/main
