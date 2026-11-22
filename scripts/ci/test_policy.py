"""Exercise the shared main and qualification gate with actual job results."""

import json
import os
import subprocess
import unittest
from pathlib import Path


def run_gate(results, heavy, light):
    return subprocess.run(
        ["python3", str(Path(__file__).with_name("policy.py"))],
        env={**os.environ, "HEAVY": str(heavy).lower(), "LIGHT_ENABLED": str(light).lower(),
             "NEEDS": json.dumps(results)}, capture_output=True, check=False,
    ).returncode


class PolicyTests(unittest.TestCase):
    def test_shared_gate_requires_all_work_selected_for_the_event(self):
        jobs = ["fmt", "roundtrip", "clippy", "test_no_default", "test", "docs", "catalogs", "lockfile",
                "security", "copies-stress", "workflow-hardening", "script-lint", "typos"]
        for heavy, light in [(False, True), (False, False), (True, True), (True, False)]:
            required = jobs + (["unused-deps"] if heavy or not light else [])
            needs = {name: {"result": "success" if name in required else "skipped"}
                     for name in jobs + ["unused-deps"]}

            with self.subTest(heavy=heavy, light=light):
                self.assertEqual(run_gate(needs, heavy, light), 0)
                for job in required:
                    self.assertNotEqual(run_gate({**needs, job: {"result": "skipped"}}, heavy, light), 0, job)
