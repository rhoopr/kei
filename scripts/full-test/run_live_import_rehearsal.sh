#!/usr/bin/env bash
# Phase 5.5 - live import-existing mini rehearsal.
#
# Seeds a tiny real photo tree with the release binary, then imports that tree
# into a fresh DB. This exercises the v0.20 TOML-first import path without
# relying on prior state in .test-cookies.

set -euo pipefail

repo_root=$(git rev-parse --show-toplevel 2>/dev/null) || {
    echo "run_live_import_rehearsal: not in a git repo" >&2
    exit 1
}
cd "$repo_root"

PROJECT_DIR="$repo_root"
# shellcheck disable=SC1091
source "$repo_root/tests/shell/lib.sh"

kei_require_env
kei_require_release_binary
kei_preflight_selection

binary=$(kei_release_bin)
recent=$(kei_live_recent)
work=$(mktemp -d "${TMPDIR:-/tmp/codex/kei/full-test/tmp}/kei-live-import-rehearsal-XXXXX")
trap 'rm -rf "$work"' EXIT

sync_data="$work/sync-data"
import_data="$work/import-data"
photos="$work/photos"
mkdir -p "$sync_data" "$import_data" "$photos"

kei_copy_session_without_state "$sync_data"
kei_copy_session_without_state "$import_data"

write_config() {
    local data_dir="$1"
    local path="$2"
    {
        printf 'data_dir = %s\n' "$(kei_toml_string "$data_dir")"
        echo
        echo "[auth]"
        printf 'username = %s\n' "$(kei_toml_string "$ICLOUD_USERNAME")"
        echo
        echo "[download]"
        printf 'directory = %s\n' "$(kei_toml_string "$photos")"
        echo
        kei_live_filters
    } >"$path"
}

sync_config="$work/sync.toml"
import_config="$work/import.toml"
write_config "$sync_data" "$sync_config"
write_config "$import_data" "$import_config"

run_and_show() {
    local name="$1"
    shift
    local out="$work/$name.out"
    local err="$work/$name.err"
    set +e
    "$@" >"$out" 2>"$err"
    local rc=$?
    set -e
    echo "--- $name stderr tail ---"
    tail -30 "$err"
    echo "--- $name stdout tail ---"
    tail -30 "$out"
    return "$rc"
}

echo "--- seed sync (bounded primary-library selection) ---"
run_and_show seed-sync \
    "$binary" sync --no-progress-bar --config "$sync_config"

file_count=$(find "$photos" -type f | wc -l | tr -d ' ')
echo "seed_files=$file_count"
if [[ "$file_count" -lt 1 ]]; then
    echo "run_live_import_rehearsal: seed sync wrote no files" >&2
    exit 1
fi

media_fingerprint() {
    python3 - "$photos" <<'PY_MEDIA'
import hashlib
import json
import os
import pathlib
import sys
root = pathlib.Path(sys.argv[1])
rows = []
for path in sorted(root.rglob("*")):
    if path.is_file():
        stat = path.stat()
        with path.open("rb") as stream:
            checksum = hashlib.file_digest(stream, "sha256").hexdigest()
        rows.append((os.fsencode(path.relative_to(root)).hex(), stat.st_ino, stat.st_size,
                     stat.st_mtime_ns, stat.st_mode, stat.st_nlink, checksum))
print(hashlib.sha256(json.dumps(rows).encode()).hexdigest())
PY_MEDIA
}

snapshot_assets() {
    python3 - "$import_data" "$1" <<'PY_ASSETS'
import json
import pathlib
import sqlite3
import sys
directory, output = map(pathlib.Path, sys.argv[1:])
databases = list(directory.glob("*.db"))
assert len(databases) <= 1, "ambiguous isolated state DB"
rows = []
if databases:
    with sqlite3.connect(databases[0].resolve().as_uri() + "?mode=ro", uri=True) as connection:
        rows = connection.execute("SELECT library,id,version_size,checksum,local_path,status FROM assets ORDER BY library,id,version_size").fetchall()
output.write_text(json.dumps(rows))
PY_ASSETS
}

