#!/usr/bin/env bash
# Optional live full-test phase for cross-zone album hydration.
#
# Maintainers can enable it with a prepared fixture:
#   KEI_FULL_TEST_CROSS_ZONE_ALBUM=<album name>
#   KEI_FULL_TEST_CROSS_ZONE_MIN_FILES=1
#
# The fixture album must contain at least one asset whose source zone is not
# PrimarySync. The script runs the release binary against that named album with
# all visible libraries enabled, then proves the state DB recorded a downloaded
# asset row in a non-primary source zone for that album.

set -euo pipefail

repo_root=$(git rev-parse --show-toplevel 2>/dev/null) || {
    echo "run_cross_zone_album_hydration: not in a git repo" >&2
    exit 1
}
cd "$repo_root"

PROJECT_DIR="$repo_root"
# shellcheck disable=SC1091
source "$repo_root/tests/shell/lib.sh"

album="${KEI_FULL_TEST_CROSS_ZONE_ALBUM:-}"
if [[ -z "$album" ]]; then
    echo "run_cross_zone_album_hydration: KEI_FULL_TEST_CROSS_ZONE_ALBUM is unset" >&2
    exit 64
fi

min_files="${KEI_FULL_TEST_CROSS_ZONE_MIN_FILES:-1}"
if ! [[ "$min_files" =~ ^[0-9]+$ ]] || [[ "$min_files" -lt 1 ]]; then
    echo "run_cross_zone_album_hydration: KEI_FULL_TEST_CROSS_ZONE_MIN_FILES must be a positive integer" >&2
    exit 64
fi

bound=$(kei_live_fixed_date) || exit 64
kei_require_env
kei_require_release_binary
command -v sqlite3 >/dev/null 2>&1 || {
    echo "run_cross_zone_album_hydration: sqlite3 is required" >&2
    exit 1
}

binary=$(kei_release_bin)
work=$(mktemp -d "${TMPDIR:-/tmp/codex/kei/full-test/tmp}/kei-cross-zone-album-XXXXX")
trap 'rm -rf "$work"' EXIT

data_dir="$work/data"
photos="$work/photos"
mkdir -p "$data_dir" "$photos"
kei_copy_session_without_state "$data_dir"

config="$work/cross-zone.toml"
{
    printf 'data_dir = %s\n' "$(kei_toml_string "$data_dir")"
    echo
    echo "[auth]"
    printf 'username = %s\n' "$(kei_toml_string "$ICLOUD_USERNAME")"
    echo
    echo "[download]"
    printf 'directory = %s\n' "$(kei_toml_string "$photos")"
    echo
    echo "[filters]"
    printf 'albums = [%s]\n' "$(kei_toml_string "$album")"
    echo "unfiled = false"
    echo 'libraries = ["all"]'
    printf 'skip_created_before = "%s"\n' "$bound"
} >"$config"

echo "--- cross-zone album sync ($album, libraries=all) ---"
"$binary" sync --no-progress-bar --config "$config"

db=$(find "$data_dir" -maxdepth 1 -name '*.db' | head -1)
if [[ -z "$db" ]]; then
    echo "run_cross_zone_album_hydration: state DB was not created" >&2
    exit 1
fi

quoted_album=${album//\'/\'\'}
cross_zone_count=$(sqlite3 "$db" "
  SELECT COUNT(DISTINCT a.library || char(31) || a.id)
  FROM assets a
  JOIN asset_albums aa
    ON aa.library = a.library
   AND aa.asset_id = a.id
  WHERE aa.album_name = '$quoted_album'
    AND a.status = 'downloaded'
    AND a.library <> 'PrimarySync';
" 2>/dev/null || echo 0)

total_album_downloads=$(sqlite3 "$db" "
  SELECT COUNT(DISTINCT a.library || char(31) || a.id)
  FROM assets a
  JOIN asset_albums aa
    ON aa.library = a.library
   AND aa.asset_id = a.id
  WHERE aa.album_name = '$quoted_album'
    AND a.status = 'downloaded';
" 2>/dev/null || echo 0)

echo "album_downloaded=$total_album_downloads"
echo "cross_zone_downloaded=$cross_zone_count"
echo "cross_zone_min_files=$min_files"

if [[ "$cross_zone_count" -lt "$min_files" ]]; then
    echo "run_cross_zone_album_hydration: expected at least $min_files non-primary downloaded asset(s) for album '$album', got $cross_zone_count" >&2
    echo "--- downloaded album rows by library ---" >&2
    sqlite3 "$db" "
    SELECT a.library, COUNT(DISTINCT a.id)
    FROM assets a
    JOIN asset_albums aa
      ON aa.library = a.library
     AND aa.asset_id = a.id
    WHERE aa.album_name = '$quoted_album'
      AND a.status = 'downloaded'
    GROUP BY a.library
    ORDER BY a.library;
  " >&2 || true
    exit 1
fi

echo "cross-zone album hydration passed"
