"""Fuzz budgets and the nightly corpus cache must stay explicit in CI."""

import re
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]


def read(relative: str) -> str:
    return (ROOT / relative).read_text()


class SeededFuzzScriptTests(unittest.TestCase):
    def test_reads_budgets_and_passes_them_to_cargo_fuzz(self):
        script = read("scripts/ci/seeded-fuzz.sh")
        self.assertRegex(script, r"FUZZ_MAX_TOTAL_TIME:-120")
        self.assertRegex(script, r"FUZZ_TIMEOUT:-10")
        self.assertRegex(script, r'-max_total_time=\s*"\$fuzz_max_total_time"')
        self.assertRegex(script, r'-timeout=\s*"\$fuzz_timeout"')


class NightlyFuzzTests(unittest.TestCase):
    def test_budgets_and_timeout_cover_every_target(self):
        workflow = read(".github/workflows/nightly.yml")
        self.assertRegex(workflow, r'FUZZ_MAX_TOTAL_TIME:\s*"120"')
        self.assertRegex(workflow, r'FUZZ_TIMEOUT:\s*"10"')
        self.assertRegex(workflow, r"timeout-minutes:\s*90")
        self.assertIn("bash scripts/ci/seeded-fuzz.sh", workflow)

    def test_corpus_is_restored_and_saved(self):
        workflow = read(".github/workflows/nightly.yml")
        for action in ("actions/cache/restore@", "actions/cache/save@"):
            self.assertRegex(workflow, re.escape(action) + r"[0-9a-f]{40}")
        self.assertRegex(workflow, r"(?m)^\s*path:\s*fuzz/corpus\s*$")
        self.assertRegex(
            workflow,
            r"(?m)^\s*key:\s*fuzz-corpus-\$\{\{\s*github\.run_id\s*\}\}\s*$",
        )
        self.assertRegex(workflow, r"(?m)^\s*restore-keys:\s*fuzz-corpus-\s*$")
        self.assertRegex(
            workflow,
            r"(?ms)actions/cache/save@[0-9a-f]{40}.*?if:\s*always\(\)",
        )


class QualificationFuzzTests(unittest.TestCase):
    def test_uses_a_larger_fixed_budget_without_the_nightly_cache(self):
        workflow = read(".github/workflows/qualification.yml")
        self.assertRegex(workflow, r'FUZZ_MAX_TOTAL_TIME:\s*"180"')
        self.assertIn("bash scripts/ci/seeded-fuzz.sh", workflow)
        self.assertNotIn("actions/cache", workflow)


if __name__ == "__main__":
    unittest.main()
