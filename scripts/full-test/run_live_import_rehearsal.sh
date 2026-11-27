#!/usr/bin/env bash
# Phase 5.5 - live import-existing mini rehearsal.
#
# Seeds a tiny real photo tree with the release binary, then imports that tree
# into separate state. Both start from one listing of the library, so the
# import must find every seeded file on disk rather than in the seed's records.
# Nothing here relies on prior state in .test-cookies.

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

binary=$(kei_release_bin)
work=$(mktemp -d "${TMPDIR:-/tmp/codex/kei/full-test/tmp}/kei-live-import-rehearsal-XXXXX")
trap 'rm -rf "$work" "$(kei_listed_state_dir)"' EXIT
kei_preflight_selection

sync_data="$work/sync-data"
import_data="$work/import-data"
photos="$work/photos"
mkdir -p "$sync_data" "$import_data" "$photos"

for data in "$sync_data" "$import_data"; do
    kei_copy_session_without_state "$data"
    kei_seed_listed_state "$data"
done

write_config() {
    local data_dir="$1"
    local path="$2"
    {
        printf 'data_dir = %s\n' "$(kei_toml_string "$data_dir")"
        echo
        echo "config_version = 1"
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

# The copies a state records: identity, path, recorded bytes and state.
snapshot_copies() {
    python3 - "$1" "$2" <<'PY_COPIES'
import json
import pathlib
import sqlite3
import sys
directory, output = map(pathlib.Path, sys.argv[1:])
databases = [path for path in directory.glob("*.sync.db")]
assert len(databases) == 1, f"expected one isolated state DB, found {databases}"
with sqlite3.connect(databases[0].resolve().as_uri() + "?mode=ro", uri=True) as connection:
    rows = connection.execute(
        "SELECT zone, asset, rendition, album, path, size, checksum, sha256, state, wanted "
        "FROM files ORDER BY zone, asset, rendition, album, slot"
    ).fetchall()
output.write_text(json.dumps(rows))
PY_COPIES
}

original_media=$(media_fingerprint)
snapshot_copies "$import_data" "$work/before-import.json"

echo "--- import dry-run ---"
run_and_show import-dry-run \
    "$binary" import-existing --dry-run --no-progress-bar --config "$import_config"
snapshot_copies "$import_data" "$work/after-dry-run.json"
[[ "$(media_fingerprint)" == "$original_media" ]] || {
    echo "import dry-run changed seed media" >&2
    exit 1
}

echo "--- import real ---"
run_and_show import-real \
    "$binary" import-existing --no-progress-bar --config "$import_config"
snapshot_copies "$import_data" "$work/after-real.json"

echo "--- import repeat dry-run ---"
run_and_show import-repeat-dry-run \
    "$binary" import-existing --dry-run --no-progress-bar --config "$import_config"
snapshot_copies "$import_data" "$work/after-repeat.json"

echo "--- sync from the imported state ---"
run_and_show import-sync \
    "$binary" sync --no-progress-bar --config "$import_config"
[[ "$(media_fingerprint)" == "$original_media" ]] || {
    echo "import or the following sync changed seed media" >&2
    exit 1
}
echo "seed_media_unchanged=1"

python3 - "$sync_data" "$import_data" "$work" <<'PY'
import json
import pathlib
import re
import sqlite3
import sys

seed_dir, import_dir, work = map(pathlib.Path, sys.argv[1:])

def summary(name):
    values = {}
    for line in (work / f"{name}.out").read_text().splitlines():
        line = line.strip()
        for key, pattern in [
            ("matched", r"^Adopted (\d+) existing local files\.$"),
            ("matched", r"^Local files that match a photo:\s+(\d+)$"),
            ("already", r"^Already recorded:\s+(\d+)$"),
            ("missing", r"^Copies with no local file yet:\s+(\d+)"),
            ("refused", r"^Could not adopt:\s+(\d+)"),
        ]:
            found = re.match(pattern, line)
            if found:
                assert key not in values, f"duplicate {key} in {name}"
                values[key] = int(found.group(1))
    for key in ("matched", "already", "missing"):
        assert key in values, f"{name} printed no {key} count"
    values.setdefault("refused", 0)
    return values

def saved(directory):
    database = next(directory.glob("*.sync.db"))
    with sqlite3.connect(database.resolve().as_uri() + "?mode=ro", uri=True) as connection:
        return set(connection.execute(
            "SELECT zone, asset, rendition, album, path, sha256 FROM files "
            "WHERE state = 'done' AND wanted = 1"
        ).fetchall())

def synced(name, field):
    total = None
    for line in (work / f"{name}.err").read_text().splitlines():
        line = re.sub(r"\x1b\[[0-9;]*m", "", line)
        if "Library synced" in line:
            found = re.search(rf"\b{field}=(\d+)", line)
            if found:
                total = (total or 0) + int(found.group(1))
    assert total is not None, f"{name} logged no {field} count"
    return total

seed = saved(seed_dir)
assert seed, "seed sync recorded no saved copies"
dry, real, repeat = (summary(name) for name in ("import-dry-run", "import-real", "import-repeat-dry-run"))
assert dry["matched"] == len(seed) and dry["already"] == 0, f"dry run must find every seed file: {dry}"
assert dry["refused"] == 0 and real["refused"] == 0, "no seed path may hold another file"
assert real["matched"] == len(seed), f"import must adopt every seed file: {real}"
assert repeat["matched"] == 0 and repeat["already"] == len(seed), f"repeat must find them recorded: {repeat}"
assert dry["missing"] == real["missing"] == repeat["missing"], "the planned copies changed between runs"

before = json.loads((work / "before-import.json").read_text())
assert json.loads((work / "after-dry-run.json").read_text()) == before, "dry run changed the import state"
assert json.loads((work / "after-repeat.json").read_text()) == json.loads((work / "after-real.json").read_text()), \
    "repeat dry run changed the import state"
assert saved(import_dir) == seed, "import receipts must equal the seed's: path, identity and bytes"
assert synced("import-sync", "downloaded") == 0 and synced("import-sync", "adopted") == 0, \
    "sync after import must neither download nor adopt"
print(f"import_matched={real['matched']}")
print(f"import_owed={real['missing']}")
print("seed_receipts_equal=1")
print("dry_run_state_unchanged=1")
print("repeat_state_unchanged=1")
print("sync_after_import_quiet=1")
PY

echo "live import rehearsal passed"
