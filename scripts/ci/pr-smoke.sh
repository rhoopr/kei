#!/usr/bin/env bash
# Fixed Linux sync-safety suite. Libtest unions the filters and runs overlaps once.
set -euo pipefail

cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.."

groups=(
    ::contract_
    sync_loop::runner::tests::checkpoints::
    download::file::transfer::tests::
    download::orchestration::fixture_tests::
    private_unchanged_sidecar_packets_survive_reseeds_and_two_quiet_reopens
    icloud::photos::asset::
    download::paths::
    state::db::
    state::schema::
    commands::import::
    download::filter::
    selection::tests::
)
sentinels=(
    download::orchestration::fixture_tests::recovery::bundled_no_overwrite_preserves_existing_media
    sync_loop::runner::tests::checkpoints::run_cycle_durable_retry_survives_checkpoint_commit_failure
    download::file::transfer::tests::attempt_download_happy_path_writes_and_renames
)

log="$(mktemp)"
trap 'rm -f "$log"' EXIT

cargo test --locked --lib --all-features -- "${groups[@]}" --test-threads=2 \
    --skip download::filter::collisions::tests::post_rename_pre_state_sigkill_child \
    --skip state::db::legacy_preservation::path_cache::tests::legacy_path_cache_benchmark \
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
