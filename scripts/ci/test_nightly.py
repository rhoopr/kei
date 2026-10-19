"""Nightly freshness and notification transitions without remote writes."""

import json
import os
import re
import sys
import unittest
from datetime import datetime, timedelta, timezone
from unittest.mock import patch

import nightly

REPO = "example/repo"
NOW = datetime(2026, 10, 16, 12, 0, tzinfo=timezone.utc)
BOT = {"login": "github-actions[bot]"}


def completed_run(
    run_id: int,
    *,
    conclusion: str = "failure",
    age: timedelta = timedelta(),
    attempt: int = 1,
    created_at: datetime | None = None,
) -> dict:
    created = created_at or datetime.now(timezone.utc) - age
    return {
        "id": run_id,
        "status": "completed",
        "head_branch": "main",
        "event": "schedule",
        "conclusion": conclusion,
        "run_attempt": attempt,
        "created_at": created.isoformat(),
        "html_url": f"https://github.com/{REPO}/actions/runs/{run_id}",
    }


def issue(
    number: int,
    *,
    state: str = "open",
    body: str = "",
    author: dict = BOT,
    labels: tuple[str, ...] = (),
    comments: tuple[str, ...] = (),
) -> dict:
    return {
        "number": number,
        "state": state,
        "body": body,
        "user": author,
        "labels": [{"name": label} for label in labels],
        "comments": list(comments),
    }


def legacy_body(message: str, url: str) -> str:
    """Incident body written before findings were recorded, such as #999."""
    return f"{nightly.MARKER}\n{message}\n\n{url}\n\n{nightly.FOOTER}"


def run_url(run_id: int, attempt: int = 1) -> str:
    base = f"https://github.com/{REPO}/actions/runs/{run_id}"
    return base if attempt == 1 else f"{base}/attempts/{attempt}"


class FakeGitHub:
    """Stateful stand-in for the GitHub REST calls that nightly.py makes."""

    def __init__(
        self,
        *,
        issues: tuple[dict, ...] = (),
        runs: tuple[dict, ...] = (),
        completed_runs: tuple[dict, ...] = (),
        labels: tuple[str, ...] = nightly.INCIDENT_LABELS,
        page_size: int = 100,
    ):
        self.issues = list(issues)
        self.runs = list(runs)
        self.completed_runs = list(completed_runs)
        self.labels = [{"name": name} for name in labels]
        self.page_size = page_size
        self.reads = []

    def check_output(self, command):
        path = command[command.index("--slurp") + 1]
        self.reads.append(path)
        if path == f"repos/{REPO}/issues?state=all&per_page=100":
            items = self.issues
        elif path == f"repos/{REPO}/labels?per_page=100":
            items = self.labels
        else:
            raise AssertionError(f"unexpected read: {path}")
        pages = [
            items[index : index + self.page_size]
            for index in range(0, len(items), self.page_size)
        ] or [[]]
        return json.dumps(pages).encode()

    def api(self, path: str, *, method: str | None = None, **fields):
        if path == f"repos/{REPO}/actions/workflows/nightly.yml/runs?branch=main&per_page=30":
            return {"workflow_runs": self.runs}
        if path == f"repos/{REPO}/actions/workflows/nightly.yml/runs?branch=main&status=completed&per_page=1":
            return {"workflow_runs": self.completed_runs}
        if path == f"repos/{REPO}/labels":
            self.labels.append({"name": fields["name"]})
            return {}
        if path == f"repos/{REPO}/issues":
            number = max((item["number"] for item in self.issues), default=0) + 1
            self.issues.append(
                issue(number, body=fields["body"], labels=tuple(fields.get("labels", ())))
            )
            return {"number": number}
        match = re.fullmatch(rf"repos/{REPO}/issues/(\d+)(?:/(comments|labels))?", path)
        if not match:
            raise AssertionError(f"unexpected write: {method} {path}")
        target = next(item for item in self.issues if item["number"] == int(match.group(1)))
        if match.group(2) == "comments":
            target["comments"].append(fields["body"])
        elif match.group(2) == "labels":
            target["labels"] += [{"name": label} for label in fields["labels"]]
        else:
            target.update(fields)
        return target

    def incidents(self) -> list[dict]:
        return [
            item
            for item in self.issues
            if (item["body"] or "").startswith(nightly.MARKER)
            and item["user"]["login"] == nightly.INCIDENT_AUTHOR
        ]

    def open_incidents(self) -> list[dict]:
        return [item for item in self.incidents() if item["state"] == "open"]


