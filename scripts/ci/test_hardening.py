"""Deliberate violations must fail the production workflow guard."""

import shutil
import subprocess
import tempfile
import unittest
from pathlib import Path


class HardeningTests(unittest.TestCase):
    def test_publication_and_required_job_mutations_fail(self):
        root = Path(__file__).resolve().parents[2]
        script = root / ".github/scripts/check_workflow_hardening.py"
        with tempfile.TemporaryDirectory() as directory:
            work = Path(directory)
            shutil.copytree(root / ".github/workflows", work / ".github/workflows")
            shutil.copyfile(root / "build.rs", work / "build.rs")
            def check():
                return subprocess.run(["python3", str(script)], cwd=work, capture_output=True, text=True, check=False)
            self.assertEqual(check().returncode, 0)
            for name, old, new, expected in [
                ("ci.yml", "actions/checkout@34e114876b0b11c390a56381ad16ebd13914f8d5", "actions/checkout@main", "not pinned"),
                ("ci.yml", "needs: [plan, checks, qualification]", "needs: [plan]", "missing qualification guard"),
                ("qualification.yml", "needs: [quality, artifacts, service, specialized]", "needs: [quality]", "missing qualification guard"),
                ("qualification.yml", "cargo fetch --locked", "echo missing bootstrap", "missing qualification guard"),
                ("release.yml", "needs: qualification", "needs: validate", "missing qualification guard"),
                ("service-smoke.yml", "service-smoke-${{ github.workflow }}-", "service-smoke-${{ github.workflow }}-${{ github.run_id }}-", "cancel obsolete runs"),
                ("nightly.yml", "contents: read", "contents: write", "publication credentials"),
            ]:
                path = work / ".github/workflows" / name
                original = path.read_text()
                self.assertIn(old, original)
                path.write_text(original.replace(old, new))
                result = check()
                self.assertNotEqual(result.returncode, 0, name)
                self.assertIn(expected, result.stderr)
                path.write_text(original)


if __name__ == "__main__":
    unittest.main()
