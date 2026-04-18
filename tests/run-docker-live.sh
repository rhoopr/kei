#!/bin/bash
# Docker live integration tests.
#
# Tests actual sync inside the Docker container with real iCloud credentials.
# See tests/lib.sh for the ICLOUD_* / KEI_TEST_* / KEI_DOCKER_IMAGE env vars.
#
# Usage: ./tests/run-docker-live.sh
# Override image: KEI_DOCKER_IMAGE=kei:v1.0.0 ./tests/run-docker-live.sh

set -o pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"
# shellcheck disable=SC1091
source "$SCRIPT_DIR/lib.sh"
kei_require_env

IMAGE="$(kei_docker_image)"
COOKIES="$(kei_cookie_dir)"
USER_SLUG="$(kei_user_slug)"
ALBUM="$(kei_album)"

PASS=0
FAIL=0

check() {
    local label="$1"
    local result="$2"
    if [ "$result" -eq 0 ]; then
        echo "  PASS: $label"
        PASS=$((PASS + 1))
    else
        echo "  FAIL: $label"
        FAIL=$((FAIL + 1))
    fi
}

echo "=== Docker Live Integration Tests ==="
echo "Image:    $IMAGE"
echo "Username: $ICLOUD_USERNAME"
echo ""

# ── Setup: copy all session files ─────────────────────────────────────
DOCKER_CONFIG=$(mktemp -d "${TMPDIR:-/tmp}/kei-docker-config-XXXXX")
DOCKER_PHOTOS=$(mktemp -d "${TMPDIR:-/tmp}/kei-docker-photos-XXXXX")
trap "rm -rf '$DOCKER_CONFIG' '$DOCKER_PHOTOS'" EXIT

# Copy ALL files including dotfiles (.session, .lock)
cp "$COOKIES/"* "$DOCKER_CONFIG/" 2>/dev/null
cp "$COOKIES/".* "$DOCKER_CONFIG/" 2>/dev/null
# Remove lock files so Docker doesn't conflict
rm -f "$DOCKER_CONFIG/"*.lock "$DOCKER_CONFIG/.lock"

echo "--- Test 1: Docker sync ($ALBUM album) ---"
docker run --rm \
    -v "$DOCKER_CONFIG:/config" \
    -v "$DOCKER_PHOTOS:/photos" \
    "$IMAGE" sync \
        --username "$ICLOUD_USERNAME" \
        --password "$ICLOUD_PASSWORD" \
        --data-dir /config \
        --directory /photos \
        --album "$ALBUM" \
        --no-progress-bar \
        --no-incremental \
    2>&1
EC=$?
check "sync exits successfully (exit $EC)" "$([ "$EC" -eq 0 ]; echo $?)"

echo ""
echo "--- Test 2: Files downloaded ---"
FILE_COUNT=$(find "$DOCKER_PHOTOS" -type f 2>/dev/null | wc -l | tr -d ' ')
echo "  Files: $FILE_COUNT"
find "$DOCKER_PHOTOS" -type f 2>/dev/null | sort | while read -r f; do
    SIZE=$(stat -f%z "$f" 2>/dev/null || stat -c%s "$f" 2>/dev/null)
    echo "    $f ($SIZE bytes)"
done
check "at least 1 file downloaded" "$([ "$FILE_COUNT" -ge 1 ]; echo $?)"

echo ""
echo "--- Test 3: All files non-empty ---"
EMPTY=0
for f in $(find "$DOCKER_PHOTOS" -type f 2>/dev/null); do
    SIZE=$(stat -f%z "$f" 2>/dev/null || stat -c%s "$f" 2>/dev/null)
    [ "$SIZE" -eq 0 ] && EMPTY=$((EMPTY + 1))
done
check "no empty files (found $EMPTY empty)" "$([ "$EMPTY" -eq 0 ]; echo $?)"

echo ""
echo "--- Test 4: health.json ---"
if [ -f "$DOCKER_CONFIG/health.json" ]; then
    cat "$DOCKER_CONFIG/health.json"
    echo ""
    CF=$(python3 -c "import json; d=json.load(open('$DOCKER_CONFIG/health.json')); print(d.get('consecutive_failures', -1))" 2>/dev/null)
    check "health.json consecutive_failures == 0" "$([ "$CF" = "0" ]; echo $?)"