class NightlyCase(unittest.TestCase):
    def run_nightly(
        self,
        github: FakeGitHub,
        argv: list[str],
        *,
        result: str | None = None,
        run_id: str = "39125943231",
        attempt: str = "1",
    ) -> int:
        """Run main() against the fake and return its exit code."""
        env = {"GITHUB_REPOSITORY": REPO, "GITHUB_REF": "refs/heads/main"}
        if result is not None:
            env.update({"RESULT": result, "GITHUB_RUN_ID": run_id, "GITHUB_RUN_ATTEMPT": attempt})
        with (
            patch.dict(os.environ, env),
            patch.object(sys, "argv", ["nightly.py", *argv]),
            patch.object(nightly, "api", github.api),
            patch.object(nightly.subprocess, "check_output", github.check_output),
        ):
            try:
                nightly.main()
            except SystemExit as status:
                return int(status.code)
        return 0


class HealthTests(NightlyCase):
    def test_fresh_missing_failed_cancelled_and_stale(self):
        run = completed_run(7, conclusion="success", created_at=NOW)
        self.assertTrue(nightly.health([run], NOW).healthy)
        for runs in (
            [],
            [{**run, "status": "in_progress"}],
            [{**run, "conclusion": "failure"}],
            [{**run, "conclusion": "cancelled"}],
            [{**run, "head_branch": "feature"}],
            [{**run, "created_at": (NOW - timedelta(hours=37)).isoformat()}],
        ):
            self.assertFalse(nightly.health(runs, NOW).healthy)

    def test_identity_uses_run_and_attempt_not_message(self):
        failed = completed_run(39125943231, attempt=2, created_at=NOW)
        self.assertEqual(nightly.health([failed], NOW).identity, "run 39125943231 attempt 2")
        self.assertEqual(nightly.health([], NOW).identity, nightly.MISSING_IDENTITY)
        old = completed_run(39125943231, conclusion="success", created_at=NOW - timedelta(hours=37))
        self.assertEqual(nightly.health([old], NOW).identity, "stale run 39125943231")
        fresh = completed_run(39125943231, conclusion="success", created_at=NOW)
        self.assertIsNone(nightly.health([fresh], NOW).identity)

    def test_findings_are_read_from_records_and_legacy_urls(self):
        body = (
            f"{nightly.RECORDS_HEADER}\n- run 39125943231 attempt 2\n"
            "- stale run 39168454114\n"
            f"- {nightly.MISSING_IDENTITY}\n"
        )
        self.assertEqual(
            nightly.findings_in(body),
            {"run 39125943231 attempt 2", "stale run 39168454114", nightly.MISSING_IDENTITY},
        )
        self.assertEqual(
            nightly.findings_in(legacy_body("failed", run_url(39125943231))),
            {"run 39125943231 attempt 1"},
        )
        self.assertEqual(
            nightly.findings_in(legacy_body("failed", run_url(39125943231, attempt=3))),
            {"run 39125943231 attempt 3"},
        )
        self.assertEqual(nightly.findings_in(f"{nightly.MARKER}\nno findings yet\n"), set())

    @patch.object(nightly.subprocess, "check_output", return_value=b"{}")
    def test_issue_updates_use_patch(self, output):
        nightly.api("repos/example/repo/issues/1", method="PATCH", state="closed")
        self.assertEqual(output.call_args.args[0], ["gh", "api", "--method", "PATCH",
                         "repos/example/repo/issues/1", "-f", "state=closed"])

    @patch.object(nightly.subprocess, "check_output", return_value=b"{}")
    def test_issue_label_arrays_preserve_spaces(self, output):
        nightly.api("repos/example/repo/issues", labels=nightly.INCIDENT_LABELS)
        self.assertEqual(output.call_args.args[0], [
            "gh", "api", "--method", "POST", "repos/example/repo/issues",
            "-f", "labels[]=bug", "-f", "labels[]=@infra", "-f", "labels[]=@test",
            "-f", "labels[]=nightly fail",
        ])

    @patch.object(nightly, "api")
    @patch.object(nightly.subprocess, "check_output")
    def test_nightly_label_is_created_only_when_absent(self, output, api):
        output.return_value = b'[[{"name":"bug"}], [{"name":"nightly fail"}]]'
        nightly.ensure_nightly_label("example/repo")
        api.assert_not_called()
        output.return_value = b'[[{"name":"bug"}]]'
        nightly.ensure_nightly_label("example/repo")
        api.assert_called_once_with("repos/example/repo/labels", name="nightly fail",
                                   color="B60205", description="Nightly qualification failed or became stale")


