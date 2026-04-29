#!/usr/bin/env bash
# Fail when this branch adds/changes a serializer in src/ without a
# corresponding round-trip test edit. Catches `parse(serialize(x))`-shape
# bugs that pass per-side review by demanding the property test exist.
#
# Triggers on added lines (`^+`, not `^+++`) matching any of:
#   fn to_raw / fn to_string / fn serialize / fn to_toml / fn to_json
#   impl <...> Serialize for
#
# Looks for a corresponding signal in test diffs:
#   roundtrip / round_trip / serialize.*parse / parse.*serialize /
#   to_raw.*from_raw / from_raw.*to_raw
#
# Override: KEI_SKIP_ROUNDTRIP_GATE=1 (use only with a written justification).

set -euo pipefail

BASE_REF="${KEI_ROUNDTRIP_BASE:-origin/main}"

if [ "${KEI_SKIP_ROUNDTRIP_GATE:-0}" = "1" ]; then
    echo "roundtrip-gate: skipped via KEI_SKIP_ROUNDTRIP_GATE=1" >&2
    exit 0
fi

if ! git rev-parse --verify "$BASE_REF" >/dev/null 2>&1; then
    # CI environments always have origin/main fetched; a missing base ref
    # there is a real configuration bug and we fail loudly. Locally, we
    # warn and skip so a fresh checkout doesn't hard-block on `just gate`
    # before the user has had a chance to run `git fetch origin main`.
    if [ "${CI:-}" = "true" ] || [ -n "${GITHUB_ACTIONS:-}" ]; then
        echo "roundtrip-gate: base ref '$BASE_REF' not found in CI; check checkout config" >&2
        exit 1
    fi
    echo "roundtrip-gate: WARNING base ref '$BASE_REF' not found locally; skipping detector. Run \`git fetch origin main\` to enable the gate." >&2
    exit 0
fi

MERGE_BASE=$(git merge-base "$BASE_REF" HEAD)

# Diff body, added lines only, excluding the +++ header.
ADDED_LINES=$(git diff "$MERGE_BASE...HEAD" -- ':(glob)src/**/*.rs' | grep -E '^\+[^+]' || true)

SERIALIZER_HITS=$(printf '%s\n' "$ADDED_LINES" | grep -E '\bfn (to_raw|to_string|serialize|to_toml|to_json)\b|\bimpl[^{]*\bSerialize\b' || true)

if [ -z "$SERIALIZER_HITS" ]; then
    exit 0
fi

# Test diff (tests/ tree + any *.rs in src/ since #[cfg(test)] modules live there).
TEST_DIFF=$(git diff "$MERGE_BASE...HEAD" -- ':(glob)tests/**/*.rs' ':(glob)src/**/*.rs' | grep -E '^\+[^+]' || true)

ROUNDTRIP_SIGNAL=$(printf '%s\n' "$TEST_DIFF" | grep -E 'roundtrip|round_trip|serialize.*parse|parse.*serialize|to_raw.*from_raw|from_raw.*to_raw' || true)

if [ -n "$ROUNDTRIP_SIGNAL" ]; then
    exit 0
fi

cat >&2 <<EOF
roundtrip-gate: serializer change detected without a round-trip test edit.

Changed serializers (diff vs $MERGE_BASE):
$(printf '%s\n' "$SERIALIZER_HITS" | sed 's/^/  /')

Add a test that exercises \`parse(serialize(x)) == x\` (or equivalent
inverse) for the type whose serializer changed. Look for an existing
\`*_roundtrip\` / \`round_trip\` test in tests/ or src/ as a pattern.

If this serializer truly has no inverse and no round-trip property,
add the test name with a short comment explaining why
(\`fn no_roundtrip_*\`) so the gate sees the signal and the next
reviewer sees the rationale.

To bypass for an emergency hotfix: KEI_SKIP_ROUNDTRIP_GATE=1 just gate
EOF

exit 1