else
    check "health.json exists" 1
fi

echo ""
echo "--- Test 5: State database ---"
if [ -f "$DOCKER_CONFIG/${USER_SLUG}.db" ]; then
    ASSET_COUNT=$(sqlite3 "$DOCKER_CONFIG/${USER_SLUG}.db" "SELECT COUNT(*) FROM assets WHERE status='downloaded'" 2>/dev/null)
    echo "  Downloaded assets in DB: $ASSET_COUNT"
    check "state DB has downloaded assets" "$([ "$ASSET_COUNT" -ge 1 ]; echo $?)"
else
    check "state database exists" 1
fi

echo ""
echo "--- Test 6: Idempotent re-sync (no new downloads) ---"
docker run --rm \
    -v "$DOCKER_CONFIG:/config" \
    -v "$DOCKER_PHOTOS:/photos" \
    "$IMAGE" sync \
        --username "$ICLOUD_USERNAME" \
        --password "$ICLOUD_PASSWORD" \
        --data-dir /config \
        --directory /photos \
        --album "$ALBUM" \
        --no-progress-bar \
        --log-level info \
    2>&1 | tee /dev/stderr | grep -qE "downloaded=0|No new photos"
EC=$?
check "re-sync downloads 0 files" "$EC"

echo ""
echo "--- Test 7: Dry run ---"
DRY_PHOTOS=$(mktemp -d "${TMPDIR:-/tmp}/kei-docker-dry-XXXXX")
docker run --rm \
    -v "$DOCKER_CONFIG:/config" \
    -v "$DRY_PHOTOS:/photos" \
    "$IMAGE" sync \
        --username "$ICLOUD_USERNAME" \
        --password "$ICLOUD_PASSWORD" \
        --data-dir /config \
        --directory /photos \
        --album "$ALBUM" \
        --no-progress-bar \
        --dry-run \
    2>&1
DRY_COUNT=$(find "$DRY_PHOTOS" -type f 2>/dev/null | wc -l | tr -d ' ')
check "dry run writes 0 files (got $DRY_COUNT)" "$([ "$DRY_COUNT" -eq 0 ]; echo $?)"
rm -rf "$DRY_PHOTOS"

echo ""
echo "--- Test 8: Password backend in container ---"
BACKEND=$(docker run --rm \
    -v "$DOCKER_CONFIG:/config" \
    "$IMAGE" password --username "$ICLOUD_USERNAME" --data-dir /config backend 2>&1)
echo "  Backend: $BACKEND"
check "credential backend reports a value" "$([ -n "$BACKEND" ]; echo $?)"

echo ""
echo "--- Test 9: List albums in container ---"
docker run --rm \
    -v "$DOCKER_CONFIG:/config" \
    "$IMAGE" list albums \
        --username "$ICLOUD_USERNAME" \
        --data-dir /config \
    2>&1 | grep -qF "$ALBUM"
check "list-albums shows $ALBUM album" "$?"

echo ""
echo "--- Test 10: Watch mode cycles + graceful SIGTERM ---"
WATCH_PHOTOS=$(mktemp -d "${TMPDIR:-/tmp}/kei-docker-watch-XXXXX")
WATCH_NAME="kei-docker-watch-$$"
# Start detached. --watch-with-interval 60 drives at least 2 cycles in ~130s.
docker run -d --name "$WATCH_NAME" \
    -v "$DOCKER_CONFIG:/config" \
    -v "$WATCH_PHOTOS:/photos" \
    "$IMAGE" sync \
        --username "$ICLOUD_USERNAME" \
        --password "$ICLOUD_PASSWORD" \
        --data-dir /config \
        --directory /photos \
        --album "$ALBUM" \
        --no-progress-bar \
        --watch-with-interval 60 \
        --log-level info >/dev/null

# Wait past first cycle + into second cycle's wait.
sleep 130
# SIGTERM → wait up to 30s for graceful shutdown.
docker stop --time 30 "$WATCH_NAME" >/dev/null 2>&1
LOGS=$(docker logs "$WATCH_NAME" 2>&1)
EXIT_CODE=$(docker inspect --format '{{.State.ExitCode}}' "$WATCH_NAME" 2>/dev/null)
docker rm "$WATCH_NAME" >/dev/null 2>&1

