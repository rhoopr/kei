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
    if "cargo build --locked --release --target ${{ matrix.target }}" not in text:
        errors.append("release.yml: release builds must use cargo build --locked")
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
    text = workflow_text("ci.yml")
    for needle in (
        "push:\n    branches: [main]",
        'if [[ "$EVENT_NAME" != "pull_request" ]]; then',
        "github.event_name == 'pull_request' && needs.detect.outputs.code == 'true'",
    ):
        if needle not in text:
            errors.append(f"ci.yml: missing push/main or PR-only coverage guard: {needle}")


def check_service_smoke_paths(errors: list[str]) -> None:
    text = workflow_text("service-smoke.yml")
    for path in (
        "src/service/**",
        "src/commands/service.rs",
        "src/cli.rs",
        "src/config.rs",
        "src/config/**",
        "src/lib.rs",
        "src/commands/status.rs",
    ):
        if f"- '{path}'" not in text:
            errors.append(f"service-smoke.yml: path filter must include {path}")


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
    check_service_smoke_paths(errors)
    check_coverage_comment(errors)
    check_windows_stack_link_arg(errors)

    if errors:
        print("\n".join(errors), file=sys.stderr)
        return 1

    print("Workflow hardening checks passed.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
