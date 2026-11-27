#!/usr/bin/env bash
# Fixed Linux sync-safety suite. Libtest unions the filters and runs overlaps once.
set -euo pipefail

cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.."

groups=(
    ::contract_
    sync_core::tests::
    sync_core::state::tests::
    sync_core::plan::
    download::file::
    download::metadata_pass::
    icloud::photos::asset::
    download::paths::
    commands::import::
    selection::tests::
    fs_util::
)
sentinels=(
    download::file::publication::tests::contract_file_publish_no_overwrite_destination_already_exists
    sync_core::tests::an_interrupted_listing_resumes_from_its_last_saved_page
    download::file::transfer::tests::attempt_download_happy_path_writes_and_renames
)

log="$(mktemp)"
trap 'rm -f "$log"' EXIT

cargo test --locked --lib --all-features -- "${groups[@]}" --test-threads=2 \
    --skip sync_core::tests::copies_stress \
    --format pretty --color never --show-output >"$log" 2>&1 || {
    tail -200 "$log" >&2
    exit 1
}
grep -E '^test result: ok\. [1-9][0-9]* passed; 0 failed; 0 ignored;' "$log"
passed="$(grep -E '^test [^ ]+ \.\.\. ok$' "$log")"
for group in "${groups[@]}"; do
    if ! grep -Fq -- "$group" <<<"$passed"; then
        echo "PR sync suite: no passing test for $group" >&2
        exit 1
    fi
done
for name in "${sentinels[@]}"; do
    grep -Fxq "test $name ... ok" <<<"$passed" || {
        echo "PR sync suite: missing $name" >&2
        exit 1
    }
done
if grep -Eq 'skipping .*loopback bind is not permitted' "$log"; then
    echo "PR smoke failed: a required test skipped because loopback bind is unavailable" >&2
    exit 1
fi

echo "PR smoke passed"
