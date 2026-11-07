#!/bin/sh
# kei notification script: ping a Synology NAS after a sync cycle.
#
# Wire into kei via [notifications].script in config.toml.
# See the Synology wiki page for setup details.
#
# Two things to know before relying on this:
#
# 1. Synology Photos auto-indexes via inotify within a few seconds of a
#    file landing. For most users, no explicit reindex is needed and
#    this script is purely informational (a DSM notification when a
#    sync cycle finishes).
#
# 2. There IS a Synology Photos reindex API but the exact endpoint
#    varies across DSM versions and is not officially documented for
#    third-party use. If you need explicit reindex (e.g. inotify isn't
#    keeping up), the safest path is SSH from the host to the NAS:
#        ssh admin@nas synoindex -R /volume1/photo/kei
#    Drop that into the "explicit reindex" section below if you've set
#    up key-based SSH from your kei host to the NAS.
#
# Required environment (set via your compose file or .env):
#   SYNOLOGY_URL         Base URL of your DSM (e.g. https://nas.local:5001)
#   SYNOLOGY_USERNAME    DSM user with notification permissions
#   SYNOLOGY_PASSWORD    Password (prefer a Docker secret over a plain env var)
#
# Optional:
#   SYNOLOGY_INSECURE=1  Pass -k to curl (skip TLS verify; LAN only)

set -e

# Only fire on completed cycles. 2fa_required, sync_failed, etc. fall
# through; copy and adapt this script if you want to handle those too.
if [ "$KEI_EVENT" != "sync_complete" ]; then
    exit 0
fi

# Skip the notification if nothing changed.
if [ "${KEI_DOWNLOADED:-0}" = "0" ]; then
    exit 0
fi

: "${SYNOLOGY_URL:?SYNOLOGY_URL must be set}"
: "${SYNOLOGY_USERNAME:?SYNOLOGY_USERNAME must be set}"
: "${SYNOLOGY_PASSWORD:?SYNOLOGY_PASSWORD must be set}"

# Use `set --` to build curl opts as positional params; unquoted string
# expansion (CURL_OPTS=...; curl $CURL_OPTS) word-splits incorrectly on
# values containing spaces (shellcheck SC2086).
set -- -s --max-time 30
if [ "${SYNOLOGY_INSECURE:-0}" = "1" ]; then
    set -- "$@" -k
fi

# 1. Login -> session id (DSM Auth API is stable since DSM 6).
LOGIN=$(curl "$@" -G "$SYNOLOGY_URL/webapi/entry.cgi" \
    --data-urlencode "api=SYNO.API.Auth" \
    --data-urlencode "version=6" \
    --data-urlencode "method=login" \
    --data-urlencode "account=$SYNOLOGY_USERNAME" \
    --data-urlencode "passwd=$SYNOLOGY_PASSWORD" \
    --data-urlencode "session=DSM" \
    --data-urlencode "format=sid")

# Extract the sid via grep -o so an unrelated earlier "sid" field in an
# error envelope can't match. The first "sid":"..." occurrence wins.
SID=$(printf '%s' "$LOGIN" | grep -o '"sid":"[^"]*"' | head -1 | sed 's/.*"sid":"\([^"]*\)"/\1/')

if [ -z "$SID" ]; then
    echo "kei-notify-synology: login failed: $LOGIN" >&2
    exit 1
fi

# 2. Send a DSM notification via the Notification.Mail send API. This
# is documented and stable; reuses whatever notification channel the
# user has configured in DSM (email, push, etc).
MSG="kei sync complete: ${KEI_DOWNLOADED} downloaded, ${KEI_FAILED:-0} failed, ${KEI_SKIPPED:-0} skipped"

curl "$@" "$SYNOLOGY_URL/webapi/entry.cgi" \
    --data-urlencode "api=SYNO.Core.Notification.Mail" \
    --data-urlencode "version=1" \
    --data-urlencode "method=send" \
    --data-urlencode "subject=kei sync" \
    --data-urlencode "message=$MSG" \
    --data-urlencode "_sid=$SID" >/dev/null ||
    echo "kei-notify-synology: notification send failed (continuing)" >&2

# 3. Optional: explicit reindex via the documented `synoindex` CLI on
# the host. Requires SSH key-based access from the kei container to the
# NAS. Uncomment and set SYNOLOGY_SSH_TARGET / SYNOLOGY_REINDEX_PATH if
# you need it.
#
# if [ -n "${SYNOLOGY_SSH_TARGET:-}" ] && [ -n "${SYNOLOGY_REINDEX_PATH:-}" ]; then
#     ssh -o StrictHostKeyChecking=accept-new "$SYNOLOGY_SSH_TARGET" \
#         synoindex -R "$SYNOLOGY_REINDEX_PATH" || \
#             echo "kei-notify-synology: synoindex failed (continuing)" >&2
# fi

# 4. Logout (best-effort).
curl "$@" -G "$SYNOLOGY_URL/webapi/entry.cgi" \
    --data-urlencode "api=SYNO.API.Auth" \
    --data-urlencode "version=6" \
    --data-urlencode "method=logout" \
    --data-urlencode "session=DSM" \
    --data-urlencode "_sid=$SID" >/dev/null 2>&1 || true

echo "kei-notify-synology: notified DSM ($MSG)"
