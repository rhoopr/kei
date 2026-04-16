# Shared bash helpers for kei's live-integration shell scripts.
#
# Source this file after setting SCRIPT_DIR and PROJECT_DIR. Loads .env,
# validates credentials, and exposes helpers that keep tests portable to
# any iCloud account.
#
# Environment variables (all optional unless noted):
#   ICLOUD_USERNAME             (required) Apple ID email
#   ICLOUD_PASSWORD             (required) Apple ID password
#   ICLOUD_TEST_COOKIE_DIR      path to pre-authenticated session (default: $PROJECT_DIR/.test-cookies)
#   KEI_TEST_ALBUM              name of the test album in iCloud (default: kei-test)
#   KEI_DOCKER_IMAGE            docker image to test (default: kei:latest)

: "${PROJECT_DIR:?PROJECT_DIR must be set by the caller}"

# Load .env for credentials if the caller hasn't already.
if [ -z "${ICLOUD_USERNAME:-}" ] && [ -f "$PROJECT_DIR/.env" ]; then
    # shellcheck disable=SC1091
    source "$PROJECT_DIR/.env"
fi

kei_require_env() {
    if [ -z "${ICLOUD_USERNAME:-}" ] || [ -z "${ICLOUD_PASSWORD:-}" ]; then
        echo "ABORT: ICLOUD_USERNAME and ICLOUD_PASSWORD must be set (via .env or environment)."
        exit 1
    fi
}

# Strip non-alphanumeric characters, matching kei's Session::sanitized_filename().
kei_user_slug() {
    printf '%s' "$ICLOUD_USERNAME" | tr -cd '[:alnum:]'
}

kei_cookie_dir() {
    if [ -n "${ICLOUD_TEST_COOKIE_DIR:-}" ]; then
        # Expand leading ~ manually; not all shells do it for env vars.
        case "$ICLOUD_TEST_COOKIE_DIR" in
            "~/"*) printf '%s/%s' "$HOME" "${ICLOUD_TEST_COOKIE_DIR#~/}" ;;
            *)     printf '%s' "$ICLOUD_TEST_COOKIE_DIR" ;;
        esac
    else
        printf '%s/.test-cookies' "$PROJECT_DIR"
    fi
}

kei_db_path() {
    printf '%s/%s.db' "$(kei_cookie_dir)" "$(kei_user_slug)"
}

kei_album() {
    printf '%s' "${KEI_TEST_ALBUM:-kei-test}"
}

kei_docker_image() {
    printf '%s' "${KEI_DOCKER_IMAGE:-kei:latest}"
}
