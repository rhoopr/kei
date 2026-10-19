#!/usr/bin/env python3
"""Report nightly failures once and close the incident after recovery."""

import json
import os
import re
import subprocess
import sys
from dataclasses import dataclass
from datetime import datetime, timedelta, timezone

MARKER = "<!-- kei-nightly-qualification -->"
RECORDS_HEADER = "Recorded findings:"
FOOTER = "Release qualification remains independent and must pass on the release commit."
MAX_AGE = timedelta(hours=36)
INCIDENT_LABELS = ("bug", "@infra", "@test", "nightly fail")
INCIDENT_AUTHOR = "github-actions[bot]"
MISSING_IDENTITY = "missing completed run"
RUN_RECORD = re.compile(r"^-\s*run (\d+) attempt (\d+)\s*$", re.MULTILINE)
STALE_RECORD = re.compile(r"^-\s*stale run (\d+)\s*$", re.MULTILINE)
MISSING_RECORD = re.compile(
    rf"^-\s*({re.escape(MISSING_IDENTITY)}(?: after run \d+)?)\s*$", re.MULTILINE
)
RUN_URL = re.compile(r"/actions/runs/(\d+)(?:/attempts/(\d+))?")


@dataclass(frozen=True)
class Finding:
    """One nightly outcome and the failure identity that incidents record."""

    message: str
    url: str
    identity: str | None

    @property
    def healthy(self) -> bool:
        return self.identity is None


def api(path: str, *, method: str | None = None, **fields):
    command = ["gh", "api", "--method", method or ("POST" if fields else "GET"), path]
    for key, value in fields.items():
        if isinstance(value, (list, tuple)):
            for item in value:
                command.extend(["-f", f"{key}[]={item}"])
        else:
            command.extend(["-f", f"{key}={value}"])
    return json.loads(subprocess.check_output(command))


def paginated(path: str) -> list[dict]:
    pages = json.loads(subprocess.check_output(["gh", "api", "--paginate", "--slurp", path]))
    return [item for page in pages for item in page]


def run_identity(run_id: int | str, attempt: int | str) -> str:
    return f"run {run_id} attempt {attempt}"


def missing_identity(last_completed: dict | None) -> str:
    """Return the missing identity for the episode after the last completed run."""
    if last_completed is None:
        return MISSING_IDENTITY
    return f"{MISSING_IDENTITY} after run {last_completed['id']}"


def health(runs: list[dict], now: datetime, last_completed: dict | None = None) -> Finding:
    eligible = [
        run
        for run in runs
        if run.get("head_branch") == "main"
        and run.get("event") in {"schedule", "workflow_dispatch"}
    ]
    completed = [run for run in eligible if run.get("status") == "completed"]
    if not completed:
        return Finding(
            "No completed nightly qualification is available.", "", missing_identity(last_completed)
        )
    latest = completed[0]
    started = datetime.fromisoformat(latest["created_at"].replace("Z", "+00:00"))
    if now - started > MAX_AGE or started > now:
        return Finding(
            "Nightly qualification is stale (no completed run started within 36 hours).",
            latest["html_url"],
            f"stale run {latest['id']}",
        )
    if latest.get("conclusion") != "success":
        return Finding(
            f"Nightly qualification concluded {latest.get('conclusion', 'unknown')}.",
            latest["html_url"],
            run_identity(latest["id"], latest.get("run_attempt", 1)),
        )
    return Finding("Nightly qualification is current and passed.", latest["html_url"], None)


def findings_in(body: str) -> set[str]:
    """Return the failure identities that a body records."""
    records = {run_identity(run_id, attempt) for run_id, attempt in RUN_RECORD.findall(body)}
    records |= {f"stale run {run_id}" for run_id in STALE_RECORD.findall(body)}
    records |= set(MISSING_RECORD.findall(body))
    if RECORDS_HEADER not in body:
        # Incidents created before findings were recorded carry only the run URL.
        records |= {run_identity(run_id, attempt or 1) for run_id, attempt in RUN_URL.findall(body)}
    return records


def record_finding(body: str, identity: str) -> str:
    """Return a body that records the identity in addition to its findings."""
    bullet = f"- {identity}\n"
    if RECORDS_HEADER in body:
        return body.replace(f"{RECORDS_HEADER}\n", f"{RECORDS_HEADER}\n{bullet}", 1)
    section = f"{RECORDS_HEADER}\n{bullet}\n"
    return body.replace(FOOTER, f"{section}{FOOTER}", 1)


