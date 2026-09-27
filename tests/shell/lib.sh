# shellcheck shell=bash
# Shared helpers for kei's shell-native test scripts.
#
# Source this file after setting PROJECT_DIR (usually
# `PROJECT_DIR=$(cd "$(dirname "$0")/../.." && pwd)` in the caller).
# Loads .env for credentials and exposes helpers the three shell suites
# share: release-binary resolution, session preflight, scratch-dir
# allocation, a PASS/FAIL counter, and scoped lock cleanup.
#
# Environment variables (all optional unless noted):
#   ICLOUD_USERNAME             (required) Apple ID email
#   ICLOUD_PASSWORD             (required) Apple ID password
#   ICLOUD_TEST_COOKIE_DIR      pre-authenticated session dir (default: $PROJECT_DIR/.test-cookies)
#   KEI_DOCKER_IMAGE            docker image to test (default: kei:latest)
#   KEI_TEST_SCRATCH_DIR        base dir for per-suite scratch (default: /tmp/codex/kei/shell-tests-$USER)

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
        case "$ICLOUD_TEST_COOKIE_DIR" in
            \~/*) printf '%s/%s' "$HOME" "${ICLOUD_TEST_COOKIE_DIR#\~/}" ;;
            *) printf '%s' "$ICLOUD_TEST_COOKIE_DIR" ;;
        esac
    else
        printf '%s/.test-cookies' "$PROJECT_DIR"
    fi
}

kei_copy_session_without_state() {
    local dest="${1:?destination dir required}"
    local cookies
    cookies="$(kei_cookie_dir)"
    mkdir -p "$dest"
    cp "$cookies/"* "$dest/" 2>/dev/null || true
    cp "$cookies/".* "$dest/" 2>/dev/null || true
    rm -f "$dest/"*.lock "$dest/.lock" "$dest/"*.db "$dest/health.json" 2>/dev/null || true
}

kei_db_path() {
    printf '%s/%s.db' "$(kei_cookie_dir)" "$(kei_user_slug)"
}

kei_live_filters() {
    cat "$PROJECT_DIR/tests/data/live-selection.toml"
}

# import-existing reads its scan cap from --recent, not filters.recent.
kei_live_recent() {
    awk '$1 == "recent" && $2 == "=" { print $3 }' "$PROJECT_DIR/tests/data/live-selection.toml"
}

# No download or shared-state mutation. Fail before account-dependent suites.
kei_preflight_selection() (
    local work config output count
    work=$(mktemp -d "${TMPDIR:-/tmp}/kei-live-selection-XXXXXX") || exit 1
    trap 'rm -rf "$work"' EXIT
    kei_copy_session_without_state "$work/data"
    config=$(kei_write_sync_config "$work/data" "$work/media")
    if ! output=$(KEI_DATA_DIR="$work/data" "$(kei_release_bin)" sync --config "$config" --only-print-filenames --no-progress-bar); then
        echo "ABORT: bounded primary-library selection failed" >&2
        exit 1
    fi
    count=$(printf '%s\n' "$output" | awk 'NF { count++ } END { print count+0 }')
    echo "Live selection: primary library, albums=none, unfiled=true; eligible filenames=$count" >&2
    if [ "$count" -eq 0 ]; then
        echo "ABORT: empty bounded live selection. Add at least one eligible recent asset. Selection:" >&2
        kei_live_filters >&2
        exit 1
    fi
)

kei_toml_string() {
    local s="$1"
    s="${s//\\/\\\\}"
    s="${s//\"/\\\"}"
    printf '"%s"' "$s"
}

kei_append_toml_fragment() {
    local fragment="$1"
    if [ -z "$fragment" ]; then
        return
    fi
    printf '%s' "$fragment"
    case "$fragment" in
        *$'\n') ;;
        *) echo ;;
    esac
}

kei_write_sync_config() {
    local data_dir="${1:?data dir required}"
    local download_dir="${2:?download dir required}"
    local download_extra="${KEI_SYNC_DOWNLOAD_TOML:-}"
    local filters_extra="${KEI_SYNC_FILTERS_TOML:-}"
    local photos_extra="${KEI_SYNC_PHOTOS_TOML:-}"
    local config="$data_dir/.kei-shell-sync-$$.toml"
    mkdir -p "$data_dir"
    {
        echo "[download]"
        printf 'directory = %s\n' "$(kei_toml_string "$download_dir")"
        kei_append_toml_fragment "$download_extra"
        kei_live_filters
        kei_append_toml_fragment "$filters_extra"
        if [ -n "$photos_extra" ]; then
            echo "[photos]"
            kei_append_toml_fragment "$photos_extra"
        fi
    } >"$config"
    printf '%s' "$config"
}

kei_docker_image() {
    printf '%s' "${KEI_DOCKER_IMAGE:-kei:latest}"
}

# Base dir for per-suite scratch output. Under /tmp/codex so the repo checkout
# stays clean and parallel suite invocations don't collide through repo-local
# paths.
kei_scratch_base() {
    if [ -n "${KEI_TEST_SCRATCH_DIR:-}" ]; then
        printf '%s' "$KEI_TEST_SCRATCH_DIR"
    else
        printf '/tmp/codex/kei/shell-tests-%s' "${USER:-$(id -un)}"
    fi
}

# Allocate a suite-specific scratch directory. Usage:
#   DIR=$(kei_scratch_dir concurrency/resume)
#   # ... use $DIR ...
#   rm -rf "$DIR"
kei_scratch_dir() {
    local suite="${1:?kei_scratch_dir: suite name required}"
    local dir
    dir="$(kei_scratch_base)/$suite-$$"
    mkdir -p "$dir"
    printf '%s' "$dir"
}

kei_release_bin() {
    printf '%s/release/kei' "$("$PROJECT_DIR/scripts/full-test/cargo_target_dir.sh")"
}

# Build if missing or older than Cargo.toml/Cargo.lock. The stat short-
# circuit skips cargo's own freshness check (a warm but noop cargo call
# is ~300-500ms) on the common up-to-date path.
kei_require_release_binary() {
    local bin
    bin="$(kei_release_bin)"
    local needs_build=0
    if [ ! -x "$bin" ]; then
        needs_build=1
    elif [ "$PROJECT_DIR/Cargo.toml" -nt "$bin" ] || [ "$PROJECT_DIR/Cargo.lock" -nt "$bin" ]; then
        needs_build=1
    fi
    if [ "$needs_build" -eq 1 ]; then
        echo "Building release binary (required by shell suites)..."
        (cd "$PROJECT_DIR" && cargo build --release) || {
            echo "ABORT: cargo build --release failed"
            exit 1
        }
    fi
}

# Validate the pre-authenticated session. Increments the PASS counter on
# success so callers don't reach into `_KEI_PASS`; exits the script on a
# bad session so we don't cascade 12 subsequent sync failures.
kei_preflight_session() {
    local bin cookies out
    bin="$(kei_release_bin)"
    cookies="$(kei_cookie_dir)"
    out=$(KEI_DATA_DIR="$cookies" "$bin" login \
        --password "$ICLOUD_PASSWORD" 2>&1)
    if echo "$out" | grep -q "Authentication completed\|Session OK\|already authenticated"; then
        kei_check "session valid" 0
        kei_preflight_selection || exit 1
        return 0
    fi
    echo "  ABORT: session invalid or rate-limited"
    echo "$out" | tail -3
    echo "  Re-authenticate: KEI_DATA_DIR=$cookies cargo run --release -- login"
    exit 1
}

# sqlite3 against the state DB. `kei_db_query` returns the first column
# of each row on stdout; `kei_db_exec` runs a mutating statement and
# discards output. Both suppress the "unable to open" error that fires
# before the first sync has created the DB.
kei_sql_string() {
    local value="$1"
    value="${value//\'/\'\'}"
    printf "'%s'" "$value"
}

kei_db_query() {
    sqlite3 "$(kei_db_path)" "$1" 2>/dev/null
}

kei_db_exec() {
    sqlite3 "$(kei_db_path)" "$1" 2>/dev/null
}

kei_check_init() {
    _KEI_PASS=0
    _KEI_FAIL=0
    _KEI_SKIP=0
}

# Usage: kei_check "<label>" [<result>]
# Result defaults to $? so `foo && bar; kei_check "baz"` works. Capturing
# $? must be the FIRST statement in the function: any prior assignment
# (e.g. `local label="$1"`) succeeds and clobbers $? to 0, which would
# silently turn every implicit-$? assertion into PASS.
kei_check() {
    local result=$?
    local label="$1"
    if [ "$#" -ge 2 ]; then
        result="$2"
    fi
    if [ "$result" -eq 0 ]; then
        echo "  PASS: $label"
        _KEI_PASS=$((_KEI_PASS + 1))
    else
        echo "  FAIL: $label"
        _KEI_FAIL=$((_KEI_FAIL + 1))
    fi
}

kei_skip() {
    echo "  SKIP: $1"
    _KEI_SKIP=$((_KEI_SKIP + 1))
}

# Usage: kei_check_summary "<title>"; exit $?
kei_check_summary() {
    local title="${1:-RESULTS}"
    echo ""
    echo "=================================================="
    if [ "$_KEI_SKIP" -gt 0 ]; then
        echo "  $title: $_KEI_PASS pass, $_KEI_FAIL fail, $_KEI_SKIP skip"
    else
        echo "  $title: $_KEI_PASS pass, $_KEI_FAIL fail"
    fi
    echo "=================================================="
    [ "$_KEI_FAIL" -eq 0 ]
}

# Format elapsed time as `Xm YYs`. Usage: start=$(date +%s); ...; kei_elapsed "$start"
kei_elapsed() {
    local start="$1"
    local now
    now=$(date +%s)
    local delta=$((now - start))
    printf '%dm %02ds' $((delta / 60)) $((delta % 60))
}

# Remove the current user's lock only, not every lock in the cookie
# dir - other kei processes may share the dir.
kei_clear_stale_lock() {
    local cookies slug
    cookies="$(kei_cookie_dir)"
    slug="$(kei_user_slug)"
    rm -f "$cookies/$slug.lock"
}

# Install an EXIT trap that removes this PID's scratch dirs. Crashed
# runs would otherwise accumulate `<base>/<suite>-<pid>` directories
# under /tmp/codex across a day of development.
kei_install_scratch_cleanup() {
    local base
    base="$(kei_scratch_base)"
    # shellcheck disable=SC2064
    trap "rm -rf \"$base\"/*-$$" EXIT
}

kei_suite_banner() {
    local title="${1:?kei_suite_banner: title required}"
    echo "=================================================="
    echo "  $title"
    echo "  $(date '+%Y-%m-%d %H:%M:%S')"
    echo "=================================================="
}
