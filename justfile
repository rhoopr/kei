# Local dev recipes. Bare `just` lists them. No one-shot aliases over
# raw cargo commands - recipes only exist when they compose, set up
# env, or dispatch on a mode.

set shell := ["bash", "-euo", "pipefail", "-c"]
set tempdir := "/tmp"

_default:
    @just --list

# Static source/tooling checks. Does not run behavior tests.
# The round-trip gate fails when this branch adds/changes a serializer in src/
# without a paired round-trip test; see scripts/check-roundtrip-gate.sh.
static-checks:
    cargo fmt --all --check
    cargo clippy --all-targets --all-features -- -D warnings
    cargo clippy --all-targets --no-default-features -- -D warnings
    RUSTDOCFLAGS="-Dwarnings" cargo doc --no-deps --all-features
    cargo fetch --locked
    cargo audit --deny warnings
    just lint-workflows
    just lint-scripts
    scripts/check-contracts
    typos
    bash scripts/check-roundtrip-gate.sh

# Pre-push gate: static checks + offline behavior tests.
gate:
    just static-checks
    just test offline

# Check GitHub workflow helpers with the repo hardening guard plus optional
# actionlint when it is installed locally.
lint-workflows:
    #!/usr/bin/env bash
    set -euo pipefail
    pycache_dir="${PYTHONPYCACHEPREFIX:-/tmp/codex/kei/pycache}"
    mkdir -p "$pycache_dir"
    python3 .github/scripts/check_workflow_hardening.py
    PYTHONPYCACHEPREFIX="$pycache_dir" python3 -m py_compile .github/scripts/*.py
    if command -v actionlint >/dev/null 2>&1; then
        actionlint .github/workflows/*.yml
    else
        echo "lint-workflows: actionlint not installed; skipping optional workflow syntax lint" >&2
    fi

# Check local shell and Python helpers. External format/lint tools are
# check-only and optional locally so missing developer tools don't break the
# baseline gate.
lint-scripts:
    #!/usr/bin/env bash
    set -euo pipefail
    pycache_dir="${PYTHONPYCACHEPREFIX:-/tmp/codex/kei/pycache}"
    mkdir -p "$pycache_dir"
    mapfile -t shell_files < <(find scripts tests/shell docker -maxdepth 3 -type f \( -name '*.sh' -o -name 'entrypoint.sh' \) -print | sort)
    mapfile -t python_files < <(find scripts .github/scripts -maxdepth 3 -type f -name '*.py' -print | sort)
    python_files+=(scripts/check-contracts)
    for shell_file in "${shell_files[@]}"; do
        bash -n "$shell_file"
    done
    PYTHONPYCACHEPREFIX="$pycache_dir" python3 -m py_compile "${python_files[@]}"
    if command -v shellcheck >/dev/null 2>&1; then
        shellcheck -x -P tests/shell:scripts:scripts/full-test "${shell_files[@]}"
    else
        echo "lint-scripts: shellcheck not installed; skipping optional shell lint" >&2
    fi
    if command -v shfmt >/dev/null 2>&1; then
        shfmt -d "${shell_files[@]}"
    else
        echo "lint-scripts: shfmt not installed; skipping optional shell format check" >&2
    fi
    if command -v ruff >/dev/null 2>&1; then
        ruff check "${python_files[@]}"
    else
        echo "lint-scripts: ruff not installed; skipping optional Python lint" >&2
    fi

# Pre-release battery with phase logs, metrics, Docker smokes, and live smokes.
# Stops on first failure. Keeps a /tmp/codex/kei/full-test/logs/kei-full-test-*.log only when failing.
full-test:
    #!/usr/bin/env bash
    set -Eeuo pipefail
    log_dir="${KEI_FULLTEST_LOG_DIR:-/tmp/codex/kei/full-test/logs}"
    mkdir -p "$log_dir"
    log_path=$(mktemp "$log_dir/kei-full-test-$(date +%Y%m%dT%H%M%S)-XXXXXX.log")
    failed_line=""
    failed_command=""
    record_failure() {
        failed_line="${BASH_LINENO[0]:-unknown}"
        failed_command="$BASH_COMMAND"
    }
    finish() {
        rc=$?
        if [ "$rc" -eq 0 ]; then
            rm -f "$log_path"
            return 0
        fi
        {
            echo ""
            echo "full-test: failed with exit code $rc"
            if [ -n "$failed_command" ]; then
                echo "full-test: failed command near line $failed_line: $failed_command"
            fi
            echo "full-test: log saved at $log_path"
        } >&2
        exit "$rc"
    }
    trap record_failure ERR
    trap finish EXIT
    exec > >(tee "$log_path") 2>&1
    echo "full-test: keeping failure log at $log_path if this run fails"
    scripts/full-test/run_all.sh

# Compact history table for previous `just full-test` runs.
full-test-history N="10":
    scripts/full-test/history.sh "{{N}}"

# Compact agent preflight: branch, worktree, diff size, and latest full-test record.
agent-status:
    #!/usr/bin/env bash
    set -euo pipefail
    echo "repo: $(pwd)"
    echo
    git status --short --branch
    echo
    echo "diff stat:"
    if git diff --quiet -- . && git diff --cached --quiet -- .; then
        echo "(clean)"
    else
        git diff --stat
        git diff --cached --stat
    fi
    echo
    runs_dir="${KEI_FULL_TEST_RUNS_DIR:-/tmp/codex/kei/full-test/test-runs}"
    latest=$(find "$runs_dir" -maxdepth 1 -name '*.json' -type f -printf '%T@ %p\n' 2>/dev/null | sort -nr | head -n 1 | cut -d' ' -f2- || true)
    echo "latest full-test:"
    if [[ -n "$latest" ]]; then
        echo "$latest"
        if rg -q '"status": "fail"' "$latest"; then
            result=fail
        else
            result=pass
        fi
        scripts/full-test/render_summary.py "$latest" --result "$result" | sed -n '1,80p'
    else
        echo "(no run records found)"
    fi
    echo
    scripts/just/review-scope.sh --validation-only
    echo
    echo "recent full-test history:"
    scripts/full-test/history.sh 3 2>/dev/null || true

# Exact branch review input plus workspace and validation provenance.
review-scope BASE="origin/main":
    scripts/just/review-scope.sh "{{BASE}}"

# Summarize the latest full-test phase log, or pass LOG=/path/to/log.
agent-failure-summary LOG="":
    #!/usr/bin/env bash
    set -euo pipefail
    log="{{LOG}}"
    log="${log#LOG=}"
    if [[ -z "$log" ]]; then
        log_dir="${KEI_FULLTEST_LOG_DIR:-/tmp/codex/kei/full-test/logs}"
        log=$(find "$log_dir" -maxdepth 1 -type f \( -name 'full-test-*.log' -o -name 'kei-full-test-*.log' \) -printf '%T@ %p\n' 2>/dev/null | sort -nr | head -n 1 | cut -d' ' -f2- || true)
    fi
    if [[ -z "$log" || ! -f "$log" ]]; then
        echo "agent-failure-summary: no log found" >&2
        exit 1
    fi
    echo "log: $log"
    echo
    echo "failure markers:"
    rg -n "FAILED|failures:|error:|panicked|PermissionDenied|Operation not permitted|Failed to bind|Could not resolve|timed out|timeout|rc=[1-9]" "$log" | tail -n 80 || echo "(no common failure markers found)"
    echo
    echo "tail:"
    tail -n 80 "$log"

# Check lightweight source CONTRACT markers against their contract_ tests.
check-contracts:
    scripts/check-contracts

# Fast offline v0.20 patch-release smoke for the May 27 regression set.
release-smoke:
    scripts/full-test/run_release_regression_smoke.sh

# Test dispatcher: offline | fast | scenario NAME | scenarios | live | live-smoke | live-shell | packaging | docker-full | service | host-service | PATTERN.
test MODE="" *ARGS="":
    #!/usr/bin/env bash
    set -euo pipefail
    _live_env() {
        source scripts/just/live-env.sh
    }
    run_scenario() {
        local name="$1"
        local script="scripts/test-scenarios/$name.sh"
        if [[ ! -f "$script" ]]; then
            echo "unknown scenario: $name" >&2
            echo "available scenarios:" >&2
            scripts/test-scenarios/list.sh >&2
            exit 2
        fi
        bash "$script"
    }
    run_live_smokes() {
        scripts/full-test/run_live_smokes.sh
        scripts/full-test/run_live_import_rehearsal.sh
        if [[ -n "${KEI_FULL_TEST_CROSS_ZONE_ALBUM:-}" ]]; then
            scripts/full-test/run_cross_zone_album_hydration.sh
        fi
    }
    run_docker_full() {
        just docker build
        scripts/full-test/run_docker_puid_smoke.sh
        just docker multiarch
        docker run --rm "${KEI_DOCKER_IMAGE:-kei:dev}" --version
        docker run --rm "${KEI_DOCKER_IMAGE:-kei:dev}" --help
        set +e
        timeout 8 docker run --rm -e ICLOUD_USERNAME=dummy@example.com "${KEI_DOCKER_IMAGE:-kei:dev}"
        rc=$?
        set -e
        [[ $rc -ne 2 ]]
    }
    mode="{{MODE}}"
    case "$mode" in
        "")
            cargo test --all-features
            ;;
        fast)
            cargo test --lib -- --test-threads=1
            cargo test --test cli --test behavioral --test service_cli --test service_linux --test service_macos --test service_windows --test service_status
            ;;
        offline)
            cargo test --all-features
            cargo test --no-default-features
            scripts/test-scenarios/check.sh
            ;;
        scenarios)
            scenarios=$(scripts/test-scenarios/list.sh)
            [[ -n "$scenarios" ]] || { echo "no scenarios found" >&2; exit 2; }
            while read -r scenario; do
                run_scenario "$scenario"
            done <<<"$scenarios"
            ;;
        scenario)
            args=({{ARGS}})
            if [[ ${#args[@]} -ne 1 ]]; then
                echo "usage: just test scenario NAME" >&2
                scripts/test-scenarios/list.sh >&2
                exit 2
            fi
            run_scenario "${args[0]}"
            ;;
        scenario-*)
            run_scenario "${mode#scenario-}"
            ;;
        live)
            _live_env
            cargo test --all-features --test sync -- --ignored --test-threads=1
            cargo test --all-features --test state_auth -- --ignored --test-threads=1
            cargo test --all-features --test import_existing_live -- --ignored --test-threads=1
            ;;
        live-smoke)
            _live_env
            run_live_smokes
            ;;
        live-shell)
            _live_env
            scripts/full-test/run_shell_suites.sh
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
        packaging|package)
            cargo build --release
            scripts/full-test/run_release_archive_smoke.sh
            ;;
        docker-full)
            run_docker_full
            ;;
        service)
            just service-smoke
            ;;
        host-service)
            KEI_FULL_TEST_REAL_SERVICE=1 scripts/full-test/run_real_service_lifecycle.sh
            ;;
        nightly-tools)
            if rustup toolchain list 2>/dev/null | grep -q '^nightly' && command -v cargo-fuzz >/dev/null 2>&1; then
                cargo +nightly fuzz build
            else
                echo "nightly-tools: skipping fuzz build; nightly toolchain or cargo-fuzz not installed" >&2
            fi
            cargo +nightly --version >/dev/null
            cargo udeps --version >/dev/null
            cargo +nightly udeps --all-targets
            ;;
        *)
            cargo test "$mode"
            ;;
    esac

# Coverage: (none) | html | check | live | patch [BASE]. `live` merges sync + state_auth into the offline baseline.
cov MODE="" BASE="main":
    #!/usr/bin/env bash
    set -euo pipefail
    _live_env() {
        source scripts/just/live-env.sh
    }
    case "{{MODE}}" in
        "")
            cargo llvm-cov --all-features
            ;;
        html)
            cargo llvm-cov --all-features --html
            echo "Report: target/llvm-cov/html/index.html"
            ;;
        check)
            cargo llvm-cov --all-features --summary-only --fail-under-lines 90
            ;;
        live)
            _live_env
            # --no-report accumulates coverage across multiple test
            # binary invocations so we can run the live suites under the
            # same profile data as the offline ones. The final `report`
            # call prints the merged summary.
            cargo llvm-cov clean --workspace
            cargo llvm-cov --no-report --lib
            cargo llvm-cov --no-report --test cli
            cargo llvm-cov --no-report --test behavioral
            cargo llvm-cov --no-report --test sync -- --include-ignored --test-threads=1
            cargo llvm-cov --no-report --test state_auth -- --include-ignored --test-threads=1
            cargo llvm-cov --no-report --test import_existing_live -- --include-ignored --test-threads=1
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
            echo "Modes: (none) | html | check | live | patch [BASE]" >&2
            exit 1
            ;;
    esac

# Run any kei subcommand under cargo run with .env + temp data/photos dirs pre-applied.
dev CMD *ARGS:
    #!/usr/bin/env bash
    set -euo pipefail
    if [ -f .env ]; then
        set -a; source .env; set +a
    fi
    export KEI_DATA_DIR="${KEI_DEV_DATA_DIR:-$HOME/.config/kei}"
    cfg="$(mktemp)"
    trap 'rm -f "$cfg"' EXIT
    printf 'data_dir = "%s"\n[download]\ndirectory = "%s"\n' "$KEI_DATA_DIR" "${KEI_DEV_PHOTOS_DIR:-/tmp/codex/kei/dev-photos}" > "$cfg"
    cargo run -- {{CMD}} --config "$cfg" {{ARGS}}

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
            container=$(docker ps --filter ancestor=kei:dev --quiet | head -1)
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
    cargo build --locked --release --target "$target"
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
    checksum_file="dist/SHA256SUMS.txt"
    archive_name=$(basename "$archive")
    checksum=$(cd dist && sha256sum "$archive_name")
    tmp=$(mktemp "dist/SHA256SUMS.XXXXXX")
    if [ -f "$checksum_file" ]; then
        awk -v name="$archive_name" '$2 != name { print }' "$checksum_file" > "$tmp"
    fi
    printf '%s\n' "$checksum" >> "$tmp"
    mv "$tmp" "$checksum_file"
    echo ""
    echo "Archive: $archive"
    echo "Checksum written to dist/SHA256SUMS.txt"
    echo ""
    version=$(awk -F'"' '/^version = "/ {print $2; exit}' Cargo.toml)
    echo "=== CHANGELOG [$version] ==="
    awk -v ver="$version" '
        /^## \[/ { in_section = ($0 ~ "^## \\[" ver "\\]"); next }
        in_section { print }
    ' CHANGELOG.md | sed '/./,$!d' | awk 'NR==1 && /^$/ {next} {print}'

# Fuzz: list | build | run TARGET [SECONDS] | all [SECONDS]. Requires nightly + cargo-fuzz; install with `rustup install nightly && cargo install cargo-fuzz`.
fuzz MODE="list" *ARGS="":
    #!/usr/bin/env bash
    set -euo pipefail
    if ! command -v cargo-fuzz >/dev/null 2>&1; then
        echo "cargo-fuzz not installed. Run: cargo install cargo-fuzz" >&2
        exit 1
    fi
    if ! rustup toolchain list | grep -q '^nightly'; then
        echo "nightly toolchain not installed. Run: rustup install nightly" >&2
        exit 1
    fi
    case "{{MODE}}" in
        list)
            cargo +nightly fuzz list
            ;;
        build)
            cargo +nightly fuzz build
            ;;
        all)
            args=({{ARGS}})
            seconds="${args[0]:-60}"
            mapfile -t targets < <(cargo +nightly fuzz list)
            if [ "${#targets[@]}" -eq 0 ]; then
                echo "no fuzz targets found" >&2
                exit 1
            fi
            for target in "${targets[@]}"; do
                echo ""
                echo "=== fuzz $target (${seconds}s) ==="
                mkdir -p "fuzz/corpus/$target"
                extra=()
                if [ -d "fuzz/seeds/$target" ]; then
                    extra+=("fuzz/seeds/$target")
                fi
                cargo +nightly fuzz run "$target" "fuzz/corpus/$target" "${extra[@]}" -- -max_total_time="$seconds"
            done
            ;;
        run)
            args=({{ARGS}})
            target="${args[0]:-}"
            seconds="${args[1]:-60}"
            if [ -z "$target" ]; then
                echo "usage: just fuzz run TARGET [SECONDS]" >&2
                cargo +nightly fuzz list
                exit 2
            fi
            # libfuzzer treats the FIRST corpus dir as input/output (it
            # writes new coverage-improving inputs there) and any later dirs
            # as read-only auxiliary corpora. Pass fuzz/corpus/<target>
            # first so libfuzzer's auto-saved finds land there, and
            # fuzz/seeds/<target> second so checked-in regression inputs
            # replay every run without getting clobbered by autogen entries.
            mkdir -p "fuzz/corpus/$target"
            extra=()
            if [ -d "fuzz/seeds/$target" ]; then
                extra+=("fuzz/seeds/$target")
            fi
            cargo +nightly fuzz run "$target" "fuzz/corpus/$target" "${extra[@]}" -- -max_total_time="$seconds"
            ;;
        *)
            echo "Unknown mode: {{MODE}}" >&2
            echo "Modes: list | build | run TARGET [SECONDS] | all [SECONDS]" >&2
            exit 1
            ;;
    esac

# Check local tools needed by gate/full-test/cov/fuzz/docker recipes.
doctor:
    #!/usr/bin/env bash
    set -euo pipefail
    status=0
    ok() { printf '[ok]   %s\n' "$1"; }
    fail() { printf '[miss] %s\n' "$1"; status=1; }
    note() { printf '[note] %s\n' "$1"; }
    have_cmd() {
        local cmd="$1"
        local label="${2:-$1}"
        local version=""
        if command -v "$cmd" >/dev/null 2>&1; then
            version=$("$cmd" --version 2>/dev/null | head -1 || true)
            ok "$label${version:+: $version}"
        else
            fail "$label not found"
        fi
    }
    have_cargo_subcommand() {
        local subcmd="$1"
        local label="${2:-cargo $subcmd}"
        local version=""
        if cargo "$subcmd" --version >/dev/null 2>&1; then
            version=$(cargo "$subcmd" --version 2>/dev/null | head -1 || true)
            ok "$label${version:+: $version}"
        else
            fail "$label not available"
        fi
    }
    have_cmd just
    have_cmd cargo
    have_cmd rustup
    if cargo +nightly --version >/dev/null 2>&1; then
        ok "nightly toolchain: $(cargo +nightly --version | head -1)"
    else
        fail "nightly toolchain not installed"
    fi
    have_cmd cargo-fuzz
    have_cargo_subcommand udeps "cargo-udeps"
    have_cargo_subcommand audit "cargo-audit"
    have_cargo_subcommand llvm-cov "cargo-llvm-cov"
    have_cmd typos
    have_cmd docker
    if docker buildx version >/dev/null 2>&1; then
        ok "docker buildx: $(docker buildx version | head -1)"
    else
        fail "docker buildx not available"
    fi
    if command -v systemd-analyze >/dev/null 2>&1 || command -v plutil >/dev/null 2>&1; then
        ok "service-smoke verifier available"
    else
        note "service-smoke verifier not found; service-smoke is Linux/macOS only"
    fi
    if [ -f .env ]; then
        ok ".env present for live recipes"
    else
        note ".env missing; live recipes need ICLOUD_USERNAME/ICLOUD_PASSWORD in environment"
    fi
    if [ -d .test-cookies ]; then
        ok ".test-cookies present"
    else
        note ".test-cookies missing; live auth may need a fresh login"
    fi
    exit "$status"

# Service-install smoke: builds release, dry-run install, validates artifact, uninstall, asserts clean.
# Mirrors .github/workflows/service-smoke.yml. Linux + macOS only.
service-smoke:
    #!/usr/bin/env bash
    set -euxo pipefail
    cargo build --release
    KEI="$(scripts/full-test/cargo_target_dir.sh)/release/kei"
    # `kei status` derives its state-DB path from the username; pin a
    # placeholder so the smoke does not depend on the operator's .env
    # or saved config.
    export ICLOUD_USERNAME="service-smoke@example.invalid"
    assert_service_not_installed() {
        local status
        status=$("$KEI" status)
        printf '%s\n' "$status" | grep -q '^Service: not installed'
    }
    case "$(uname -s)" in
        Linux)
            UNIT="$HOME/.config/systemd/user/kei.service"
            UNIT_PREVIEW="${RUNNER_TEMP:-${TMPDIR:-/tmp}}/kei.service"
            # Clean any leftover from a previous failed run so the
            # pre-state assertion is meaningful.
            rm -f "$UNIT" "$UNIT_PREVIEW"
            test ! -e "$UNIT"
            assert_service_not_installed
            "$KEI" install --user --dry-run > "$UNIT_PREVIEW"
            test ! -e "$UNIT"
            systemd-analyze --user verify "$UNIT_PREVIEW"
            "$KEI" uninstall
            test ! -e "$UNIT"
            assert_service_not_installed
            ;;
        Darwin)
            PLIST="$HOME/Library/LaunchAgents/com.rhoopr.kei.plist"
            PLIST_PREVIEW="${RUNNER_TEMP:-${TMPDIR:-/tmp}}/com.rhoopr.kei.plist"
            rm -f "$PLIST" "$PLIST_PREVIEW"
            test ! -e "$PLIST"
            assert_service_not_installed
            "$KEI" install --dry-run > "$PLIST_PREVIEW"
            test ! -e "$PLIST"
            plutil -lint "$PLIST_PREVIEW"
            "$KEI" uninstall
            test ! -e "$PLIST"
            assert_service_not_installed
            ;;
        *)
            echo "service-smoke is Linux/macOS only; Windows runs in CI" >&2
            exit 2
            ;;
    esac

# Disabled: not part of the active workflow.
# Create branch NAME off a freshly fetched origin/main (AGENTS.md branch-from-fresh-main rule).
# branch NAME:
#     #!/usr/bin/env bash
#     set -euo pipefail
#     git fetch origin main
#     if git show-ref --verify --quiet "refs/heads/{{NAME}}"; then
#         echo "branch '{{NAME}}' already exists locally" >&2
#         exit 1
#     fi
#     git switch -c "{{NAME}}" origin/main
