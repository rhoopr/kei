#!/usr/bin/env bash
# Phase 2.5 - Docker entrypoint PUID/PGID smoke.
#
# This is offline. It verifies the NAS-facing entrypoint behavior without
# touching iCloud: numeric PUID/PGID drop, volume chown, default root mode,
# and clear rejection for invalid env combinations.

set -euo pipefail

repo_root=$(git rev-parse --show-toplevel 2>/dev/null || pwd)
cd "$repo_root"

image="${KEI_DOCKER_IMAGE:-kei:dev}"
work="${TMPDIR:-/tmp/codex/kei/full-test/tmp}/docker-puid-smoke"
rm -rf "$work"
mkdir -p "$work/config" "$work/photos" "$work/sub-config" "$work/sub-photos"

test_puid="${KEI_DOCKER_TEST_PUID:-4321}"
test_pgid="${KEI_DOCKER_TEST_PGID:-4322}"

cleanup() {
    docker run --rm \
        -v "$work/config:/c" \
        -v "$work/photos:/p" \
        -v "$work/sub-config:/sc" \
        -v "$work/sub-photos:/sp" \
        "$image" \
        chown -R "$(id -u):$(id -g)" /c /p /sc /sp >/dev/null 2>&1 || true
    rm -rf "$work" 2>/dev/null || true
}
trap cleanup EXIT

echo "--- UID-only, GID-only, mixed and symlink ownership repair ---"
docker run --rm -v "$work/config:/config" -v "$work/photos:/photos" "$image" sh -ec '
    mkdir -p /config/nested /photos/nested
    touch "/config/nested/uid only" "/photos/nested/gid only" /photos/nested/mixed
    chown "0:$2" "/config/nested/uid only"
    chown "$1:0" "/photos/nested/gid only"
    chown 0:0 /photos/nested/mixed
    ln -s /etc/passwd /photos/nested/outside
' sh "$test_puid" "$test_pgid"

puid_run() {
    docker run --rm -e PUID="$test_puid" -e PGID="$test_pgid" \
        -v "$work/config:/config" -v "$work/photos:/photos" "$image" "$@"
}
puid_run sh -ec '
    test "$(id -u):$(id -g)" = "$1:$2"
    for path in /config /config/nested "/config/nested/uid only" /photos /photos/nested "/photos/nested/gid only" /photos/nested/mixed /photos/nested/outside; do
        test "$(stat -c %u:%g "$path")" = "$1:$2"
    done
    test "$(stat -Lc %u:%g /photos/nested/outside)" = 0:0
    touch /config/created /photos/created
    test "$(stat -c %u:%g /photos/created)" = "$1:$2"
' sh "$test_puid" "$test_pgid"
puid_run sh -ec 'test "$(id -u):$(id -g)" = "$1:$2"' sh "$test_puid" "$test_pgid"

echo "--- photo repair opt-out leaves all photo ownership intact ---"
docker run --rm -v "$work/photos:/photos" "$image" chown -hR 0:0 /photos
docker run --rm -e PUID="$test_puid" -e PGID="$test_pgid" -e KEI_CHOWN_PHOTOS=0 \
    -v "$work/config:/config" -v "$work/photos:/photos" "$image" sh -ec '
    test "$(id -u):$(id -g)" = "$1:$2"
    test "$(stat -c %u:%g /config)" = "$1:$2"
    test "$(stat -c %u:%g /photos)" = 0:0
    test "$(stat -c %u:%g /photos/nested/mixed)" = 0:0
' sh "$test_puid" "$test_pgid"

echo "--- read-only repair warns and still drops privileges ---"
readonly_out=$(docker run --rm -e PUID="$test_puid" -e PGID="$test_pgid" \
    -v "$work/photos:/photos:ro" "$image" id -u 2>&1)
echo "$readonly_out" | grep -q 'warning: chown /photos failed'
test "${readonly_out##*$'\n'}" = "$test_puid"

echo "--- explicit numeric root UID and non-root GID ---"
docker run --rm -e PUID=0 -e PGID="$test_pgid" "$image" sh -ec '
    test "$(id -u):$(id -g)" = "0:$1"
' sh "$test_pgid"

echo "--- allocator env default ---"
arena_out=$(docker run --rm \
    "$image" sh -c 'printf "%s" "${MALLOC_ARENA_MAX:-}"' 2>&1)
