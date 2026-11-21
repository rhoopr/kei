"""Read exact-commit GitHub Actions evidence for the daily regression checks."""

import os
import subprocess
import sys
from pathlib import Path

from nightly import api

MAIN_JOB = "checks / Shared checks result"
OPTIMIZED_JOB = "Optimized library tests"
OPTIMIZED_STEP = "Run optimized library tests"


def passed(item):
    return item["status"] == "completed" and item["conclusion"] == "success"


def runs(repo, sha, workflow, query):
    return api(f"repos/{repo}/actions/workflows/{workflow}/runs?branch=main&head_sha={sha}&{query}")["workflow_runs"]


def matches(run, repo, sha, workflow, events):
    return (run["repository"]["full_name"] == repo and run["path"] == f".github/workflows/{workflow}"
            and run["head_branch"] == "main" and run["head_sha"] == sha and run["event"] in events)


def successful_job(repo, run, name):
    # Both workflows fit on one page. Never treat an incomplete listing as proof.
    page = api(f"repos/{repo}/actions/runs/{run['id']}/attempts/{run['run_attempt']}/jobs?per_page=100")
    if len(page["jobs"]) != page["total_count"]:
        return None
    jobs = [job for job in page["jobs"] if job["name"] == name and job["run_id"] == run["id"]
            and job["run_attempt"] == run["run_attempt"] and job["head_sha"] == run["head_sha"]]
    return jobs[0] if len(jobs) == 1 and passed(jobs[0]) else None


def require_main(repo, sha):
    latest = runs(repo, sha, "ci.yml", "event=push&per_page=1")
    if not latest or not matches(latest[0], repo, sha, "ci.yml", {"push"}):
        raise ValueError(f"No matching main push run: https://github.com/{repo}/commit/{sha}/checks")
    run = latest[0]
    if not passed(run) or not successful_job(repo, run, MAIN_JOB):
        raise ValueError(f"Missing successful {MAIN_JOB}: {run['html_url']} ({run['status']}/{run['conclusion']})")
    print(f"Broad main CI passed: {run['html_url']}")


def optimized_proof(repo, sha, current):
    # Look back at most one week of daily runs. Re-test if the proof ages out.
    for run in runs(repo, sha, "nightly.yml", "status=success&per_page=7"):
        if (str(run["id"]) == current or not matches(run, repo, sha, "nightly.yml", {"schedule", "workflow_dispatch"})
                or not passed(run)):
            continue
        job = successful_job(repo, run, OPTIMIZED_JOB)
        if job and any(step["name"] == OPTIMIZED_STEP and passed(step) for step in job["steps"]):
            return run["html_url"]
    return None


def plan_optimized(repo, sha, current, output):
    try:
        proof = optimized_proof(repo, sha, current)
    except (KeyError, ValueError, OSError, subprocess.CalledProcessError) as error:
        print(f"Could not verify prior optimized tests; running them: {error}", file=sys.stderr)
        proof = None
    if proof:
        print(f"Reusing optimized tests from {proof}")
    with output.open("a", encoding="utf-8") as stream:
        stream.write(f"run={'false' if proof else 'true'}\n")


if __name__ == "__main__":
    if os.environ["GITHUB_REF"] != "refs/heads/main":
        sys.exit("Nightly evidence is main-only")
    repo, sha = os.environ["GITHUB_REPOSITORY"], os.environ["GITHUB_SHA"]
    if sys.argv[1:] == ["main"]:
        require_main(repo, sha)
    elif sys.argv[1:] == ["optimized"]:
        plan_optimized(repo, sha, os.environ["GITHUB_RUN_ID"], Path(os.environ["GITHUB_OUTPUT"]))
    else:
        sys.exit("Expected main or optimized")