CYCLES=$(echo "$LOGS" | grep -c "Waiting before next cycle")
echo "  Watch cycles observed: $CYCLES"
echo "  Container exit code:   $EXIT_CODE"
check "watch drove >= 2 cycles (got $CYCLES)" "$([ "$CYCLES" -ge 2 ]; echo $?)"
# 0 = normal exit, 143 = killed by SIGTERM after handler, 130 = SIGINT.
check "container exited cleanly on SIGTERM (exit $EXIT_CODE)" \
    "$(case "$EXIT_CODE" in 0|130|143) true;; *) false;; esac; echo $?)"
rm -rf "$WATCH_PHOTOS"

echo ""
echo "--- Test 11: HEALTHCHECK probe (manual) ---"
# Run the Dockerfile's healthcheck test directly. health.json was written by
# Test 1's sync, so the probe should pass immediately.
docker run --rm --entrypoint sh \
    -v "$DOCKER_CONFIG:/config" \
    "$IMAGE" -c '
      test -f /config/health.json \
      && test "$(jq -r .consecutive_failures /config/health.json)" -lt 5 \
      && echo HEALTHY
    ' 2>&1 | tee /dev/stderr | grep -q HEALTHY
check "healthcheck probe reports HEALTHY" "$?"

echo ""
echo "--- Test 12: Password-file (Docker secrets style) ---"
SECRETS_DIR=$(mktemp -d "${TMPDIR:-/tmp}/kei-docker-secrets-XXXXX")
# Mode 400 matches Docker secret convention; no trailing newline.
printf '%s' "$ICLOUD_PASSWORD" > "$SECRETS_DIR/icloud_password"
chmod 400 "$SECRETS_DIR/icloud_password"
PWFILE_PHOTOS=$(mktemp -d "${TMPDIR:-/tmp}/kei-docker-pwfile-XXXXX")
PWFILE_OUT=$(docker run --rm \
    -v "$DOCKER_CONFIG:/config" \
    -v "$PWFILE_PHOTOS:/photos" \
    -v "$SECRETS_DIR:/run/secrets:ro" \
    "$IMAGE" sync \
        --username "$ICLOUD_USERNAME" \
        --password-file /run/secrets/icloud_password \
        --data-dir /config \
        --directory /photos \
        --album "$ALBUM" \
        --no-progress-bar \
        --dry-run \
    2>&1)
echo "$PWFILE_OUT" | tail -10
echo "$PWFILE_OUT" | grep -qE "Would download|files would be downloaded"
check "password-file auth works in container" "$?"
rm -rf "$SECRETS_DIR" "$PWFILE_PHOTOS"

echo ""
echo "--- Test 13: kei status --downloaded inside container ---"
# Test 1's sync wrote downloaded rows into $DOCKER_CONFIG. The --downloaded
# flag should list them.
STATUS_OUT=$(docker run --rm \
    -v "$DOCKER_CONFIG:/config" \
    "$IMAGE" status \
        --username "$ICLOUD_USERNAME" \
        --data-dir /config \
        --downloaded \
    2>&1)
echo "$STATUS_OUT" | tail -5
echo "$STATUS_OUT" | grep -q "Downloaded assets:"
check "--downloaded listing renders inside container" "$?"

echo ""
echo "--- Test 14: kei status --pending --failed --downloaded combined ---"
# Test 1's sync produced downloaded rows, so the downloaded section must
# render. Pending/failed sections are only emitted when their counts are > 0,
# so we don't require them here - the downloaded header is the load-bearing
# check, and the full invocation must exit 0.
COMBINED_OUT=$(docker run --rm \
    -v "$DOCKER_CONFIG:/config" \
    "$IMAGE" status \
        --username "$ICLOUD_USERNAME" \
        --data-dir /config \
        --pending --failed --downloaded \
    2>&1)
COMBINED_EC=$?
echo "$COMBINED_OUT" | grep -q "Downloaded assets:"
HAS_DOWNLOADED=$?
check "--pending --failed --downloaded combined exits 0" "$COMBINED_EC"
check "combined flags render Downloaded section" "$HAS_DOWNLOADED"

echo ""
echo "==========================================="
echo "  DOCKER LIVE TEST RESULTS: $PASS pass, $FAIL fail"
echo "==========================================="

[ "$FAIL" -eq 0 ] && exit 0 || exit 1
