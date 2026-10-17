"""Routing acceptance cases and the real CLI with a temporary git repository."""

import json
import os
import subprocess
import tempfile
import unittest
from pathlib import Path

from policy import route


class PolicyTests(unittest.TestCase):
    def test_pr_categories(self):
        cases = [
            (["README.md", "docs/ci.md"], False, False, False),
            (["src/auth/session.rs"], True, False, False),
            (["src/download/file/publication.rs"], True, True, True),
            (["src/state/db.rs"], True, True, True),
            (["src/lib.rs"], True, True, False),
            (["src/cli.rs"], True, True, False),
            (["src/types.rs"], True, True, True),
            (["src/fs_util.rs"], True, True, True),
            (["src/service/windows.rs"], True, True, False),
            (["src/icloud/photos/asset.rs"], True, False, True),
            (["Cargo.lock"], True, True, True),
            ([".dockerignore"], True, True, True),
            ([".gitignore"], True, True, True),
            ([".github/workflows/ci.yml"], True, True, True),
            (["scripts/check-contracts"], True, True, True),
            (["unrecognized-input"], True, True, True),
            (["tests/cli.rs"], True, False, False),
        ]
        for files, code, platform, stress in cases:
            with self.subTest(files=files):
                result = route(files, "pull_request", False, True, False)
                self.assertEqual((result["code"], result["platform"], result["stress"]), (code, platform, stress))
                self.assertFalse(result["coverage"])
                self.assertEqual("copies-stress" in result["required"], stress)
                self.assertEqual("test_no_default" in result["required"], code)

    def test_actual_pr_file_sets_keep_required_checks(self):
        # Exact GitHub file lists at heads 88ea35d (#984) and 30fe0e4 (#985).
        cases = [
            (984, [
                "docs/architecture.md",
                "src/download/orchestration/delta/album_progress_tests.rs",
                "src/download/orchestration/delta/tests.rs",
                "src/icloud/photos/album/changes.rs",
                "src/icloud/photos/album/hydration.rs",
                "src/icloud/photos/hydration_progress.rs",
                "src/icloud/photos/mod.rs",
                "src/icloud/photos/session.rs",
            ], False, True),
            (985, [
                "docs/architecture.md", "src/cli.rs", "src/commands/reset.rs",
                "src/lib.rs", "src/state/db/account/tests.rs",
                "tests/behavioral/state.rs", "tests/state_auth.rs",
            ], True, False),
        ]
        for pr, files, platform, stress in cases:
            with self.subTest(pr=pr):
                result = route(files, "pull_request", False, True, False)
                self.assertTrue(result["code"])
                self.assertTrue(result["cargo"])
                self.assertEqual(result["platform"], platform)
                self.assertEqual(result["stress"], stress)
                self.assertEqual("copies-stress" in result["required"], stress)
                self.assertFalse(result["qualification"])
                self.assertFalse(result["coverage"])
                self.assertFalse(result["unused"])
                for check in ("test", "test_no_default", "catalogs", "security", "lockfile"):
                    self.assertIn(check, result["required"])

    def test_stress_runtime_owners_and_fixture_helpers(self):
        paths = [
            "src/download/orchestration/generation.rs",
            "src/download/orchestration/queue_projection.rs",
            "src/download/orchestration/queue_projection/tests.rs",
            "src/download/orchestration/queue_projection/tests/late_io.rs",
            "src/download/orchestration/test_support.rs",
            "src/download/pipeline/consumer.rs", "src/download/file/publication.rs",
            "src/download/metadata_rewrite/queued.rs",
            "src/download/new_writer_owner.rs", "src/download/pipeline/tests/fixture.bin",
            "src/state/db/provider_selection.rs",
            "src/state/db/provider_inbox.rs", "src/state/db/provider_generations.rs",
            "src/state/db.rs", "src/state/schema.rs", "src/state/new_store.rs",
            "src/icloud/photos/asset.rs", "src/icloud/photos/album/lookup.rs",
            "src/icloud/photos/album/hydration.rs", "src/icloud/photos/session.rs",
            "src/icloud/photos/inbox.rs", "src/icloud/photos/new_adapter.rs",
            "src/icloud/error.rs",
            "src/commands/service/types.rs", "src/sync_loop/runner.rs",
            "src/sync_cycle.rs", "src/config.rs", "src/retry.rs",
            "src/selection.rs", "src/types.rs", "src/fs_util.rs",
            "src/test_helpers.rs", "tests/data/media/pattern.jpg",
        ]
        for path in paths:
            with self.subTest(path=path):
                result = route([path], "pull_request", False, True, False)
                self.assertTrue(result["stress"])
                self.assertIn("copies-stress", result["required"])

    def test_stress_skips_outer_commands_and_other_unit_test_modules(self):
        paths = [
            "src/cli.rs", "src/lib.rs", "src/commands/reset.rs",
            "src/commands/status.rs", "src/commands/list.rs", "src/account.rs",
            "src/icloud/service.rs", "src/auth/session.rs",
            "src/state/db/account/tests.rs", "src/state/db/connection_tests.rs",
            "src/download/orchestration/delta/album_progress_tests.rs",
            "src/download/orchestration/delta/tests.rs",
            "src/icloud/photos/album/lookup/tests.rs",
        ]
        for path in paths:
            with self.subTest(path=path):
                result = route([path], "pull_request", False, True, False)
                self.assertTrue(result["code"])
                self.assertFalse(result["stress"])
                self.assertNotIn("copies-stress", result["required"])
                self.assertIn("test", result["required"])
                self.assertIn("test_no_default", result["required"])
        mixed = route(["src/commands/reset.rs", "src/download/planner.rs"],
                      "pull_request", False, True, False)
        self.assertTrue(mixed["stress"])

    def test_stress_unknown_inputs_and_non_pr_events_fail_safe(self):
        for files in (["unknown-owner.rs"], ["new-build-input"], ["scripts/new-check"]):
            result = route(files, "pull_request", False, True, False)
            self.assertTrue(result["qualification"])
            self.assertTrue(result["stress"])
        for event in ("push", "schedule", "workflow_dispatch"):
            result = route([], event, False, True, False)
            self.assertTrue(result["stress"], event)

    def test_rollout_and_explicit_full_override(self):
        for light, full in [(False, False), (True, True)]:
            result = route(["src/auth/session.rs"], "pull_request", False, light, full)
            self.assertTrue(result["platform"])
            self.assertTrue(result["stress"])
            self.assertTrue(result["coverage"])

    def test_heavy_has_no_path_skips(self):
        for event in ("pull_request", "push", "schedule", "workflow_dispatch"):
            result = route([], event, True, True, False)
            for key in ("code", "cargo", "platform", "stress", "coverage", "unused"):
                self.assertTrue(result[key], (event, key))
            self.assertIn("catalogs", result["required"])
            self.assertIn("coverage-head", result["required"])

    def test_cli_reads_real_git_diff(self):
        script = Path(__file__).with_name("policy.py").resolve()
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            def git(*args):
                return subprocess.check_output(["git", *args], cwd=root, text=True).strip()
            git("init", "-q")
            git("-c", "user.name=CI Test", "-c", "user.email=ci@example.invalid", "commit", "--allow-empty", "-qm", "base")
            base = git("rev-parse", "HEAD")
            (root / "src").mkdir()
            (root / "src" / "auth").mkdir()
            (root / "src" / "auth" / "session.rs").write_text("// fixture\n")
            git("add", "src/auth/session.rs")
            git("-c", "user.name=CI Test", "-c", "user.email=ci@example.invalid", "commit", "-qm", "change")
            output = root / "outputs"
            subprocess.run(["python3", str(script)], cwd=root, env={**os.environ,
                "EVENT_NAME": "pull_request", "BASE_SHA": base, "LIGHT_ENABLED": "true",
                "HEAVY": "false", "FULL": "false", "GITHUB_OUTPUT": str(output)}, check=True)
            values = dict(line.split("=", 1) for line in output.read_text().splitlines())
            self.assertEqual(values["platform"], "false")
            self.assertIn("test_no_default", json.loads(values["required"]))
            # A move out of a writer owner must retain the old path as a trigger.
            writer = root / "src" / "download" / "pipeline" / "task.rs"
            writer.parent.mkdir(parents=True)
            writer.write_text("// writer fixture\n" * 100)
            git("add", "src/download/pipeline/task.rs")
            git("-c", "user.name=CI Test", "-c", "user.email=ci@example.invalid", "commit", "-qm", "writer")
            writer_base = git("rev-parse", "HEAD")
            git("mv", "src/download/pipeline/task.rs", "src/auth/renamed.rs")
            git("-c", "user.name=CI Test", "-c", "user.email=ci@example.invalid", "commit", "-qm", "move writer")
            self.assertIn("rename", git("diff", "--summary", writer_base, "HEAD"))
            moved_output = root / "moved-outputs"
            subprocess.run(["python3", str(script)], cwd=root, env={**os.environ,
                "EVENT_NAME": "pull_request", "BASE_SHA": writer_base, "LIGHT_ENABLED": "true",
                "HEAVY": "false", "FULL": "false", "GITHUB_OUTPUT": str(moved_output)}, check=True)
            moved = dict(line.split("=", 1) for line in moved_output.read_text().splitlines())
            self.assertEqual(moved["stress"], "true")
            self.assertIn("copies-stress", json.loads(moved["required"]))
            failed = subprocess.run(["python3", str(script)], cwd=root, env={**os.environ,
                "EVENT_NAME": "pull_request", "BASE_SHA": "invalid", "GITHUB_OUTPUT": str(root / "missing")}, capture_output=True, check=False)
            self.assertNotEqual(failed.returncode, 0)
            self.assertFalse((root / "missing").exists())


if __name__ == "__main__":
    unittest.main()
