# shellcheck shell=bash
# Shared live-test environment setup for just recipes.
#
# Source this file from bash recipes that need live iCloud credentials. It
# loads .env only when ICLOUD_USERNAME is not already present so callers can
# override credentials from the environment.

if [ -z "${ICLOUD_USERNAME:-}" ] && [ -f .env ]; then
    set -a
    # shellcheck disable=SC1091
    source .env
    set +a
fi

: "${ICLOUD_USERNAME:?ICLOUD_USERNAME must be set (via .env or environment)}"
