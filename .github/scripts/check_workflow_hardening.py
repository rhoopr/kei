"""Guard security-sensitive GitHub Actions invariants."""

from __future__ import annotations

import re
import sys
from pathlib import Path

WORKFLOW_DIR = Path(".github/workflows")
SHA_RE = re.compile(r"[0-9a-f]{40}")
USES_RE = re.compile(r"\buses:\s*([^\s#]+)")


def workflow_text(name: str) -> str:
    return (WORKFLOW_DIR / name).read_text()


def workflow_paths() -> list[Path]:
    return sorted([*WORKFLOW_DIR.glob("*.yml"), *WORKFLOW_DIR.glob("*.yaml")])


def check_action_refs(errors: list[str]) -> None:
    for path in workflow_paths():
        for line_no, line in enumerate(path.read_text().splitlines(), start=1):
            if "\t" in line:
                errors.append(f"{path}:{line_no}: tab character")

            match = USES_RE.search(line)
            if match is None:
                continue

            ref = match.group(1)
            # Local reusable workflows resolve at the caller's exact commit.
            if re.fullmatch(r"\./\.github/workflows/[a-z0-9_-]+\.yml", ref):
                if not Path(ref).is_file():
                    errors.append(f"{path}:{line_no}: missing reusable workflow: {ref}")
                continue
            parts = ref.rsplit("@", maxsplit=1)
            if len(parts) != 2:
                errors.append(f"{path}:{line_no}: action ref is missing @: {ref}")
                continue

            if SHA_RE.fullmatch(parts[1]) is None:
                errors.append(
                    f"{path}:{line_no}: action ref is not pinned to a full commit SHA: {ref}"
                )


def check_rust_toolchains(errors: list[str]) -> None:
    for path in workflow_paths():
        lines = path.read_text().splitlines()
        for idx, line in enumerate(lines):
            if "uses: dtolnay/rust-toolchain@" not in line:
                continue

            window = lines[idx : idx + 6]
            toolchain_lines = [entry.strip() for entry in window if entry.strip().startswith("toolchain:")]
            if not toolchain_lines:
                errors.append(f"{path}:{idx + 1}: rust-toolchain step is missing explicit toolchain")
                continue

            value = toolchain_lines[0].split(":", maxsplit=1)[1].strip().strip('"\'')
            if value in {"stable", "nightly"}:
                errors.append(f"{path}:{idx + 1}: rust toolchain must not be a moving channel: {value}")


def check_docker_publish(errors: list[str]) -> None:
    text = workflow_text("docker.yml")
    if "workflow_dispatch:" in text:
        errors.append("docker.yml: publishing workflow must not accept arbitrary manual refs")
    if "packages: write" not in text:
        errors.append("docker.yml: publishing workflow must keep explicit packages: write permission")
    if "tags:" in text.split("concurrency:", maxsplit=1)[0] or "value=latest" in text:
        errors.append("docker.yml: release publication must go through release qualification")
    if "push: true" not in text:
        errors.append("docker.yml: publishing workflow must still publish images")


def check_docker_test(errors: list[str]) -> None:
    text = workflow_text("docker-test.yml")
    if "workflow_dispatch:" not in text:
        errors.append("docker-test.yml: manual test build must keep workflow_dispatch")
    if "packages: write" in text:
        errors.append("docker-test.yml: manual test build must not have package write permission")
    if "push: false" not in text:
        errors.append("docker-test.yml: manual test build must not push images")
    if "cache-to:" in text:
        errors.append("docker-test.yml: manual test build must not write shared caches")


def check_release(errors: list[str]) -> None:
    text = workflow_text("release.yml")
    before_jobs = text.split("jobs:", maxsplit=1)[0]
    if "permissions:\n  contents: read\n" not in before_jobs:
        errors.append("release.yml: missing top-level read-only token permissions")
    if "cargo build --locked --release --target ${{ matrix.target }}" not in workflow_text("artifacts.yml"):
        errors.append("release.yml: release builds must use cargo build --locked")
    for needle in ("statuses: read", "needs: qualification", "needs: [qualification, release]", "needs: [release, containers]",
                   "scripts/ci/local_release.py check", "uses: ./.github/workflows/qualification.yml",
                   "python3 scripts/ci/artifact.py verify", "docker load --input"):
        if needle not in text:
            errors.append(f"release.yml: missing qualification guard: {needle}")
    if text.count("if: github.event_name == 'push' && startsWith(github.ref, 'refs/tags/v')") != 2:
        errors.append("release.yml: release and containers must both be tag-only")
    if 'curl -fsSL "$BASE/SHA256SUMS.txt"' not in text:
        errors.append("release.yml: Homebrew updater must download SHA256SUMS.txt fail-fast")
    if 'curl -fsSL "$BASE/$file"' not in text:
        errors.append("release.yml: Homebrew updater must download release assets fail-fast")
    for needle in (
        'expected_sha=$(awk -v file="$file"',
        'if [ "$actual_sha" != "$expected_sha" ]; then',
        'SHAS[$key]="$actual_sha"',
    ):
        if needle not in text:
            errors.append(f"release.yml: Homebrew updater missing checksum guard: {needle}")