class IncidentTests(NightlyCase):
    def test_close_acknowledges_the_same_failed_run(self):
        github = FakeGitHub(runs=(completed_run(39125943231),))
        self.assertEqual(self.run_nightly(github, ["report"], result="failure"), 1)
        incident = github.open_incidents()[0]
        self.assertIn("- run 39125943231 attempt 1\n", incident["body"])
        incident["state"] = "closed"
        snapshot = json.dumps(github.issues)
        self.assertEqual(self.run_nightly(github, ["health"]), 1)
        self.assertEqual(json.dumps(github.issues), snapshot)
        self.assertEqual(len(github.incidents()), 1)

    def test_new_failed_run_after_close_opens_a_new_incident(self):
        closed = issue(
            999,
            state="closed",
            body=legacy_body("Nightly qualification: failure.", run_url(39125943231)),
            labels=nightly.INCIDENT_LABELS,
        )
        github = FakeGitHub(issues=(closed,), runs=(completed_run(39168454114),))
        self.assertEqual(self.run_nightly(github, ["health"]), 1)
        opened = github.open_incidents()
        self.assertEqual(len(opened), 1)
        self.assertIn("- run 39168454114 attempt 1", opened[0]["body"])
        self.assertEqual(closed["state"], "closed")

    def test_message_text_does_not_identify_failures(self):
        github = FakeGitHub(runs=(completed_run(39125943231),))
        self.assertEqual(self.run_nightly(github, ["health"]), 1)
        self.assertEqual(
            self.run_nightly(github, ["report"], result="failure", run_id="39125943231"), 1
        )
        opened = github.open_incidents()
        self.assertEqual(len(opened), 1)
        self.assertEqual(opened[0]["body"].count("- run 39125943231 attempt 1"), 1)
        self.assertEqual(opened[0]["comments"], [])

    def test_open_incident_records_later_failures_without_comment(self):
        github = FakeGitHub(runs=(completed_run(39125943233),))
        self.assertEqual(
            self.run_nightly(github, ["report"], result="failure", run_id="39125943231"), 1
        )
        self.assertEqual(
            self.run_nightly(github, ["report"], result="failure", run_id="39125943232"), 1
        )
        self.assertEqual(self.run_nightly(github, ["health"]), 1)
        opened = github.open_incidents()
        self.assertEqual(len(opened), 1)
        self.assertEqual(len(github.incidents()), 1)
        self.assertEqual(opened[0]["comments"], [])
        for run_id in (39125943231, 39125943232, 39125943233):
            identity = f"run {run_id} attempt 1"
            self.assertIn(f"- {identity}\n", opened[0]["body"])
        opened[0]["state"] = "closed"
        snapshot = json.dumps(github.issues)
        self.assertEqual(self.run_nightly(github, ["health"]), 1)
        self.assertEqual(json.dumps(github.issues), snapshot)

    def test_legacy_incident_dedups_without_backfill(self):
        closed = issue(
            999,
            state="closed",
            body=legacy_body("Nightly qualification: failure.", run_url(39125943231)),
            labels=nightly.INCIDENT_LABELS,
        )
        github = FakeGitHub(issues=(issue(998, body="unrelated"), closed), runs=(completed_run(39125943231),), page_size=1)
        snapshot = json.dumps(github.issues)
        self.assertEqual(self.run_nightly(github, ["health"]), 1)
        self.assertEqual(json.dumps(github.issues), snapshot)
        self.assertIn(f"repos/{REPO}/issues?state=all&per_page=100", github.reads)

    def test_rerun_attempt_is_a_distinct_failure(self):
        closed = issue(
            999,
            state="closed",
            body=legacy_body("Nightly qualification: failure.", run_url(39125943231)),
            labels=nightly.INCIDENT_LABELS,
        )
        github = FakeGitHub(issues=(closed,), runs=(completed_run(39125943231, attempt=2),))
        self.assertEqual(self.run_nightly(github, ["health"]), 1)
        opened = github.open_incidents()
        self.assertEqual(len(opened), 1)
        self.assertIn("- run 39125943231 attempt 2", opened[0]["body"])

    def test_missing_and_stale_alerts_stay_distinct_from_failures(self):
        github = FakeGitHub(runs=())
        self.assertEqual(self.run_nightly(github, ["health"]), 1)
        missing = github.open_incidents()[0]
        self.assertIn("No completed nightly qualification is available.", missing["body"])
        self.assertIn(f"- {nightly.MISSING_IDENTITY}\n", missing["body"])
        missing["state"] = "closed"

        github.runs = [completed_run(39125943231, age=timedelta(hours=37))]
        self.assertEqual(self.run_nightly(github, ["health"]), 1)
        stale = github.open_incidents()
        self.assertEqual(len(stale), 1)
        self.assertIn("- stale run 39125943231\n", stale[0]["body"])
        self.assertIn("Nightly qualification is stale", stale[0]["body"])
        self.assertNotEqual(stale[0]["number"], missing["number"])

    def test_closed_missing_incident_does_not_suppress_a_later_missing_episode(self):
        github = FakeGitHub(
            runs=(),
            completed_runs=(completed_run(39125943231, conclusion="success"),),
        )
        self.assertEqual(self.run_nightly(github, ["health"]), 1)
        first = github.open_incidents()[0]
        snapshot = json.dumps(github.issues)
        self.assertEqual(self.run_nightly(github, ["health"]), 1)
        self.assertEqual(json.dumps(github.issues), snapshot)
        first["state"] = "closed"

        github.runs = [completed_run(39168454114, conclusion="success")]
        self.assertEqual(self.run_nightly(github, ["health"]), 0)

        github.runs = []
        github.completed_runs = [completed_run(39168454114, conclusion="success")]
        self.assertEqual(self.run_nightly(github, ["health"]), 1)
        opened = github.open_incidents()
        self.assertEqual(len(opened), 1)
        self.assertNotEqual(opened[0]["number"], first["number"])
        self.assertIn("- missing completed run after run 39168454114\n", opened[0]["body"])

    def test_acknowledged_failure_does_not_suppress_a_later_stale_alert(self):
        github = FakeGitHub(runs=(completed_run(39125943231),))
        self.assertEqual(self.run_nightly(github, ["health"]), 1)
        github.open_incidents()[0]["state"] = "closed"
        github.runs = [completed_run(39125943231, age=timedelta(hours=37))]
        self.assertEqual(self.run_nightly(github, ["health"]), 1)
        opened = github.open_incidents()
        self.assertEqual(len(opened), 1)
        self.assertIn("- stale run 39125943231\n", opened[0]["body"])

    def test_new_run_after_close_still_alerts(self):
        github = FakeGitHub(runs=(completed_run(39125943231),))
        self.assertEqual(
            self.run_nightly(github, ["report"], result="failure", run_id="39125943231"), 1
        )
        github.open_incidents()[0]["state"] = "closed"
        github.runs = [completed_run(39168454114)]
        self.assertEqual(self.run_nightly(github, ["health"]), 1)
        self.assertEqual(len(github.open_incidents()), 1)
        self.assertIn("- run 39168454114 attempt 1", github.open_incidents()[0]["body"])

    def test_recovery_comments_and_closes_once(self):
        github = FakeGitHub(runs=(completed_run(39125943231),))
        self.assertEqual(
            self.run_nightly(github, ["report"], result="failure", run_id="39125943231"), 1
        )
        incident = github.open_incidents()[0]
        github.runs = [completed_run(39125943231, conclusion="success")]
        self.assertEqual(self.run_nightly(github, ["health"]), 0)
        self.assertEqual(incident["state"], "closed")
        self.assertEqual(len(incident["comments"]), 1)
        self.assertIn("current and passed", incident["comments"][0])
        snapshot = json.dumps(github.issues)
        self.assertEqual(self.run_nightly(github, ["health"]), 0)
        self.assertEqual(json.dumps(github.issues), snapshot)

    def test_recovery_without_incident_writes_nothing(self):
        github = FakeGitHub(runs=(completed_run(39125943231, conclusion="success"),))
        self.assertEqual(self.run_nightly(github, ["health"]), 0)
        self.assertEqual(github.issues, [])

    def test_untrusted_marker_issues_are_ignored(self):
        spoofed = issue(
            1,
            body=legacy_body("fake", run_url(39125943231)),
            author={"login": "someone"},
        )
        github = FakeGitHub(issues=(spoofed,), runs=(completed_run(39125943231),))
        self.assertEqual(self.run_nightly(github, ["health"]), 1)
        self.assertEqual(spoofed["state"], "open")
        self.assertEqual(spoofed["labels"], [])
        self.assertEqual(len(github.open_incidents()), 1)
        self.assertNotEqual(github.open_incidents()[0]["number"], 1)

        github.runs = [completed_run(39125943231, conclusion="success")]
        self.assertEqual(self.run_nightly(github, ["health"]), 0)
        self.assertEqual(spoofed["state"], "open")
        self.assertEqual(spoofed["comments"], [])
        self.assertEqual(spoofed["labels"], [])

    def test_existing_incident_gets_only_missing_labels(self):
        github = FakeGitHub(labels=("bug", "@infra", "@test"), runs=(completed_run(39125943231),))
        self.assertEqual(
            self.run_nightly(github, ["report"], result="failure", run_id="39125943231"), 1
        )
        incident = github.open_incidents()[0]
        incident["labels"] = [{"name": "bug"}, {"name": "priority:high"}]
        body = incident["body"]
        self.assertEqual(self.run_nightly(github, ["health"]), 1)
        self.assertEqual(
            [label["name"] for label in incident["labels"]],
            ["bug", "priority:high", "@infra", "@test", "nightly fail"],
        )
        self.assertIn("nightly fail", [label["name"] for label in github.labels])
        self.assertEqual(incident["body"], body)
        self.assertEqual(incident["comments"], [])


if __name__ == "__main__":
    unittest.main()
