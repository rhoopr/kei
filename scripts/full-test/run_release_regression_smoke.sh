#!/usr/bin/env bash
# Offline release regression smoke.
#
# This is the quick patch-release gate for the May 27, 2026 regression set:
# token reuse, targeted retry, on-disk pending adoption, pagination-shortfall
# diagnostics, permissive valid-media validation, inactive pass
# templates, and dotted config paths.

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

echo "--- stored token decisions stay incremental ---"
run_lib_test sync_cycle::tests::determine_sync_mode_two_normal_syncs_reuse_stored_token

echo "--- failed rows use targeted retry without full enumeration ---"
run_lib_test download::orchestration::recovery::tests::incremental_with_failed_rows_uses_targeted_retry_not_full_enumeration

echo "--- pending rows with existing files are adopted ---"
run_lib_test download::pipeline::adoption::tests::producer_adopts_pending_on_disk_skip_as_downloaded

echo "--- pagination shortfall fixtures remain diagnostic warnings ---"
run_lib_test download::orchestration::full::tests::classify_pagination_shortfall_issue_498_fixture_reports_shortfall
run_lib_test download::orchestration::full::tests::classify_pagination_shortfall_billimek_sharedsync_fixture_reports_shortfall

echo "--- valid JPEG bytes under .PNG are saved ---"
run_lib_test download::file::tests::attempt_download_promotes_valid_jpeg_with_png_extension_issue_507

echo "--- inactive album and smart-folder templates do not force full enumeration ---"
run_lib_test download::orchestration::dispatch::tests::unfiled_only_incremental_ignores_inactive_album_path_templates

echo "--- dotted config path handling ---"
run_lib_test config::paths::tests::test_expand_tilde_with_injected_home_uses_path_join
host=$(rustc -vV | awk '/^host:/ { print $2 }')
if [[ "$host" == *windows* ]]; then
    run_lib_test config::paths::tests::test_expand_tilde_windows_home_keeps_separator_before_dot_config
else
    echo "skipping cfg(windows) dotted path smoke on host $host"
fi

echo "release regression smoke passed"
