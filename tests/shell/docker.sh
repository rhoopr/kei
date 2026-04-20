#!/bin/bash
# Docker live integration tests.
#
# Exercises actual sync inside the kei container against live iCloud:
# volume mounts, healthcheck, watch-mode cycles + graceful SIGTERM,
# password-file auth, status flags inside the container.
#
# Requires KEI_DOCKER_IMAGE (default kei:latest) to be built locally
# (`just docker build`) or pulled.
#
# Usage: ./tests/shell/docker.sh

set -o pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/../.." && pwd)"
# shellcheck disable=SC1091
source "$SCRIPT_DIR/lib.sh"

kei_require_env

IMAGE="$(kei_docker_image)"
COOKIES="$(kei_cookie_dir)"
USER_SLUG="$(kei_user_slug)"
ALBUM="$(kei_album)"
kei_check_init

kei_suite_banner "DOCKER LIVE INTEGRATION"
echo "Image:    $IMAGE"
echo "Username: $ICLOUD_USERNAME"
echo ""

# ── Setup: copy the pre-authenticated session into a fresh config dir ───
DOCKER_CONFIG=$(mktemp -d "${TMPDIR:-/tmp}/kei-docker-config-XXXXX")
DOCKER_PHOTOS=$(mktemp -d "${TMPDIR:-/tmp}/kei-docker-photos-XXXXX")
trap "rm -rf '$DOCKER_CONFIG' '$DOCKER_PHOTOS'" EXIT

cp "$COOKIES/"* "$DOCKER_CONFIG/" 2>/dev/null
cp "$COOKIES/".* "$DOCKER_CONFIG/" 2>/dev/null
# Strip lock files so the container doesn't conflict with the host kei.
rm -f "$DOCKER_CONFIG/"*.lock "$DOCKER_CONFIG/.lock"

echo "--- 1. Docker sync ($ALBUM album) ---"
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
kei_check "sync exits successfully"

echo ""
echo "--- 2. Files downloaded ---"
FILE_COUNT=$(find "$DOCKER_PHOTOS" -type f 2>/dev/null | wc -l | tr -d ' ')
echo "  Files: $FILE_COUNT"
find "$DOCKER_PHOTOS" -type f 2>/dev/null | sort | while read -r f; do
    size=$(stat -f%z "$f" 2>/dev/null || stat -c%s "$f" 2>/dev/null)
    echo "    $f ($size bytes)"
done
[ "$FILE_COUNT" -ge 1 ]; kei_check "at least 1 file downloaded"

echo ""
echo "--- 3. All files non-empty ---"
EMPTY=0
while IFS= read -r -d '' f; do
    size=$(stat -f%z "$f" 2>/dev/null || stat -c%s "$f" 2>/dev/null)
    [ "$size" -eq 0 ] && EMPTY=$((EMPTY + 1))
done < <(find "$DOCKER_PHOTOS" -type f -print0 2>/dev/null)
[ "$EMPTY" -eq 0 ]; kei_check "no empty files (found $EMPTY empty)"

echo ""
echo "--- 4. health.json ---"
if [ -f "$DOCKER_CONFIG/health.json" ]; then
    cat "$DOCKER_CONFIG/health.json"
    echo ""
    CF=$(python3 -c "import json; d=json.load(open('$DOCKER_CONFIG/health.json')); print(d.get('consecutive_failures', -1))" 2>/dev/null)
    [ "$CF" = "0" ]; kei_check "health.json consecutive_failures == 0"
else
    kei_check "health.json exists" 1
fi

echo ""
echo "--- 5. State database ---"
if [ -f "$DOCKER_CONFIG/${USER_SLUG}.db" ]; then
    ASSET_COUNT=$(sqlite3 "$DOCKER_CONFIG/${USER_SLUG}.db" "SELECT COUNT(*) FROM assets WHERE status='downloaded'" 2>/dev/null)
    echo "  Downloaded assets in DB: $ASSET_COUNT"
    [ "$ASSET_COUNT" -ge 1 ]; kei_check "state DB has downloaded assets"
