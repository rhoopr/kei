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
            (["src/lib.rs"], True, True, True),
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
            failed = subprocess.run(["python3", str(script)], cwd=root, env={**os.environ,
                "EVENT_NAME": "pull_request", "BASE_SHA": "invalid", "GITHUB_OUTPUT": str(root / "missing")}, capture_output=True, check=False)
            self.assertNotEqual(failed.returncode, 0)
            self.assertFalse((root / "missing").exists())


if __name__ == "__main__":
    unittest.main()
