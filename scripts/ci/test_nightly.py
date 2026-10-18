"""Nightly freshness and notification transitions without remote writes."""

import json
import unittest
from datetime import datetime, timedelta, timezone
from unittest.mock import patch

import nightly


class NightlyTests(unittest.TestCase):
    def test_fresh_missing_failed_cancelled_and_stale(self):
        now = datetime(2026, 10, 16, tzinfo=timezone.utc)
        run = {"status": "completed", "head_branch": "main", "event": "schedule", "conclusion": "success",
               "created_at": now.isoformat(), "html_url": "https://github.com/example/repo/actions/runs/1"}
        self.assertTrue(nightly.health([run], now)[0])
        for runs in ([], [{**run, "status": "in_progress"}], [{**run, "conclusion": "failure"}],
                     [{**run, "conclusion": "cancelled"}], [{**run, "head_branch": "feature"}],
                     [{**run, "created_at": (now - timedelta(hours=37)).isoformat()}]):
            self.assertFalse(nightly.health(runs, now)[0])

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

    @patch.object(nightly, "ensure_nightly_label")
    @patch.object(nightly, "api")
    @patch.object(nightly.subprocess, "check_output")
    def test_only_failure_and_recovery_mutate_issues(self, output, api, ensure_label):
        output.return_value = b"[[]]"
        nightly.notify("example/repo", True, "healthy", "url")
        api.assert_not_called()
        ensure_label.assert_not_called()
        nightly.notify("example/repo", False, "failed", "url")
        api.assert_called_once_with("repos/example/repo/issues", title="Nightly qualification needs attention",
                                   body=f"{nightly.MARKER}\nfailed\n\nurl\n\nRelease qualification remains independent and must pass on the release commit.",
                                   labels=list(nightly.INCIDENT_LABELS))
        ensure_label.assert_called_once_with("example/repo")
        incident = {"number": 1, "body": nightly.MARKER, "user": {"login": "github-actions[bot]"},
                    "labels": [{"name": label} for label in nightly.INCIDENT_LABELS]}
        output.return_value = json.dumps([[incident]]).encode()
        api.reset_mock()
        ensure_label.reset_mock()
        nightly.notify("example/repo", False, "still failed", "url")
        api.assert_not_called()
        ensure_label.assert_not_called()
        nightly.notify("example/repo", True, "recovered", "url")
        self.assertEqual(api.call_count, 2)
        self.assertEqual(api.call_args.kwargs, {"method": "PATCH", "state": "closed"})
        ensure_label.assert_not_called()

    @patch.object(nightly, "ensure_nightly_label")
    @patch.object(nightly, "api")
    @patch.object(nightly.subprocess, "check_output")
    def test_existing_incident_gets_only_missing_labels_without_duplicate_or_comment(self, output, api, ensure_label):
        unrelated = {"number": 2, "body": nightly.MARKER, "user": {"login": "someone"}}
        incident = {"number": 1, "body": nightly.MARKER, "user": {"login": "github-actions[bot]"},
                    "labels": [{"name": "bug"}, {"name": "priority:high"}]}
        output.return_value = json.dumps([[unrelated], [incident]]).encode()
        nightly.notify("example/repo", False, "failed", "url")
        api.assert_called_once_with("repos/example/repo/issues/1/labels", labels=["@infra", "@test", "nightly fail"])
        ensure_label.assert_called_once_with("example/repo")
        self.assertIn({"name": "priority:high"}, incident["labels"])


if __name__ == "__main__":
    unittest.main()