def check_rust_ci(errors: list[str]) -> None:
    text = workflow_text("checks.yml")
    for needle in (
        "run: python3 scripts/ci/policy.py",
        "run: python3 scripts/check-contracts",
        "LIGHT_ENABLED: ${{ vars.CI_LIGHT_PR_ENABLED }}",
        "if: needs.detect.outputs.coverage == 'true'",
        "      - coverage-head\n",
        "check(json.loads(os.environ[\"NEEDS\"]), required)",
    ):
        if needle not in text:
            errors.append(f"checks.yml: missing required routing/gate guard: {needle}")


def check_qualification(errors: list[str]) -> None:
    guards = {
        "ci.yml": ["needs: [plan, checks, service, qualification]", "mode not in {\"true\", \"false\"}",
                   "[\"plan\", \"qualification\" if mode == \"true\" else \"checks\"]",
                   "missing service route", "required += [\"service\"]",
                   "uses: ./.github/workflows/service-smoke.yml"],
        "qualification.yml": ["needs: [quality, artifacts, service, specialized]",
                              "python3 scripts/ci/gate.py quality artifacts service specialized",
                              "cargo fetch --locked\n          just test packaging",
                              "python3 scripts/ci/artifact.py verify"],
        "artifacts.yml": ["push: false", "labels: ${{ steps.meta.outputs.labels }}", "run_docker_packaging_smoke.sh", "run_docker_puid_smoke.sh"],
    }
    for name, markers in guards.items():
        text = workflow_text(name)
        for marker in markers:
            if marker not in text:
                errors.append(f"{name}: missing qualification guard: {marker}")
    for name in ("ci.yml", "checks.yml", "qualification.yml", "artifacts.yml", "nightly.yml", "nightly-health.yml"):
        text = workflow_text(name)
        if re.search(r"(?:contents|packages): write|push: true|secrets: inherit", text):
            errors.append(f"{name}: qualification must not receive publication credentials")


def check_service_smoke(errors: list[str]) -> None:
    text = workflow_text("service-smoke.yml")
    if "group: service-smoke-${{ github.workflow }}-${{ github.event.pull_request.number || github.ref }}" not in text:
        errors.append("service-smoke.yml: cancel obsolete runs without cancelling another caller")
    if "  workflow_call:" not in text or "  pull_request:" in text:
        errors.append("service-smoke.yml: service smoke must be reusable only; CI owns routing")


def check_coverage_comment(errors: list[str]) -> None:
    text = workflow_text("coverage-comment.yml")
    for needle in ("Validate coverage comment artifact", "gt 60000", "@<!-- -->"):
        if needle not in text:
            errors.append(f"coverage-comment.yml: missing artifact hardening marker: {needle}")


def check_windows_stack_link_arg(errors: list[str]) -> None:
    text = Path("build.rs").read_text()
    if 'cargo:rustc-link-arg=/STACK:4194304' not in text:
        errors.append("build.rs: missing Windows stack linker argument")
    if 'RUSTFLAGS="-Dwarnings"' not in text:
        errors.append("build.rs: Windows stack fix must document CI RUSTFLAGS override")


def main() -> int:
    errors: list[str] = []
    check_action_refs(errors)
    check_rust_toolchains(errors)
    check_docker_publish(errors)
    check_docker_test(errors)
    check_release(errors)
    check_rust_ci(errors)
    check_qualification(errors)
    check_service_smoke(errors)
    check_coverage_comment(errors)
    check_windows_stack_link_arg(errors)

    if errors:
        print("\n".join(errors), file=sys.stderr)
        return 1

    print("Workflow hardening checks passed.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
