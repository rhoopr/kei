#!/usr/bin/env python3
"""Check the shared main-push or heavy qualification results."""

import json
import os

from gate import check


def required(heavy: bool, light_enabled: bool) -> list[str]:
    jobs = [
        "workflow-hardening", "script-lint", "typos", "fmt", "roundtrip", "clippy",
        "test_no_default", "test", "docs", "catalogs", "lockfile", "security", "copies-stress",
    ]
    if heavy or not light_enabled:
        jobs.append("unused-deps")
    return jobs


if __name__ == "__main__":
    check(json.loads(os.environ["NEEDS"]), required(
        os.environ["HEAVY"] == "true", os.environ["LIGHT_ENABLED"].lower() == "true",
    ))