else
    kei_check "state database exists" 1
fi

echo ""
echo "--- 6. Idempotent re-sync (no new downloads) ---"
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
kei_check "re-sync downloads 0 files"

echo ""
echo "--- 7. Dry run ---"
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
[ "$DRY_COUNT" -eq 0 ]; kei_check "dry run writes 0 files (got $DRY_COUNT)"
rm -rf "$DRY_PHOTOS"

echo ""
echo "--- 8. Password backend in container ---"
BACKEND=$(docker run --rm \
    -v "$DOCKER_CONFIG:/config" \
    "$IMAGE" password --username "$ICLOUD_USERNAME" --data-dir /config backend 2>&1)
echo "  Backend: $BACKEND"
[ -n "$BACKEND" ]; kei_check "credential backend reports a value"

echo ""
echo "--- 9. List albums in container ---"
docker run --rm \
    -v "$DOCKER_CONFIG:/config" \
    "$IMAGE" list albums \
        --username "$ICLOUD_USERNAME" \
        --data-dir /config \
    2>&1 | grep -qF "$ALBUM"
kei_check "list-albums shows $ALBUM album"

echo ""
echo "--- 10. Watch mode cycles + graceful SIGTERM ---"
WATCH_PHOTOS=$(mktemp -d "${TMPDIR:-/tmp}/kei-docker-watch-XXXXX")
WATCH_NAME="kei-docker-watch-$$"
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

sleep 130
docker stop --time 30 "$WATCH_NAME" >/dev/null 2>&1
LOGS=$(docker logs "$WATCH_NAME" 2>&1)
EXIT_CODE=$(docker inspect --format '{{.State.ExitCode}}' "$WATCH_NAME" 2>/dev/null)
docker rm "$WATCH_NAME" >/dev/null 2>&1

CYCLES=$(echo "$LOGS" | grep -c "Waiting before next cycle")
echo "  Watch cycles observed: $CYCLES"
echo "  Container exit code:   $EXIT_CODE"
[ "$CYCLES" -ge 2 ]; kei_check "watch drove >= 2 cycles (got $CYCLES)"
# 0 = normal, 130 = SIGINT, 143 = SIGTERM after handler.
case "$EXIT_CODE" in 0|130|143) true;; *) false;; esac
kei_check "container exited cleanly on SIGTERM (exit $EXIT_CODE)"
rm -rf "$WATCH_PHOTOS"

echo ""
echo "--- 11. HEALTHCHECK probe ---"
docker run --rm --entrypoint sh \
    -v "$DOCKER_CONFIG:/config" \
    "$IMAGE" -c '
      test -f /config/health.json \
      && test "$(jq -r .consecutive_failures /config/health.json)" -lt 5 \
      && echo HEALTHY
    ' 2>&1 | tee /dev/stderr | grep -q HEALTHY
kei_check "healthcheck probe reports HEALTHY"

echo ""
echo "--- 12. Password-file (Docker secrets style) ---"
SECRETS_DIR=$(mktemp -d "${TMPDIR:-/tmp}/kei-docker-secrets-XXXXX")
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
kei_check "password-file auth works in container"
rm -rf "$SECRETS_DIR" "$PWFILE_PHOTOS"

echo ""
echo "--- 13. kei status --downloaded inside container ---"
STATUS_OUT=$(docker run --rm \
    -v "$DOCKER_CONFIG:/config" \
    "$IMAGE" status \
        --username "$ICLOUD_USERNAME" \
        --data-dir /config \
        --downloaded \
    2>&1)
echo "$STATUS_OUT" | tail -5
echo "$STATUS_OUT" | grep -q "Downloaded assets:"
kei_check "--downloaded listing renders inside container"

echo ""
echo "--- 14. kei status --pending --failed --downloaded combined ---"
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
kei_check "--pending --failed --downloaded combined exits 0" "$COMBINED_EC"
kei_check "combined flags render Downloaded section" "$HAS_DOWNLOADED"

kei_check_summary "DOCKER RESULTS"
