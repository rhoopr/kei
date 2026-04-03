# List available recipes
default:
    @just --list

# Run all checks (fmt, clippy, test)
check: fmt clippy test

# Check formatting
fmt:
    cargo fmt -- --check

# Run clippy
clippy:
    cargo clippy --all-targets --all-features

# Run tests (excludes credential-dependent tests)
test:
    cargo test --all-features --test cli --test state --test auth

# Run all tests including auth-dependent (requires .env credentials)
test-all:
    cargo test --all-features

# Coverage vs thresholds (concise table)
coverage:
    #!/usr/bin/env bash
    set -euo pipefail
    cargo llvm-cov --all-features --bin kei --test cli --test state --json --output-path /tmp/kei-cov.json 2>&1 | tail -1
    overall=$(jq '.data[0].totals.lines.percent' /tmp/kei-cov.json)
    threshold=$(jq '.overall' coverage-thresholds.json)
    printf "\n\033[1m%-42s %8s %8s %6s\033[0m\n" "File" "Cov" "Thresh" "Gap"
    printf "%-42s %8s %8s %6s\n" "----" "---" "------" "---"
    for file in $(jq -r '.files | keys[]' coverage-thresholds.json); do
      ft=$(jq -r --arg f "$file" '.files[$f]' coverage-thresholds.json)
      fc=$(jq -r --arg f "$file" '.data[0].files[] | select(.filename | endswith($f)) | .summary.lines.percent' /tmp/kei-cov.json)
      if [ -n "$fc" ]; then
        gap=$(echo "$fc - $ft" | bc -l)
        short=$(echo "$file" | sed 's|^src/||')
        printf "%-42s %7.1f%% %7.0f%% %+5.1f\n" "$short" "$fc" "$ft" "$gap"
      fi
    done
    printf "%-42s %7.1f%% %7.0f%%\n" "OVERALL" "$overall" "$threshold"
    rm -f /tmp/kei-cov.json

# Coverage with pass/fail threshold enforcement
coverage-check:
    cargo llvm-cov --all-features --bin kei --test cli --test state --json --output-path coverage.json
    scripts/check-coverage.sh coverage.json
    @rm -f coverage.json

# HTML coverage report opened in browser
coverage-html:
    cargo llvm-cov --all-features --bin kei --test cli --test state --html --open

# Generate lcov report
coverage-lcov:
    cargo llvm-cov --all-features --bin kei --test cli --test state --lcov --output-path lcov.info
    @echo "Written to lcov.info"

# Build release binary
build:
    cargo build --release

# Run the app with arguments
run *ARGS:
    cargo run -- {{ARGS}}

# Clean build artifacts
clean:
    cargo clean

# Run full codebase code review via Claude
code-review:
    cat ~/git/codereview/RUST_CODE_REVIEW_PROMPT.md | claude --verbose -p --output-format stream-json --allowedTools 'Bash(cargo*) Bash(wc*) Bash(date*) Read Write Glob Grep Edit' | format-claude-stream

# Run PR/branch code review via Claude (reviews changes vs main)
pr-review:
    cat ~/git/codereview/RUST_PR_REVIEW_PROMPT.md | claude --verbose -p --output-format stream-json --allowedTools 'Bash(cargo*) Bash(git*) Bash(wc*) Bash(date*) Bash(xargs*) Read Write Glob Grep Edit' | format-claude-stream
