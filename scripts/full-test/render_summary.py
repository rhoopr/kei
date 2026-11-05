#!/usr/bin/env python3
"""Render the short end-of-run full-test summary card."""
from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path
from typing import Any

PHASE_ORDER = {
    "static_checks": 10,
    "offline_core": 20,
    "scenarios": 25,
    "nightly_tools": 30,
    "package": 40,
    "docker_full": 50,
    "live_provider": 60,
    "live_status": 70,
    "live_libraries": 71,
    "live_albums": 72,
    "live_dryrun": 73,
    "live_config_show": 74,
    "live_verify": 75,
    "live_reconcile_dryrun": 76,
    "live_password_backend": 77,
    "live_import_dryrun": 78,
    "live_import_rehearsal": 79,
    "live_cross_zone_album": 80,
    # Legacy phase names from earlier full-test layouts.
    "gate": 10,
    "nodefault": 15,
    "fuzz_build": 17,
    "udeps": 18,
    "offline_all": 19,
    "scope_matrix": 19,
    "build_release": 20,
    "release_archive_smoke": 21,
    "docker_build": 30,
    "docker_puid_smoke": 31,
    "docker_multiarch": 32,
    "docker_version": 33,
    "docker_help": 34,
    "docker_default_cmd": 35,
    "test_live": 60,
}


def sort_key(item: tuple[str, dict[str, Any]]) -> tuple[int, str]:
    name, _phase = item
    if name.startswith("test_shell_"):
        return (65, name)
    if name.startswith("test_"):
        return (12, name)
    return (PHASE_ORDER.get(name, 999), name)


def read_phases(path: Path) -> dict[str, dict[str, Any]]:
    if not path.exists():
        return {}
    if path.suffix == ".jsonl" or path.name.startswith(".current"):
        phases: dict[str, dict[str, Any]] = {}
        with path.open(errors="replace") as fh:
            for line in fh:
                line = line.strip()
                if not line:
                    continue
                rec = json.loads(line)
                phase = rec.pop("phase")
                phases[phase] = rec
        return phases
    with path.open(errors="replace") as fh:
        rec = json.load(fh)
    return rec.get("phases", {})


def phase_line(name: str, phase: dict[str, Any], status: str) -> str:
    parts: list[str] = []
    wall = phase.get("wall_s")
    if isinstance(wall, (int, float)):
        parts.append(f"{wall:.2f}s")
    tests = phase.get("tests")
    if isinstance(tests, int):
        parts.append(f"{tests} tests")
    reason = phase.get("reason")
    if reason:
        parts.append(str(reason))
    if status == "fail":
        exit_code = phase.get("exit")
        if exit_code is not None:
            parts.append(f"rc={exit_code}")
        log_path = phase.get("log_path")
        if log_path:
            parts.append(f"log: {log_path}")
    suffix = f" ({'; '.join(parts)})" if parts else ""
    return f"{name}{suffix}"


def render(phases: dict[str, dict[str, Any]], result: str) -> None:
    passed: list[tuple[str, dict[str, Any]]] = []
    skipped: list[tuple[str, dict[str, Any]]] = []
    failed: list[tuple[str, dict[str, Any]]] = []
    for name, phase in sorted(phases.items(), key=sort_key):
        status = phase.get("status")
        if status == "pass":
            passed.append((name, phase))
        elif status in {"skipped", "rate_limited"}:
            skipped.append((name, phase))
        elif status == "fail":
            failed.append((name, phase))

    total_wall = sum(
        phase.get("wall_s", 0)
        for phase in phases.values()
        if isinstance(phase.get("wall_s"), (int, float))
    )
    total_tests = sum(
        phase.get("tests", 0)
        for phase in phases.values()
        if isinstance(phase.get("tests"), int)
    )

    print()
    print("full-test summary")
    print("=================")
    print(f"Result: {result.upper()}")
    print()

    print("Passed phases:")
    if passed:
        for name, phase in passed:
            print("  [x] " + phase_line(name, phase, "pass"))
    else:
        print("  none")

    if skipped:
        print()
        print("Skipped phases:")
        for name, phase in skipped:
            print("  [-] " + phase_line(name, phase, str(phase.get("status"))))

    if failed:
        print()
        print("Failed phases:")
        for name, phase in failed:
            print("  [!] " + phase_line(name, phase, "fail"))

    print()
    print(
        "Totals: "
        f"{len(passed)} passed, "
        f"{len(skipped)} skipped/rate-limited, "
        f"{len(failed)} failed, "
        f"{total_tests} tests counted, "
        f"{total_wall:.1f}s wall"
    )


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("path", type=Path)
    parser.add_argument("--result", choices=["pass", "fail"], required=True)
    parser.add_argument("--fallback-failure", nargs=2, metavar=("PHASE", "REASON"))
    args = parser.parse_args()

    phases = read_phases(args.path)
    has_failure = any(phase.get("status") == "fail" for phase in phases.values())
    if args.result == "fail" and not has_failure and args.fallback_failure:
        phase, reason = args.fallback_failure
        phases[phase] = {
            "status": "fail",
            "wall_s": 0.0,
            "exit": 1,
            "reason": reason,
        }
    render(phases, args.result)
    return 0


if __name__ == "__main__":
    sys.exit(main())
