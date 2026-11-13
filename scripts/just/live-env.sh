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

# Reject invalid live bounds before Cargo builds or the first lookup authenticates.
PROJECT_DIR="${PROJECT_DIR:-$PWD}"
# shellcheck disable=SC1091
source "$PROJECT_DIR/tests/shell/lib.sh"
kei_live_fixed_date >/dev/null || return 1
