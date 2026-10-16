#!/usr/bin/env python3
"""Report nightly failures once and close the incident after recovery."""

import json
import os
import subprocess
import sys
from datetime import datetime, timedelta, timezone

MARKER = "<!-- kei-nightly-qualification -->"
MAX_AGE = timedelta(hours=36)


def api(path: str, *, method: str | None = None, **fields):
    command = ["gh", "api", "--method", method or ("POST" if fields else "GET"), path]
    for key, value in fields.items():
        command.extend(["-f", f"{key}={value}"])
    return json.loads(subprocess.check_output(command))


def health(runs: list[dict], now: datetime) -> tuple[bool, str, str]:
    eligible = [r for r in runs if r.get("head_branch") == "main" and r.get("event") in {"schedule", "workflow_dispatch"}]
    completed = [r for r in eligible if r.get("status") == "completed"]
    if not completed:
        return False, "No completed nightly qualification is available.", ""
    latest = completed[0]
    started = datetime.fromisoformat(latest["created_at"].replace("Z", "+00:00"))
    if now - started > MAX_AGE or started > now:
        return False, "Nightly qualification is stale (no completed run started within 36 hours).", latest["html_url"]
    if latest.get("conclusion") != "success":
        return False, f"Nightly qualification concluded {latest.get('conclusion', 'unknown')}.", latest["html_url"]
    return True, "Nightly qualification is current and passed.", latest["html_url"]


def notify(repo: str, healthy: bool, message: str, url: str) -> None:
    pages = json.loads(subprocess.check_output(["gh", "api", "--paginate", "--slurp", f"repos/{repo}/issues?state=open&per_page=100"]))
    incident = next((i for page in pages for i in page if i.get("user", {}).get("login") == "github-actions[bot]" and (i.get("body") or "").startswith(MARKER)), None)
    body = f"{message}\n\n{url}\n\nRelease qualification remains independent and must pass on the release commit."
    if healthy and incident:
        api(f"repos/{repo}/issues/{incident['number']}/comments", body=body)
        api(f"repos/{repo}/issues/{incident['number']}", method="PATCH", state="closed")
    elif not healthy and not incident:
        api(f"repos/{repo}/issues", title="Nightly qualification needs attention", body=f"{MARKER}\n{body}")
    print(message)


def main() -> None:
    repo = os.environ["GITHUB_REPOSITORY"]
    if os.environ.get("GITHUB_REF") != "refs/heads/main":
        raise ValueError("nightly notifications may only run from main")
    if sys.argv[1:] == ["health"]:
        runs = api(f"repos/{repo}/actions/workflows/nightly.yml/runs?branch=main&per_page=30")["workflow_runs"]
        healthy, message, url = health(runs, datetime.now(timezone.utc))
    elif sys.argv[1:] == ["report"]:
        healthy = os.environ["RESULT"] == "success"
        message = f"Nightly qualification: {os.environ['RESULT']}."
        url = f"https://github.com/{repo}/actions/runs/{os.environ['GITHUB_RUN_ID']}"
    else:
        raise ValueError("expected report or health")
    notify(repo, healthy, message, url)
    if not healthy:
        sys.exit(1)


if __name__ == "__main__":
    try:
        main()
    except (KeyError, ValueError, OSError, subprocess.CalledProcessError) as error:
        sys.exit(str(error))
