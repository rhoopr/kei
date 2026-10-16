"""Release attestation rejects incomplete, stale, and wrong-commit evidence."""

import unittest
from datetime import datetime, timedelta, timezone

import local_release


class LocalReleaseTests(unittest.TestCase):
    def record(self):
        phases = local_release.BASE_PHASES | local_release.LIVE_COMMANDS | {"test_shell_cli_flags"}
        return {"head": "a" * 40, "end_head": "a" * 40, "start_worktree_clean": True,
                "end_worktree_clean": True, "phases": {p: {"status": "pass", "exit": 0, "tests": 1} for p in phases}}

    def test_complete_record(self):
        local_release.validate_record(self.record(), "a" * 40, ["cli-flags"])

    def test_rejects_skips_missing_phases_failure_and_dirty_head(self):
        for phase in self.record()["phases"]:
            for status in ("skipped", "rate_limited", "fail", None):
                record = self.record()
                record["phases"][phase]["status"] = status
                with self.subTest(phase=phase, status=status), self.assertRaises(ValueError):
                    local_release.validate_record(record, "a" * 40, ["cli-flags"])
        for field, value in [("head", "b" * 40), ("end_head", "b" * 40), ("start_worktree_clean", False), ("end_worktree_clean", False)]:
            record = self.record()
            record[field] = value
            with self.subTest(field=field), self.assertRaises(ValueError):
                local_release.validate_record(record, "a" * 40, ["cli-flags"])

    def test_zero_selected_live_tests_are_rejected(self):
        for name in ("live_provider", "test_shell_cli_flags"):
            record = self.record()
            record["phases"][name]["tests"] = 0
            with self.assertRaises(ValueError):
                local_release.validate_record(record, "a" * 40, ["cli-flags"])

    def test_latest_owner_attestation_only(self):
        now = datetime(2026, 10, 16, tzinfo=timezone.utc)
        success = {"context": local_release.CONTEXT, "state": "success", "creator": {"login": "owner"},
                   "created_at": now.isoformat(), "description": "local qualification sha256:" + "a" * 32}
        local_release.validate_statuses([success], "owner", now)
        bad = [{}, {**success, "state": "failure"}, {**success, "creator": {"login": "someone"}},
               {**success, "created_at": (now - timedelta(days=8)).isoformat()},
               {**success, "created_at": (now + timedelta(days=1)).isoformat()},
               {**success, "description": "unchecked"}]
        for value in bad:
            with self.subTest(value=value), self.assertRaises(ValueError):
                local_release.validate_statuses([value], "owner", now)
        with self.assertRaises(ValueError):
            local_release.validate_statuses([{**success, "state": "failure"}, success], "owner", now)


if __name__ == "__main__":
    unittest.main()
