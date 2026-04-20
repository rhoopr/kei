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

# Coverage: (none) | html | patch [BASE] - patch reproduces the sticky PR comment locally.
cov MODE="" BASE="main":
    #!/usr/bin/env bash
    set -euo pipefail
    case "{{MODE}}" in
        "")
            cargo llvm-cov --all-features
            ;;
        html)
            cargo llvm-cov --all-features --html
            echo "Report: target/llvm-cov/html/index.html"
            ;;
        patch)
            cargo llvm-cov --all-features --lcov --output-path head.lcov
            git worktree add ../.kei-cov-base "{{BASE}}" >/dev/null
            (cd ../.kei-cov-base && cargo llvm-cov --all-features --lcov --output-path "$OLDPWD/base.lcov")
            git worktree remove ../.kei-cov-base >/dev/null
            python3 .github/scripts/patch_coverage.py \
                --head head.lcov \
                --base base.lcov \
                --base-ref "{{BASE}}" \
                --head-ref HEAD
            rm -f head.lcov base.lcov
            ;;
        *)
            echo "Unknown mode: {{MODE}}" >&2
            echo "Modes: (none) | html | patch [BASE]" >&2
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
            docker buildx build --platform linux/amd64,linux/arm64 -t kei:dev .
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

# Create ../kei-NAME on branch BRANCH (default: NAME). Complements CLAUDE.md's worktree rule.
wt NAME BRANCH="":
    #!/usr/bin/env bash
    set -euo pipefail
    branch="{{BRANCH}}"
    if [ -z "$branch" ]; then
        branch="{{NAME}}"
    fi
    if git show-ref --verify --quiet "refs/heads/$branch"; then
        git worktree add "../kei-{{NAME}}" "$branch"
    else
        git worktree add "../kei-{{NAME}}" -b "$branch"
    fi

# Remove a worktree created with `just wt`.
wt-rm NAME:
    git worktree remove "../kei-{{NAME}}"
