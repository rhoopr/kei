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
    for needle in ("statuses: read", "needs: qualification", "needs: [qualification, release]", "needs: [release, containers, resume_containers]",
                   "scripts/ci/local_release.py check", "uses: ./.github/workflows/qualification.yml",
                   "python3 scripts/ci/artifact.py verify", "docker load --input"):
        if needle not in text:
            errors.append(f"release.yml: missing qualification guard: {needle}")
    for needle in (
        "github.ref == 'refs/heads/main'",
        'scripts/ci/publication.py --repo "$GITHUB_REPOSITORY"',
        'run-id: ${{ steps.validate.outputs.run_id }}',
        'ref: ${{ steps.validate.outputs.sha }}',
        'env GITHUB_SHA="$RELEASE_SHA" python3 ../scripts/ci/artifact.py verify',
        'env GITHUB_SHA="$RELEASE_SHA" python3 ../scripts/ci/artifact.py verify-loaded-container',
        "needs.resume_containers.result == 'success'",
    ):
        if needle not in text:
            errors.append(f"release.yml: missing publication-resume guard: {needle}")
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

    attestation_action = (
        "actions/attest-build-provenance@"
        "977bb373ede98d70efdf65b84cb5f73e068dcc2a"
    )
    if text.count(attestation_action) != 5:
        errors.append(
            "release.yml: archive and both container jobs must pin the "
            "build-provenance attestation action"
        )
    checksums = text.find("sha256sum -- *.tar.gz *.zip > SHA256SUMS.txt")
    archive_attestation = text.find("subject-path: |")
    upload = text.find("- name: Upload release assets")
    if not 0 <= checksums < archive_attestation < upload:
        errors.append(
            "release.yml: archive attestation must run after checksums and before upload"
        )
    for needle in (
        "candidate/*.tar.gz",
        "candidate/*.zip",
        "candidate/SHA256SUMS.txt",
        'docker buildx imagetools inspect "$image:$version-$arch" --format \'{{json .Manifest}}\'',
        "jq -r .digest",
        "subject-name: ghcr.io/${{ github.repository }}",
        "subject-digest: ${{ steps.publish.outputs.amd64 }}",
        "subject-digest: ${{ steps.publish.outputs.arm64 }}",
        "push-to-registry: true",
    ):
        if needle not in text:
            errors.append(f"release.yml: missing release provenance guard: {needle}")
    for permission in ("id-token: write", "attestations: write"):
        if text.count(permission) != 3:
            errors.append(
                "release.yml: release, containers, and resume_containers "
                f"must grant {permission}"
            )
    if text.count("subject-path: |") != 1:
        errors.append(
            "release.yml: archive attestation must use subject-path for the released files"
        )
    if text.count("subject-digest: ${{ steps.publish.outputs.amd64 }}") != 2:
        errors.append(
            "release.yml: containers and resume_containers must attest the amd64 digest"
        )
    if text.count("subject-digest: ${{ steps.publish.outputs.arm64 }}") != 2:
        errors.append(
            "release.yml: containers and resume_containers must attest the arm64 digest"
        )
    if text.count("push-to-registry: true") != 4:
        errors.append(
            "release.yml: each container attestation must push to the registry"
        )


def check_rust_ci(errors: list[str]) -> None:
    text = workflow_text("checks.yml")
    for needle in (
        "run: python3 scripts/ci/policy.py",
        "run: python3 scripts/check-contracts",
        "LIGHT_ENABLED: ${{ vars.CI_LIGHT_PR_ENABLED }}",
        "if: inputs.heavy",
        "HEAVY: ${{ inputs.heavy }}",
        "\n  machete:\n",
        "name: Unused dependencies (machete)",
        "tool: cargo-machete",
        "run: cargo machete",
        "\n      - machete\n",
    ):
        if needle not in text:
            errors.append(f"checks.yml: missing required routing/gate guard: {needle}")


def check_qualification(errors: list[str]) -> None:
    guards = {
        "pr.yml": ["needs: [quality, clippy, smoke]", "if: always()", "name: CI",
                   "python3 scripts/ci/gate.py quality clippy smoke",
                   "KEI_ROUNDTRIP_BASE: ${{ github.event.pull_request.base.sha }}"],
        "ci.yml": ["  push:\n    branches: [main]", "  workflow_dispatch:",
                   "uses: ./.github/workflows/checks.yml"],
        "qualification.yml": ["needs: [quality, artifacts, service, specialized]",
                              "python3 scripts/ci/gate.py quality artifacts service specialized",
                              "cargo fetch --locked\n          just test packaging",
                              "bash scripts/ci/seeded-fuzz.sh",
                              "python3 scripts/ci/artifact.py verify"],
        "nightly.yml": ["needs: [broad, fuzz, audit, optimized]",
                        "scripts/ci/gate.py broad fuzz audit optimized",
                        "RESULT: ${{ needs.result.result }}", "bash scripts/ci/seeded-fuzz.sh"],
        "artifacts.yml": ["push: false", "labels: ${{ steps.meta.outputs.labels }}", "run_docker_packaging_smoke.sh", "run_docker_puid_smoke.sh"],
    }
    for name, markers in guards.items():
        text = workflow_text(name)
        for marker in markers:
            if marker not in text:
                errors.append(f"{name}: missing qualification guard: {marker}")
    if "uses: ./.github/workflows/qualification.yml" in workflow_text("ci.yml"):
        errors.append("ci.yml: manual dispatch must call checks.yml, not qualification.yml")
    if re.search(r"(?m)^\s*schedule\s*:", workflow_text("qualification.yml")):
        errors.append("qualification.yml: heavy qualification must not run on a schedule")
    for name in ("pr.yml", "ci.yml", "checks.yml", "qualification.yml", "artifacts.yml",
                 "nightly.yml", "nightly-health.yml", "daily-wiki.yml"):
        text = workflow_text(name)
        if re.search(r"(?:contents|packages): write|push: true|secrets: inherit", text):
            errors.append(f"{name}: qualification must not receive publication credentials")


def check_service_smoke(errors: list[str]) -> None:
    text = workflow_text("service-smoke.yml")
    if "group: service-smoke-${{ github.workflow }}-${{ github.event.pull_request.number || github.ref }}" not in text:
        errors.append("service-smoke.yml: cancel obsolete runs without cancelling another caller")
    if "  workflow_call:" not in text or "  pull_request:" in text:
        errors.append("service-smoke.yml: service smoke must be reusable only; CI owns routing")


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
    check_windows_stack_link_arg(errors)

    if errors:
        print("\n".join(errors), file=sys.stderr)
        return 1

    print("Workflow hardening checks passed.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
