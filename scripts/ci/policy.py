#!/usr/bin/env python3
"""Conservative PR routing. Unknown paths escalate instead of silently skipping."""

import json
import os
import subprocess
from pathlib import Path

ALWAYS = ["detect", "workflow-hardening", "script-lint", "typos"]
CODE = ["fmt", "roundtrip", "clippy", "test_no_default", "test", "docs", "catalogs"]


def route(files: list[str], event: str, heavy: bool, light_enabled: bool, full: bool) -> dict:
    tooling = any(p.startswith((".github/", "scripts/", ".cargo/")) or p in {"justfile", "build.rs", "Cargo.toml", "Cargo.lock", ".gitignore", ".dockerignore"} for p in files)
    known = ("src/", "tests/", "docs/", "fuzz/", "docker/")
    documentation = {"LICENSE"}
    unknown = any(not p.startswith(known) and not p.endswith(".md") and p not in documentation for p in files)
    code = heavy or event != "pull_request" or tooling or unknown or any(p.startswith(("src/", "tests/", "fuzz/", "docker/")) or p == "Dockerfile" for p in files)
    broad = heavy or full or not light_enabled or event != "pull_request" or tooling or unknown
    shared = any(p in {"src/lib.rs", "src/cli.rs", "src/types.rs", "src/test_helpers.rs", "src/fs_util.rs"} for p in files)
    platform = code and (broad or shared or any(p.startswith(("src/download/file", "src/download/paths", "src/download/metadata", "src/state/", "src/service", "src/config", "src/fs_util", "tests/service", "tests/behavioral", "docker/")) for p in files))
    stress = code and (broad or shared or any(p.startswith(("src/download/", "src/state/", "src/sync_loop", "src/sync_cycle", "src/commands/", "src/icloud/", "src/config", "src/account", "src/retry", "src/selection", "tests/data/")) for p in files))
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
        files = subprocess.check_output(["git", "diff", "--name-only", "-z", f"{os.environ['BASE_SHA']}...HEAD"]).decode().rstrip("\0").split("\0")
        files = [p for p in files if p]
    result = route(files, event, os.environ.get("HEAVY") == "true",
                   os.environ.get("LIGHT_ENABLED") == "true", os.environ.get("FULL") == "true")
    with Path(os.environ["GITHUB_OUTPUT"]).open("a") as stream:
        stream.writelines(f"{key}={json.dumps(value, separators=(',', ':'))}\n" for key, value in result.items())


if __name__ == "__main__":
    main()
