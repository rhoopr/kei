#!/usr/bin/env python3
"""Conservative PR routing. Unknown paths escalate instead of silently skipping."""

import json
import os
import subprocess
from pathlib import Path

ALWAYS = ["detect", "workflow-hardening", "script-lint", "typos"]
CODE = ["fmt", "roundtrip", "clippy", "test_no_default", "test", "docs", "catalogs"]


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
    source = Path(path)
    # Other unit-test modules do not run inside the selected ignored harness.
    # Keep test_support/helpers and inline tests in runtime files conservative.
    if path.startswith("src/") and source.suffix == ".rs" and (
        "tests" in source.parts or source.name == "tests.rs" or source.name.endswith("_tests.rs")
    ):
        return False
    return path in COPIES_STRESS_FILES or path.startswith(COPIES_STRESS_PREFIXES)


def route(files: list[str], event: str, heavy: bool, light_enabled: bool, full: bool) -> dict:
    tooling = any(p.startswith((".github/", "scripts/", ".cargo/")) or p in {"justfile", "build.rs", "Cargo.toml", "Cargo.lock", ".gitignore", ".dockerignore"} for p in files)
    known = ("src/", "tests/", "docs/", "fuzz/", "docker/")
    documentation = {"LICENSE"}
    unknown = any(not p.startswith(known) and not p.endswith(".md") and p not in documentation for p in files)
    code = heavy or event != "pull_request" or tooling or unknown or any(p.startswith(("src/", "tests/", "fuzz/", "docker/")) or p == "Dockerfile" for p in files)
    broad = heavy or full or not light_enabled or event != "pull_request" or tooling or unknown
    shared = any(p in {"src/lib.rs", "src/cli.rs", "src/types.rs", "src/test_helpers.rs", "src/fs_util.rs"} for p in files)
    platform = code and (broad or shared or any(p.startswith(("src/download/file", "src/download/paths", "src/download/metadata", "src/state/", "src/service", "src/config", "src/fs_util", "tests/service", "tests/behavioral", "docker/")) for p in files))
    stress = code and (broad or any(copies_stress_path(p) for p in files))
    cargo = code or any(p in {"Cargo.toml", "Cargo.lock", ".cargo/audit.toml"} for p in files)
    coverage = heavy or (code and event == "pull_request" and (full or not light_enabled))
    required = ALWAYS + (CODE if code else []) + (["lockfile", "security"] if cargo else [])
    if stress:
        required += ["copies-stress"]
    if heavy or (code and not light_enabled):
        required += ["unused-deps"]
    if coverage:
        required += ["coverage-head"]
    return {"code": code, "cargo": cargo, "stress": stress, "platform": platform,
            "coverage": coverage, "unused": "unused-deps" in required, "required": required,
            "qualification": heavy or full or event == "workflow_dispatch" or tooling or unknown
            or any(p == "Dockerfile" or p.startswith("fuzz/") for p in files)}


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
