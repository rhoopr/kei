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

# Run tests (excludes auth-dependent tests)
test:
    cargo test --all-features --test cli --test state

# Run all tests including auth-dependent (requires .env credentials)
test-all:
    cargo test --all-features

# Generate coverage report (text summary)
coverage:
    cargo llvm-cov --all-features --test cli --test state

# Generate coverage with threshold enforcement
coverage-check:
    cargo llvm-cov --all-features --test cli --test state --json --output-path coverage.json
    scripts/check-coverage.sh coverage.json
    @rm -f coverage.json

# Generate HTML coverage report and open in browser
coverage-html:
    cargo llvm-cov --all-features --test cli --test state --html --open

# Generate lcov report
coverage-lcov:
    cargo llvm-cov --all-features --test cli --test state --lcov --output-path lcov.info
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
