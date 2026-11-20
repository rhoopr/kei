#!/usr/bin/env bash
# Shared workflow and script lint for the PR baseline and protected CI.
# The tool list and invocation mirror the script-lint job in checks.yml.
set -euo pipefail

repo_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$repo_root"

mapfile -t shell_files < <(find scripts tests/shell docker -maxdepth 3 -type f \( -name '*.sh' -o -name 'entrypoint.sh' \) -print | sort)
mapfile -t python_files < <(find scripts .github/scripts -maxdepth 3 -type f -name '*.py' -print | sort)
python_files+=(scripts/check-contracts)
for shell_file in "${shell_files[@]}"; do
    bash -n "$shell_file"
done
mkdir -p /tmp/codex/kei/pycache
PYTHONPYCACHEPREFIX=/tmp/codex/kei/pycache python3 -m py_compile "${python_files[@]}"
shellcheck -x -P tests/shell:scripts:scripts/full-test "${shell_files[@]}"
shfmt -d "${shell_files[@]}"
ruff check "${python_files[@]}"
actionlint .github/workflows/*.yml
