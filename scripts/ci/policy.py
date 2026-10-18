#!/usr/bin/env python3
"""Conservative PR routing. Unknown paths escalate instead of silently skipping."""

import json
import os
import subprocess
from pathlib import Path

ALWAYS = ["detect", "workflow-hardening", "script-lint", "typos"]
CODE = ["fmt", "roundtrip", "clippy", "test_no_default", "test", "docs", "catalogs"]


# These clients/publishers have executable Python tests in the always-on
# workflow-hardening job. They do not build or exercise the Rust product.
MAINTENANCE_FILES = {
    "scripts/ci/nightly.py", "scripts/ci/opencode_review.py",
    "scripts/ci/policy.py", "scripts/ci/gate.py", "scripts/ci/local_release.py",
    "scripts/check-contracts",
    ".github/scripts/check_workflow_hardening.py",
    ".github/workflows/opencode-review.yml", ".github/workflows/nightly-health.yml",
    ".github/workflows/coverage-comment.yml",
    # Retired probe: a deletion still belongs to Python/workflow maintenance.
    ".github/workflows/opencode-cli-smoke.yml",
}
# These change Rust test/gate execution. Exercise the ordinary Rust matrix,
# not release artifacts.
RUST_CHECK_FILES = {
    "scripts/check-roundtrip-gate.sh", "scripts/test-copies-stress.sh",
}
STRESS_CHECK_FILES = {
    "scripts/test-copies-stress.sh",
}
QUALIFICATION_FILES = {
    "Cargo.toml", "Cargo.lock", "build.rs", "justfile", "Dockerfile",
    ".gitignore", ".dockerignore", "scripts/ci/artifact.py",
    ".github/workflows/ci.yml", ".github/workflows/checks.yml",
    ".github/workflows/artifacts.yml", ".github/workflows/qualification.yml",
    ".github/workflows/release.yml", ".github/workflows/nightly.yml",
    ".github/workflows/service-smoke.yml", ".github/workflows/docker.yml",
    ".github/workflows/docker-test.yml",
}
QUALIFICATION_PREFIXES = (".cargo/", "fuzz/", "docker/")
# One owner for the native service smoke previously selected by workflow paths.
SERVICE_FILES = {
    "src/service.rs", "src/commands/service.rs", "src/commands/status.rs",
    "src/cli.rs", "src/config.rs", "src/lib.rs", "Cargo.toml", "Cargo.lock",
    ".github/workflows/service-smoke.yml",
}
SERVICE_PREFIXES = ("src/service/", "src/commands/service/", "src/commands/status/", "src/config/")


def maintenance_path(path: str) -> bool:
    source = Path(path)
    return path in MAINTENANCE_FILES or (
        source.parent == Path("scripts/ci")
        and source.name.startswith("test_") and source.suffix == ".py"
    )


def rust_test_path(path: str) -> bool:
    source = Path(path)
    return source.suffix == ".rs" and (path.startswith("tests/") or (
        path.startswith("src/") and (
            "tests" in source.parts or source.name == "tests.rs" or source.name.endswith("_tests.rs")
        )
    ))


# The scale fixture calls download_photos_with_sync directly, with retained
# sources, two destination passes and a stub PhotosSession. Keep runtime owners
# broad within that graph, rather than routing every CLI/auth/provider change.
COPIES_STRESS_PREFIXES = (
    "src/download/", "src/state/", "src/icloud/photos/",
    "src/commands/service", "src/sync_loop", "src/sync_cycle",
    "src/config", "src/retry", "src/selection", "tests/data/",
)
COPIES_STRESS_FILES = {
    "src/types.rs", "src/fs_util.rs", "src/test_helpers.rs", "src/icloud/error.rs",
}


def copies_stress_path(path: str) -> bool:
    # This owner contains the ignored fixture and its shared construction code.
    if path == "src/download/orchestration/queue_projection.rs" or path.startswith(
        "src/download/orchestration/queue_projection/"
    ):
        return True
    # Other unit-test modules do not run inside the selected ignored harness.
    # Keep test_support/helpers and inline tests in runtime files conservative.
    if path.startswith("src/") and rust_test_path(path):
        return False
    return path in COPIES_STRESS_FILES or path.startswith(COPIES_STRESS_PREFIXES)


def route(files: list[str], event: str, heavy: bool, light_enabled: bool, full: bool) -> dict:
    qualification_input = any(p in QUALIFICATION_FILES or p.startswith(QUALIFICATION_PREFIXES) for p in files)
    rust_tooling = any(p in RUST_CHECK_FILES or p.startswith("scripts/test-scenarios/") for p in files)
    known = ("src/", "tests/", "docs/", "fuzz/", "docker/", "scripts/test-scenarios/")
    unknown = any(not p.startswith(known) and not p.endswith(".md") and p != "LICENSE"
                  and p not in QUALIFICATION_FILES and not p.startswith(QUALIFICATION_PREFIXES)
                  and p not in RUST_CHECK_FILES and not maintenance_path(p) for p in files)
    code = heavy or full or event != "pull_request" or qualification_input or rust_tooling or unknown or any(p.startswith(("src/", "tests/")) for p in files)
    broad = heavy or full or not light_enabled or event != "pull_request" or qualification_input or rust_tooling or unknown
    shared = any(p in {"src/lib.rs", "src/cli.rs", "src/types.rs", "src/test_helpers.rs", "src/fs_util.rs"} for p in files)
    platform = code and (broad or shared or any(rust_test_path(p) for p in files) or any(p.startswith(("src/download/file", "src/download/paths", "src/download/metadata", "src/state/", "src/service", "src/config", "src/fs_util", "tests/service", "tests/behavioral", "docker/")) for p in files))
    stress = code and (heavy or full or event != "pull_request" or qualification_input or unknown
                       or any(p in STRESS_CHECK_FILES or copies_stress_path(p) for p in files))
    service = heavy or full or qualification_input or unknown or any(
        p in SERVICE_FILES or p.startswith(SERVICE_PREFIXES) for p in files)
    cargo = code or any(p in {"Cargo.toml", "Cargo.lock", ".cargo/audit.toml"} for p in files)
    coverage = heavy or (code and event == "pull_request" and (full or not light_enabled))
    required = ALWAYS + (CODE if code else []) + (["lockfile", "security"] if cargo else [])
    if stress:
        required += ["copies-stress"]
    if heavy or full or (code and not light_enabled):
        required += ["unused-deps"]
    if coverage:
        required += ["coverage-head"]
    return {"code": code, "cargo": cargo, "stress": stress, "platform": platform, "service": service,
            "coverage": coverage, "unused": "unused-deps" in required, "required": required,
            "qualification": heavy or full or event == "workflow_dispatch" or qualification_input or unknown}


def main() -> None:
    event = os.environ["EVENT_NAME"]
    files = []
    if event == "pull_request":
        files = subprocess.check_output(["git", "diff", "--no-renames", "--name-only", "-z", f"{os.environ['BASE_SHA']}...HEAD"]).decode().rstrip("\0").split("\0")
        files = [p for p in files if p]
    result = route(files, event, os.environ.get("HEAVY") == "true",
                   os.environ.get("LIGHT_ENABLED") == "true", os.environ.get("FULL") == "true")
    with Path(os.environ["GITHUB_OUTPUT"]).open("a") as stream:
        stream.writelines(f"{key}={json.dumps(value, separators=(',', ':'))}\n" for key, value in result.items())


if __name__ == "__main__":
    main()
