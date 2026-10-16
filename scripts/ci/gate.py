#!/usr/bin/env python3
"""Fail closed when a mandatory job is missing, failed, cancelled, or skipped."""

import json
import os
import sys


def check(needs: dict, required: list[str]) -> None:
    for name in required:
        if needs.get(name, {}).get("result") != "success":
            raise ValueError(f"required job {name} did not succeed")
    for name, job in needs.items():
        if job.get("result") not in {"success", "skipped"}:
            raise ValueError(f"job {name}: {job.get('result', 'missing')}")


if __name__ == "__main__":
    try:
        check(json.loads(os.environ["NEEDS"]), sys.argv[1:])
    except (KeyError, ValueError, TypeError) as error:
        sys.exit(str(error))
