"""The same required baseline must be reachable on draft and ready PRs."""

import os
import re
import shutil
import subprocess
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
WORKFLOWS = ROOT / ".github/workflows"


class PullRequestTests(unittest.TestCase):
    def test_one_baseline_on_every_pr(self):
        workflow = (WORKFLOWS / "pr.yml").read_text()
        admission, jobs = workflow.split("\njobs:\n")
        self.assertIn("  pull_request:", admission)
        for event in ("opened", "reopened", "synchronize", "edited"):
            self.assertIn(event, admission)
        self.assertIn("cancel-in-progress: true", admission)
        # Two pushes to one PR must have the same key, even when their SHAs differ.
        self.assertRegex(admission, r"(?m)^  group: pr-baseline-\$\{\{ github.event.pull_request.number \}\}$")
        self.assertNotRegex(workflow, r"(?m)^\s+(?:paths(?:-ignore)?|branches(?:-ignore)?|ref):")
        self.assertNotRegex(workflow, r"pull_request\.(?:draft|labels)|CI_LIGHT_PR_ENABLED|ci:full")
        self.assertEqual(set(re.findall(r"(?m)^  ([\w-]+):$", jobs)), {"quality", "clippy", "smoke", "ci"})
        for job in ("quality", "clippy", "smoke"):
            body = re.split(r"(?m)^  [\w-]+:\n", jobs.split(f"  {job}:\n")[1], maxsplit=1)[0]
            self.assertNotRegex(body, r"(?m)^    needs:", "baseline jobs must start independently")
        for name in ("ci.yml", "checks.yml", "qualification.yml", "artifacts.yml", "service-smoke.yml"):
            self.assertNotIn("  pull_request:", (WORKFLOWS / name).read_text())

    def test_smoke_rejects_incomplete_libtest_success(self):
        # Stale filters, #[ignore], and a loopback fixture's early return can all exit 0.
        with tempfile.TemporaryDirectory() as directory:
            cargo = Path(directory) / "cargo"
            cargo.write_text('#!/bin/sh\nprintf "%s\\n" "$LIBTEST_OUTPUT"\nexit "$CARGO_STATUS"\n')
            cargo.chmod(0o755)
            names = [
                "download::file::publication::tests::contract_file_publish_no_overwrite_destination_already_exists",
                "sync_core::tests::an_interrupted_listing_resumes_from_its_last_saved_page",
                "download::file::transfer::tests::attempt_download_happy_path_writes_and_renames",
                "sync_core::state::tests::example", "sync_core::plan::tests::example",
                "download::metadata_pass::tests::example", "icloud::photos::asset::example",
                "download::paths::example", "commands::import::example", "selection::tests::example",
                "fs_util::tests::example",
            ]
            success = "\n".join(f"test {name} ... ok" for name in names)
            success += "\ntest result: ok. 11 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out;"
            skipped = "test fixture ... skipping sync_loop::tests: loopback bind is not permitted on this host: denied\n"
            for output, code, expected in [
                (success, 0, True), (success, 1, False), (success.replace("commands::import::", "other::"), 0, False),
                (success.replace("its_last_saved_page", "other"), 0, False),
                (success.replace("0 ignored", "1 ignored"), 0, False),
                (skipped + success, 0, False),
            ]:
                with self.subTest(output=output, code=code):
                    result = subprocess.run(["bash", str(ROOT / "scripts/ci/pr-smoke.sh")], env={
                        **os.environ, "PATH": f"{directory}:{os.environ['PATH']}", "LIBTEST_OUTPUT": output,
                        "CARGO_STATUS": str(code),
                    }, capture_output=True, check=False)
                    self.assertEqual(result.returncode == 0, expected, result.stderr)

    def test_contract_catalog_rejects_missing_production_owner(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for name in ("scripts", "docs", "src"):
                (root / name).mkdir()
            checker = root / "scripts/check-contracts"
            shutil.copyfile(ROOT / "scripts/check-contracts", checker)
            catalog = root / "docs/architecture.md"
            catalog.write_text("| `SAFE_MEDIA` | synthetic contract |\n")
            (root / "src/owner.rs").write_text("// CONTRACT: SAFE_MEDIA\nfn contract_safe_media() {}\n")
            command = ["python3", str(checker)]
            self.assertEqual(subprocess.run(command, capture_output=True, check=False).returncode, 0)
            catalog.write_text("| `MISSING_OWNER` | invalid synthetic contract |\n")
            result = subprocess.run(command, capture_output=True, text=True, check=False)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("documented contract has no production owner marker", result.stderr)
