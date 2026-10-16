#!/usr/bin/env python3
"""Validate local provider and host-service evidence and attest it for one release commit.

Publishing is explicit. Only phase status and a report digest leave the host.
Release CI accepts the latest repository-owner attestation, never PR artifacts.
"""

import argparse
import hashlib
import json
import os
import re
import subprocess
import sys
from datetime import datetime, timedelta, timezone
from pathlib import Path

CONTEXT = "Release local qualification"
MAX_AGE = timedelta(days=7)
BASE_PHASES = {"build_release", "build_live_container", "live_provider", "live_import_rehearsal", "host_service"}
LIVE_COMMANDS = {"live_status", "live_libraries", "live_albums", "live_dryrun", "live_config_show",
                 "live_verify", "live_reconcile_dryrun", "live_password_backend", "live_import_dryrun"}


def validate_record(record: dict, sha: str, shell_suites: list[str]) -> None:
    if record.get("head") != sha or record.get("end_head") != sha:
        raise ValueError("local evidence is not for the release commit")
    if record.get("start_worktree_clean") is not True or record.get("end_worktree_clean") is not True:
        raise ValueError("local qualification must start and end with a clean worktree")
    phases = record.get("phases", {})
    required = BASE_PHASES | LIVE_COMMANDS | {f"test_shell_{name.replace('-', '_')}" for name in shell_suites}
    for phase in required:
        result = phases.get(phase, {})
        if result.get("status") != "pass" or result.get("exit") != 0:
            raise ValueError(f"local release prerequisite did not pass: {phase}")
    for name in {"live_provider"} | {f"test_shell_{suite.replace('-', '_')}" for suite in shell_suites}:
        if phases[name].get("tests", 0) <= 0:
            raise ValueError(f"local prerequisite selected no tests: {name}")
    for name, phase in phases.items():
        if phase.get("status") != "pass" or phase.get("exit") != 0:
            raise ValueError(f"local qualification contains non-passing phase: {name}")


def validate_statuses(statuses: list[dict], owner: str, now: datetime) -> dict:
    matching = [s for s in statuses if s.get("context") == CONTEXT]
    if not matching:
        raise ValueError("missing local release qualification; run local prerequisites first")
    # The API is reverse chronological. Never fall back past a newer failure.
    latest = matching[0]
    if latest.get("state") != "success" or latest.get("creator", {}).get("login") != owner:
        raise ValueError("latest local qualification must be successful and attested by the repository owner")
    created = datetime.fromisoformat(latest["created_at"].replace("Z", "+00:00"))
    if not timedelta(0) <= now - created <= MAX_AGE:
        raise ValueError("local release attestation is stale or future-dated")
    if not re.fullmatch(r"local qualification sha256:[0-9a-f]{32}", latest.get("description", "")):
        raise ValueError("local qualification is missing its evidence digest")
    return latest


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("mode", choices=["record", "check", "finalize"])
    parser.add_argument("--repo", required=True)
    parser.add_argument("--sha", required=True)
    parser.add_argument("--report", type=Path)
    parser.add_argument("--evidence-url")
    parser.add_argument("--publish", action="store_true")
    args = parser.parse_args()
    if not re.fullmatch(r"[0-9a-f]{40}", args.sha) or not re.fullmatch(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+", args.repo):
        raise ValueError("expected repository owner/name and full commit SHA")
    if args.mode == "check":
        pages = json.loads(subprocess.check_output(["gh", "api", "--paginate", "--slurp", f"repos/{args.repo}/commits/{args.sha}/statuses?per_page=100"]))
        validate_statuses([s for page in pages for s in page], args.repo.split("/")[0], datetime.now(timezone.utc))
        print(f"local release qualification verified for {args.sha}")
        return
    if args.report is None:
        raise ValueError("--report is required")
    if subprocess.check_output(["git", "rev-parse", "HEAD"], text=True).strip() != args.sha:
        raise ValueError("check out the exact release commit before attesting")
    if subprocess.check_output(["git", "status", "--porcelain", "--untracked-files=all"], text=True).strip():
        raise ValueError("worktree must still be clean")
    if args.mode == "finalize":
        directory = Path(os.environ["KEI_FULL_TEST_RUNS_DIR"])
        phases = {}
        for line in (directory / ".current.jsonl").read_text().splitlines():
            item = json.loads(line)
            name = item.pop("phase")
            if name in phases:
                raise ValueError(f"duplicate local qualification phase: {name}")
            phases[name] = item
        record = {"head": (directory / ".run-start-head").read_text().strip(),
                  "end_head": args.sha,
                  "start_worktree_clean": (directory / ".run-start-worktree-clean").read_text().strip() == "true",
                  "end_worktree_clean": True, "phases": phases}
    else:
        record = json.loads(args.report.read_text())
    suites = [p.stem for p in Path("tests/shell").glob("*.sh") if p.stem != "lib"]
    validate_record(record, args.sha, suites)
    if args.mode == "finalize":
        args.report.write_text(json.dumps(record, indent=2) + "\n")
        (directory / ".current.jsonl").unlink()
        print(f"local release evidence: {args.report}")
    digest = hashlib.sha256(args.report.read_bytes()).hexdigest()
    summary = {"sha": args.sha, "report_sha256": digest, "phases": sorted(record["phases"])}
    print(json.dumps(summary, indent=2))
    if args.publish:
        if not args.evidence_url or not args.evidence_url.startswith(f"https://github.com/{args.repo}/"):
            raise ValueError("publish a redacted evidence summary in this repository and supply --evidence-url")
        subprocess.run(["gh", "api", f"repos/{args.repo}/statuses/{args.sha}", "-f", "state=success",
                        "-f", f"context={CONTEXT}", "-f", f"description=local qualification sha256:{digest[:32]}",
                        "-f", f"target_url={args.evidence_url}"], check=True, stdout=subprocess.DEVNULL)


if __name__ == "__main__":
    try:
        main()
    except (KeyError, ValueError, OSError, subprocess.CalledProcessError) as error:
        sys.exit(str(error))