original_media=$(media_fingerprint)
echo "--- import dry-run into fresh DB ---"
run_and_show import-dry-run \
    "$binary" import-existing --dry-run --force-empty --recent "$recent" --no-progress-bar --config "$import_config"
snapshot_assets "$work/after-dry-run.json"
[[ "$(media_fingerprint)" == "$original_media" ]] || {
    echo "import dry-run changed seed media" >&2
    exit 1
}

echo "--- import real into fresh DB ---"
run_and_show import-real \
    "$binary" import-existing --force-empty --recent "$recent" --no-progress-bar --config "$import_config"
snapshot_assets "$work/after-real.json"

echo "--- import repeat dry-run ---"
run_and_show import-repeat-dry-run \
    "$binary" import-existing --dry-run --force-empty --recent "$recent" --no-progress-bar --config "$import_config"
[[ "$(media_fingerprint)" == "$original_media" ]] || {
    echo "import changed seed media" >&2
    exit 1
}
echo "seed_media_unchanged=1"

# The count sample can include records outside the date-bounded local tree.
# Keep the sample unchanged and require exact known receipts and quiet repetition.
python3 - "$sync_data" "$import_data" "$recent" "$work/after-dry-run.json" "$work/after-real.json" "$work/import-dry-run.out" "$work/import-real.out" "$work/import-repeat-dry-run.out" <<'PY'
import json
import pathlib
import sqlite3
import sys

seed_dir, import_dir, recent, dry_snapshot, real_snapshot, *reports = sys.argv[1:]
recent = int(recent)
assert recent > 0, "import sample cap must be positive"
keys = ("Total assets scanned", "Files matched", "Unmatched versions", "Hash errors")
summaries = []
for report in reports:
    values = {}
    for line in pathlib.Path(report).read_text().splitlines():
        key, separator, value = line.strip().partition(":")
        if separator and key in keys:
            assert key not in values, "duplicate import summary field"
            values[key] = int(value.strip())
    assert all(key in values and values[key] >= 0 for key in keys), "missing import summary"
    assert 0 < values["Total assets scanned"] <= recent, "scan exceeded the positive import sample cap"
    summaries.append(values)
assert len(summaries) == 3 and all(value == summaries[0] for value in summaries), "dry-run, real and repeat summaries differ"
summary = summaries[0]
matched = summary["Files matched"]
assert matched > 0 and summary["Hash errors"] == 0, "import must match without hash errors"

def asset_rows(directory):
    databases = list(pathlib.Path(directory).glob("*.db"))
    assert len(databases) == 1, "expected one isolated state DB"
    with sqlite3.connect(databases[0].resolve().as_uri() + "?mode=ro", uri=True) as connection:
        rows = connection.execute("SELECT library,id,version_size,checksum,local_path,status FROM assets ORDER BY library,id,version_size").fetchall()
    assert len(rows) == len({row[:3] for row in rows}), "duplicate asset receipt identity"
    return rows

seed_rows = asset_rows(seed_dir)
import_rows = asset_rows(import_dir)
seed = {row[:5] for row in seed_rows if row[5] == "downloaded"}
imported = {row[:5] for row in import_rows if row[5] == "downloaded"}
assert json.loads(pathlib.Path(dry_snapshot).read_text()) == [], "dry-run wrote asset rows"
assert json.loads(pathlib.Path(real_snapshot).read_text()) == [list(row) for row in import_rows], "repeat dry-run changed asset receipts"
assert seed and imported and imported <= seed, "import receipts must match independent seed receipts"
assert len(imported) == matched, "matched count must equal durable imported versions"
assert len({row[:2] for row in seed}) <= recent, "seed fixture exceeds import sample capacity; refusing incomplete coverage"
# Cardinality is only a capacity check. Exact observed receipt equality proves
# overlap for this fixture; date/album gaps must refuse, not infer eligibility.
assert imported == seed, "fixture/sample overlap incomplete: known seed receipt missing"
print(f"import_matched={matched}")
print(f"db_downloaded={len(imported)}")
print("seed_sample_receipts_equal=1")
print("dry_run_asset_rows=0")
print("repeat_asset_receipts_unchanged=1")
PY

echo "live import rehearsal passed"