if [[ "$arena_out" != "2" ]]; then
    echo "run_docker_puid_smoke: expected MALLOC_ARENA_MAX=2, got '$arena_out'" >&2
    exit 1
fi

echo "--- default root mode ---"
root_out=$(docker run --rm \
    --entrypoint /usr/local/bin/entrypoint.sh \
    "$image" id -u 2>&1)
if [[ "$root_out" != "0" ]]; then
    echo "run_docker_puid_smoke: expected default uid 0, got $root_out" >&2
    exit 1
fi

echo "--- invalid PUID rejected ---"
bad_out=$(docker run --rm \
    -e PUID=notanumber \
    -e PGID="$test_pgid" \
    --entrypoint /usr/local/bin/entrypoint.sh \
    "$image" id 2>&1 || true)
printf '%s\n' "$bad_out"
echo "$bad_out" | grep -q "PUID/PGID must be numeric"

echo "--- invalid PGID and photo policy rejected ---"
for assignment in PGID=notanumber KEI_CHOWN_PHOTOS=invalid; do
    invalid_out=$(docker run --rm -e PUID="$test_puid" -e PGID="$test_pgid" -e "$assignment" "$image" id 2>&1 || true)
    echo "$invalid_out" | grep -Eq 'must be numeric|must be 0 or 1'
done

echo "--- partial PUID/PGID rejected ---"
partial_out=$(docker run --rm \
    -e PUID="$test_puid" \
    --entrypoint /usr/local/bin/entrypoint.sh \
    "$image" id 2>&1 || true)
printf '%s\n' "$partial_out"
echo "$partial_out" | grep -q "must be set together"

pgid_only_out=$(docker run --rm -e PGID="$test_pgid" "$image" id 2>&1 || true)
echo "$pgid_only_out" | grep -q "must be set together"

echo "--- v0.20 Docker preflight: removed env config requires /config/config.toml ---"
prefail_out=$(docker run --rm \
    -e KEI_DOWNLOAD_DIR=/legacy/photos \
    -e KEI_ALBUM="Legacy Album" \
    --entrypoint /usr/local/bin/entrypoint.sh \
    "$image" sync --dry-run 2>&1 || true)
printf '%s\n' "$prefail_out"
echo "$prefail_out" | grep -q "/config/config.toml is required for v0.20 Docker sync settings"
echo "$prefail_out" | grep -q "KEI_DOWNLOAD_DIR"
echo "$prefail_out" | grep -q "docs/v0.20-migration.md"

echo "--- v0.20 Docker preflight: --version bypasses removed env config check ---"
preflight_version_out=$(docker run --rm \
    -e KEI_DOWNLOAD_DIR=/legacy/photos \
    --entrypoint /usr/local/bin/entrypoint.sh \
    "$image" --version 2>&1)
printf '%s\n' "$preflight_version_out"
echo "$preflight_version_out" | grep -q "^kei "

echo "--- v0.20 Docker preflight: explicit non-default --config bypasses check ---"
custom_cfg_dir="$work/custom-config"
mkdir -p "$custom_cfg_dir"
cat >"$custom_cfg_dir/custom.toml" <<'TOML'
[auth]
username = "docker-preflight@example.invalid"
TOML
preflight_custom_out=$(docker run --rm \
    -e KEI_DOWNLOAD_DIR=/legacy/photos \
    -v "$custom_cfg_dir:/tmp/cfg" \
    --entrypoint /usr/local/bin/entrypoint.sh \
    "$image" config show --config /tmp/cfg/custom.toml 2>&1)
printf '%s\n' "$preflight_custom_out"
echo "$preflight_custom_out" | grep -q "docker-preflight@example.invalid"

echo "--- kei subcommand under dropped uid ---"
sub_out=$(docker run --rm \
    -e ICLOUD_USERNAME=docker-puid@example.invalid \
    -e KEI_DATA_DIR=/config \
    -e PUID="$test_puid" \
    -e PGID="$test_pgid" \
    -v "$work/sub-config:/config" \
    -v "$work/sub-photos:/photos" \
    "$image" status --downloaded 2>&1)
printf '%s\n' "$sub_out" | tail -5
echo "$sub_out" | grep -q "No state database found"

sub_owner=$(stat -c %u "$work/sub-config" 2>/dev/null || echo "")
if [[ "$sub_owner" != "$test_puid" ]]; then
    echo "run_docker_puid_smoke: expected /config owner $test_puid, got $sub_owner" >&2
    exit 1
fi

echo "docker PUID smoke passed"
