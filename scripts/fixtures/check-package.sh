#!/usr/bin/env bash
# Exercise checked-in media from a real extracted Cargo source package.
set -euo pipefail

repo_root=$(git rev-parse --show-toplevel)
cd "$repo_root"
target_dir=$(scripts/full-test/cargo_target_dir.sh)
version=$(awk -F'"' '/^version = "/ { print $2; exit }' Cargo.toml)
work=$(mktemp -d "${TMPDIR:-/tmp}/kei-fixture-package-XXXXXX")
trap 'rm -rf "$work"' EXIT

# Cargo omits the nested fuzz package. Its shared regression inputs have
# package-local copies so library tests compile from the extracted crate.
for seed in replacement-fits replacement-grows multi-xmp conflicting-tmap-xmp; do
    cmp "fuzz/seeds/heif_rewrite/$seed" "tests/data/heif-rewrite/$seed"
done

# Repackaging can retain trailing bytes from an older, larger archive.
# Generate the source archive in fresh scratch space, retaining build caches.
cargo package --target-dir "$work/package-target" --allow-dirty --no-verify --offline
tar -xzf "$work/package-target/package/kei-$version.crate" -C "$work"
cd "$work/kei-$version"
export CARGO_TARGET_DIR="$target_dir/fixture-package"
# Retain dependency builds, but rebuild kei with this extraction's absolute
# CARGO_MANIFEST_DIR instead of reusing a test binary from a removed directory.
cargo clean --offline --release --package kei

for feature in --all-features --no-default-features; do
    cargo test --offline --release "$feature" --test media_fixtures
    cargo test --offline --release "$feature" --lib bundled_ -- --test-threads=1
done
