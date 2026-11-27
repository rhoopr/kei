#!/usr/bin/env bash
# Offline release regression smoke.
#
# A quick patch-release gate over the sync core's durable behavior: the saved
# position, retry backoff, adoption of files already on disk, recovery of an
# interrupted or rejected listing, permissive valid-media validation, and
# dotted config paths.

set -euo pipefail

repo_root=$(git rev-parse --show-toplevel 2>/dev/null) || {
    echo "run_release_regression_smoke: not in a git repo" >&2
    exit 1
}
cd "$repo_root"

available_tests=$(cargo test --lib -- --list)
run_lib_test() {
    local test_name="${1:?test name required}"
    if ! grep -Fqx "$test_name: test" <<<"$available_tests"; then
        echo "run_release_regression_smoke: test not found: $test_name" >&2
        exit 1
    fi
    cargo test --lib "$test_name" -- --exact
}

echo "--- a listed library reads only new changes ---"
run_lib_test sync_core::tests::full_listing_saves_unfiled_and_album_copies_then_stays_quiet
run_lib_test sync_core::tests::a_new_photo_needs_only_its_own_work

echo "--- failed copies are retried with backoff ---"
run_lib_test sync_core::state::tests::failed_copies_retry_every_cycle_at_first_then_up_to_daily

echo "--- files already at their planned paths are adopted ---"
run_lib_test sync_core::tests::an_existing_file_with_the_same_size_is_adopted

echo "--- interrupted and rejected listings recover ---"
run_lib_test sync_core::tests::an_interrupted_listing_resumes_from_its_last_saved_page
run_lib_test sync_core::tests::a_rejected_position_relists_without_downloading_again

echo "--- valid JPEG bytes under .PNG are saved ---"
run_lib_test download::file::transfer::tests::attempt_download_promotes_valid_jpeg_with_png_extension_issue_507

echo "--- dotted config path handling ---"
run_lib_test config::paths::tests::test_expand_tilde_with_injected_home_uses_path_join
host=$(rustc -vV | awk '/^host:/ { print $2 }')
if [[ "$host" == *windows* ]]; then
    run_lib_test config::paths::tests::test_expand_tilde_windows_home_keeps_separator_before_dot_config
else
    echo "skipping cfg(windows) dotted path smoke on host $host"
fi

echo "release regression smoke passed"
