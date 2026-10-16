"""Nightly freshness and notification transitions without remote writes."""

from datetime import datetime, timedelta, timezone
import json
import unittest
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

    @patch.object(nightly, "api")
    @patch.object(nightly.subprocess, "check_output")
    def test_only_failure_and_recovery_mutate_issues(self, output, api):
        output.return_value = b"[[]]"
        nightly.notify("example/repo", True, "healthy", "url")
        api.assert_not_called()
        nightly.notify("example/repo", False, "failed", "url")
        self.assertEqual(api.call_count, 1)
        incident = {"number": 1, "body": nightly.MARKER, "user": {"login": "github-actions[bot]"}}
        output.return_value = json.dumps([[incident]]).encode()
        api.reset_mock()
        nightly.notify("example/repo", False, "still failed", "url")
        api.assert_not_called()
        nightly.notify("example/repo", True, "recovered", "url")
        self.assertEqual(api.call_count, 2)
        self.assertEqual(api.call_args.kwargs, {"state": "closed"})


if __name__ == "__main__":
    unittest.main()
