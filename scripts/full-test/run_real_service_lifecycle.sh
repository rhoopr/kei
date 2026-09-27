#!/usr/bin/env bash
# Opt-in phase - real Linux user-service lifecycle.
#
# This mutates the operator's user service state: it writes a real user
# systemd unit, starts it, checks status, then uninstalls it. run_all.sh only
# calls this when KEI_FULL_TEST_REAL_SERVICE=1.

set -euo pipefail

if [[ "${KEI_FULL_TEST_REAL_SERVICE:-0}" != "1" ]]; then
    echo "run_real_service_lifecycle: set KEI_FULL_TEST_REAL_SERVICE=1 to run" >&2
    exit 64
fi

repo_root=$(git rev-parse --show-toplevel 2>/dev/null) || {
    echo "run_real_service_lifecycle: not in a git repo" >&2
    exit 1
}
cd "$repo_root"

if [[ "$(uname -s)" != "Linux" ]]; then
    echo "run_real_service_lifecycle: Linux systemd user service smoke only" >&2
    exit 64
fi

PROJECT_DIR="$repo_root"
# shellcheck disable=SC1091
source "$repo_root/tests/shell/lib.sh"

kei_require_env
kei_require_release_binary
kei_preflight_selection

binary=$(kei_release_bin)
cookies=$(kei_cookie_dir)
work=$(mktemp -d "${TMPDIR:-/tmp/codex/kei/full-test/tmp}/kei-real-service-XXXXX")
service_uninstalled=0
pre_linger=$(loginctl show-user "$(id -un)" -p Linger 2>/dev/null || true)

cleanup() {
    if [[ "$service_uninstalled" -eq 0 ]]; then
        "$binary" uninstall >/dev/null 2>&1 || true
    fi
    if [[ "$pre_linger" == "Linger=no" ]]; then
        loginctl disable-linger "$(id -un)" >/dev/null 2>&1 || true
    fi
    rm -rf "$work" 2>/dev/null || true
}
trap cleanup EXIT

pre_enabled=$(systemctl --user is-enabled kei.service 2>/dev/null || true)
pre_active=$(systemctl --user is-active kei.service 2>/dev/null || true)
echo "pre_enabled=$pre_enabled"
echo "pre_active=$pre_active"

if [[ "$pre_enabled" != "not-found" || "$pre_active" != "inactive" ]]; then
    echo "run_real_service_lifecycle: kei.service already exists or is active; refusing to mutate it" >&2
    exit 1
fi

data="$work/data"
photos="$work/photos"
mkdir -p "$data" "$photos"
cp "$cookies/"* "$data/" 2>/dev/null || true
cp "$cookies/".* "$data/" 2>/dev/null || true
rm -f "$data/"*.lock "$data/.lock" "$data/"*.db "$data/health.json" 2>/dev/null || true

password_file="$work/icloud_password"
printf '%s' "$ICLOUD_PASSWORD" >"$password_file"
chmod 600 "$password_file"

config="$work/config.toml"
{
    printf 'data_dir = %s\n' "$(kei_toml_string "$data")"
    echo
    echo "[auth]"
    printf 'username = %s\n' "$(kei_toml_string "$ICLOUD_USERNAME")"
    printf 'password_file = %s\n' "$(kei_toml_string "$password_file")"
    echo
    echo "[download]"
    printf 'directory = %s\n' "$(kei_toml_string "$photos")"
    echo
    kei_live_filters
    echo
    echo "[watch]"
    echo "interval = 86400"
} >"$config"

"$binary" install --user --config "$config"
sleep 5

post_enabled=$(systemctl --user is-enabled kei.service 2>/dev/null || true)
post_active=$(systemctl --user is-active kei.service 2>/dev/null || true)
echo "post_install_enabled=$post_enabled"
echo "post_install_active=$post_active"

if [[ "$post_enabled" != "enabled" || "$post_active" != "active" ]]; then
    systemctl --user --no-pager status kei.service || true
    exit 1
fi

"$binary" service status | tee "$work/service-status.out"
grep -q "Service: running" "$work/service-status.out"

systemctl --user --no-pager status kei.service | sed -n '1,80p'

"$binary" uninstall
service_uninstalled=1

final_enabled=$(systemctl --user is-enabled kei.service 2>/dev/null || true)
final_active=$(systemctl --user is-active kei.service 2>/dev/null || true)
echo "post_uninstall_enabled=$final_enabled"
echo "post_uninstall_active=$final_active"

if [[ "$final_enabled" != "not-found" || "$final_active" != "inactive" ]]; then
    echo "run_real_service_lifecycle: service was not removed cleanly" >&2
    exit 1
fi

echo "real service lifecycle passed"