def incident_body(finding: Finding) -> str:
    """Build the body of a new incident."""
    lines = [MARKER, finding.message, ""]
    if finding.url:
        lines += [finding.url, ""]
    lines += [RECORDS_HEADER, f"- {finding.identity}", "", FOOTER]
    return "\n".join(lines)


def is_incident(issue: dict) -> bool:
    body = issue.get("body") or ""
    return (
        issue.get("user", {}).get("login") == INCIDENT_AUTHOR
        and "pull_request" not in issue
        and body.startswith(MARKER)
    )


def missing_labels(repo: str, incident: dict | None) -> list[str]:
    existing = {label["name"] for label in incident["labels"]} if incident else set()
    missing = [label for label in INCIDENT_LABELS if label not in existing]
    if "nightly fail" in missing:
        ensure_nightly_label(repo)
    return missing


def ensure_nightly_label(repo: str) -> None:
    labels = paginated(f"repos/{repo}/labels?per_page=100")
    if not any(label["name"] == "nightly fail" for label in labels):
        api(f"repos/{repo}/labels", name="nightly fail", color="B60205",
            description="Nightly qualification failed or became stale")


def notify(repo: str, finding: Finding) -> None:
    issues = paginated(f"repos/{repo}/issues?state=all&per_page=100")
    incidents = [issue for issue in issues if is_incident(issue)]
    open_incident = next((issue for issue in incidents if issue["state"] == "open"), None)
    if finding.healthy:
        if open_incident:
            number = open_incident["number"]
            body = f"{finding.message}\n\n{finding.url}\n\n{FOOTER}"
            api(f"repos/{repo}/issues/{number}/comments", body=body)
            api(f"repos/{repo}/issues/{number}", method="PATCH", state="closed")
    else:
        acknowledged = {
            identity for issue in incidents for identity in findings_in(issue.get("body") or "")
        }
        if open_incident is None:
            if finding.identity not in acknowledged:
                api(f"repos/{repo}/issues", title="Nightly qualification needs attention",
                    body=incident_body(finding), labels=missing_labels(repo, None))
        else:
            number = open_incident["number"]
            labels = missing_labels(repo, open_incident)
            if labels:
                api(f"repos/{repo}/issues/{number}/labels", labels=labels)
            if finding.identity not in acknowledged:
                body = open_incident.get("body") or ""
                api(f"repos/{repo}/issues/{number}", method="PATCH",
                    body=record_finding(body, finding.identity))
    print(finding.message)


def main() -> None:
    repo = os.environ["GITHUB_REPOSITORY"]
    if os.environ.get("GITHUB_REF") != "refs/heads/main":
        raise ValueError("nightly notifications may only run from main")
    if sys.argv[1:] == ["health"]:
        path = f"repos/{repo}/actions/workflows/nightly.yml/runs?branch=main&per_page=30"
        now = datetime.now(timezone.utc)
        runs = api(path)["workflow_runs"]
        finding = health(runs, now)
        if finding.identity == MISSING_IDENTITY:
            # Anchor a missing episode to the last completed run so a later
            # episode after recovery is not suppressed by a closed incident.
            completed_path = (
                f"repos/{repo}/actions/workflows/nightly.yml/runs"
                "?branch=main&status=completed&per_page=1"
            )
            completed = api(completed_path)["workflow_runs"]
            if completed:
                finding = health(runs, now, completed[0])
    elif sys.argv[1:] == ["report"]:
        run_id = os.environ["GITHUB_RUN_ID"]
        result = os.environ["RESULT"]
        identity = None if result == "success" else run_identity(
            run_id, os.environ["GITHUB_RUN_ATTEMPT"]
        )
        finding = Finding(
            message=f"Nightly qualification: {result}.",
            url=f"https://github.com/{repo}/actions/runs/{run_id}",
            identity=identity,
        )
    else:
        raise ValueError("expected report or health")
    notify(repo, finding)
    if not finding.healthy:
        sys.exit(1)


if __name__ == "__main__":
    try:
        main()
    except (KeyError, ValueError, OSError, subprocess.CalledProcessError) as error:
        sys.exit(str(error))
